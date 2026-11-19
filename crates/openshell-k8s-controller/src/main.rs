// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt;
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams},
};
use kube_runtime::{
    Controller,
    controller::{self, Action},
    watcher,
};
use openshell_k8s_controller::{
    Binding, FINALIZER, Gateway, GatewayError, OpenShellSandbox, Outcome, Persistence,
    PersistenceError, RemoteSandbox, SandboxStatus, advance, crds, next_observation,
    sync_conditions,
    template::{
        FAMILY_LABEL, GENERATION_LABEL, OpenShellSandboxTemplate, Revision, SOURCE_UID_LABEL,
        TemplateGateway, apply_publish, revision_name, revision_request_id,
    },
};
use openshell_sdk::{
    AuthConfig, ClientConfig, ClientIdentity, OpenShellClient,
    raw::{
        CreateSandboxRequest, DeleteSandboxRequest, GetSandboxRequest,
        proto::{
            self, CreateSandboxTemplateRequest, DeleteSandboxTemplateRequest,
            GetSandboxTemplateRequest, ListSandboxTemplatesRequest, SandboxPhase, SandboxResources,
            SandboxWorkloadConfig, SandboxWorkloadTemplate, SandboxWorkloadTemplateSpec,
            datamodel::v1::ObjectMeta,
        },
    },
};
use serde_json::json;
use tokio::sync::Mutex;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser)]
#[command(about = "Experimental single-replica Sandbox CRD controller")]
struct Args {
    #[arg(long)]
    print_crd: bool,
    #[arg(long, required_unless_present = "print_crd")]
    namespace: Option<String>,
    #[arg(long, required_unless_present = "print_crd")]
    workspace: Option<String>,
    #[arg(long, required_unless_present = "print_crd")]
    gateway: Option<String>,
    #[arg(long)]
    token_file: Option<PathBuf>,
    #[arg(long)]
    tls_ca: Option<PathBuf>,
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Reconciles that may run at once. Bounds the burst of gateway calls
    /// when the controller starts with many existing resources.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u16).range(1..))]
    max_concurrent_reconciles: u16,
    /// Report the gateway's runtime phase in status. It never affects health;
    /// set to false to avoid a status write each time a sandbox sleeps or wakes.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    report_gateway_phase: bool,
}

struct RpcGateway {
    client: OpenShellClient,
}

fn create_request(binding: &Binding) -> CreateSandboxRequest {
    CreateSandboxRequest {
        workspace_scope: Some(proto::workspace_selector(&binding.workspace)),
        name: binding.sandbox_name.clone(),
        workload_template: binding.workload_template.clone(),
        request_id: binding.create_request_id.clone(),
        spec: Some(proto::SandboxSpec::default()),
        // Lets CLI users see which Kubernetes resource owns the sandbox.
        // Informational only: ownership is never inferred from these values.
        annotations: HashMap::from([
            (
                "kubernetes.openshell.ai/namespace".into(),
                binding.namespace.clone(),
            ),
            (
                "kubernetes.openshell.ai/name".into(),
                binding.source_name.clone(),
            ),
            (
                "kubernetes.openshell.ai/uid".into(),
                binding.source_uid.clone(),
            ),
        ]),
        ..Default::default()
    }
}

fn remote(sandbox: openshell_sdk::raw::Sandbox) -> Result<RemoteSandbox, GatewayError> {
    let meta = sandbox.metadata.ok_or(GatewayError::Uncertain)?;
    let phase = sandbox
        .status
        .and_then(|s| SandboxPhase::try_from(s.phase).ok())
        .map_or_else(
            || "Unknown".to_owned(),
            |p| {
                p.as_str_name().strip_prefix("SANDBOX_PHASE_").map_or_else(
                    || "Unknown".to_owned(),
                    |v| {
                        let lower = v.to_ascii_lowercase();
                        let mut chars = lower.chars();
                        chars.next().map_or_else(String::new, |c| {
                            c.to_uppercase().collect::<String>() + chars.as_str()
                        })
                    },
                )
            },
        );
    Ok(RemoteSandbox {
        id: meta.id,
        name: meta.name,
        phase,
    })
}

#[async_trait]
impl Gateway for RpcGateway {
    async fn create(&self, binding: &Binding) -> Result<RemoteSandbox, GatewayError> {
        let request = create_request(binding);
        let result =
            tokio::time::timeout(CALL_TIMEOUT, self.client.raw_grpc().create_sandbox(request))
                .await
                .map_err(|_| GatewayError::Uncertain)?;
        match result {
            Ok(response) => remote(
                response
                    .into_inner()
                    .sandbox
                    .ok_or(GatewayError::Uncertain)?,
            ),
            Err(status) if status.code() == tonic::Code::AlreadyExists => {
                Err(GatewayError::Collision)
            }
            // Validation, missing-template, and authorization refusals precede
            // any gateway side effect.
            Err(status)
                if matches!(
                    status.code(),
                    tonic::Code::InvalidArgument
                        | tonic::Code::NotFound
                        | tonic::Code::FailedPrecondition
                        | tonic::Code::PermissionDenied
                        | tonic::Code::Unauthenticated
                ) =>
            {
                Err(GatewayError::Rejected)
            }
            Err(_) => Err(GatewayError::Uncertain),
        }
    }

    async fn get(&self, binding: &Binding) -> Result<Option<RemoteSandbox>, GatewayError> {
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.client.raw_grpc().get_sandbox(GetSandboxRequest {
                workspace_scope: Some(proto::workspace_selector(&binding.workspace)),
                name: binding.sandbox_name.clone(),
            }),
        )
        .await
        .map_err(|_| GatewayError::Uncertain)?;
        match result {
            Ok(response) => remote(
                response
                    .into_inner()
                    .sandbox
                    .ok_or(GatewayError::Uncertain)?,
            )
            .map(Some),
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(_) => Err(GatewayError::Uncertain),
        }
    }

    async fn delete(&self, binding: &Binding) -> Result<(), GatewayError> {
        tokio::time::timeout(
            CALL_TIMEOUT,
            self.client.raw_grpc().delete_sandbox(DeleteSandboxRequest {
                workspace_scope: Some(proto::workspace_selector(&binding.workspace)),
                name: binding.sandbox_name.clone(),
                allow_missing: true,
                request_id: binding.delete_request_id.clone(),
            }),
        )
        .await
        .map_err(|_| GatewayError::Uncertain)?
        .map_err(|_| GatewayError::Uncertain)?;
        Ok(())
    }
}

fn template_request(revision: &Revision) -> CreateSandboxTemplateRequest {
    let resources = revision.spec.resources.clone().unwrap_or_default();
    CreateSandboxTemplateRequest {
        workspace_scope: Some(proto::workspace_selector(&revision.workspace)),
        request_id: revision.request_id.clone(),
        template: Some(SandboxWorkloadTemplate {
            metadata: Some(ObjectMeta {
                name: revision.name.clone(),
                labels: HashMap::from([
                    (FAMILY_LABEL.into(), revision.family.clone()),
                    (GENERATION_LABEL.into(), revision.generation.to_string()),
                    (SOURCE_UID_LABEL.into(), revision.source_uid.clone()),
                ]),
                ..Default::default()
            }),
            spec: Some(SandboxWorkloadTemplateSpec {
                workload: Some(SandboxWorkloadConfig {
                    image: revision.spec.image.clone(),
                    environment: revision.spec.environment.clone().into_iter().collect(),
                    resources: Some(SandboxResources {
                        cpu: resources.cpu,
                        memory: resources.memory,
                        gpu: None,
                    }),
                }),
                ..Default::default()
            }),
        }),
    }
}

#[async_trait]
impl TemplateGateway for RpcGateway {
    async fn publish(&self, revision: &Revision) -> Result<(), GatewayError> {
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.client
                .raw_grpc()
                .create_sandbox_template(template_request(revision)),
        )
        .await
        .map_err(|_| GatewayError::Uncertain)?;
        match result {
            Ok(_) => Ok(()),
            Err(status) if status.code() == tonic::Code::AlreadyExists => {
                // Adopt only a revision this resource published earlier.
                let existing = tokio::time::timeout(
                    CALL_TIMEOUT,
                    self.client
                        .raw_grpc()
                        .get_sandbox_template(GetSandboxTemplateRequest {
                            workspace_scope: Some(proto::workspace_selector(&revision.workspace)),
                            name: revision.name.clone(),
                        }),
                )
                .await
                .map_err(|_| GatewayError::Uncertain)?
                .map_err(|_| GatewayError::Uncertain)?
                .into_inner()
                .template
                .and_then(|t| t.metadata)
                .ok_or(GatewayError::Uncertain)?;
                if existing.labels.get(SOURCE_UID_LABEL) == Some(&revision.source_uid) {
                    Ok(())
                } else {
                    Err(GatewayError::Collision)
                }
            }
            Err(status)
                if matches!(
                    status.code(),
                    tonic::Code::InvalidArgument
                        | tonic::Code::FailedPrecondition
                        | tonic::Code::PermissionDenied
                        | tonic::Code::Unauthenticated
                ) =>
            {
                Err(GatewayError::Rejected)
            }
            Err(_) => Err(GatewayError::Uncertain),
        }
    }

    async fn remove_all(&self, workspace: &str, source_uid: &str) -> Result<(), GatewayError> {
        let listed = tokio::time::timeout(
            CALL_TIMEOUT,
            self.client
                .raw_grpc()
                .list_sandbox_templates(ListSandboxTemplatesRequest {
                    workspace_scope: Some(proto::workspace_selector(workspace)),
                    page_size: 1000,
                    page_token: String::new(),
                    label_selector: format!("{SOURCE_UID_LABEL}={source_uid}"),
                }),
        )
        .await
        .map_err(|_| GatewayError::Uncertain)?
        .map_err(|_| GatewayError::Uncertain)?
        .into_inner();
        for template in listed.templates {
            let name = template.metadata.map(|m| m.name).unwrap_or_default();
            let result = tokio::time::timeout(
                CALL_TIMEOUT,
                self.client
                    .raw_grpc()
                    .delete_sandbox_template(DeleteSandboxTemplateRequest {
                        workspace_scope: Some(proto::workspace_selector(workspace)),
                        name,
                        allow_missing: true,
                        request_id: String::new(),
                    }),
            )
            .await
            .map_err(|_| GatewayError::Uncertain)?;
            match result {
                Ok(_) => {}
                Err(status) if status.code() == tonic::Code::FailedPrecondition => {
                    return Err(GatewayError::Rejected);
                }
                Err(_) => return Err(GatewayError::Uncertain),
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Kubernetes request failed")]
    Kube(#[from] kube::Error),
    #[error("Kubernetes persistence failed")]
    Persistence(#[from] PersistenceError),
    #[error("resource lacks a valid UUID identity or generation")]
    Identity,
}

struct Context {
    api: Api<OpenShellSandbox>,
    templates: Api<OpenShellSandboxTemplate>,
    gateway: Arc<dyn Gateway>,
    template_gateway: Arc<dyn TemplateGateway>,
    gateway_url: String,
    workspace: String,
    namespace: String,
    report_gateway_phase: bool,
}

struct KubePersistence {
    api: Api<OpenShellSandbox>,
    object: Mutex<OpenShellSandbox>,
    /// Reconcile time used for condition transitions.
    now: i64,
    report_gateway_phase: bool,
}

#[async_trait]
impl Persistence for KubePersistence {
    async fn save(&self, status: &SandboxStatus) -> Result<(), PersistenceError> {
        // Derive conditions on every write so they never lag phase and reason.
        let mut status = status.clone();
        sync_conditions(&mut status, self.now);
        if !self.report_gateway_phase {
            status.gateway_phase = None;
        }
        let mut current = self.object.lock().await;
        if current.status.as_ref() == Some(&status) {
            return Ok(());
        }
        *current = self
            .api
            .patch_status(
                &current.name_any(),
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata": {"resourceVersion": current.resource_version(), "uid": current.uid()},
                    "status": status
                })),
            )
            .await
            .map_err(|_| PersistenceError)?;
        Ok(())
    }
}

async fn reconcile(object: Arc<OpenShellSandbox>, context: Arc<Context>) -> Result<Action, Error> {
    let name = object.name_any();
    let deleting = object.metadata.deletion_timestamp.is_some();
    let has_finalizer = object.finalizers().iter().any(|f| f == FINALIZER);
    if !has_finalizer {
        if deleting {
            return Ok(Action::await_change());
        }
        let mut finalizers = object.finalizers().to_vec();
        finalizers.push(FINALIZER.into());
        context
            .api
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata": {
                        "resourceVersion": object.resource_version(),
                        "uid": object.uid(),
                        "finalizers": finalizers
                    }
                })),
            )
            .await?;
        // Observe the acknowledged finalizer before persisting/submitting intent.
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let now = unix_now()?;
    let uid = object.uid().ok_or(Error::Identity)?;
    let generation = object.metadata.generation.ok_or(Error::Identity)?;
    // Resolve the template once, before the first submission. The journal then
    // pins the revision, so a later template change never alters this sandbox.
    let template = match object.status.as_ref() {
        Some(status) => status.journal.binding.workload_template.clone(),
        None => match resolve_template(&context.templates, &object.spec.workload_template).await? {
            Some(template) => template,
            None => return Ok(Action::requeue(TEMPLATE_WAIT)),
        },
    };
    let binding = Binding::new(
        &uid,
        &name,
        &context.namespace,
        &context.gateway_url,
        &context.workspace,
        &template,
        now,
    )
    .map_err(|_| Error::Identity)?;
    let mut status = object
        .status
        .clone()
        .unwrap_or_else(|| SandboxStatus::new(binding.clone(), generation));
    let store = KubePersistence {
        api: context.api.clone(),
        object: Mutex::new((*object).clone()),
        now,
        report_gateway_phase: context.report_gateway_phase,
    };
    let mut expected = binding;
    expected.create_started_at = status.journal.binding.create_started_at;
    if status.journal.binding != expected {
        status.phase = "Blocked".into();
        status.reason = "BindingChanged".into();
        store.save(&status).await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }
    // Kubernetes can advance generation when deletion metadata changes. The
    // immutable authored spec is checked against the journal above; generation
    // alone must never prevent cleanup of an already accepted sandbox.
    status.observed_generation = generation;
    store.save(&status).await?;
    if advance(&mut status, deleting, now, context.gateway.as_ref(), &store).await?
        == Outcome::Finalize
    {
        let current = store.object.lock().await;
        let finalizers: Vec<_> = current
            .finalizers()
            .iter()
            .filter(|f| f.as_str() != FINALIZER)
            .collect();
        context
            .api
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata": {
                        "resourceVersion": current.resource_version(),
                        "uid": current.uid(),
                        "finalizers": finalizers
                    }
                })),
            )
            .await?;
        return Ok(Action::await_change());
    }
    let delay = next_observation(&status, now, deleting);
    Ok(Action::requeue(Duration::from_secs(delay.unsigned_abs())))
}

/// Delay before checking again for a template that isn't published yet.
const TEMPLATE_WAIT: Duration = Duration::from_secs(10);

/// Gateway template for a sandbox's `workloadTemplate`: the current revision of
/// an `OpenShellSandboxTemplate` with that name, or else the name itself, an
/// existing gateway template. `None` while the named resource isn't published.
async fn resolve_template(
    templates: &Api<OpenShellSandboxTemplate>,
    name: &str,
) -> Result<Option<String>, Error> {
    let Some(template) = templates.get_opt(name).await? else {
        return Ok(Some(name.to_owned()));
    };
    let generation = template.metadata.generation.ok_or(Error::Identity)?;
    Ok(template
        .status
        .as_ref()
        .and_then(|status| status.ready_revision(generation))
        .map(str::to_owned))
}

fn error_policy<K>(_: Arc<K>, _: &Error, _: Arc<Context>) -> Action {
    // Never print server errors: they can contain credentials and workload data.
    eprintln!("reconciliation failed; retrying Kubernetes observation");
    Action::requeue(Duration::from_secs(30))
}

/// Patch the resource's finalizers, guarded by its resource version and UID.
async fn set_finalizers<K>(api: &Api<K>, object: &K, finalizers: Vec<String>) -> Result<(), Error>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug + Sync,
{
    api.patch(
        &object.name_any(),
        &PatchParams::default(),
        &Patch::Merge(json!({
            "metadata": {
                "resourceVersion": object.resource_version(),
                "uid": object.uid(),
                "finalizers": finalizers
            }
        })),
    )
    .await?;
    Ok(())
}

/// Publish each generation of a template as an immutable gateway revision.
async fn reconcile_template(
    object: Arc<OpenShellSandboxTemplate>,
    context: Arc<Context>,
) -> Result<Action, Error> {
    let has_finalizer = object.finalizers().iter().any(|f| f == FINALIZER);
    let deleting = object.metadata.deletion_timestamp.is_some();
    if !has_finalizer {
        if deleting {
            return Ok(Action::await_change());
        }
        let mut finalizers = object.finalizers().to_vec();
        finalizers.push(FINALIZER.into());
        set_finalizers(&context.templates, object.as_ref(), finalizers).await?;
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let now = unix_now()?;
    let name = object.name_any();
    let uid = object.uid().ok_or(Error::Identity)?;
    let generation = object.metadata.generation.ok_or(Error::Identity)?;
    let mut status = object.status.clone().unwrap_or_default();
    let delay = if deleting {
        match context
            .template_gateway
            .remove_all(&context.workspace, &uid)
            .await
        {
            Ok(()) => {
                let finalizers = object
                    .finalizers()
                    .iter()
                    .filter(|f| f.as_str() != FINALIZER)
                    .cloned()
                    .collect();
                set_finalizers(&context.templates, object.as_ref(), finalizers).await?;
                return Ok(Action::await_change());
            }
            Err(GatewayError::Rejected) => {
                status.report(generation, "Blocked", "TemplateInUse", now);
                Duration::from_mins(1)
            }
            Err(_) => {
                status.report(generation, "Progressing", "RemoveUncertain", now);
                Duration::from_secs(30)
            }
        }
    } else if status.ready_revision(generation).is_some() {
        return Ok(Action::requeue(Duration::from_hours(1)));
    } else if let Some(revision) = revision_name(&name, generation) {
        let result = context
            .template_gateway
            .publish(&Revision {
                workspace: context.workspace.clone(),
                name: revision.clone(),
                family: name.clone(),
                generation,
                source_uid: uid.clone(),
                request_id: revision_request_id(&uid, generation).map_err(|_| Error::Identity)?,
                spec: object.spec.clone(),
            })
            .await;
        apply_publish(&mut status, generation, &revision, result, now);
        if status.phase == "Progressing" {
            Duration::from_secs(30)
        } else {
            Duration::from_mins(5)
        }
    } else {
        status.report(generation, "Blocked", "NameTooLong", now);
        Duration::from_hours(1)
    };
    if object.status.as_ref() != Some(&status) {
        context
            .templates
            .patch_status(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata": {"resourceVersion": object.resource_version(), "uid": uid},
                    "status": status
                })),
            )
            .await?;
    }
    Ok(Action::requeue(delay))
}

fn report(failed: bool) -> futures::future::Ready<()> {
    if failed {
        eprintln!("controller observation failed");
    }
    futures::future::ready(())
}

fn unix_now() -> Result<i64, Error> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .ok_or(Error::Identity)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.print_crd {
        let documents = crds()?
            .iter()
            .map(serde_yml::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        print!("{}", documents.join("---\n"));
        return Ok(());
    }
    let namespace = args.namespace.ok_or("--namespace is required")?;
    let workspace = args.workspace.ok_or("--workspace is required")?;
    let gateway_url = args.gateway.ok_or("--gateway is required")?;
    if [namespace.as_str(), workspace.as_str(), gateway_url.as_str()]
        .iter()
        .any(|value| value.trim().is_empty())
    {
        return Err("binding flags must be nonempty".into());
    }
    let mut config = ClientConfig::new(&gateway_url);
    if let Some(path) = args.token_file {
        config.auth = Some(AuthConfig::oidc(std::fs::read_to_string(path)?.trim()));
    }
    if let Some(path) = args.tls_ca {
        config.ca_cert = Some(std::fs::read(path)?);
    }
    if let (Some(cert), Some(key)) = (args.tls_cert, args.tls_key) {
        config.client_identity = Some(ClientIdentity::new(
            std::fs::read(cert)?,
            std::fs::read(key)?,
        ));
    }
    let gateway = Arc::new(RpcGateway {
        client: tokio::time::timeout(CALL_TIMEOUT, OpenShellClient::connect(config))
            .await
            .map_err(|_| "gateway connection timed out")?
            .map_err(|_| "gateway connection failed")?,
    });
    let client = Client::try_default().await?;
    let api = Api::namespaced(client.clone(), &namespace);
    let templates = Api::namespaced(client, &namespace);
    let context = Arc::new(Context {
        api: api.clone(),
        templates: templates.clone(),
        gateway: gateway.clone(),
        template_gateway: gateway,
        gateway_url,
        workspace,
        namespace,
        report_gateway_phase: args.report_gateway_phase,
    });
    let config = controller::Config::default().concurrency(args.max_concurrent_reconciles);
    let sandboxes = Controller::new(api, watcher::Config::default())
        .with_config(config.clone())
        .shutdown_on_signal()
        .run(reconcile, error_policy, Arc::clone(&context))
        .for_each(|result| report(result.is_err()));
    let templates = Controller::new(templates, watcher::Config::default())
        .with_config(config)
        .shutdown_on_signal()
        .run(reconcile_template, error_policy, context)
        .for_each(|result| report(result.is_err()));
    futures::join!(sandboxes, templates);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Mutex as SyncMutex};

    use http_body_util::{BodyExt, Full};
    use openshell_k8s_controller::{
        SandboxSpec,
        template::{TemplateSpec, TemplateStatus},
    };
    use serde::Serialize;
    use tower::service_fn;

    use super::*;

    type SharedObject = Arc<SyncMutex<OpenShellSandbox>>;
    type SharedTemplate = Arc<SyncMutex<Option<OpenShellSandboxTemplate>>>;
    type CallLog = Arc<SyncMutex<Vec<&'static str>>>;

    struct MockGateway {
        object: SharedObject,
        calls: CallLog,
        published: Arc<SyncMutex<Vec<Revision>>>,
    }

    #[async_trait]
    impl TemplateGateway for MockGateway {
        async fn publish(&self, revision: &Revision) -> Result<(), GatewayError> {
            self.calls.lock().unwrap().push("publish");
            self.published.lock().unwrap().push(revision.clone());
            Ok(())
        }

        async fn remove_all(&self, _: &str, _: &str) -> Result<(), GatewayError> {
            self.calls.lock().unwrap().push("remove");
            Ok(())
        }
    }

    #[async_trait]
    impl Gateway for MockGateway {
        async fn create(&self, binding: &Binding) -> Result<RemoteSandbox, GatewayError> {
            let object = self.object.lock().unwrap();
            assert!(object.finalizers().iter().any(|f| f == FINALIZER));
            let status = object.status.as_ref().unwrap();
            assert!(status.journal.create_attempts > 0);
            assert_eq!(status.journal.binding, *binding);
            self.calls.lock().unwrap().push("create");
            Ok(RemoteSandbox {
                id: "receipt".into(),
                name: binding.sandbox_name.clone(),
                phase: "Ready".into(),
            })
        }

        async fn get(&self, _: &Binding) -> Result<Option<RemoteSandbox>, GatewayError> {
            self.calls.lock().unwrap().push("get");
            Ok(None)
        }

        async fn delete(&self, _: &Binding) -> Result<(), GatewayError> {
            // The record is absent, so an allow-missing delete succeeds.
            self.calls.lock().unwrap().push("delete");
            Ok(())
        }
    }

    struct Fixture {
        context: Arc<Context>,
        object: SharedObject,
        template: SharedTemplate,
        calls: CallLog,
        published: Arc<SyncMutex<Vec<Revision>>>,
    }

    fn json_response(status: u16, body: &serde_json::Value) -> http::Response<Full<bytes::Bytes>> {
        http::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Full::new(serde_json::to_vec(body).unwrap().into()))
            .unwrap()
    }

    fn not_found() -> http::Response<Full<bytes::Bytes>> {
        json_response(
            404,
            &json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "NotFound", "code": 404}),
        )
    }

    /// Apply a guarded merge patch the way the API server would, bumping the
    /// resource version.
    fn apply_patch<K: kube::Resource + Serialize + serde::de::DeserializeOwned>(
        object: &mut K,
        endpoint: &str,
        patch: &serde_json::Value,
        calls: &CallLog,
    ) -> serde_json::Value {
        assert_eq!(
            patch["metadata"]["resourceVersion"],
            object.meta().resource_version.clone().unwrap()
        );
        assert_eq!(patch["metadata"]["uid"], object.meta().uid.clone().unwrap());
        let mut value = serde_json::to_value(&*object).unwrap();
        if endpoint.ends_with("/status") {
            calls.lock().unwrap().push("status");
            value["status"] = patch["status"].clone();
        } else {
            calls.lock().unwrap().push("finalizers");
            value["metadata"]["finalizers"] = patch["metadata"]["finalizers"].clone();
        }
        *object = serde_json::from_value(value).unwrap();
        let rv: u64 = object
            .meta()
            .resource_version
            .as_ref()
            .unwrap()
            .parse()
            .unwrap();
        object.meta_mut().resource_version = Some((rv + 1).to_string());
        serde_json::to_value(&*object).unwrap()
    }

    fn fixture() -> (Arc<Context>, SharedObject, CallLog) {
        let fixture = fixture_with_template(None);
        (fixture.context, fixture.object, fixture.calls)
    }

    fn fixture_with_template(template: Option<OpenShellSandboxTemplate>) -> Fixture {
        let mut object = OpenShellSandbox::new(
            "agent",
            SandboxSpec {
                workload_template: "agent-v1".into(),
            },
        );
        object.metadata.uid = Some("fd229996-bc63-413e-9dc0-cc5f05abe5bd".into());
        object.metadata.namespace = Some("agents".into());
        object.metadata.resource_version = Some("1".into());
        object.metadata.generation = Some(1);
        object.metadata.finalizers = Some(vec!["other.example/finalizer".into()]);
        let object = Arc::new(SyncMutex::new(object));
        let template = Arc::new(SyncMutex::new(template));
        let calls = Arc::new(SyncMutex::new(Vec::new()));
        let published = Arc::new(SyncMutex::new(Vec::new()));
        let server_object = Arc::clone(&object);
        let server_template = Arc::clone(&template);
        let server_calls = Arc::clone(&calls);
        let service = service_fn(move |request: http::Request<kube::client::Body>| {
            let object = Arc::clone(&server_object);
            let template = Arc::clone(&server_template);
            let calls = Arc::clone(&server_calls);
            async move {
                let method = request.method().clone();
                let endpoint = request.uri().path().to_owned();
                let is_template = endpoint.contains("/openshellsandboxtemplates/");
                if method == http::Method::GET {
                    assert!(is_template, "only templates are read: {endpoint}");
                    let template = template.lock().unwrap();
                    return Ok::<_, Infallible>(template.as_ref().map_or_else(not_found, |t| {
                        json_response(200, &serde_json::to_value(t).unwrap())
                    }));
                }
                assert_eq!(method, http::Method::PATCH);
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let patch: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let body = if is_template {
                    let mut template = template.lock().unwrap();
                    apply_patch(template.as_mut().unwrap(), &endpoint, &patch, &calls)
                } else {
                    apply_patch(&mut *object.lock().unwrap(), &endpoint, &patch, &calls)
                };
                Ok(json_response(200, &body))
            }
        });
        let client = Client::new(service, "agents");
        let gateway = Arc::new(MockGateway {
            object: Arc::clone(&object),
            calls: Arc::clone(&calls),
            published: Arc::clone(&published),
        });
        let context = Arc::new(Context {
            api: Api::namespaced(client.clone(), "agents"),
            templates: Api::namespaced(client, "agents"),
            gateway: gateway.clone(),
            template_gateway: gateway,
            gateway_url: "http://gateway:8080".into(),
            workspace: "default".into(),
            namespace: "agents".into(),
            report_gateway_phase: true,
        });
        Fixture {
            context,
            object,
            template,
            calls,
            published,
        }
    }

    fn declared_template(status: Option<TemplateStatus>) -> OpenShellSandboxTemplate {
        let mut template = OpenShellSandboxTemplate::new(
            "agent-v1",
            TemplateSpec {
                image: "registry.example.com/agent@sha256:9f2c".into(),
                environment: std::collections::BTreeMap::new(),
                resources: None,
            },
        );
        template.metadata.uid = Some("0b5e8f2a-3c1d-4e6f-8a9b-1c2d3e4f5a6b".into());
        template.metadata.namespace = Some("agents".into());
        template.metadata.resource_version = Some("1".into());
        template.metadata.generation = Some(2);
        template.status = status;
        template
    }

    #[tokio::test]
    async fn acknowledged_finalizer_and_durable_intent_precede_create() {
        let (context, object, calls) = fixture();
        let first = object.lock().unwrap().clone();
        reconcile(Arc::new(first), Arc::clone(&context))
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["finalizers"]);
        let second = object.lock().unwrap().clone();
        reconcile(Arc::new(second), context).await.unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["finalizers", "status", "status", "create", "status"]
        );
        assert_eq!(
            object
                .lock()
                .unwrap()
                .status
                .as_ref()
                .unwrap()
                .sandbox_id
                .as_deref(),
            Some("receipt")
        );
    }

    #[tokio::test]
    async fn deleting_before_submission_preserves_other_finalizers_without_gateway_call() {
        let (context, object, calls) = fixture();
        {
            let mut object = object.lock().unwrap();
            object
                .metadata
                .finalizers
                .as_mut()
                .unwrap()
                .push(FINALIZER.into());
            object.metadata.deletion_timestamp =
                Some(serde_json::from_value(json!("2026-11-11T00:00:00Z")).unwrap());
        }
        let snapshot = object.lock().unwrap().clone();
        reconcile(Arc::new(snapshot), context).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["status", "finalizers"]);
        assert_eq!(
            object.lock().unwrap().finalizers(),
            &["other.example/finalizer".to_owned()]
        );
    }

    #[tokio::test]
    async fn deletion_generation_bump_does_not_block_accepted_sandbox_cleanup() {
        let (context, object, calls) = fixture();
        let first = object.lock().unwrap().clone();
        reconcile(Arc::new(first), Arc::clone(&context))
            .await
            .unwrap();
        let second = object.lock().unwrap().clone();
        reconcile(Arc::new(second), Arc::clone(&context))
            .await
            .unwrap();
        {
            let mut object = object.lock().unwrap();
            object.metadata.generation = Some(2);
            object.metadata.deletion_timestamp =
                Some(serde_json::from_value(json!("2026-11-11T00:00:00Z")).unwrap());
        }
        calls.lock().unwrap().clear();
        let deleting = object.lock().unwrap().clone();
        reconcile(Arc::new(deleting), Arc::clone(&context))
            .await
            .unwrap();
        // A missing record isn't proof of deletion until the gateway accepts one.
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["status", "get", "status", "delete", "status"]
        );
        assert!(
            object
                .lock()
                .unwrap()
                .finalizers()
                .iter()
                .any(|f| f == FINALIZER)
        );
        calls.lock().unwrap().clear();
        let accepted = object.lock().unwrap().clone();
        reconcile(Arc::new(accepted), context).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["get", "finalizers"]);
        let object = object.lock().unwrap();
        assert_eq!(object.status.as_ref().unwrap().observed_generation, 2);
        assert!(!object.finalizers().iter().any(|f| f == FINALIZER));
        assert_eq!(object.finalizers(), &["other.example/finalizer".to_owned()]);
    }

    #[tokio::test]
    async fn identical_status_does_not_write_to_kubernetes() {
        let (context, object, calls) = fixture();
        let binding = Binding::new(
            &object.lock().unwrap().uid().unwrap(),
            "agent",
            "agents",
            "http://gateway:8080",
            "default",
            "agent-v1",
            1,
        )
        .unwrap();
        let mut status = SandboxStatus::new(binding, 1);
        sync_conditions(&mut status, 1);
        object.lock().unwrap().status = Some(status.clone());
        let store = KubePersistence {
            api: context.api.clone(),
            object: Mutex::new(object.lock().unwrap().clone()),
            now: 2,
            report_gateway_phase: true,
        };
        store.save(&status).await.unwrap();
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn template_submission_includes_explicit_governance_envelope() {
        let binding = Binding::new(
            "fd229996-bc63-413e-9dc0-cc5f05abe5bd",
            "agent",
            "agents",
            "http://gateway:8080",
            "default",
            "agent-v1",
            1,
        )
        .unwrap();
        let request = create_request(&binding);
        assert_eq!(request.spec, Some(proto::SandboxSpec::default()));
        assert_eq!(request.workload_template, "agent-v1");
        assert_eq!(
            request
                .annotations
                .get("kubernetes.openshell.ai/name")
                .map(String::as_str),
            Some("agent")
        );
        assert_eq!(
            request
                .annotations
                .get("kubernetes.openshell.ai/namespace")
                .map(String::as_str),
            Some("agents")
        );
        assert_eq!(
            request.workspace_scope,
            Some(proto::workspace_selector("default"))
        );
    }

    #[tokio::test]
    async fn sandbox_pins_the_published_revision_of_a_declared_template() {
        let mut ready = TemplateStatus {
            current_revision: Some("agent-v1-r2".into()),
            ..Default::default()
        };
        ready.report(2, "Ready", "Published", 1);
        let fixture = fixture_with_template(Some(declared_template(Some(ready))));
        for _ in 0..2 {
            let snapshot = fixture.object.lock().unwrap().clone();
            reconcile(Arc::new(snapshot), Arc::clone(&fixture.context))
                .await
                .unwrap();
        }
        let object = fixture.object.lock().unwrap();
        let status = object.status.as_ref().unwrap();
        assert_eq!(status.journal.binding.workload_template, "agent-v1-r2");
        assert_eq!(status.template_revision.as_deref(), Some("agent-v1-r2"));
        assert_eq!(
            create_request(&status.journal.binding).workload_template,
            "agent-v1-r2"
        );
    }

    #[tokio::test]
    async fn sandbox_waits_for_an_unpublished_template_without_writing_status() {
        let fixture = fixture_with_template(Some(declared_template(None)));
        for _ in 0..2 {
            let snapshot = fixture.object.lock().unwrap().clone();
            reconcile(Arc::new(snapshot), Arc::clone(&fixture.context))
                .await
                .unwrap();
        }
        assert_eq!(*fixture.calls.lock().unwrap(), vec!["finalizers"]);
        assert!(fixture.object.lock().unwrap().status.is_none());
    }

    #[tokio::test]
    async fn template_generation_is_published_as_a_labeled_revision() {
        let fixture = fixture_with_template(Some(declared_template(None)));
        for _ in 0..2 {
            let snapshot = fixture.template.lock().unwrap().clone().unwrap();
            reconcile_template(Arc::new(snapshot), Arc::clone(&fixture.context))
                .await
                .unwrap();
        }
        assert_eq!(
            *fixture.calls.lock().unwrap(),
            vec!["finalizers", "publish", "status"]
        );
        let revision = fixture.published.lock().unwrap()[0].clone();
        assert_eq!(revision.name, "agent-v1-r2");
        assert_eq!(revision.family, "agent-v1");
        let request = template_request(&revision);
        let labels = &request.template.unwrap().metadata.unwrap().labels;
        assert_eq!(
            labels.get(FAMILY_LABEL).map(String::as_str),
            Some("agent-v1")
        );
        assert_eq!(labels.get(GENERATION_LABEL).map(String::as_str), Some("2"));
        let template = fixture.template.lock().unwrap().clone().unwrap();
        let status = template.status.unwrap();
        assert_eq!(status.ready_revision(2), Some("agent-v1-r2"));

        // A published generation needs no further gateway calls.
        fixture.calls.lock().unwrap().clear();
        let snapshot = fixture.template.lock().unwrap().clone().unwrap();
        reconcile_template(Arc::new(snapshot), Arc::clone(&fixture.context))
            .await
            .unwrap();
        assert!(fixture.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_phase_is_omitted_when_reporting_is_off() {
        let (context, object, _) = fixture();
        let binding = Binding::new(
            &object.lock().unwrap().uid().unwrap(),
            "agent",
            "agents",
            "http://gateway:8080",
            "default",
            "agent-v1",
            1,
        )
        .unwrap();
        let mut status = SandboxStatus::new(binding, 1);
        status.gateway_phase = Some("Suspended".into());
        let store = KubePersistence {
            api: context.api.clone(),
            object: Mutex::new(object.lock().unwrap().clone()),
            now: 2,
            report_gateway_phase: false,
        };
        store.save(&status).await.unwrap();
        let saved = object.lock().unwrap().status.clone().unwrap();
        assert_eq!(saved.gateway_phase, None);
    }
}
