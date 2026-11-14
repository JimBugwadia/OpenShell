// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Install-time sandbox managers: the identities allowed to provision and
//! change a gateway's sandboxes.
//!
//! An empty list keeps the default behavior, where any authorized caller may
//! provision. A non-empty list makes the listed identities the only writers.
//! Everyone else keeps read and debug access, and operators or holders of the
//! configured admin role keep an emergency stop and delete.

use openshell_core::proto::Sandbox;
use prost::Message;

use crate::auth::identity::{Identity, IdentityProvider};
use crate::auth::principal::Principal;
use crate::persistence::{ObjectCursor, ObjectType, Store};

/// Annotation that records which identity created a sandbox.
pub const SANDBOX_MANAGER_ANNOTATION: &str = "internal.openshell.ai/sandbox-manager";

/// Annotation that records the managers list in effect when a sandbox was
/// created, so the gateway can tell when the list changed under it.
pub const SANDBOX_MANAGERS_CONFIG_ANNOTATION: &str = "internal.openshell.ai/sandbox-managers";

/// Sandbox-changing methods restricted to managers.
///
/// `UpdateConfig` is checked in its handler, because its scope (one sandbox
/// or the whole gateway) is in the request body.
const MANAGED_METHODS: &[&str] = &[
    "/openshell.v1.OpenShell/CreateSandbox",
    "/openshell.v1.OpenShell/BeginRootfsTarStaging",
    "/openshell.v1.OpenShell/DeleteSandbox",
    "/openshell.v1.OpenShell/StopSandbox",
    "/openshell.v1.OpenShell/StartSandbox",
    "/openshell.v1.OpenShell/SuspendSandbox",
    "/openshell.v1.OpenShell/ResumeSandbox",
    "/openshell.v1.OpenShell/AttachSandboxProvider",
    "/openshell.v1.OpenShell/DetachSandboxProvider",
    "/openshell.v1.OpenShell/ExposeService",
    "/openshell.v1.OpenShell/DeleteService",
    "/openshell.v1.OpenShell/ApproveDraftChunk",
    "/openshell.v1.OpenShell/RejectDraftChunk",
    "/openshell.v1.OpenShell/ApproveAllDraftChunks",
    "/openshell.v1.OpenShell/EditDraftChunk",
    "/openshell.v1.OpenShell/UndoDraftChunk",
    "/openshell.v1.OpenShell/ClearDraftChunks",
    "/openshell.v1.SandboxUpdates/UpdateSandbox",
];

/// Methods an administrator may still call on a managed gateway.
const EMERGENCY_METHODS: &[&str] = &[
    "/openshell.v1.OpenShell/StopSandbox",
    "/openshell.v1.OpenShell/DeleteSandbox",
];

/// Error message for callers that aren't sandbox managers.
pub const NOT_A_MANAGER: &str =
    "this gateway's sandboxes are managed by another identity; use it to make this change";

/// Error message for a manager changing another manager's sandbox.
pub const NOT_THE_RECORDED_MANAGER: &str =
    "this sandbox is managed by another identity; use it to make this change";

const SCAN_PAGE_SIZE: u32 = 1000;

/// Canonical, order-independent form of a managers list, recorded on each
/// sandbox. An empty list, the default, is `[]`.
#[must_use]
pub fn canonical(managers: &[String]) -> String {
    let mut sorted = managers.to_vec();
    sorted.sort();
    sorted.dedup();
    serde_json::to_string(&sorted).unwrap_or_default()
}

/// Refuse to start when any existing sandbox was created under a different
/// managers list. Sandboxes created before the list was recorded count as
/// created under the default, empty list.
///
/// # Errors
///
/// Returns a description of the change when it isn't safe, or of the store
/// failure that prevented the check.
pub async fn verify_unchanged(store: &Store, managers: &[String]) -> Result<(), String> {
    let current = canonical(managers);
    let mut cursor = None;
    loop {
        let records = store
            .list_by_type_after(Sandbox::object_type(), cursor.as_ref(), SCAN_PAGE_SIZE)
            .await
            .map_err(|error| format!("could not check sandbox managers: {error}"))?;
        cursor = records.last().map(ObjectCursor::from);
        for record in &records {
            let Ok(sandbox) = Sandbox::decode(record.payload.as_slice()) else {
                continue;
            };
            let recorded = sandbox
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.annotations.get(SANDBOX_MANAGERS_CONFIG_ANNOTATION))
                .map_or("[]", String::as_str);
            if recorded != current {
                return Err(format!(
                    "openshell.gateway.sandbox_managers changed from {recorded} to {current} \
                     while sandboxes exist (for example {}/{}); restore the previous list, \
                     or delete the gateway's sandboxes before changing it",
                    record.workspace, record.name
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

/// The configured sandbox managers.
#[derive(Debug, Clone, Default)]
pub struct SandboxManagers {
    managers: Vec<String>,
    admin_role: String,
}

impl SandboxManagers {
    /// `admin_role` is the configured OIDC admin role; empty means no user has
    /// emergency access, so an unset role never grants it to everyone.
    #[must_use]
    pub fn new(managers: Vec<String>, admin_role: impl Into<String>) -> Self {
        Self {
            managers,
            admin_role: admin_role.into(),
        }
    }

    /// Whether the gateway restricts provisioning to listed identities.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        !self.managers.is_empty()
    }

    /// Whether `principal` is a listed manager.
    #[must_use]
    pub fn is_manager(&self, principal: &Principal) -> bool {
        manager_id(principal).is_some_and(|id| self.managers.contains(&id))
    }

    /// Whether `principal` may change sandboxes, apart from emergency access.
    /// Sandbox, peer, and supervisor principals have their own method limits.
    #[must_use]
    pub fn permits_changes(&self, principal: &Principal) -> bool {
        !self.is_enforced()
            || !matches!(principal, Principal::User(_) | Principal::Operator(_))
            || self.is_manager(principal)
    }

    /// Whether `principal` may call the method at `path`.
    #[must_use]
    pub fn permits(&self, principal: &Principal, path: &str) -> bool {
        if !MANAGED_METHODS.contains(&path) || self.permits_changes(principal) {
            return true;
        }
        EMERGENCY_METHODS.contains(&path) && self.has_emergency_access(principal)
    }

    /// Whether `principal` may change one sandbox, given the manager recorded
    /// on it. Only that manager may, apart from emergency stop and delete.
    /// A sandbox with no recorded manager can't exist under an enforced list,
    /// because the gateway refuses to start after the list changes.
    #[must_use]
    pub fn permits_sandbox(
        &self,
        principal: &Principal,
        recorded_manager: Option<&str>,
        emergency: bool,
    ) -> bool {
        if !self.is_enforced() || !matches!(principal, Principal::User(_) | Principal::Operator(_))
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

    const CREATE: &str = "/openshell.v1.OpenShell/CreateSandbox";
    const DELETE: &str = "/openshell.v1.OpenShell/DeleteSandbox";
    const START: &str = "/openshell.v1.OpenShell/StartSandbox";
    const GET: &str = "/openshell.v1.OpenShell/GetSandbox";
    const EXEC: &str = "/openshell.v1.OpenShell/ExecSandbox";

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

    fn managed() -> SandboxManagers {
        SandboxManagers::new(vec!["mtls:k8s-controller".into()], "openshell-admin")
    }

    #[test]
    fn empty_list_keeps_default_behavior() {
        let managers = SandboxManagers::new(vec![], "openshell-admin");
        assert!(!managers.is_enforced());
        assert!(managers.permits(&user("alice"), CREATE));
        assert!(managers.permits(&mtls_user("cli", &[]), DELETE));
    }

    #[test]
    fn only_listed_identities_change_sandboxes() {
        let managers = managed();
        assert!(managers.permits(&mtls_user("k8s-controller", &[]), CREATE));
        assert!(managers.permits(&mtls_user("k8s-controller", &[]), START));
        assert!(!managers.permits(&mtls_user("cli", &[]), CREATE));
        assert!(!managers.permits(&mtls_user("cli", &[]), START));
    }

    #[test]
    fn identities_are_qualified_by_provider() {
        let managers = managed();
        // An OIDC subject that matches the certificate CN is a different identity.
        assert!(!managers.permits(&user("k8s-controller"), CREATE));
        assert_eq!(
            manager_id(&user("k8s-controller")).as_deref(),
            Some("oidc:k8s-controller")
        );
    }

    #[test]
    fn everyone_keeps_read_and_debug_access() {
        let managers = managed();
        for path in [GET, EXEC, "/openshell.v1.OpenShell/ListSandboxes"] {
            assert!(managers.permits(&mtls_user("cli", &[]), path));
        }
    }

    #[test]
    fn sandbox_principals_are_unaffected() {
        assert!(managed().permits(&sandbox("sandbox-1"), CREATE));
    }

    #[test]
    fn administrators_keep_emergency_stop_and_delete_only() {
        let managers = managed();
        let admin = mtls_user("oncall", &["openshell-admin"]);
        assert!(managers.permits(&admin, DELETE));
        assert!(managers.permits(&admin, "/openshell.v1.OpenShell/StopSandbox"));
        assert!(!managers.permits(&admin, CREATE));
        assert!(!managers.permits(&admin, START));
        assert!(managers.permits(&operator("oncall"), DELETE));
        assert!(!managers.permits(&operator("oncall"), CREATE));
    }

    #[test]
    fn only_the_recorded_manager_changes_a_sandbox() {
        let managers = SandboxManagers::new(
            vec!["mtls:cluster-a".into(), "mtls:cluster-b".into()],
            "openshell-admin",
        );
        let a = mtls_user("cluster-a", &[]);
        let b = mtls_user("cluster-b", &[]);
        assert!(managers.permits_sandbox(&a, Some("mtls:cluster-a"), false));
        assert!(!managers.permits_sandbox(&b, Some("mtls:cluster-a"), false));
        assert!(!managers.permits_sandbox(&b, Some("mtls:cluster-a"), true));
        assert!(!managers.permits_sandbox(&a, None, false));
        let admin = mtls_user("oncall", &["openshell-admin"]);
        assert!(managers.permits_sandbox(&admin, Some("mtls:cluster-a"), true));
        assert!(!managers.permits_sandbox(&admin, Some("mtls:cluster-a"), false));
    }

    #[test]
    fn recorded_manager_is_ignored_without_a_list() {
        let managers = SandboxManagers::new(vec![], "");
        assert!(managers.permits_sandbox(&user("alice"), Some("oidc:bob"), false));
        assert!(managed().permits_sandbox(&sandbox("sandbox-1"), None, false));
    }

    #[test]
    fn canonical_list_ignores_order_and_duplicates() {
        let a = canonical(&["mtls:b".into(), "mtls:a".into(), "mtls:a".into()]);
        assert_eq!(a, canonical(&["mtls:a".into(), "mtls:b".into()]));
        assert_eq!(a, r#"["mtls:a","mtls:b"]"#);
        assert_eq!(canonical(&[]), "[]");
    }

    #[tokio::test]
    async fn startup_refuses_a_changed_list_while_sandboxes_exist() {
        let store = Store::connect("sqlite::memory:").await.unwrap();
        // An empty gateway accepts any list.
        verify_unchanged(&store, &["mtls:k8s".into()])
            .await
            .unwrap();

        let sandbox = Sandbox {
            metadata: Some(openshell_core::proto::datamodel::v1::ObjectMeta {
                id: "sb-1".into(),
                name: "agent".into(),
                workspace: "default".into(),
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

        verify_unchanged(&store, &["mtls:k8s".into()])
            .await
            .unwrap();
        for changed in [vec![], vec!["mtls:other".to_string()]] {
            let error = verify_unchanged(&store, &changed).await.unwrap_err();
            assert!(error.contains("default/agent"), "{error}");
        }
    }

    #[test]
    fn unset_admin_role_grants_no_emergency_access() {
        let managers = SandboxManagers::new(vec!["mtls:k8s-controller".into()], "");
        assert!(!managers.permits(&mtls_user("cli", &["openshell-admin"]), DELETE));
    }
}
