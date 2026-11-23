// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Experimental, single-replica CRD adapter. The gateway remains the lifecycle owner.

use async_trait::async_trait;
use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod leader;
pub mod provider;
pub mod template;

pub const FINALIZER: &str = "openshell.ai/gateway-cleanup";
pub const READY_CONDITION: &str = "Ready";
const REPLAY_SECONDS: i64 = 23 * 60 * 60;
const MAX_ATTEMPTS: i32 = 10;
const BASE_BACKOFF_SECONDS: i64 = 30;
const MAX_BACKOFF_SECONDS: i64 = 60 * 60;
/// Interval for observing a sandbox that is changing.
pub const ACTIVE_OBSERVATION_SECONDS: i64 = 30;
/// Interval for observing a sandbox that is settled, healthy or blocked.
/// Watch events still trigger an immediate reconcile.
pub const SETTLED_OBSERVATION_SECONDS: i64 = 5 * 60;

/// Seconds until the controller should observe this sandbox again.
///
/// Settled sandboxes are polled rarely so gateway load stays low for large,
/// stable fleets. A deterministic jitter of up to 10% from the resource UID
/// spreads the polls of many resources created at the same time.
#[must_use]
pub fn next_observation(status: &SandboxStatus, now: i64, deleting: bool) -> i64 {
    let base = match status.journal.retry_after {
        Some(retry_after) if retry_after > now => {
            (retry_after - now).clamp(1, SETTLED_OBSERVATION_SECONDS)
        }
        _ if deleting => ACTIVE_OBSERVATION_SECONDS,
        _ if matches!(status.phase.as_str(), "Ready" | "Blocked") => SETTLED_OBSERVATION_SECONDS,
        _ => ACTIVE_OBSERVATION_SECONDS,
    };
    let seed = status
        .journal
        .binding
        .source_uid
        .bytes()
        .fold(0_i64, |acc, byte| (acc * 31 + i64::from(byte)) % 1_000_003);
    base + seed % (base / 10 + 1)
}

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "openshell.ai",
    version = "v1alpha1",
    kind = "OpenShellSandbox",
    plural = "openshellsandboxes",
    shortname = "ossbx",
    namespaced,
    status = "SandboxStatus",
    printcolumn = r#"{"name":"Sandbox","type":"string","jsonPath":".status.sandboxName"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.reason"}"#,
    printcolumn = r#"{"name":"Gateway Phase","type":"string","jsonPath":".status.gatewayPhase"}"#,
    printcolumn = r#"{"name":"Template Revision","type":"string","priority":1,"jsonPath":".status.templateRevision"}"#,
    printcolumn = r#"{"name":"Sandbox ID","type":"string","priority":1,"jsonPath":".status.sandboxId"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    /// The workload's template: an `OpenShellSandboxTemplate`, or else an
    /// existing gateway workload template. Omit it to describe the workload
    /// inline with `workload`, or to use the gateway's default image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_ref: Option<TemplateRef>,
    #[serde(flatten)]
    pub inputs: SandboxInputs,
}

/// A reference to a workload template.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateRef {
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
    /// The template's namespace. Defaults to the sandbox's namespace, which is
    /// currently the only namespace the controller resolves templates in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Create inputs besides the template. No credentials: providers are
/// referenced by name.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxInputs {
    /// An inline workload, for a sandbox without a template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<template::TemplateSpec>,
    /// Gateway providers to attach, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
    /// The agent's start command. The image's entrypoint and command never
    /// run, so a sandbox without a command starts no agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// What the gateway does when the command exits. Defaults to `Never`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<RestartPolicy>,
    /// HTTP services in the sandbox to expose through gateway-managed URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_exposures: Vec<ServiceExposure>,
    /// The sandbox policy, in the same format as `openshell sandbox create
    /// --policy`. Omitting it applies the gateway's default policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "free_form_object")]
    pub policy: Option<serde_json::Value>,
}

/// An object the API server stores as written; the controller validates it.
fn free_form_object(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceExposure {
    /// Service name. Omit it for the sandbox's unnamed service.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service: String,
    /// Loopback TCP port the service listens on inside the sandbox.
    #[schemars(range(min = 1, max = 65535))]
    pub target_port: i32,
    /// Whether the service receives the caller's Authorization header.
    /// Defaults to `Strip`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<ServiceAuthorization>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ServiceAuthorization {
    Strip,
    BearerPassthrough,
}

impl SandboxSpec {
    /// Why the controller refuses this spec before submitting it, if it does.
    /// The spec is immutable, so the answer never changes for a resource.
    #[must_use]
    pub fn refusal(&self, namespace: &str) -> Option<&'static str> {
        let other_namespace = self
            .template_ref
            .as_ref()
            .and_then(|reference| reference.namespace.as_deref())
            .is_some_and(|ns| ns != namespace);
        if other_namespace {
            return Some("TemplateNamespaceNotAllowed");
        }
        let invalid_policy = self.inputs.policy.as_ref().is_some_and(|policy| {
            openshell_policy::parse_sandbox_policy(&policy.to_string()).is_err()
        });
        invalid_policy.then_some("PolicyInvalid")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub source_uid: String,
    pub source_name: String,
    pub namespace: String,
    pub gateway: String,
    pub workspace: String,
    pub sandbox_name: String,
    pub create_request_id: String,
    pub delete_request_id: String,
    /// Unix seconds in memory; RFC 3339 in the serialized status.
    #[serde(with = "rfc3339")]
    #[schemars(with = "Rfc3339Time")]
    pub create_started_at: i64,
    pub workload_template: String,
    /// The create inputs, recorded so every replay sends the original request.
    #[serde(default)]
    pub inputs: SandboxInputs,
}

impl Binding {
    pub fn new(
        uid: &str,
        name: &str,
        namespace: &str,
        gateway: &str,
        workspace: &str,
        template: &str,
        now: i64,
    ) -> Result<Self, uuid::Error> {
        let id = uuid::Uuid::parse_str(uid)?;
        // Kubernetes UIDs are UUIDs in this prototype. Domain separation is identity
        // bookkeeping, not a cryptographic operation; no hashing or secret material.
        let mut delete_bytes = *id.as_bytes();
        delete_bytes[15] ^= 1;
        Ok(Self {
            source_uid: uid.into(),
            source_name: name.into(),
            namespace: namespace.into(),
            gateway: gateway.into(),
            workspace: workspace.into(),
            // Gateway routable names are limited to 19 characters. Short-name
            // collisions fail closed at Create; the full UID remains the journal
            // identity and replay key, never a lookup-based ownership claim.
            sandbox_name: format!("os-{:016x}", id.as_u128() >> 64),
            create_request_id: id.to_string(),
            delete_request_id: uuid::Uuid::from_bytes(delete_bytes).to_string(),
            create_started_at: now,
            workload_template: template.into(),
            inputs: SandboxInputs::default(),
        })
    }

    #[must_use]
    pub fn with_inputs(mut self, inputs: SandboxInputs) -> Self {
        self.inputs = inputs;
        self
    }
}

/// Operational recovery journal. Not intended for end users.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Journal {
    pub binding: Binding,
    pub create_attempts: i32,
    /// True once any create may have reached the gateway without a receipt.
    /// Set before each submission; cleared only by a definitive rejection.
    /// Defaults to true so journals written before this field fail closed.
    #[serde(default = "fail_closed")]
    pub create_may_exist: bool,
    pub delete_attempts: i32,
    #[serde(default, with = "rfc3339::option")]
    #[schemars(with = "Option<Rfc3339Time>")]
    pub delete_started_at: Option<i64>,
    pub delete_accepted: bool,
    #[serde(default, with = "rfc3339::option")]
    #[schemars(with = "Option<Rfc3339Time>")]
    pub retry_after: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

/// Standard Kubernetes condition shape. `message` is always fixed, safe text.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: ConditionStatus,
    pub reason: String,
    pub message: String,
    #[serde(with = "rfc3339")]
    #[schemars(with = "Rfc3339Time")]
    pub last_transition_time: i64,
    pub observed_generation: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    pub observed_generation: i64,
    pub phase: String,
    pub reason: String,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    pub sandbox_name: String,
    pub sandbox_id: Option<String>,
    /// Gateway template the sandbox was created from: a revision of an
    /// `OpenShellSandboxTemplate`, or a template that already existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_revision: Option<String>,
    /// Informational runtime phase. Never affects `phase` or the `Ready`
    /// condition, and is omitted when the controller doesn't report it.
    pub gateway_phase: Option<String>,
    pub journal: Journal,
}

const fn fail_closed() -> bool {
    true
}

impl SandboxStatus {
    pub fn new(binding: Binding, generation: i64) -> Self {
        Self {
            observed_generation: generation,
            phase: "Progressing".into(),
            reason: "PendingSubmission".into(),
            conditions: Vec::new(),
            sandbox_name: binding.sandbox_name.clone(),
            sandbox_id: None,
            template_revision: Some(binding.workload_template.clone()),
            gateway_phase: None,
            journal: Journal {
                binding,
                create_attempts: 0,
                create_may_exist: false,
                delete_attempts: 0,
                delete_started_at: None,
                delete_accepted: false,
                retry_after: None,
            },
        }
    }

    fn report(&mut self, phase: &str, reason: &str) {
        self.phase = phase.into();
        self.reason = reason.into();
    }
}

/// Fixed, operator-facing text for each reason. Never includes gateway errors.
fn ready_message(phase: &str, reason: &str) -> &'static str {
    match reason {
        "GatewayReady" => "The gateway reports the sandbox phase as Ready.",
        "Provisioned" => {
            "The sandbox is provisioned; it is starting, stopped, suspended, or completed."
        }
        "PendingSubmission" => "The controller has not submitted the sandbox to the gateway.",
        "Submitting" => "The controller is submitting the sandbox to the gateway.",
        "Accepted" => "The gateway accepted the sandbox.",
        "GatewayProgressing" => "The gateway is provisioning the sandbox.",
        "GatewayNotReady" => "The gateway reports the sandbox in an error or unknown phase.",
        "CreateUncertain" => "The create result is unknown; the controller replays the request.",
        "CreateReplayExhausted" => "Create replay stopped; investigate the gateway sandbox.",
        "NameCollision" => "A gateway sandbox with this name already exists.",
        "CreateRejected" => {
            "The gateway rejected the create request; check the workload and that the controller is a sandbox manager."
        }
        "TemplateNotFound" => {
            "The workload template doesn't exist in the namespace or the gateway workspace."
        }
        "ProviderNotFound" => {
            "A provider the sandbox lists doesn't exist in the gateway workspace."
        }
        "TemplateNamespaceNotAllowed" => {
            "The workload template must be in the sandbox's namespace."
        }
        "PolicyInvalid" => "The sandbox policy isn't a valid OpenShell policy.",
        "AcceptedSandboxMissing" => "The gateway no longer reports the accepted sandbox.",
        "ObservationUnavailable" => "The controller cannot observe the gateway sandbox.",
        "IdentityMismatch" => "The gateway sandbox ID differs from the recorded ID.",
        "BindingChanged" => "The recorded binding differs from the controller binding.",
        "Deleting" => "The controller is deleting the gateway sandbox.",
        "DeleteUncertain" => "The delete result is unknown; the controller replays the request.",
        "WaitingForAbsence" => "The controller is waiting for the gateway to remove the sandbox.",
        "DeleteReplayExhausted" => "Delete replay stopped; investigate the gateway sandbox.",
        _ if phase == "Blocked" => "The sandbox is blocked; investigate the status reason.",
        _ => "The controller is reconciling the sandbox.",
    }
}

fn ready_status(phase: &str, reason: &str) -> ConditionStatus {
    match (phase, reason) {
        ("Ready", _) => ConditionStatus::True,
        (_, "CreateUncertain" | "DeleteUncertain" | "ObservationUnavailable") => {
            ConditionStatus::Unknown
        }
        _ => ConditionStatus::False,
    }
}

/// Derive `status.conditions` from `phase` and `reason`.
///
/// `lastTransitionTime` changes only when the condition status changes.
pub fn sync_conditions(status: &mut SandboxStatus, now: i64) {
    sync_condition(
        &mut status.conditions,
        ready_status(&status.phase, &status.reason),
        &status.reason,
        ready_message(&status.phase, &status.reason),
        status.observed_generation,
        now,
    );
}

/// Set the `Ready` condition. `lastTransitionTime` changes only when its
/// status changes.
pub fn sync_condition(
    conditions: &mut Vec<Condition>,
    desired: ConditionStatus,
    reason: &str,
    message: &str,
    generation: i64,
    now: i64,
) {
    if let Some(condition) = conditions.iter_mut().find(|c| c.type_ == READY_CONDITION) {
        if condition.status != desired {
            condition.status = desired;
            condition.last_transition_time = now;
        }
        condition.reason = reason.into();
        condition.message = message.into();
        condition.observed_generation = generation;
    } else {
        conditions.push(Condition {
            type_: READY_CONDITION.into(),
            status: desired,
            reason: reason.into(),
            message: message.into(),
            last_transition_time: now,
            observed_generation: generation,
        });
    }
}

/// Include the transition rule in the actual served CRD, not only in Rust validation.
pub fn crd() -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(OpenShellSandbox::crd())?;
    let schema = &mut value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"];
    schema["spec"]["x-kubernetes-validations"] = serde_json::json!([
        {
            "rule": "self == oldSelf",
            "message": "spec is immutable; create a new Sandbox for a different workload"
        },
        {
            "rule": "!(has(self.templateRef) && has(self.workload))",
            "message": "set templateRef or workload, not both"
        }
    ]);
    map_conditions(schema);
    Ok(value)
}

/// The template CRD. Its spec is mutable: each generation becomes a new
/// gateway revision, and existing sandboxes keep the revision they started with.
pub fn template_crd() -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(template::OpenShellSandboxTemplate::crd())?;
    map_conditions(&mut value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]);
    Ok(value)
}

/// The provider CRD. Its spec is mutable, except for its type; the controller
/// applies each change to the gateway provider.
pub fn provider_crd() -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(provider::OpenShellProvider::crd())?;
    map_conditions(&mut value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]);
    Ok(value)
}

/// Every CRD the controller serves, in install order.
pub fn crds() -> Result<Vec<serde_json::Value>, serde_json::Error> {
    Ok(vec![crd()?, template_crd()?, provider_crd()?])
}

/// The CRDs as one multi-document YAML manifest, as `--print-crd` prints it.
pub fn crd_manifest() -> Result<String, Box<dyn std::error::Error>> {
    let documents = crds()?
        .iter()
        .map(|crd| serde_yml::to_string(crd).map(|yaml| yaml.trim_end().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(documents.join("\n---\n") + "\n")
}

fn map_conditions(schema: &mut serde_json::Value) {
    let conditions = &mut schema["status"]["properties"]["conditions"];
    conditions["x-kubernetes-list-type"] = "map".into();
    conditions["x-kubernetes-list-map-keys"] = serde_json::json!(["type"]);
}

/// Schema-only marker: an RFC 3339 `date-time` string.
struct Rfc3339Time;

impl JsonSchema for Rfc3339Time {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Rfc3339Time".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        rfc3339::date_time()
    }
}

/// Serialize Unix seconds as RFC 3339 so `advance()` keeps integer arithmetic.
mod rfc3339 {
    use chrono::{DateTime, SecondsFormat};
    use schemars::{Schema, json_schema};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    fn format(secs: i64) -> Option<String> {
        DateTime::from_timestamp(secs, 0).map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
    }

    fn parse<E: Error>(value: &str) -> Result<i64, E> {
        DateTime::parse_from_rfc3339(value)
            .map(|t| t.timestamp())
            .map_err(|_| E::custom("expected an RFC 3339 timestamp"))
    }

    pub fn date_time() -> Schema {
        json_schema!({ "type": "string", "format": "date-time" })
    }

    #[allow(clippy::trivially_copy_pass_by_ref)] // serde `with` signature
    pub fn serialize<S: Serializer>(secs: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        let value =
            format(*secs).ok_or_else(|| serde::ser::Error::custom("timestamp out of range"))?;
        serializer.serialize_str(&value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        parse(&String::deserialize(deserializer)?)
    }

    pub mod option {
        use super::{Deserialize, Deserializer, Serializer};

        #[allow(clippy::ref_option)] // serde `with` signature
        pub fn serialize<S: Serializer>(
            secs: &Option<i64>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match secs {
                Some(secs) => super::serialize(secs, serializer),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<i64>, D::Error> {
            Option::<String>::deserialize(deserializer)?
                .map(|value| super::parse(&value))
                .transpose()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSandbox {
    pub id: String,
    pub name: String,
    pub phase: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayError {
    Collision,
    /// The gateway definitively refused this request before any side effect.
    Rejected,
    /// Like `Rejected`: a template or provider that the request names doesn't exist.
    Missing(Missing),
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    Template,
    Provider,
}

impl Missing {
    const fn reason(self) -> &'static str {
        match self {
            Self::Template => "TemplateNotFound",
            Self::Provider => "ProviderNotFound",
        }
    }
}

/// Reasons that end submission: the spec is immutable, so retrying can't help.
const REFUSED: [&str; 6] = [
    "NameCollision",
    "CreateRejected",
    "TemplateNotFound",
    "ProviderNotFound",
    "TemplateNamespaceNotAllowed",
    "PolicyInvalid",
];

#[derive(Debug, thiserror::Error)]
#[error("Kubernetes persistence failed")]
pub struct PersistenceError;

/// Mutations must be durably acknowledged before their dependent gateway calls.
#[async_trait]
pub trait Persistence: Send + Sync {
    async fn save(&self, status: &SandboxStatus) -> Result<(), PersistenceError>;
}

#[async_trait]
pub trait Gateway: Send + Sync {
    async fn create(&self, binding: &Binding) -> Result<RemoteSandbox, GatewayError>;
    async fn get(&self, binding: &Binding) -> Result<Option<RemoteSandbox>, GatewayError>;
    async fn delete(&self, binding: &Binding) -> Result<(), GatewayError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Requeue,
    Finalize,
}

fn retry_allowed(start: i64, now: i64, attempts: i32) -> bool {
    now >= start && now - start < REPLAY_SECONDS && attempts < MAX_ATTEMPTS
}

/// Exponential backoff after the given attempt, so the attempt budget spans
/// hours of gateway unavailability instead of minutes.
fn backoff(attempts: i32) -> i64 {
    let exponent = attempts.saturating_sub(1).clamp(0, 16);
    BASE_BACKOFF_SECONDS
        .saturating_mul(1 << exponent)
        .min(MAX_BACKOFF_SECONDS)
}

/// Called only after the finalizer and initial binding have been persisted.
///
/// A lost response is recovered only by replaying the exact original request,
/// never by adopting a lookup result (labels are not ownership enforcement).
pub async fn advance(
    status: &mut SandboxStatus,
    deleting: bool,
    now: i64,
    gateway: &dyn Gateway,
    persistence: &dyn Persistence,
) -> Result<Outcome, PersistenceError> {
    if deleting && status.sandbox_id.is_none() && !status.journal.create_may_exist {
        // No submission can have created a gateway record.
        return Ok(Outcome::Finalize);
    }
    if status.sandbox_id.is_none() {
        if REFUSED.contains(&status.reason.as_str()) {
            return Ok(Outcome::Requeue);
        }
        if !retry_allowed(
            status.journal.binding.create_started_at,
            now,
            status.journal.create_attempts,
        ) {
            status.report("Blocked", "CreateReplayExhausted");
            persistence.save(status).await?;
            return Ok(Outcome::Requeue);
        }
        if status
            .journal
            .retry_after
            .is_some_and(|retry_after| now < retry_after)
        {
            return Ok(Outcome::Requeue);
        }
        let may_already_exist = status.journal.create_may_exist;
        status.journal.create_attempts += 1;
        status.journal.create_may_exist = true;
        status.journal.retry_after =
            Some(now.saturating_add(backoff(status.journal.create_attempts)));
        status.report("Progressing", "Submitting");
        persistence.save(status).await?;
        match gateway.create(&status.journal.binding).await {
            Ok(remote)
                if remote.name == status.journal.binding.sandbox_name && !remote.id.is_empty() =>
            {
                status.sandbox_id = Some(remote.id);
                status.journal.retry_after = None;
                status.report("Progressing", "Accepted");
            }
            // A refusal proves only that this attempt created nothing. After an
            // uncertain attempt, keep replaying to recover a possible receipt.
            Err(GatewayError::Rejected) if !may_already_exist => {
                status.journal.create_may_exist = false;
                status.report("Blocked", "CreateRejected");
            }
            Err(GatewayError::Missing(missing)) if !may_already_exist => {
                status.journal.create_may_exist = false;
                status.report("Blocked", missing.reason());
            }
            Err(GatewayError::Collision) => {
                status.journal.create_may_exist = may_already_exist;
                status.report("Blocked", "NameCollision");
            }
            Ok(_)
            | Err(GatewayError::Uncertain | GatewayError::Rejected | GatewayError::Missing(_)) => {
                status.report("Progressing", "CreateUncertain");
            }
        }
        persistence.save(status).await?;
        // In particular, persist the receipt before any deletion.
        return Ok(Outcome::Requeue);
    }
    match gateway.get(&status.journal.binding).await {
        // The gateway reports a record the caller can no longer access as not
        // found, so absence is final only after it accepted this delete.
        Ok(None) if deleting && status.journal.delete_accepted => return Ok(Outcome::Finalize),
        Err(_) => status.report("Progressing", "ObservationUnavailable"),
        Ok(Some(remote)) if Some(&remote.id) != status.sandbox_id.as_ref() => {
            status.report("Blocked", "IdentityMismatch");
        }
        Ok(found) if deleting => {
            // Otherwise submit the delete; it succeeds for a missing record and
            // fails for an inaccessible one, so nothing is silently orphaned.
            let deleting_elsewhere = found
                .as_ref()
                .is_some_and(|remote| remote.phase == "Deleting");
            if status.journal.delete_accepted || deleting_elsewhere {
                status.report("Progressing", "WaitingForAbsence");
            } else {
                let start = *status.journal.delete_started_at.get_or_insert(now);
                if retry_allowed(start, now, status.journal.delete_attempts) {
                    if status
                        .journal
                        .retry_after
                        .is_some_and(|retry_after| now < retry_after)
                    {
                        return Ok(Outcome::Requeue);
                    }
                    status.journal.delete_attempts += 1;
                    status.journal.retry_after =
                        Some(now.saturating_add(backoff(status.journal.delete_attempts)));
                    status.report("Progressing", "Deleting");
                    persistence.save(status).await?;
                    if gateway.delete(&status.journal.binding).await.is_ok() {
                        status.journal.delete_accepted = true;
                        status.report("Progressing", "WaitingForAbsence");
                    } else {
                        status.report("Progressing", "DeleteUncertain");
                    }
                } else {
                    status.report("Blocked", "DeleteReplayExhausted");
                }
            }
        }
        Ok(None) => status.report("Blocked", "AcceptedSandboxMissing"),
        Ok(Some(remote)) => {
            status.gateway_phase = Some(remote.phase.clone());
            // Stopped, completed, and missing records never trigger Create/Start.
            // Status describes the declaration, not the runtime: a provisioned
            // sandbox that is stopped, suspended, or completed stays Ready, so
            // sleeping and waking never flip GitOps health.
            match remote.phase.as_str() {
                "Ready" => status.report("Ready", "GatewayReady"),
                "Starting" | "Stopping" | "Stopped" | "Suspending" | "Suspended" | "Completed" => {
                    status.report("Ready", "Provisioned");
                }
                "Provisioning" => status.report("Progressing", "GatewayProgressing"),
                _ => status.report("Blocked", "GatewayNotReady"),
            }
        }
    }
    persistence.save(status).await?;
    Ok(Outcome::Requeue)
}
