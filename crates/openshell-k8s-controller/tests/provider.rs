// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

use async_trait::async_trait;
use openshell_k8s_controller::{
    ConditionStatus, GatewayError,
    provider::{
        CredentialSource, Desired, MaterialSource, ProviderGateway, ProviderSpec, ProviderStatus,
        RefreshSource, RefreshStrategy, SecretData, SecretKeyRef, SecretReader, SecretsError,
        Source, Step, references_secret, sync,
    },
    provider_crd,
};

const NOW: i64 = 10_000;
const UID: &str = "0b5e8f2a-3c1d-4e6f-8a9b-1c2d3e4f5a6b";
const SOURCE: Source<'static> = Source {
    workspace: "default",
    name: "nvidia",
    uid: UID,
};

/// A Secret's resource version and data.
type Stored = (String, BTreeMap<String, Vec<u8>>);

/// Secrets by name.
struct Secrets(Mutex<BTreeMap<String, Stored>>);

impl Secrets {
    fn with(entries: &[(&str, &str, &str, &str)]) -> Self {
        let secrets = Self(Mutex::new(BTreeMap::new()));
        for (name, version, key, value) in entries {
            secrets.put(name, version, key, value);
        }
        secrets
    }

    fn put(&self, name: &str, version: &str, key: &str, value: &str) {
        let mut secrets = self.0.lock().unwrap();
        let entry = secrets
            .entry(name.into())
            .or_insert_with(|| (String::new(), BTreeMap::new()));
        entry.0 = version.into();
        entry.1.insert(key.into(), value.as_bytes().to_vec());
    }
}

#[async_trait]
impl SecretReader for Secrets {
    async fn read(&self, name: &str) -> Result<Option<SecretData>, SecretsError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(name)
            .map(|(version, data)| SecretData {
                resource_version: version.clone(),
                data: data.clone(),
            }))
    }
}

/// What one `apply` call carried, without secret values except where a test
/// checks them.
#[derive(Debug, PartialEq, Eq)]
struct Applied {
    credentials: BTreeMap<String, String>,
    removed_credentials: Vec<String>,
    removed_config: Vec<String>,
    refresh: Vec<(String, BTreeMap<String, String>, Vec<String>)>,
    retired_refresh: Vec<String>,
}

#[derive(Default)]
struct Gateway {
    apply_results: Mutex<VecDeque<Result<(), GatewayError>>>,
    remove_results: Mutex<VecDeque<Result<(), GatewayError>>>,
    applied: Mutex<Vec<Applied>>,
    removed: Mutex<Vec<String>>,
}

#[async_trait]
impl ProviderGateway for Gateway {
    async fn apply(&self, desired: &Desired) -> Result<(), GatewayError> {
        assert_eq!(desired.source_uid, UID);
        assert_eq!(desired.name, "nvidia");
        self.applied.lock().unwrap().push(Applied {
            credentials: desired.credentials.clone(),
            removed_credentials: desired.removed_credentials.clone(),
            removed_config: desired.removed_config.clone(),
            refresh: desired
                .refresh
                .iter()
                .map(|r| {
                    (
                        r.credential.clone(),
                        r.material.clone(),
                        r.secret_keys.clone(),
                    )
                })
                .collect(),
            retired_refresh: desired.retired_refresh.clone(),
        });
        self.apply_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn remove(&self, _: &str, name: &str, uid: &str) -> Result<(), GatewayError> {
        assert_eq!(uid, UID);
        self.removed.lock().unwrap().push(name.into());
        self.remove_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()))
    }
}

fn reference(name: &str, key: &str) -> SecretKeyRef {
    SecretKeyRef {
        name: name.into(),
        key: key.into(),
    }
}

fn spec() -> ProviderSpec {
    ProviderSpec {
        type_: "nvidia".into(),
        global_profile: false,
        credentials: vec![CredentialSource {
            name: "NVIDIA_API_KEY".into(),
            secret_key_ref: reference("nvidia-credentials", "api-key"),
        }],
        config: [("region".to_owned(), "us".to_owned())].into(),
        refresh: Vec::new(),
    }
}

async fn run(
    spec: &ProviderSpec,
    status: &mut ProviderStatus,
    generation: i64,
    deleting: bool,
    secrets: &Secrets,
    gateway: &Gateway,
) -> Step {
    sync(
        spec, status, generation, &SOURCE, deleting, secrets, gateway, NOW,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn applies_secret_values_and_records_only_versions() {
    let secrets = Secrets::with(&[("nvidia-credentials", "7", "api-key", "first")]);
    let gateway = Gateway::default();
    let mut status = ProviderStatus::default();
    let step = run(&spec(), &mut status, 1, false, &secrets, &gateway).await;
    assert_eq!(step, Step::Requeue(3600));
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Ready", "Applied")
    );
    assert_eq!(status.conditions[0].status, ConditionStatus::True);
    assert_eq!(
        status.secret_versions,
        BTreeMap::from([("nvidia-credentials".to_owned(), "7".to_owned())])
    );
    assert_eq!(status.credential_keys, ["NVIDIA_API_KEY"]);
    assert_eq!(status.config_keys, ["region"]);
    let serialized = serde_json::to_string(&status).unwrap();
    assert!(!serialized.contains("first"), "status never holds values");
    assert_eq!(
        gateway.applied.lock().unwrap()[0].credentials["NVIDIA_API_KEY"],
        "first"
    );

    // Unchanged spec and Secrets need no gateway call.
    run(&spec(), &mut status, 1, false, &secrets, &gateway).await;
    assert_eq!(gateway.applied.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn secret_rotation_applies_the_new_value() {
    let secrets = Secrets::with(&[("nvidia-credentials", "7", "api-key", "first")]);
    let gateway = Gateway::default();
    let mut status = ProviderStatus::default();
    run(&spec(), &mut status, 1, false, &secrets, &gateway).await;
    secrets.put("nvidia-credentials", "8", "api-key", "second");
    run(&spec(), &mut status, 1, false, &secrets, &gateway).await;
    let applied = gateway.applied.lock().unwrap();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[1].credentials["NVIDIA_API_KEY"], "second");
    assert_eq!(status.secret_versions["nvidia-credentials"], "8");
}

#[tokio::test]
async fn missing_secret_or_key_blocks_without_a_gateway_call() {
    let gateway = Gateway::default();
    let mut status = ProviderStatus::default();
    let none = Secrets::with(&[]);
    let step = run(&spec(), &mut status, 1, false, &none, &gateway).await;
    assert_eq!(step, Step::Requeue(60));
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Blocked", "SecretNotFound")
    );
    let wrong_key = Secrets::with(&[("nvidia-credentials", "7", "token", "first")]);
    run(&spec(), &mut status, 1, false, &wrong_key, &gateway).await;
    assert_eq!(status.reason, "SecretNotFound");
    assert!(gateway.applied.lock().unwrap().is_empty());

    // Creating the Secret unblocks the provider.
    wrong_key.put("nvidia-credentials", "8", "api-key", "first");
    run(&spec(), &mut status, 1, false, &wrong_key, &gateway).await;
    assert_eq!(status.reason, "Applied");
}

#[tokio::test]
async fn gateway_outcomes_map_to_reasons() {
    let secrets = Secrets::with(&[("nvidia-credentials", "7", "api-key", "first")]);
    let gateway = Gateway::default();
    gateway.apply_results.lock().unwrap().extend([
        Err(GatewayError::Collision),
        Err(GatewayError::Rejected),
        Err(GatewayError::Uncertain),
    ]);
    let mut status = ProviderStatus::default();
    run(&spec(), &mut status, 1, false, &secrets, &gateway).await;
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Blocked", "NameCollision")
    );
    run(&spec(), &mut status, 1, false, &secrets, &gateway).await;
    assert_eq!(status.reason, "ProviderRejected");
    let step = run(&spec(), &mut status, 1, false, &secrets, &gateway).await;
    assert_eq!(step, Step::Requeue(30));
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Progressing", "ApplyUncertain")
    );
    assert_eq!(status.conditions[0].status, ConditionStatus::Unknown);
    // Only a successful apply records what the gateway holds.
    assert!(status.credential_keys.is_empty());
}

#[tokio::test]
async fn undeclared_keys_and_refresh_are_removed() {
    let secrets = Secrets::with(&[
        ("nvidia-credentials", "7", "api-key", "first"),
        ("oauth", "3", "client-secret", "shh"),
    ]);
    let gateway = Gateway::default();
    let mut status = ProviderStatus::default();
    let mut declared = spec();
    declared.refresh = vec![RefreshSource {
        credential: "ACCESS_TOKEN".into(),
        strategy: RefreshStrategy::OAuth2ClientCredentials,
        material: vec![
            MaterialSource {
                name: "client_id".into(),
                value: Some("agent".into()),
                secret_key_ref: None,
            },
            MaterialSource {
                name: "client_secret".into(),
                value: None,
                secret_key_ref: Some(reference("oauth", "client-secret")),
            },
        ],
    }];
    run(&declared, &mut status, 1, false, &secrets, &gateway).await;
    {
        let applied = gateway.applied.lock().unwrap();
        let (credential, material, secret_keys) = &applied[0].refresh[0];
        assert_eq!(credential, "ACCESS_TOKEN");
        assert_eq!(material["client_id"], "agent");
        assert_eq!(material["client_secret"], "shh");
        assert_eq!(
            secret_keys,
            &["client_secret"],
            "inline values aren't secret"
        );
    }
    assert_eq!(status.refresh_credentials, ["ACCESS_TOKEN"]);
    assert_eq!(status.secret_versions.len(), 2);

    let mut reduced = spec();
    reduced.credentials.clear();
    reduced.config.clear();
    run(&reduced, &mut status, 2, false, &secrets, &gateway).await;
    let applied = gateway.applied.lock().unwrap();
    assert_eq!(applied[1].removed_credentials, ["NVIDIA_API_KEY"]);
    assert_eq!(applied[1].removed_config, ["region"]);
    assert_eq!(applied[1].retired_refresh, ["ACCESS_TOKEN"]);
    assert!(status.credential_keys.is_empty());
    assert!(status.secret_versions.is_empty());
}

#[tokio::test]
async fn deletion_removes_only_an_owned_provider_and_waits_while_in_use() {
    let secrets = Secrets::with(&[]);
    let gateway = Gateway::default();
    gateway.remove_results.lock().unwrap().extend([
        Err(GatewayError::Rejected),
        Err(GatewayError::Uncertain),
        Ok(()),
    ]);
    let mut status = ProviderStatus::default();
    // Deletion needs no Secrets.
    let step = run(&spec(), &mut status, 1, true, &secrets, &gateway).await;
    assert_eq!(step, Step::Requeue(60));
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Blocked", "ProviderInUse")
    );
    run(&spec(), &mut status, 1, true, &secrets, &gateway).await;
    assert_eq!(status.reason, "RemoveUncertain");
    let step = run(&spec(), &mut status, 1, true, &secrets, &gateway).await;
    assert_eq!(step, Step::Finalize);

    // Another owner's provider is left alone and the finalizer is released.
    let gateway = Gateway::default();
    gateway
        .remove_results
        .lock()
        .unwrap()
        .push_back(Err(GatewayError::Collision));
    let step = run(&spec(), &mut status, 1, true, &secrets, &gateway).await;
    assert_eq!(step, Step::Finalize);
}

#[test]
fn secret_references_include_refresh_material() {
    let mut declared = spec();
    declared.refresh = vec![RefreshSource {
        credential: "ACCESS_TOKEN".into(),
        strategy: RefreshStrategy::OAuth2RefreshToken,
        material: vec![MaterialSource {
            name: "refresh_token".into(),
            value: None,
            secret_key_ref: Some(reference("oauth", "refresh-token")),
        }],
    }];
    assert!(references_secret(&declared, "nvidia-credentials"));
    assert!(references_secret(&declared, "oauth"));
    assert!(!references_secret(&declared, "other"));
}

#[test]
fn provider_crd_keeps_type_immutable_and_uses_the_osprov_short_name() {
    let crd = provider_crd().unwrap();
    assert_eq!(
        crd["spec"]["names"]["shortNames"],
        serde_json::json!(["osprov"])
    );
    let schema = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"];
    let rules = schema["spec"]["x-kubernetes-validations"]
        .as_array()
        .unwrap();
    assert!(
        rules[0]["rule"]
            .as_str()
            .unwrap()
            .starts_with("self.type == oldSelf.type")
    );
    assert!(rules[1]["rule"].as_str().unwrap().contains("refresh"));
    let material =
        &schema["spec"]["properties"]["refresh"]["items"]["properties"]["material"]["items"];
    assert_eq!(
        material["x-kubernetes-validations"][0]["rule"],
        "has(self.value) != has(self.secretKeyRef)"
    );
    assert!(
        schema["spec"]["properties"]["credentials"]["items"]["properties"]
            .get("value")
            .is_none(),
        "credentials only reference Secrets"
    );
    assert_eq!(
        schema["status"]["properties"]["conditions"]["x-kubernetes-list-type"],
        "map"
    );
}
