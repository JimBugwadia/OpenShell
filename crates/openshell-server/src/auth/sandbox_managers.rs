// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Per-workspace sandbox managers: the identities allowed to provision and
//! change a workspace's sandboxes.
//!
//! A workspace without a list keeps the default behavior, where any
//! authorized caller may provision. A listed workspace makes its identities
//! the only writers there, so whether every sandbox has a declared owner is a
//! property of a workspace, never of the whole gateway. Everyone else keeps
//! read and debug access, and operators or holders of the configured admin
//! role keep an emergency stop and delete.

use std::collections::BTreeMap;

use openshell_core::proto::Sandbox;
use prost::Message;

use crate::auth::identity::{Identity, IdentityProvider};
use crate::auth::principal::Principal;
use crate::persistence::{ObjectCursor, ObjectType, Store};

/// Annotation that records which identity created a sandbox.
pub const SANDBOX_MANAGER_ANNOTATION: &str = "internal.openshell.ai/sandbox-manager";

/// Annotation that records the workspace's managers list in effect when a
/// sandbox was created, so the gateway can tell when the list changed under it.
pub const SANDBOX_MANAGERS_CONFIG_ANNOTATION: &str = "internal.openshell.ai/sandbox-managers";

/// Error message for callers that aren't a workspace's sandbox managers.
pub const NOT_A_MANAGER: &str =
    "this workspace's sandboxes are managed by another identity; use it to make this change";

/// Error message for a manager changing another manager's sandbox.
pub const NOT_THE_RECORDED_MANAGER: &str =
    "this sandbox is managed by another identity; use it to make this change";

const SCAN_PAGE_SIZE: u32 = 1000;

/// Canonical, order-independent form of a managers list, recorded on each
/// sandbox. An unlisted workspace, the default, is `[]`.
#[must_use]
pub fn canonical(managers: &[String]) -> String {
    let mut sorted = managers.to_vec();
    sorted.sort();
    sorted.dedup();
    serde_json::to_string(&sorted).unwrap_or_default()
}

/// Refuse to start when any existing sandbox was created under a different
/// managers list for its workspace. Sandboxes created before the list was
/// recorded count as created under the default, empty list.
///
/// # Errors
///
/// Returns a description of the change when it isn't safe, or of the store
/// failure that prevented the check.
pub async fn verify_unchanged(
    store: &Store,
    managers: &BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    let mut cursor = None;
    loop {
        let records = store
            .list_by_type_after(Sandbox::object_type(), cursor.as_ref(), SCAN_PAGE_SIZE)
            .await
            .map_err(|error| format!("could not check sandbox managers: {error}"))?;
        cursor = records.last().map(ObjectCursor::from);
        for record in &records {
            // Fail closed: an unreadable record could hide a different list.
            let sandbox = Sandbox::decode(record.payload.as_slice()).map_err(|error| {
                format!(
                    "could not check sandbox managers: sandbox {}/{} is unreadable: {error}",
                    record.workspace, record.name
                )
            })?;
            let recorded = sandbox
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.annotations.get(SANDBOX_MANAGERS_CONFIG_ANNOTATION))
                .map_or("[]", String::as_str);
            let current = canonical(
                managers
                    .get(&record.workspace)
                    .map_or(&[][..], Vec::as_slice),
            );
            if recorded != current {
                return Err(format!(
                    "openshell.gateway.sandbox_managers for workspace {} changed from \
                     {recorded} to {current} while it has sandboxes (for example {}/{}); \
                     restore the previous list, or delete the workspace's sandboxes before \
                     changing it",
                    record.workspace, record.workspace, record.name
                ));
            }
        }
        if records.len() < SCAN_PAGE_SIZE as usize {
            return Ok(());
        }
    }
}

/// Provider-qualified, stable name for a caller, as written in the gateway
/// configuration: `oidc:<sub>`, `mtls:<certificate CN>`,
/// `operator:<certificate CN>`, `cloudflare_access:<sub>`, or
/// `local_dev:<subject>`.
#[must_use]
pub fn manager_id(principal: &Principal) -> Option<String> {
    match principal {
        Principal::User(user) => Some(qualified(&user.identity)),
        Principal::Operator(operator) => Some(format!("operator:{}", operator.identity.subject)),
        _ => None,
    }
}

fn qualified(identity: &Identity) -> String {
    let provider = match identity.provider {
        IdentityProvider::Oidc => "oidc",
        IdentityProvider::Mtls => "mtls",
        IdentityProvider::CloudflareAccess => "cloudflare_access",
        IdentityProvider::LocalDev => "local_dev",
    };
    format!("{provider}:{}", identity.subject)
}

/// Prefixes accepted in the `sandbox_managers` setting.
pub const MANAGER_ID_PREFIXES: &[&str] = &[
    "oidc:",
    "mtls:",
    "operator:",
    "cloudflare_access:",
    "local_dev:",
];

/// The configured sandbox managers, by workspace name.
#[derive(Debug, Clone, Default)]
pub struct SandboxManagers {
    managers: BTreeMap<String, Vec<String>>,
    admin_role: String,
}

impl SandboxManagers {
    /// `admin_role` is the configured OIDC admin role; empty means no user has
    /// emergency access, so an unset role never grants it to everyone.
    #[must_use]
    pub fn new(managers: BTreeMap<String, Vec<String>>, admin_role: impl Into<String>) -> Self {
        Self {
            managers,
            admin_role: admin_role.into(),
        }
    }

    /// The managers list for `workspace`; empty when it has none.
    #[must_use]
    pub fn for_workspace(&self, workspace: &str) -> &[String] {
        self.managers.get(workspace).map_or(&[], Vec::as_slice)
    }

    /// Whether `workspace` restricts provisioning to listed identities.
    #[must_use]
    pub fn is_enforced(&self, workspace: &str) -> bool {
        !self.for_workspace(workspace).is_empty()
    }

    /// Whether `principal` is one of `workspace`'s managers.
    #[must_use]
    pub fn is_manager(&self, principal: &Principal, workspace: &str) -> bool {
        manager_id(principal).is_some_and(|id| self.for_workspace(workspace).contains(&id))
    }

    /// Whether `principal` may provision sandboxes in `workspace`. Sandbox,
    /// peer, and supervisor principals have their own method limits.
    #[must_use]
    pub fn permits_changes(&self, principal: &Principal, workspace: &str) -> bool {
        !self.is_enforced(workspace)
            || !matches!(principal, Principal::User(_) | Principal::Operator(_))
            || self.is_manager(principal, workspace)
    }

    /// Whether `principal` may change one sandbox in `workspace`, given the
    /// manager recorded on it. Only that manager may, apart from emergency
    /// stop and delete. A sandbox with no recorded manager can't exist in an
    /// enforced workspace, because the gateway refuses to start after a
    /// workspace's list changes.
    #[must_use]
    pub fn permits_sandbox(
        &self,
        principal: &Principal,
        workspace: &str,
        recorded_manager: Option<&str>,
        emergency: bool,
    ) -> bool {
        if !self.is_enforced(workspace)
            || !matches!(principal, Principal::User(_) | Principal::Operator(_))
        {
            return true;
        }
        if emergency && self.has_emergency_access(principal) {
            return true;
        }
        manager_id(principal).is_some_and(|id| recorded_manager == Some(id.as_str()))
    }

    fn has_emergency_access(&self, principal: &Principal) -> bool {
        match principal {
            Principal::Operator(_) => true,
            Principal::User(user) => {
                !self.admin_role.is_empty() && user.identity.roles.contains(&self.admin_role)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::principal::test_support::{sandbox, user};
    use crate::auth::principal::{OperatorPrincipal, UserPrincipal};

    fn mtls_user(cn: &str, roles: &[&str]) -> Principal {
        Principal::User(UserPrincipal {
            identity: Identity {
                subject: cn.to_string(),
                display_name: None,
                roles: roles.iter().map(ToString::to_string).collect(),
                scopes: vec![],
                provider: IdentityProvider::Mtls,
            },
        })
    }

    fn operator(cn: &str) -> Principal {
        Principal::Operator(OperatorPrincipal {
            identity: Identity {
                subject: cn.to_string(),
                display_name: None,
                roles: vec!["operator".into()],
                scopes: vec![],
                provider: IdentityProvider::Mtls,
            },
            certificate_sha256: "fingerprint".into(),
        })
    }

    fn lists(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(workspace, ids)| {
                (
                    (*workspace).to_string(),
                    ids.iter().map(ToString::to_string).collect(),
                )
            })
            .collect()
    }

    fn managed() -> SandboxManagers {
        SandboxManagers::new(
            lists(&[("agents", &["mtls:k8s-controller"])]),
            "openshell-admin",
        )
    }

    #[test]
    fn unlisted_workspaces_keep_default_behavior() {
        let managers = managed();
        assert!(!managers.is_enforced("default"));
        assert!(managers.permits_changes(&mtls_user("cli", &[]), "default"));
        assert!(managers.permits_sandbox(&user("alice"), "default", Some("oidc:bob"), false));
    }

    #[test]
    fn only_listed_identities_provision_in_a_listed_workspace() {
        let managers = managed();
        assert!(managers.is_enforced("agents"));
        assert!(managers.permits_changes(&mtls_user("k8s-controller", &[]), "agents"));
        assert!(!managers.permits_changes(&mtls_user("cli", &[]), "agents"));
        // A workspace's manager has no special standing in another workspace.
        let other = SandboxManagers::new(
            lists(&[
                ("agents", &["mtls:k8s-controller"]),
                ("sessions", &["mtls:sessions"]),
            ]),
            "",
        );
        assert!(!other.permits_changes(&mtls_user("k8s-controller", &[]), "sessions"));
    }

    #[test]
    fn identities_are_qualified_by_provider() {
        // An OIDC subject that matches the certificate CN is a different identity.
        assert!(!managed().permits_changes(&user("k8s-controller"), "agents"));
        assert_eq!(
            manager_id(&user("k8s-controller")).as_deref(),
            Some("oidc:k8s-controller")
        );
    }

    #[test]
    fn sandbox_principals_are_unaffected() {
        assert!(managed().permits_changes(&sandbox("sandbox-1"), "agents"));
        assert!(managed().permits_sandbox(&sandbox("sandbox-1"), "agents", None, false));
    }

    #[test]
    fn only_the_recorded_manager_changes_a_sandbox() {
        let managers = SandboxManagers::new(
            lists(&[("agents", &["mtls:cluster-a", "mtls:cluster-b"])]),
            "openshell-admin",
        );
        let a = mtls_user("cluster-a", &[]);
        let b = mtls_user("cluster-b", &[]);
        assert!(managers.permits_sandbox(&a, "agents", Some("mtls:cluster-a"), false));
        assert!(!managers.permits_sandbox(&b, "agents", Some("mtls:cluster-a"), false));
        assert!(!managers.permits_sandbox(&b, "agents", Some("mtls:cluster-a"), true));
        assert!(!managers.permits_sandbox(&a, "agents", None, false));
    }

    #[test]
    fn administrators_keep_emergency_stop_and_delete_only() {
        let managers = managed();
        let admin = mtls_user("oncall", &["openshell-admin"]);
        let recorded = Some("mtls:k8s-controller");
        assert!(managers.permits_sandbox(&admin, "agents", recorded, true));
        assert!(!managers.permits_sandbox(&admin, "agents", recorded, false));
        assert!(!managers.permits_changes(&admin, "agents"));
        assert!(managers.permits_sandbox(&operator("oncall"), "agents", recorded, true));
        assert!(!managers.permits_changes(&operator("oncall"), "agents"));
    }

    #[test]
    fn unset_admin_role_grants_no_emergency_access() {
        let managers = SandboxManagers::new(lists(&[("agents", &["mtls:k8s-controller"])]), "");
        let admin = mtls_user("cli", &["openshell-admin"]);
        assert!(!managers.permits_sandbox(&admin, "agents", Some("mtls:k8s-controller"), true));
    }

    #[test]
    fn canonical_list_ignores_order_and_duplicates() {
        let a = canonical(&["mtls:b".into(), "mtls:a".into(), "mtls:a".into()]);
        assert_eq!(a, canonical(&["mtls:a".into(), "mtls:b".into()]));
        assert_eq!(a, r#"["mtls:a","mtls:b"]"#);
        assert_eq!(canonical(&[]), "[]");
    }

    #[tokio::test]
    async fn startup_refuses_a_changed_list_for_a_workspace_with_sandboxes() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        let k8s = lists(&[("agents", &["mtls:k8s"])]);
        // An empty gateway accepts any lists.
        verify_unchanged(&store, &k8s).await.unwrap();

        let sandbox = Sandbox {
            metadata: Some(openshell_core::proto::datamodel::v1::ObjectMeta {
                id: "sb-1".into(),
                name: "agent".into(),
                workspace: "agents".into(),
                annotations: [(
                    SANDBOX_MANAGERS_CONFIG_ANNOTATION.to_string(),
                    canonical(&["mtls:k8s".into()]),
                )]
                .into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        store.put_message(&sandbox).await.unwrap();

        verify_unchanged(&store, &k8s).await.unwrap();
        // Other workspaces can change freely while they have no sandboxes.
        verify_unchanged(
            &store,
            &lists(&[("agents", &["mtls:k8s"]), ("sessions", &["mtls:s"])]),
        )
        .await
        .unwrap();
        for changed in [lists(&[]), lists(&[("agents", &["mtls:other"])])] {
            let error = verify_unchanged(&store, &changed).await.unwrap_err();
            assert!(error.contains("agents/agent"), "{error}");
        }
    }
}
