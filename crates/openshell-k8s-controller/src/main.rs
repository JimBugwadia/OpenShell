// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt;
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams},
};
use kube_runtime::{Controller, controller::Action, watcher};
use openshell_k8s_controller::{
    Binding, FINALIZER, Gateway, GatewayError, OpenShellSandbox, Outcome, Persistence,
    PersistenceError, RemoteSandbox, SandboxStatus, advance, crd,
};
use openshell_sdk::{
    AuthConfig, ClientConfig, ClientIdentity, OpenShellClient,
    raw::{
        CreateSandboxRequest, DeleteSandboxRequest, GetSandboxRequest,
        proto::{self, SandboxPhase},
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
    gateway: Arc<dyn Gateway>,
    gateway_url: String,
    workspace: String,
    namespace: String,
}

struct KubePersistence {
    api: Api<OpenShellSandbox>,
    object: Mutex<OpenShellSandbox>,
}

#[async_trait]
impl Persistence for KubePersistence {
    async fn save(&self, status: &SandboxStatus) -> Result<(), PersistenceError> {
        let mut current = self.object.lock().await;
        if current.status.as_ref() == Some(status) {
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .ok_or(Error::Identity)?;
    let uid = object.uid().ok_or(Error::Identity)?;
    let generation = object.metadata.generation.ok_or(Error::Identity)?;
    let binding = Binding::new(
        &uid,
        &name,
        &context.namespace,
        &context.gateway_url,
        &context.workspace,
        &object.spec.workload_template,
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
    };
    let mut expected = binding;
    expected.create_started_at = status.binding.create_started_at;
    if status.binding != expected {
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
    Ok(Action::requeue(Duration::from_secs(30)))
}

fn error_policy(_: Arc<OpenShellSandbox>, _: &Error, _: Arc<Context>) -> Action {
    // Never print server errors: they can contain credentials and workload data.
    eprintln!("reconciliation failed; retrying Kubernetes observation");
    Action::requeue(Duration::from_secs(30))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.print_crd {
        println!("{}", serde_yml::to_string(&crd()?)?);
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
    let gateway = RpcGateway {
        client: tokio::time::timeout(CALL_TIMEOUT, OpenShellClient::connect(config))
            .await
            .map_err(|_| "gateway connection timed out")?
            .map_err(|_| "gateway connection failed")?,
    };
    let api = Api::namespaced(Client::try_default().await?, &namespace);
    let context = Arc::new(Context {
        api: api.clone(),
        gateway: Arc::new(gateway),
        gateway_url,
        workspace,
        namespace,
    });
    Controller::new(api, watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            if result.is_err() {
                eprintln!("controller observation failed");
            }
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Mutex as SyncMutex};

    use http_body_util::{BodyExt, Full};
    use openshell_k8s_controller::SandboxSpec;
    use tower::service_fn;

    use super::*;

    type SharedObject = Arc<SyncMutex<OpenShellSandbox>>;
    type CallLog = Arc<SyncMutex<Vec<&'static str>>>;

    struct MockGateway {
        object: SharedObject,
        calls: CallLog,
    }

    #[async_trait]
    impl Gateway for MockGateway {
        async fn create(&self, binding: &Binding) -> Result<RemoteSandbox, GatewayError> {
            let object = self.object.lock().unwrap();
            assert!(object.finalizers().iter().any(|f| f == FINALIZER));
            let status = object.status.as_ref().unwrap();
            assert!(status.create_attempts > 0);
            assert_eq!(status.binding, *binding);
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
            panic!("record is absent; deletion must not be submitted")
        }
    }

    fn fixture() -> (Arc<Context>, SharedObject, CallLog) {
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
        let calls = Arc::new(SyncMutex::new(Vec::new()));
        let server_object = Arc::clone(&object);
        let server_calls = Arc::clone(&calls);
        let service = service_fn(move |request: http::Request<kube::client::Body>| {
            let object = Arc::clone(&server_object);
            let calls = Arc::clone(&server_calls);
            async move {
                assert_eq!(request.method(), http::Method::PATCH);
                let endpoint = request.uri().path().to_owned();
                let bytes = request.into_body().collect().await.unwrap().to_bytes();
                let patch: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let mut object = object.lock().unwrap();
                assert_eq!(
                    patch["metadata"]["resourceVersion"],
                    object.resource_version().unwrap()
                );
                assert_eq!(patch["metadata"]["uid"], object.uid().unwrap());
                if endpoint.ends_with("/status") {
                    calls.lock().unwrap().push("status");
                    object.status = Some(serde_json::from_value(patch["status"].clone()).unwrap());
                } else {
                    calls.lock().unwrap().push("finalizers");
                    object.metadata.finalizers = Some(
                        serde_json::from_value(patch["metadata"]["finalizers"].clone()).unwrap(),
                    );
                }
                let rv: u64 = object.resource_version().unwrap().parse().unwrap();
                object.metadata.resource_version = Some((rv + 1).to_string());
                Ok::<_, Infallible>(
                    http::Response::builder()
                        .header("content-type", "application/json")
                        .body(Full::new(serde_json::to_vec(&*object).unwrap().into()))
                        .unwrap(),
                )
            }
        });
        let api = Api::namespaced(Client::new(service, "agents"), "agents");
        let context = Arc::new(Context {
            api,
            gateway: Arc::new(MockGateway {
                object: Arc::clone(&object),
                calls: Arc::clone(&calls),
            }),
            gateway_url: "http://gateway:8080".into(),
            workspace: "default".into(),
            namespace: "agents".into(),
        });
        (context, object, calls)
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
        reconcile(Arc::new(deleting), context).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec!["status", "get", "finalizers"]);
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
        let status = SandboxStatus::new(binding, 1);
        object.lock().unwrap().status = Some(status.clone());
        let store = KubePersistence {
            api: context.api.clone(),
            object: Mutex::new(object.lock().unwrap().clone()),
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
            request.workspace_scope,
            Some(proto::workspace_selector("default"))
        );
    }
}
