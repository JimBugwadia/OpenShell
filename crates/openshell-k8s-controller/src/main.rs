// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::{coordination::v1::Lease, core::v1::Secret};
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams},
};
use kube_runtime::{
    Controller,
    controller::{self, Action},
    reflector::ObjectRef,
    watcher,
};
use openshell_k8s_controller::{
    Binding, FINALIZER, Gateway, GatewayError, Missing, OpenShellSandbox, Outcome, Persistence,
    PersistenceError, RemoteSandbox, RestartPolicy, SandboxInputs, SandboxStatus,
    ServiceAuthorization, advance, crd_manifest,
    leader::{Election, LeaseLost},
    next_observation,
    provider::{
        self, Desired, OpenShellProvider, ProviderGateway, RefreshStrategy, SecretData,
        SecretReader, SecretsError, Step,
    },
    sync_conditions,
    template::{
        FAMILY_LABEL, GENERATION_LABEL, OpenShellSandboxTemplate, Revision, SOURCE_UID_LABEL,
        TemplateGateway, apply_publish, revision_name,
    },
};
use openshell_sdk::{
    AuthConfig, ClientConfig, ClientIdentity, OpenShellClient,
    raw::{
        CreateSandboxRequest, DeleteSandboxRequest, GetSandboxRequest,
        proto::{
            self, ConfigureProviderRefreshRequest, CreateProviderRequest,
            CreateSandboxTemplateRequest, DeleteProviderRefreshRequest, DeleteProviderRequest,
            DeleteSandboxTemplateRequest, GetProviderRequest, GetSandboxTemplateRequest,
            ListSandboxTemplatesRequest, SandboxPhase, SandboxResources, SandboxWorkloadConfig,
            SandboxWorkloadTemplate, SandboxWorkloadTemplateSpec, UpdateProviderRequest,
            datamodel::v1::{ObjectMeta, Provider},
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
    /// Run the controllers only while holding this Lease, so several replicas
    /// can run with one active. Without it, run only one replica.
    #[arg(long)]
    leader_election_lease: Option<String>,
    /// Namespace of the Lease (default: --namespace).
    #[arg(long, requires = "leader_election_lease")]
    leader_election_namespace: Option<String>,
    /// This replica's identity in the Lease (default: the pod name).
    #[arg(long, env = "POD_NAME", requires = "leader_election_lease")]
    leader_election_identity: Option<String>,
    /// Seconds the Lease stays valid without a renewal.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(3..))]
    lease_duration_seconds: u64,
    /// Seconds between checks of the credential files; 0 disables reloading.
    /// A rotated Secret mounted as files takes effect without a restart.
    #[arg(long, default_value_t = 10)]
    credential_reload_seconds: u64,
}

struct RpcGateway {
    client: std::sync::RwLock<OpenShellClient>,
}

impl RpcGateway {
    fn grpc(&self) -> openshell_sdk::raw::AuthedGrpcClient {
        self.client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .raw_grpc()
    }
}

/// The paths of the gateway credentials.
struct CredentialFiles {
    token: Option<PathBuf>,
    ca: Option<PathBuf>,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
}

/// The contents of the gateway credential files.
#[derive(PartialEq, Eq)]
struct Credentials {
    token: Option<Vec<u8>>,
    ca: Option<Vec<u8>>,
    cert: Option<Vec<u8>>,
    key: Option<Vec<u8>>,
}

impl CredentialFiles {
    fn read(&self) -> std::io::Result<Credentials> {
        let read = |path: &Option<PathBuf>| path.as_ref().map(std::fs::read).transpose();
        Ok(Credentials {
            token: read(&self.token)?,
            ca: read(&self.ca)?,
            cert: read(&self.cert)?,
            key: read(&self.key)?,
        })
    }
}

async fn connect(url: &str, credentials: &Credentials) -> Result<OpenShellClient, String> {
    let mut config = ClientConfig::new(url);
    if let Some(token) = &credentials.token {
        let token = String::from_utf8(token.clone()).map_err(|_| "token file isn't UTF-8")?;
        config.auth = Some(AuthConfig::oidc(token.trim()));
    }
    config.ca_cert.clone_from(&credentials.ca);
    if let (Some(cert), Some(key)) = (&credentials.cert, &credentials.key) {
        config.client_identity = Some(ClientIdentity::new(cert.clone(), key.clone()));
    }
    tokio::time::timeout(CALL_TIMEOUT, OpenShellClient::connect(config))
        .await
        .map_err(|_| "gateway connection timed out".to_owned())?
        .map_err(|_| "gateway connection failed".to_owned())
}

/// Reconnect with new credentials when the files change. Kubernetes updates
/// a mounted Secret atomically, so the certificate and key always match.
/// A failed reconnect keeps the current connection and retries.
async fn reload_credentials(
    gateway: Arc<RpcGateway>,
    url: String,
    files: CredentialFiles,
    mut current: Credentials,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        let next = match files.read() {
            Ok(next) if next != current => next,
            Ok(_) => continue,
            Err(error) => {
                eprintln!("credential reload: cannot read the credential files: {error}");
                continue;
            }
        };
        match connect(&url, &next).await {
            Ok(client) => {
                *gateway
                    .client
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = client;
                current = next;
                eprintln!("credential reload: reconnected to the gateway with new credentials");
            }
            Err(error) => {
                eprintln!("credential reload: {error}; keeping the current connection");
            }
        }
    }
}

fn sandbox_spec(inputs: &SandboxInputs) -> proto::SandboxSpec {
    let template = inputs.workload.as_ref().map(|workload| {
        let resources = workload.resources.clone().unwrap_or_default();
        let limits: std::collections::BTreeMap<String, prost_types::Value> =
            [("cpu", resources.cpu), ("memory", resources.memory)]
                .into_iter()
                .filter(|(_, quantity)| !quantity.is_empty())
                .map(|(key, quantity)| {
                    (
                        key.to_owned(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(quantity)),
                        },
                    )
                })
                .collect();
        proto::SandboxTemplate {
            image: workload.image.clone(),
            environment: workload.environment.clone().into_iter().collect(),
            resources: (!limits.is_empty()).then(|| prost_types::Struct {
                fields: std::collections::BTreeMap::from([(
                    "limits".to_owned(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
                            fields: limits,
                        })),
                    },
                )]),
            }),
            ..Default::default()
        }
    });
    // `SandboxSpec::refusal` blocks an invalid policy before submission.
    let policy = inputs
        .policy
        .as_ref()
        .and_then(|policy| openshell_policy::parse_sandbox_policy(&policy.to_string()).ok());
    proto::SandboxSpec {
        template,
        policy,
        providers: inputs.providers.clone(),
        command: inputs.command.clone(),
        restart_policy: match inputs.restart_policy {
            None => proto::SandboxRestartPolicy::Unspecified,
            Some(RestartPolicy::Never) => proto::SandboxRestartPolicy::Never,
            Some(RestartPolicy::OnFailure) => proto::SandboxRestartPolicy::OnFailure,
            Some(RestartPolicy::Always) => proto::SandboxRestartPolicy::Always,
        }
        .into(),
        ..Default::default()
    }
}

fn create_request(binding: &Binding) -> CreateSandboxRequest {
    CreateSandboxRequest {
        workspace_scope: Some(proto::workspace_selector(&binding.workspace)),
        name: binding.sandbox_name.clone(),
        workload_template: binding.workload_template.clone(),
        request_id: binding.create_request_id.clone(),
        spec: Some(sandbox_spec(&binding.inputs)),
        service_exposures: binding
            .inputs
            .service_exposures
            .iter()
            .map(|exposure| proto::SandboxServiceExposure {
                service: exposure.service.clone(),
                // The schema limits the port to 1-65535; the gateway rejects 0.
                target_port: u32::try_from(exposure.target_port).unwrap_or_default(),
                authorization_mode: match exposure.authorization {
                    None => proto::ServiceAuthorizationMode::Unspecified,
                    Some(ServiceAuthorization::Strip) => proto::ServiceAuthorizationMode::Strip,
                    Some(ServiceAuthorization::BearerPassthrough) => {
                        proto::ServiceAuthorizationMode::BearerPassthrough
                    }
                }
                .into(),
            })
            .collect(),
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

/// The gateway reports an unknown provider as `provider '<name>' not found`.
fn missing_provider(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::FailedPrecondition
        && status.message().starts_with("provider '")
        && status.message().ends_with("' not found")
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
        let result = tokio::time::timeout(CALL_TIMEOUT, self.grpc().create_sandbox(request))
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
            // Only the workload template lookup reports NotFound on create.
            Err(status) if status.code() == tonic::Code::NotFound => {
                Err(GatewayError::Missing(Missing::Template))
            }
            Err(status) if missing_provider(&status) => {
                Err(GatewayError::Missing(Missing::Provider))
            }
            // Validation and authorization refusals precede any gateway side
            // effect.
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
            self.grpc().get_sandbox(GetSandboxRequest {
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
            self.grpc().delete_sandbox(DeleteSandboxRequest {
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
        request_id: String::new(),
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
            self.grpc()
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
                    self.grpc().get_sandbox_template(GetSandboxTemplateRequest {
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
            self.grpc()
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
                self.grpc()
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

fn refresh_strategy(strategy: RefreshStrategy) -> proto::ProviderCredentialRefreshStrategy {
    match strategy {
        RefreshStrategy::OAuth2RefreshToken => {
            proto::ProviderCredentialRefreshStrategy::Oauth2RefreshToken
        }
        RefreshStrategy::OAuth2ClientCredentials => {
            proto::ProviderCredentialRefreshStrategy::Oauth2ClientCredentials
        }
        RefreshStrategy::GoogleServiceAccountJwt => {
            proto::ProviderCredentialRefreshStrategy::GoogleServiceAccountJwt
        }
        RefreshStrategy::AwsStsAssumeRole => {
            proto::ProviderCredentialRefreshStrategy::AwsStsAssumeRole
        }
        RefreshStrategy::GithubAppInstallation => {
            proto::ProviderCredentialRefreshStrategy::GithubAppInstallation
        }
    }
}

/// Provider fields for a create or update. Removed keys are sent empty, which
/// the gateway treats as a delete.
fn provider_message(desired: &Desired) -> Provider {
    fn removed(keys: &[String]) -> impl Iterator<Item = (String, String)> + '_ {
        keys.iter().map(|key| (key.clone(), String::new()))
    }
    Provider {
        metadata: Some(ObjectMeta {
            name: desired.name.clone(),
            labels: HashMap::from([(SOURCE_UID_LABEL.into(), desired.source_uid.clone())]),
            ..Default::default()
        }),
        r#type: desired.type_.clone(),
        profile_workspace: if desired.global_profile {
            String::new()
        } else {
            desired.workspace.clone()
        },
        credentials: desired
            .credentials
            .clone()
            .into_iter()
            .chain(removed(&desired.removed_credentials))
            .collect(),
        config: desired
            .config
            .clone()
            .into_iter()
            .chain(removed(&desired.removed_config))
            .collect(),
        ..Default::default()
    }
}

/// Refusals that precede any gateway side effect.
fn refused(status: &tonic::Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::InvalidArgument
            | tonic::Code::NotFound
            | tonic::Code::FailedPrecondition
            | tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
    )
}

impl RpcGateway {
    /// The provider's owner label, `Ok(None)` when it doesn't exist.
    async fn provider_owner(
        &self,
        workspace: &str,
        name: &str,
    ) -> Result<Option<(String, String)>, GatewayError> {
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.grpc().get_provider(GetProviderRequest {
                workspace_scope: Some(proto::workspace_selector(workspace)),
                name: name.into(),
            }),
        )
        .await
        .map_err(|_| GatewayError::Uncertain)?;
        match result {
            Ok(response) => {
                let provider = response
                    .into_inner()
                    .provider
                    .ok_or(GatewayError::Uncertain)?;
                let owner = provider
                    .metadata
                    .and_then(|m| m.labels.get(SOURCE_UID_LABEL).cloned())
                    .unwrap_or_default();
                Ok(Some((owner, provider.r#type)))
            }
            Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
            Err(_) => Err(GatewayError::Uncertain),
        }
    }
}

#[async_trait]
impl ProviderGateway for RpcGateway {
    async fn apply(&self, desired: &Desired) -> Result<(), GatewayError> {
        let workspace = Some(proto::workspace_selector(&desired.workspace));
        let mut grpc = self.grpc();
        let result = match self
            .provider_owner(&desired.workspace, &desired.name)
            .await?
        {
            None => {
                tokio::time::timeout(
                    CALL_TIMEOUT,
                    grpc.create_provider(CreateProviderRequest {
                        workspace_scope: workspace.clone(),
                        provider: Some(provider_message(desired)),
                        request_id: String::new(),
                    }),
                )
                .await
            }
            Some((owner, _)) if owner != desired.source_uid => return Err(GatewayError::Collision),
            Some((_, kind)) if !kind.eq_ignore_ascii_case(&desired.type_) => {
                return Err(GatewayError::Rejected);
            }
            Some(_) => {
                tokio::time::timeout(
                    CALL_TIMEOUT,
                    grpc.update_provider(UpdateProviderRequest {
                        workspace_scope: workspace.clone(),
                        provider: Some(provider_message(desired)),
                        request_id: String::new(),
                        ..Default::default()
                    }),
                )
                .await
            }
        }
        .map_err(|_| GatewayError::Uncertain)?;
        match result {
            Ok(_) => {}
            // A concurrent create; the next reconcile checks its owner.
            Err(status) if status.code() == tonic::Code::AlreadyExists => {
                return Err(GatewayError::Uncertain);
            }
            Err(status) if refused(&status) => return Err(GatewayError::Rejected),
            Err(_) => return Err(GatewayError::Uncertain),
        }
        for credential in &desired.retired_refresh {
            tokio::time::timeout(
                CALL_TIMEOUT,
                grpc.delete_provider_refresh(DeleteProviderRefreshRequest {
                    workspace_scope: workspace.clone(),
                    provider: desired.name.clone(),
                    credential_key: credential.clone(),
                    allow_missing: true,
                    request_id: String::new(),
                }),
            )
            .await
            .map_err(|_| GatewayError::Uncertain)?
            .map_err(|status| {
                if refused(&status) {
                    GatewayError::Rejected
                } else {
                    GatewayError::Uncertain
                }
            })?;
        }
        for refresh in &desired.refresh {
            tokio::time::timeout(
                CALL_TIMEOUT,
                grpc.configure_provider_refresh(ConfigureProviderRefreshRequest {
                    workspace_scope: workspace.clone(),
                    provider: desired.name.clone(),
                    credential_key: refresh.credential.clone(),
                    strategy: refresh_strategy(refresh.strategy).into(),
                    material: refresh.material.clone().into_iter().collect(),
                    secret_material_keys: refresh.secret_keys.clone(),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|_| GatewayError::Uncertain)?
            .map_err(|status| {
                if refused(&status) {
                    GatewayError::Rejected
                } else {
                    GatewayError::Uncertain
                }
            })?;
        }
        Ok(())
    }

    async fn remove(
        &self,
        workspace: &str,
        name: &str,
        source_uid: &str,
    ) -> Result<(), GatewayError> {
        match self.provider_owner(workspace, name).await? {
            None => return Ok(()),
            Some((owner, _)) if owner != source_uid => return Err(GatewayError::Collision),
            Some(_) => {}
        }
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.grpc().delete_provider(DeleteProviderRequest {
                workspace_scope: Some(proto::workspace_selector(workspace)),
                name: name.into(),
                allow_missing: true,
                request_id: String::new(),
            }),
        )
        .await
        .map_err(|_| GatewayError::Uncertain)?;
        match result {
            Ok(_) => Ok(()),
            // The gateway refuses to delete a provider that sandboxes use.
            Err(status) if status.code() == tonic::Code::FailedPrecondition => {
                Err(GatewayError::Rejected)
            }
            Err(_) => Err(GatewayError::Uncertain),
        }
    }
}

/// Reads Secrets in the controller's namespace.
struct KubeSecrets {
    api: Api<Secret>,
}

#[async_trait]
impl SecretReader for KubeSecrets {
    async fn read(&self, name: &str) -> Result<Option<SecretData>, SecretsError> {
        let secret = self.api.get_opt(name).await.map_err(|_| SecretsError)?;
        Ok(secret.map(|secret| SecretData {
            resource_version: secret.resource_version().unwrap_or_default(),
            data: secret
                .data
                .unwrap_or_default()
                .into_iter()
                .map(|(key, value)| (key, value.0))
                .collect(),
        }))
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Kubernetes request failed")]
    Kube(#[from] kube::Error),
    #[error("Kubernetes persistence failed")]
    Persistence(#[from] PersistenceError),
    #[error("reading a Secret failed")]
    Secrets(#[from] SecretsError),
    #[error("resource lacks a valid UUID identity or generation")]
    Identity,
}

struct Context {
    api: Api<OpenShellSandbox>,
    templates: Api<OpenShellSandboxTemplate>,
    providers: Api<OpenShellProvider>,
    secrets: Arc<dyn SecretReader>,
    gateway: Arc<dyn Gateway>,
    template_gateway: Arc<dyn TemplateGateway>,
    provider_gateway: Arc<dyn ProviderGateway>,
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
    let refusal = object.spec.refusal(&context.namespace);
    let template = match (object.status.as_ref(), &object.spec.template_ref) {
        (Some(status), _) => status.journal.binding.workload_template.clone(),
        (None, Some(reference)) if refusal.is_none() => {
            match resolve_template(&context.templates, &reference.name).await? {
                Some(template) => template,
                None => return Ok(Action::requeue(TEMPLATE_WAIT)),
            }
        }
        // An inline workload, the gateway's default image, or a refused spec.
        (None, _) => String::new(),
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
    .map_err(|_| Error::Identity)?
    .with_inputs(object.spec.inputs.clone());
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
    // A refused spec never reaches the gateway, so deleting it needs no cleanup.
    if let Some(reason) = refusal
        && !deleting
    {
        if status.reason != reason {
            status.phase = "Blocked".into();
            status.reason = reason.into();
            store.save(&status).await?;
        }
        return Ok(Action::requeue(REFUSED_WAIT));
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
/// A refused spec can't change, so it's rechecked only to observe deletion.
const REFUSED_WAIT: Duration = Duration::from_mins(5);

/// Gateway template for a sandbox's `templateRef`: the current revision of
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

/// Apply a provider's spec and Secrets to its gateway provider.
async fn reconcile_provider(
    object: Arc<OpenShellProvider>,
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
        set_finalizers(&context.providers, object.as_ref(), finalizers).await?;
        return Ok(Action::requeue(Duration::from_secs(1)));
    }
    let now = unix_now()?;
    let name = object.name_any();
    let uid = object.uid().ok_or(Error::Identity)?;
    let generation = object.metadata.generation.ok_or(Error::Identity)?;
    let mut status = object.status.clone().unwrap_or_default();
    let step = provider::sync(
        &object.spec,
        &mut status,
        generation,
        &provider::Source {
            workspace: &context.workspace,
            name: &name,
            uid: &uid,
        },
        deleting,
        context.secrets.as_ref(),
        context.provider_gateway.as_ref(),
        now,
    )
    .await?;
    if step == Step::Finalize {
        let finalizers = object
            .finalizers()
            .iter()
            .filter(|f| f.as_str() != FINALIZER)
            .cloned()
            .collect();
        set_finalizers(&context.providers, object.as_ref(), finalizers).await?;
        return Ok(Action::await_change());
    }
    if object.status.as_ref() != Some(&status) {
        context
            .providers
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
    let Step::Requeue(seconds) = step else {
        unreachable!("finalize returned above")
    };
    Ok(Action::requeue(Duration::from_secs(seconds)))
}

/// Log a failed watch. `error_policy` already logs reconcile failures, and
/// server messages can echo object contents, so only the status code is shown.
fn report<T, E>(
    watching: &'static str,
    result: Result<T, controller::Error<E, watcher::Error>>,
) -> futures::future::Ready<()> {
    match result {
        Err(controller::Error::QueueError(error)) => {
            eprintln!("watching {watching} failed: {}", watch_failure(&error));
        }
        Err(controller::Error::RunnerError(_)) => eprintln!("controller observation failed"),
        _ => {}
    }
    futures::future::ready(())
}

fn watch_failure(error: &watcher::Error) -> String {
    let code = match error {
        watcher::Error::InitialListFailed(error)
        | watcher::Error::WatchStartFailed(error)
        | watcher::Error::WatchFailed(error) => match error {
            kube::Error::Api(status) => Some(status.code),
            _ => None,
        },
        watcher::Error::WatchError(status) => Some(status.code),
        watcher::Error::NoResourceVersion => None,
    };
    match code {
        Some(403) => "HTTP 403: check the controller's RBAC".to_string(),
        Some(code) => format!("HTTP {code}"),
        None => "connection error".to_string(),
    }
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
        print!("{}", crd_manifest()?);
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
    let files = CredentialFiles {
        token: args.token_file,
        ca: args.tls_ca,
        cert: args.tls_cert,
        key: args.tls_key,
    };
    let credentials = files.read()?;
    let gateway = Arc::new(RpcGateway {
        client: std::sync::RwLock::new(connect(&gateway_url, &credentials).await?),
    });
    if args.credential_reload_seconds > 0 {
        tokio::spawn(reload_credentials(
            Arc::clone(&gateway),
            gateway_url.clone(),
            files,
            credentials,
            Duration::from_secs(args.credential_reload_seconds),
        ));
    }
    let client = Client::try_default().await?;
    let election = match args.leader_election_lease {
        Some(name) => {
            let identity = args
                .leader_election_identity
                .or_else(|| std::env::var("HOSTNAME").ok())
                .filter(|identity| !identity.trim().is_empty())
                .ok_or("--leader-election-identity is required")?;
            let lease_duration = Duration::from_secs(args.lease_duration_seconds);
            let leases: Api<Lease> = Api::namespaced(
                client.clone(),
                args.leader_election_namespace
                    .as_deref()
                    .unwrap_or(&namespace),
            );
            let election = Election {
                name,
                identity,
                lease_duration,
                retry_period: lease_duration / 3,
            };
            eprintln!(
                "leader election: waiting for Lease {} as {}",
                election.name, election.identity
            );
            election.acquire(&leases).await;
            eprintln!("leader election: acquired Lease {}", election.name);
            Some((election, leases))
        }
        None => None,
    };
    let api = Api::namespaced(client.clone(), &namespace);
    let templates = Api::namespaced(client.clone(), &namespace);
    let providers: Api<OpenShellProvider> = Api::namespaced(client.clone(), &namespace);
    let secrets: Api<Secret> = Api::namespaced(client, &namespace);
    let context = Arc::new(Context {
        api: api.clone(),
        templates: templates.clone(),
        providers: providers.clone(),
        secrets: Arc::new(KubeSecrets {
            api: secrets.clone(),
        }),
        gateway: gateway.clone(),
        template_gateway: gateway.clone(),
        provider_gateway: gateway,
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
        .for_each(|result| report("sandboxes", result));
    let templates = Controller::new(templates, watcher::Config::default())
        .with_config(config.clone())
        .shutdown_on_signal()
        .run(reconcile_template, error_policy, Arc::clone(&context))
        .for_each(|result| report("templates", result));
    // A Secret change reconciles every provider that references the Secret.
    let provider_controller = Controller::new(providers, watcher::Config::default());
    let declared = provider_controller.store();
    let providers = provider_controller
        .watches(
            secrets,
            watcher::Config::default(),
            move |secret: Secret| {
                let name = secret.name_any();
                declared
                    .state()
                    .into_iter()
                    .filter(|provider| provider::references_secret(&provider.spec, &name))
                    .map(|provider| ObjectRef::from_obj(provider.as_ref()))
                    .collect::<Vec<_>>()
            },
        )
        .with_config(config)
        .shutdown_on_signal()
        .run(reconcile_provider, error_policy, context)
        .for_each(|result| report("providers or their Secrets", result));
    let controllers = async {
        futures::join!(sandboxes, templates, providers);
    };
    match &election {
        Some((election, leases)) => {
            tokio::select! {
                () = controllers => election.release(leases).await,
                LeaseLost = election.hold(leases) => {
                    // Exit at once: the controllers must stop before another
                    // replica can take the Lease.
                    eprintln!("leader election: lost Lease {}; exiting", election.name);
                    std::process::exit(1);
                }
            }
        }
        None => controllers.await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Mutex as SyncMutex};

    use http_body_util::{BodyExt, Full};
    use openshell_k8s_controller::{
        SandboxSpec, TemplateRef,
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
    impl ProviderGateway for MockGateway {
        async fn apply(&self, _: &Desired) -> Result<(), GatewayError> {
            self.calls.lock().unwrap().push("apply");
            Ok(())
        }

        async fn remove(&self, _: &str, _: &str, _: &str) -> Result<(), GatewayError> {
            self.calls.lock().unwrap().push("remove");
            Ok(())
        }
    }

    struct NoSecrets;

    #[async_trait]
    impl SecretReader for NoSecrets {
        async fn read(&self, _: &str) -> Result<Option<SecretData>, SecretsError> {
            Ok(None)
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
                template_ref: Some(TemplateRef {
                    name: "agent-v1".into(),
                    namespace: None,
                }),
                inputs: SandboxInputs::default(),
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
            templates: Api::namespaced(client.clone(), "agents"),
            providers: Api::namespaced(client, "agents"),
            secrets: Arc::new(NoSecrets),
            gateway: gateway.clone(),
            template_gateway: gateway.clone(),
            provider_gateway: gateway,
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
    fn create_inputs_map_to_the_sandbox_spec() {
        let inputs = SandboxInputs {
            workload: Some(TemplateSpec {
                image: "registry.example.com/agent:1".into(),
                environment: [("MODE".to_owned(), "prod".to_owned())].into(),
                resources: Some(openshell_k8s_controller::template::TemplateResources {
                    cpu: "2".into(),
                    memory: String::new(),
                }),
            }),
            providers: vec!["nvidia".into()],
            command: vec!["python".into(), "/app/agent.py".into()],
            restart_policy: Some(RestartPolicy::OnFailure),
            service_exposures: vec![openshell_k8s_controller::ServiceExposure {
                service: "web".into(),
                target_port: 8080,
                authorization: Some(ServiceAuthorization::BearerPassthrough),
            }],
            policy: Some(json!({
                "version": 1,
                "network_policies": {
                    "github_api": {
                        "name": "github-api-readonly",
                        "endpoints": [{"host": "api.github.com", "port": 443}],
                        "binaries": [{"path": "/usr/bin/curl"}]
                    }
                }
            })),
        };
        let binding = Binding::new(
            "fd229996-bc63-413e-9dc0-cc5f05abe5bd",
            "agent",
            "agents",
            "http://gateway:8080",
            "default",
            "",
            1,
        )
        .unwrap()
        .with_inputs(inputs);
        let request = create_request(&binding);
        let spec = request.spec.unwrap();
        let template = spec.template.unwrap();
        assert_eq!(template.image, "registry.example.com/agent:1");
        assert_eq!(
            template.environment.get("MODE").map(String::as_str),
            Some("prod")
        );
        let limits = &template.resources.unwrap().fields["limits"];
        let Some(prost_types::value::Kind::StructValue(limits)) = &limits.kind else {
            panic!("limits must be a struct");
        };
        assert_eq!(limits.fields.len(), 1, "an empty quantity is omitted");
        assert_eq!(spec.providers, ["nvidia"]);
        assert_eq!(spec.command, ["python", "/app/agent.py"]);
        assert_eq!(
            spec.restart_policy,
            i32::from(proto::SandboxRestartPolicy::OnFailure)
        );
        assert_eq!(request.workload_template, "");
        assert_eq!(request.service_exposures.len(), 1);
        assert_eq!(request.service_exposures[0].target_port, 8080);
        assert_eq!(
            request.service_exposures[0].authorization_mode,
            i32::from(proto::ServiceAuthorizationMode::BearerPassthrough)
        );
        let policy = spec.policy.expect("the policy is sent");
        assert_eq!(policy.version, 1);
        assert!(policy.network_policies.contains_key("github_api"));
    }

    #[test]
    fn provider_message_owns_the_provider_and_clears_removed_keys() {
        let desired = Desired {
            workspace: "default".into(),
            name: "nvidia".into(),
            source_uid: "0b5e8f2a-3c1d-4e6f-8a9b-1c2d3e4f5a6b".into(),
            type_: "nvidia".into(),
            global_profile: false,
            credentials: [("NVIDIA_API_KEY".to_owned(), "value".to_owned())].into(),
            config: [("region".to_owned(), "us".to_owned())].into(),
            removed_credentials: vec!["OLD_KEY".into()],
            removed_config: vec!["old".into()],
            refresh: Vec::new(),
            retired_refresh: Vec::new(),
        };
        let provider = provider_message(&desired);
        let metadata = provider.metadata.unwrap();
        assert_eq!(metadata.name, "nvidia");
        assert_eq!(
            metadata.labels.get(SOURCE_UID_LABEL).map(String::as_str),
            Some("0b5e8f2a-3c1d-4e6f-8a9b-1c2d3e4f5a6b")
        );
        assert_eq!(provider.r#type, "nvidia");
        assert_eq!(
            provider.profile_workspace, "default",
            "a workspace profile by default"
        );
        assert_eq!(provider.credentials["NVIDIA_API_KEY"], "value");
        assert_eq!(provider.credentials["OLD_KEY"], "", "empty deletes the key");
        assert_eq!(provider.config["region"], "us");
        assert_eq!(provider.config["old"], "");
        assert_eq!(
            refresh_strategy(RefreshStrategy::OAuth2ClientCredentials),
            proto::ProviderCredentialRefreshStrategy::Oauth2ClientCredentials
        );
    }

    #[test]
    fn only_the_gateway_provider_message_counts_as_a_missing_provider() {
        let missing = tonic::Status::failed_precondition("provider 'nvidia' not found");
        assert!(missing_provider(&missing));
        let other = tonic::Status::failed_precondition("sandbox updates are not enabled");
        assert!(!missing_provider(&other));
        let wrong_code = tonic::Status::not_found("provider 'nvidia' not found");
        assert!(!missing_provider(&wrong_code));
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
