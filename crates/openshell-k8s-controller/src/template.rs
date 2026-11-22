// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `OpenShellSandboxTemplate`: a workload template declared in Kubernetes.
//!
//! Gateway templates can't be updated in place, so each spec generation is
//! published as its own immutable gateway template, a revision named
//! `<name>-r<generation>`. Revisions carry family and generation labels, so the
//! gateway can resolve the family name to the latest revision for callers that
//! create sandboxes on demand, such as session execution layers.

use std::collections::BTreeMap;

use async_trait::async_trait;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Condition, ConditionStatus, GatewayError, sync_condition};

/// Gateway label naming the family that a revision belongs to.
pub const FAMILY_LABEL: &str = "openshell.ai/template-family";
/// Gateway label holding a revision's generation, as a decimal integer.
pub const GENERATION_LABEL: &str = "openshell.ai/template-generation";
/// Gateway label holding the UID of the Kubernetes resource that owns a revision.
pub const SOURCE_UID_LABEL: &str = "kubernetes.openshell.ai/uid";
/// Gateway template names are DNS-1123 labels.
const MAX_REVISION_NAME: usize = 63;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[kube(
    group = "openshell.ai",
    version = "v1alpha1",
    kind = "OpenShellSandboxTemplate",
    plural = "openshellsandboxtemplates",
    shortname = "ossbxt",
    namespaced,
    status = "TemplateStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.reason"}"#,
    printcolumn = r#"{"name":"Revision","type":"string","jsonPath":".status.currentRevision"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSpec {
    /// Fully qualified OCI image reference. Pin a digest for reproducible releases.
    #[schemars(length(min = 1, max = 1024))]
    pub image: String,
    /// Environment variables for sandboxes created from this template. Not for secrets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<TemplateResources>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateResources {
    /// CPU quantity, for example "500m" or "2".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cpu: String,
    /// Memory quantity, for example "512Mi" or "4Gi".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub memory: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateStatus {
    pub observed_generation: i64,
    pub phase: String,
    pub reason: String,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Gateway template that new sandboxes use for this generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<String>,
}

impl TemplateStatus {
    /// Record a phase and reason for `generation` and derive the `Ready` condition.
    pub fn report(&mut self, generation: i64, phase: &str, reason: &str, now: i64) {
        self.observed_generation = generation;
        self.phase = phase.into();
        self.reason = reason.into();
        let desired = match (phase, reason) {
            ("Ready", _) => ConditionStatus::True,
            (_, "PublishUncertain" | "RemoveUncertain") => ConditionStatus::Unknown,
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

    /// True when the current generation is published and ready to use.
    #[must_use]
    pub fn ready_revision(&self, generation: i64) -> Option<&str> {
        (self.phase == "Ready" && self.observed_generation == generation)
            .then_some(self.current_revision.as_deref())
            .flatten()
    }
}

/// Fixed, operator-facing text for each reason. Never includes gateway errors.
fn message(phase: &str, reason: &str) -> &'static str {
    match reason {
        "Published" => "The gateway has a template revision for the current generation.",
        "PublishUncertain" => "The publish result is unknown; the controller retries.",
        "PublishRejected" => {
            "The gateway rejected the template; check the spec and that the controller is a workspace admin."
        }
        "NameCollision" => "A gateway template with the revision name belongs to another owner.",
        "NameTooLong" => "The resource name is too long for a gateway template revision name.",
        "Removing" => "The controller is removing the template's gateway revisions.",
        "RemoveUncertain" => "The remove result is unknown; the controller retries.",
        "TemplateInUse" => "The gateway refused to remove a revision that sandboxes still use.",
        _ if phase == "Blocked" => "The template is blocked; investigate the status reason.",
        _ => "The controller is publishing the template.",
    }
}

/// Gateway template name for one generation, or `None` when it would exceed
/// the gateway's name limit.
#[must_use]
pub fn revision_name(name: &str, generation: i64) -> Option<String> {
    let revision = format!("{name}-r{generation}");
    (revision.len() <= MAX_REVISION_NAME).then_some(revision)
}

/// One immutable gateway template to publish.
///
/// Its name is deterministic and unique in the workspace, so a retry after a lost response finds the revision
/// it already created by name and owner label, without a request ID. A request
/// ID would be worse: the gateway never replays one whose first attempt failed,
/// so a transient collision would block the template permanently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub workspace: String,
    pub name: String,
    pub family: String,
    pub generation: i64,
    pub source_uid: String,
    pub spec: TemplateSpec,
}

#[async_trait]
pub trait TemplateGateway: Send + Sync {
    /// Create the revision. An existing revision owned by the same source
    /// succeeds; one owned by another source is a `Collision`.
    async fn publish(&self, revision: &Revision) -> Result<(), GatewayError>;
    /// Delete every revision owned by `source_uid`. A revision still in use is
    /// `Rejected`.
    async fn remove_all(&self, workspace: &str, source_uid: &str) -> Result<(), GatewayError>;
}

/// Record the outcome of publishing `revision` for `generation`.
pub fn apply_publish(
    status: &mut TemplateStatus,
    generation: i64,
    revision: &str,
    result: Result<(), GatewayError>,
    now: i64,
) {
    match result {
        Ok(()) => {
            status.current_revision = Some(revision.into());
            status.report(generation, "Ready", "Published", now);
        }
        Err(GatewayError::Collision) => status.report(generation, "Blocked", "NameCollision", now),
        Err(GatewayError::Rejected | GatewayError::Missing(_)) => {
            status.report(generation, "Blocked", "PublishRejected", now);
        }
        Err(GatewayError::Uncertain) => {
            status.report(generation, "Progressing", "PublishUncertain", now);
        }
    }
}
