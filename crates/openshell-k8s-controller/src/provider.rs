// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `OpenShellProvider`: a gateway provider declared in Kubernetes.
//!
//! The resource names its credentials and refresh material by reference to
//! Kubernetes Secrets in its namespace, so values never appear in the resource
//! or its status. The controller reads the Secrets, creates or updates the
//! gateway provider with the resource's name, and applies it again whenever the
//! spec or a referenced Secret changes.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Condition, ConditionStatus, GatewayError, sync_condition};

/// Interval for rechecking a provider that is applied or blocked. Spec and
/// Secret changes still trigger an immediate reconcile.
const SETTLED_SECONDS: u64 = 60 * 60;
const BLOCKED_SECONDS: u64 = 5 * 60;
const UNCERTAIN_SECONDS: u64 = 30;
const MISSING_SECRET_SECONDS: u64 = 60;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "openshell.ai",
    version = "v1alpha1",
    kind = "OpenShellProvider",
    plural = "openshellproviders",
    shortname = "osprov",
    namespaced,
    status = "ProviderStatus",
    printcolumn = r#"{"name":"Type","type":"string","jsonPath":".spec.type"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.reason"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [
    {
        "rule": "self.type == oldSelf.type && (has(self.globalProfile) && self.globalProfile) == (has(oldSelf.globalProfile) && oldSelf.globalProfile)",
        "message": "type and globalProfile are immutable; create a new provider instead"
    },
    {
        "rule": "!has(self.refresh) || !has(self.credentials) || self.refresh.all(r, !self.credentials.exists(c, c.name == r.credential))",
        "message": "a refreshed credential is minted by the gateway; don't also list it in credentials"
    }
]))]
pub struct ProviderSpec {
    /// Provider type profile ID, for example "nvidia" or "github".
    #[serde(rename = "type")]
    #[schemars(length(min = 1, max = 128))]
    pub type_: String,
    /// Use the type profile from the gateway's global catalog instead of one
    /// imported into the workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub global_profile: bool,
    /// Credentials the gateway injects for sandboxes that use this provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub credentials: Vec<CredentialSource>,
    /// Non-secret provider settings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
    /// Gateway-owned refresh for credentials the gateway mints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub refresh: Vec<RefreshSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSource {
    /// Credential key, as the provider type profile names it.
    #[schemars(length(min = 1, max = 256))]
    pub name: String,
    pub secret_key_ref: SecretKeyRef,
}

/// A key in a Secret in the provider's namespace.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyRef {
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
    #[schemars(length(min = 1, max = 253))]
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RefreshSource {
    /// Credential key that the gateway mints and refreshes.
    #[schemars(length(min = 1, max = 256))]
    pub credential: String,
    pub strategy: RefreshStrategy,
    /// Material the strategy needs, such as a client ID or a refresh token.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub material: Vec<MaterialSource>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum RefreshStrategy {
    OAuth2RefreshToken,
    OAuth2ClientCredentials,
    GoogleServiceAccountJwt,
    AwsStsAssumeRole,
    GithubAppInstallation,
}

/// One material value: inline when it isn't secret, otherwise from a Secret.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "has(self.value) != has(self.secretKeyRef)",
    "message": "set value or secretKeyRef, not both"
}]))]
pub struct MaterialSource {
    #[schemars(length(min = 1, max = 256))]
    pub name: String,
    /// A non-secret value, such as a client ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<SecretKeyRef>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    pub observed_generation: i64,
    pub phase: String,
    pub reason: String,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Resource version of each referenced Secret when the provider was last
    /// applied. Values are never recorded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_versions: BTreeMap<String, String>,
    /// Credential keys the controller set, so it can remove undeclared ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_keys: Vec<String>,
    /// Config keys the controller set, so it can remove undeclared ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_keys: Vec<String>,
    /// Credentials whose refresh the controller configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refresh_credentials: Vec<String>,
}

impl ProviderStatus {
    /// Record a phase and reason for `generation` and derive the `Ready` condition.
    pub fn report(&mut self, generation: i64, phase: &str, reason: &str, now: i64) {
        self.observed_generation = generation;
        self.phase = phase.into();
        self.reason = reason.into();
        let desired = match (phase, reason) {
            ("Ready", _) => ConditionStatus::True,
            (_, "ApplyUncertain" | "RemoveUncertain") => ConditionStatus::Unknown,
            _ => ConditionStatus::False,
        };
        sync_condition(
            &mut self.conditions,
            desired,
            reason,
            message(phase, reason),
            generation,
            now,
        );
    }
}

/// Fixed, operator-facing text for each reason. Never includes gateway errors.
fn message(phase: &str, reason: &str) -> &'static str {
    match reason {
        "Applied" => "The gateway provider matches the spec and its Secrets.",
        "ApplyUncertain" => "The apply result is unknown; the controller retries.",
        "ProviderRejected" => {
            "The gateway rejected the provider; check the type, keys, and that the controller is a workspace admin."
        }
        "NameCollision" => "A gateway provider with this name belongs to another owner.",
        "SecretNotFound" => "A referenced Secret or key doesn't exist.",
        "SecretValueInvalid" => "A referenced Secret value isn't valid UTF-8.",
        "RemoveUncertain" => "The remove result is unknown; the controller retries.",
        "ProviderInUse" => "The gateway refused to remove a provider that sandboxes still use.",
        _ if phase == "Blocked" => "The provider is blocked; investigate the status reason.",
        _ => "The controller is applying the provider.",
    }
}

/// A Secret's data and resource version. Deliberately not `Debug`.
pub struct SecretData {
    pub resource_version: String,
    pub data: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, thiserror::Error)]
#[error("reading a Secret failed")]
pub struct SecretsError;

#[async_trait]
pub trait SecretReader: Send + Sync {
    /// The named Secret in the provider's namespace, or `None` if it doesn't exist.
    async fn read(&self, name: &str) -> Result<Option<SecretData>, SecretsError>;
}

/// Refresh configuration for one credential, with resolved material.
pub struct DesiredRefresh {
    pub credential: String,
    pub strategy: RefreshStrategy,
    pub material: BTreeMap<String, String>,
    /// Material keys whose values came from Secrets.
    pub secret_keys: Vec<String>,
}

/// The gateway provider to apply. Deliberately not `Debug`: it holds secrets.
pub struct Desired {
    pub workspace: String,
    pub name: String,
    pub source_uid: String,
    pub type_: String,
    pub global_profile: bool,
    pub credentials: BTreeMap<String, String>,
    pub config: BTreeMap<String, String>,
    /// Keys this controller set earlier that are no longer declared.
    pub removed_credentials: Vec<String>,
    pub removed_config: Vec<String>,
    pub refresh: Vec<DesiredRefresh>,
    /// Credentials whose refresh this controller configured earlier and that
    /// are no longer declared.
    pub retired_refresh: Vec<String>,
}

#[async_trait]
pub trait ProviderGateway: Send + Sync {
    /// Create the provider, or update it if this source owns it. A provider
    /// owned by another source is a `Collision`; a type change is `Rejected`.
    async fn apply(&self, desired: &Desired) -> Result<(), GatewayError>;
    /// Delete the provider if this source owns it. A provider owned by another
    /// source is a `Collision` and is left alone; one still in use is `Rejected`.
    async fn remove(
        &self,
        workspace: &str,
        name: &str,
        source_uid: &str,
    ) -> Result<(), GatewayError>;
}

/// Identity of the resource being reconciled.
pub struct Source<'a> {
    pub workspace: &'a str,
    pub name: &'a str,
    pub uid: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The gateway provider is gone or was never ours; release the finalizer.
    Finalize,
    /// Reconcile again after this many seconds.
    Requeue(u64),
}

enum Resolved {
    Ready {
        credentials: BTreeMap<String, String>,
        refresh: Vec<DesiredRefresh>,
        versions: BTreeMap<String, String>,
    },
    Blocked(&'static str),
}

/// Reads each referenced Secret once and records its resource version.
struct Lookup<'a> {
    secrets: &'a dyn SecretReader,
    cache: BTreeMap<String, Option<SecretData>>,
    versions: BTreeMap<String, String>,
}

impl Lookup<'_> {
    async fn value(
        &mut self,
        reference: &SecretKeyRef,
    ) -> Result<Result<String, &'static str>, SecretsError> {
        if !self.cache.contains_key(&reference.name) {
            let secret = self.secrets.read(&reference.name).await?;
            self.cache.insert(reference.name.clone(), secret);
        }
        let Some(secret) = self.cache.get(&reference.name).and_then(Option::as_ref) else {
            return Ok(Err("SecretNotFound"));
        };
        self.versions
            .insert(reference.name.clone(), secret.resource_version.clone());
        let Some(bytes) = secret.data.get(&reference.key) else {
            return Ok(Err("SecretNotFound"));
        };
        Ok(String::from_utf8(bytes.clone()).map_err(|_| "SecretValueInvalid"))
    }
}

async fn resolve(
    spec: &ProviderSpec,
    secrets: &dyn SecretReader,
) -> Result<Resolved, SecretsError> {
    let mut lookup = Lookup {
        secrets,
        cache: BTreeMap::new(),
        versions: BTreeMap::new(),
    };
    let mut credentials = BTreeMap::new();
    for credential in &spec.credentials {
        match lookup.value(&credential.secret_key_ref).await? {
            Ok(value) => credentials.insert(credential.name.clone(), value),
            Err(reason) => return Ok(Resolved::Blocked(reason)),
        };
    }
    let mut refresh = Vec::new();
    for source in &spec.refresh {
        let mut material = BTreeMap::new();
        let mut secret_keys = Vec::new();
        for item in &source.material {
            let value = match (&item.value, &item.secret_key_ref) {
                (_, Some(reference)) => match lookup.value(reference).await? {
                    Ok(value) => {
                        secret_keys.push(item.name.clone());
                        value
                    }
                    Err(reason) => return Ok(Resolved::Blocked(reason)),
                },
                (Some(value), None) => value.clone(),
                // Admission requires one of the two.
                (None, None) => String::new(),
            };
            material.insert(item.name.clone(), value);
        }
        refresh.push(DesiredRefresh {
            credential: source.credential.clone(),
            strategy: source.strategy,
            material,
            secret_keys,
        });
    }
    Ok(Resolved::Ready {
        credentials,
        refresh,
        versions: lookup.versions,
    })
}

fn removed<'a>(previous: &'a [String], current: impl Iterator<Item = &'a String>) -> Vec<String> {
    let current: BTreeSet<_> = current.collect();
    previous
        .iter()
        .filter(|key| !current.contains(key))
        .cloned()
        .collect()
}

/// Bring the gateway provider in line with the resource, or remove it when the
/// resource is being deleted. Updates `status`; the caller persists it.
#[allow(clippy::too_many_arguments)]
pub async fn sync(
    spec: &ProviderSpec,
    status: &mut ProviderStatus,
    generation: i64,
    source: &Source<'_>,
    deleting: bool,
    secrets: &dyn SecretReader,
    gateway: &dyn ProviderGateway,
    now: i64,
) -> Result<Step, SecretsError> {
    if deleting {
        return Ok(
            match gateway
                .remove(source.workspace, source.name, source.uid)
                .await
            {
                Ok(()) | Err(GatewayError::Collision) => Step::Finalize,
                Err(GatewayError::Rejected | GatewayError::Missing(_)) => {
                    status.report(generation, "Blocked", "ProviderInUse", now);
                    Step::Requeue(MISSING_SECRET_SECONDS)
                }
                Err(GatewayError::Uncertain) => {
                    status.report(generation, "Progressing", "RemoveUncertain", now);
                    Step::Requeue(UNCERTAIN_SECONDS)
                }
            },
        );
    }
    let (credentials, refresh, versions) = match resolve(spec, secrets).await? {
        Resolved::Ready {
            credentials,
            refresh,
            versions,
        } => (credentials, refresh, versions),
        Resolved::Blocked(reason) => {
            status.report(generation, "Blocked", reason, now);
            return Ok(Step::Requeue(MISSING_SECRET_SECONDS));
        }
    };
    if status.phase == "Ready"
        && status.observed_generation == generation
        && status.secret_versions == versions
    {
        return Ok(Step::Requeue(SETTLED_SECONDS));
    }
    let desired = Desired {
        workspace: source.workspace.into(),
        name: source.name.into(),
        source_uid: source.uid.into(),
        type_: spec.type_.clone(),
        global_profile: spec.global_profile,
        removed_credentials: removed(&status.credential_keys, credentials.keys()),
        removed_config: removed(&status.config_keys, spec.config.keys()),
        retired_refresh: removed(
            &status.refresh_credentials,
            refresh.iter().map(|r| &r.credential),
        ),
        credentials,
        config: spec.config.clone(),
        refresh,
    };
    Ok(match gateway.apply(&desired).await {
        Ok(()) => {
            status.secret_versions = versions;
            status.credential_keys = desired.credentials.keys().cloned().collect();
            status.config_keys = desired.config.keys().cloned().collect();
            status.refresh_credentials = desired
                .refresh
                .iter()
                .map(|r| r.credential.clone())
                .collect();
            status.report(generation, "Ready", "Applied", now);
            Step::Requeue(SETTLED_SECONDS)
        }
        Err(GatewayError::Collision) => {
            status.report(generation, "Blocked", "NameCollision", now);
            Step::Requeue(BLOCKED_SECONDS)
        }
        Err(GatewayError::Rejected | GatewayError::Missing(_)) => {
            status.report(generation, "Blocked", "ProviderRejected", now);
            Step::Requeue(BLOCKED_SECONDS)
        }
        Err(GatewayError::Uncertain) => {
            status.report(generation, "Progressing", "ApplyUncertain", now);
            Step::Requeue(UNCERTAIN_SECONDS)
        }
    })
}

/// True when `spec` reads the named Secret.
#[must_use]
pub fn references_secret(spec: &ProviderSpec, secret: &str) -> bool {
    spec.credentials
        .iter()
        .map(|c| &c.secret_key_ref)
        .chain(
            spec.refresh
                .iter()
                .flat_map(|r| r.material.iter().filter_map(|m| m.secret_key_ref.as_ref())),
        )
        .any(|reference| reference.name == secret)
}
