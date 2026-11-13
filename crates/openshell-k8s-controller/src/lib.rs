// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Experimental, single-replica CRD adapter. The gateway remains the lifecycle owner.

use async_trait::async_trait;
use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const FINALIZER: &str = "openshell.ai/gateway-cleanup";
const REPLAY_SECONDS: i64 = 23 * 60 * 60;
const MAX_ATTEMPTS: u32 = 10;
const BASE_BACKOFF_SECONDS: i64 = 30;
const MAX_BACKOFF_SECONDS: i64 = 60 * 60;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "openshell.ai",
    version = "v1alpha1",
    kind = "OpenShellSandbox",
    plural = "openshellsandboxes",
    namespaced,
    status = "SandboxStatus"
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

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    pub binding: Binding,
    pub observed_generation: i64,
    pub sandbox_id: Option<String>,
    pub gateway_phase: Option<String>,
    pub phase: String,
    pub reason: String,
    pub create_attempts: u32,
    /// True once any create may have reached the gateway without a receipt.
    /// Set before each submission; cleared only by a definitive rejection.
    /// Defaults to true so journals written before this field fail closed.
    #[serde(default = "fail_closed")]
    pub create_may_exist: bool,
    pub delete_attempts: u32,
    pub delete_started_at: Option<i64>,
    pub delete_accepted: bool,
    pub retry_after: Option<i64>,
}

const fn fail_closed() -> bool {
    true
}

impl SandboxStatus {
    pub fn new(binding: Binding, generation: i64) -> Self {
        Self {
            binding,
            observed_generation: generation,
            sandbox_id: None,
            gateway_phase: None,
            phase: "Progressing".into(),
            reason: "PendingSubmission".into(),
            create_attempts: 0,
            create_may_exist: false,
            delete_attempts: 0,
            delete_started_at: None,
            delete_accepted: false,
            retry_after: None,
        }
    }

    fn report(&mut self, phase: &str, reason: &str) {
        self.phase = phase.into();
        self.reason = reason.into();
    }
}

/// Include the transition rule in the actual served CRD, not only in Rust validation.
pub fn crd() -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(OpenShellSandbox::crd())?;
    value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["x-kubernetes-validations"] = serde_json::json!([{
        "rule": "self == oldSelf",
        "message": "spec is immutable; create a new Sandbox for a different workload"
    }]);
    Ok(value)
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

fn retry_allowed(start: i64, now: i64, attempts: u32) -> bool {
    now >= start && now - start < REPLAY_SECONDS && attempts < MAX_ATTEMPTS
}

/// Exponential backoff after the given attempt, so the attempt budget spans
/// hours of gateway unavailability instead of minutes.
fn backoff(attempts: u32) -> i64 {
    let exponent = attempts.saturating_sub(1).min(16);
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
    if deleting && status.sandbox_id.is_none() && !status.create_may_exist {
        // No submission can have created a gateway record.
        return Ok(Outcome::Finalize);
    }
    if status.sandbox_id.is_none() {
        if matches!(status.reason.as_str(), "NameCollision" | "CreateRejected") {
            return Ok(Outcome::Requeue);
        }
        if !retry_allowed(
            status.binding.create_started_at,
            now,
            status.create_attempts,
        ) {
            status.report("Blocked", "CreateReplayExhausted");
            persistence.save(status).await?;
            return Ok(Outcome::Requeue);
        }
        if status
            .retry_after
            .is_some_and(|retry_after| now < retry_after)
        {
            return Ok(Outcome::Requeue);
        }
        let may_already_exist = status.create_may_exist;
        status.create_attempts += 1;
        status.create_may_exist = true;
        status.retry_after = Some(now.saturating_add(backoff(status.create_attempts)));
        status.report("Progressing", "Submitting");
        persistence.save(status).await?;
        match gateway.create(&status.binding).await {
            Ok(remote) if remote.name == status.binding.sandbox_name && !remote.id.is_empty() => {
                status.sandbox_id = Some(remote.id);
                status.retry_after = None;
                status.report("Progressing", "Accepted");
            }
            // A refusal proves only that this attempt created nothing. After an
            // uncertain attempt, keep replaying to recover a possible receipt.
            Err(GatewayError::Rejected) if !may_already_exist => {
                status.create_may_exist = false;
                status.report("Blocked", "CreateRejected");
            }
            Err(GatewayError::Collision) => {
                status.create_may_exist = may_already_exist;
                status.report("Blocked", "NameCollision");
            }
            Ok(_) | Err(GatewayError::Uncertain | GatewayError::Rejected) => {
                status.report("Progressing", "CreateUncertain");
            }
        }
        persistence.save(status).await?;
        // In particular, persist the receipt before any deletion.
        return Ok(Outcome::Requeue);
    }
    match gateway.get(&status.binding).await {
        Ok(None) if deleting => return Ok(Outcome::Finalize),
        Ok(None) => status.report("Blocked", "AcceptedSandboxMissing"),
        Err(_) => status.report("Progressing", "ObservationUnavailable"),
        Ok(Some(remote)) if Some(&remote.id) != status.sandbox_id.as_ref() => {
            status.report("Blocked", "IdentityMismatch");
        }
        Ok(Some(remote)) if deleting => {
            if status.delete_accepted || remote.phase == "Deleting" {
                status.report("Progressing", "WaitingForAbsence");
            } else {
                let start = *status.delete_started_at.get_or_insert(now);
                if retry_allowed(start, now, status.delete_attempts) {
                    if status
                        .retry_after
                        .is_some_and(|retry_after| now < retry_after)
                    {
                        return Ok(Outcome::Requeue);
                    }
                    status.delete_attempts += 1;
                    status.retry_after = Some(now.saturating_add(backoff(status.delete_attempts)));
                    status.report("Progressing", "Deleting");
                    persistence.save(status).await?;
                    if gateway.delete(&status.binding).await.is_ok() {
                        status.delete_accepted = true;
                        status.report("Progressing", "WaitingForAbsence");
                    } else {
                        status.report("Progressing", "DeleteUncertain");
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
