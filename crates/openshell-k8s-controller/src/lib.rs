// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Experimental, single-replica CRD adapter. The gateway remains the lifecycle owner.

use async_trait::async_trait;
use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const FINALIZER: &str = "openshell.ai/gateway-cleanup";
pub const READY_CONDITION: &str = "Ready";
const REPLAY_SECONDS: i64 = 23 * 60 * 60;
const MAX_ATTEMPTS: i32 = 10;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "openshell.ai",
    version = "v1alpha1",
    kind = "OpenShellSandbox",
    plural = "openshellsandboxes",
    shortname = "ossb",
    namespaced,
    status = "SandboxStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.reason"}"#,
    printcolumn = r#"{"name":"Gateway Phase","type":"string","jsonPath":".status.gatewayPhase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSpec {
    /// Existing workspace-scoped gateway workload template. No credentials in the CR.
    #[schemars(length(min = 1, max = 253))]
    pub workload_template: String,
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
        })
    }
}

/// Operational recovery journal. Not intended for end users.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Journal {
    pub binding: Binding,
    pub create_attempts: i32,
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
    pub gateway_phase: Option<String>,
    pub journal: Journal,
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
            gateway_phase: None,
            journal: Journal {
                binding,
                create_attempts: 0,
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
        "PendingSubmission" => "The controller has not submitted the sandbox to the gateway.",
        "Submitting" => "The controller is submitting the sandbox to the gateway.",
        "Accepted" => "The gateway accepted the sandbox.",
        "GatewayProgressing" => "The gateway is provisioning or starting the sandbox.",
        "GatewayNotReady" => "The gateway reports a sandbox phase other than Ready.",
        "CreateUncertain" => "The create result is unknown; the controller replays the request.",
        "CreateReplayExhausted" => "Create replay stopped; investigate the gateway sandbox.",
        "NameCollision" => "A gateway sandbox with this name already exists.",
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
    let desired = ready_status(&status.phase, &status.reason);
    let reason = status.reason.clone();
    let message = ready_message(&status.phase, &status.reason).to_owned();
    let generation = status.observed_generation;
    if let Some(condition) = status
        .conditions
        .iter_mut()
        .find(|c| c.type_ == READY_CONDITION)
    {
        if condition.status != desired {
            condition.status = desired;
            condition.last_transition_time = now;
        }
        condition.reason = reason;
        condition.message = message;
        condition.observed_generation = generation;
    } else {
        status.conditions.push(Condition {
            type_: READY_CONDITION.into(),
            status: desired,
            reason,
            message,
            last_transition_time: now,
            observed_generation: generation,
        });
    }
}

/// Include the transition rule in the actual served CRD, not only in Rust validation.
pub fn crd() -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(OpenShellSandbox::crd())?;
    let schema = &mut value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"];
    schema["spec"]["x-kubernetes-validations"] = serde_json::json!([{
        "rule": "self == oldSelf",
        "message": "spec is immutable; create a new Sandbox for a different workload"
    }]);
    let conditions = &mut schema["status"]["properties"]["conditions"];
    conditions["x-kubernetes-list-type"] = "map".into();
    conditions["x-kubernetes-list-map-keys"] = serde_json::json!(["type"]);
    Ok(value)
}

/// Schema-only marker: an RFC 3339 `date-time` string.
struct Rfc3339Time;

impl JsonSchema for Rfc3339Time {
    fn schema_name() -> String {
        "Rfc3339Time".into()
    }

    fn json_schema(_: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        rfc3339::date_time()
    }
}

/// Serialize Unix seconds as RFC 3339 so `advance()` keeps integer arithmetic.
mod rfc3339 {
    use chrono::{DateTime, SecondsFormat};
    use schemars::schema::{InstanceType, Schema, SchemaObject};
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
        SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            format: Some("date-time".into()),
            ..SchemaObject::default()
        }
        .into()
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
    Uncertain,
}

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
    if deleting && status.journal.create_attempts == 0 {
        return Ok(Outcome::Finalize);
    }
    if status.sandbox_id.is_none() {
        if status.reason == "NameCollision" {
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
        status.journal.create_attempts += 1;
        status.journal.retry_after = Some(now.saturating_add(30));
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
            Ok(_) | Err(GatewayError::Uncertain) => {
                status.report("Blocked", "CreateUncertain");
            }
            Err(GatewayError::Collision) => {
                status.report("Blocked", "NameCollision");
            }
        }
        persistence.save(status).await?;
        // In particular, persist the receipt before any deletion.
        return Ok(Outcome::Requeue);
    }
    match gateway.get(&status.journal.binding).await {
        Ok(None) if deleting => return Ok(Outcome::Finalize),
        Ok(None) => status.report("Blocked", "AcceptedSandboxMissing"),
        Err(_) => status.report("Blocked", "ObservationUnavailable"),
        Ok(Some(remote)) if Some(&remote.id) != status.sandbox_id.as_ref() => {
            status.report("Blocked", "IdentityMismatch");
        }
        Ok(Some(remote)) if deleting => {
            if status.journal.delete_accepted || remote.phase == "Deleting" {
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
                    status.journal.retry_after = Some(now.saturating_add(30));
                    status.report("Progressing", "Deleting");
                    persistence.save(status).await?;
                    if gateway.delete(&status.journal.binding).await.is_ok() {
                        status.journal.delete_accepted = true;
                        status.report("Progressing", "WaitingForAbsence");
                    } else {
                        status.report("Blocked", "DeleteUncertain");
                    }
                } else {
                    status.report("Blocked", "DeleteReplayExhausted");
                }
            }
        }
        Ok(Some(remote)) => {
            status.gateway_phase = Some(remote.phase.clone());
            // Stopped, completed, and missing records never trigger Create/Start.
            if remote.phase == "Ready" {
                status.report("Ready", "GatewayReady");
            } else if matches!(remote.phase.as_str(), "Provisioning" | "Starting") {
                status.report("Progressing", "GatewayProgressing");
            } else {
                status.report("Blocked", "GatewayNotReady");
            }
        }
    }
    persistence.save(status).await?;
    Ok(Outcome::Requeue)
}
