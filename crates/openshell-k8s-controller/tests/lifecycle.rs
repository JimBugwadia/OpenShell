// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::VecDeque, sync::Mutex};

use async_trait::async_trait;
use openshell_k8s_controller::{
    Binding, Gateway, GatewayError, Outcome, Persistence, PersistenceError, RemoteSandbox,
    SandboxStatus, advance, crd,
};

const NOW: i64 = 10_000;

struct Store {
    durable: Mutex<SandboxStatus>,
    fail: bool,
}

#[async_trait]
impl Persistence for Store {
    async fn save(&self, status: &SandboxStatus) -> Result<(), PersistenceError> {
        if self.fail {
            return Err(PersistenceError);
        }
        *self.durable.lock().unwrap() = status.clone();
        Ok(())
    }
}

struct MockGateway<'a> {
    store: &'a Store,
    create_results: Mutex<VecDeque<Result<RemoteSandbox, GatewayError>>>,
    current: Mutex<Option<RemoteSandbox>>,
    creates: Mutex<Vec<String>>,
    deletes: Mutex<Vec<String>>,
    delete_error: bool,
}

#[async_trait]
impl Gateway for MockGateway<'_> {
    async fn create(&self, binding: &Binding) -> Result<RemoteSandbox, GatewayError> {
        let durable = self.store.durable.lock().unwrap();
        assert!(durable.create_attempts > 0, "intent must precede RPC");
        assert_eq!(&durable.binding, binding);
        self.creates
            .lock()
            .unwrap()
            .push(binding.create_request_id.clone());
        self.create_results.lock().unwrap().pop_front().unwrap()
    }

    async fn get(&self, _: &Binding) -> Result<Option<RemoteSandbox>, GatewayError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn delete(&self, binding: &Binding) -> Result<(), GatewayError> {
        let durable = self.store.durable.lock().unwrap();
        assert!(durable.sandbox_id.is_some(), "receipt must precede delete");
        assert!(durable.delete_attempts > 0);
        self.deletes
            .lock()
            .unwrap()
            .push(binding.delete_request_id.clone());
        if self.delete_error {
            Err(GatewayError::Uncertain)
        } else {
            Ok(())
        }
    }
}

fn initial() -> SandboxStatus {
    SandboxStatus::new(
        Binding::new(
            "fd229996-bc63-413e-9dc0-cc5f05abe5bd",
            "agent",
            "agents",
            "http://gateway:8080",
            "default",
            "agent-v1",
            NOW,
        )
        .unwrap(),
        1,
    )
}

fn remote(status: &SandboxStatus, phase: &str) -> RemoteSandbox {
    RemoteSandbox {
        id: "gateway-id".into(),
        name: status.binding.sandbox_name.clone(),
        phase: phase.into(),
    }
}

fn mock(store: &Store) -> MockGateway<'_> {
    MockGateway {
        store,
        create_results: Mutex::default(),
        current: Mutex::default(),
        creates: Mutex::default(),
        deletes: Mutex::default(),
        delete_error: false,
    }
}

#[tokio::test]
async fn lost_create_response_restarts_from_durable_intent_and_replays() {
    let mut status = initial();
    let accepted = remote(&status, "Ready");
    let store = Store {
        durable: Mutex::new(status.clone()),
        fail: false,
    };
    let gateway = mock(&store);
    gateway
        .create_results
        .lock()
        .unwrap()
        .extend([Err(GatewayError::Uncertain), Ok(accepted.clone())]);
    advance(&mut status, false, NOW, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(status.reason, "CreateUncertain");
    // Discard all volatile controller state, as on restart.
    let mut restarted = store.durable.lock().unwrap().clone();
    advance(&mut restarted, false, NOW + 30, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(restarted.sandbox_id.as_deref(), Some("gateway-id"));
    {
        let ids = gateway.creates.lock().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
    }
    *gateway.current.lock().unwrap() = Some(accepted);
    advance(&mut restarted, false, NOW + 60, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(restarted.phase, "Ready");
}

#[tokio::test]
async fn failed_durable_intent_prevents_submission() {
    let mut status = initial();
    let store = Store {
        durable: Mutex::new(status.clone()),
        fail: true,
    };
    let gateway = mock(&store);
    assert!(
        advance(&mut status, false, NOW, &gateway, &store)
            .await
            .is_err()
    );
    assert!(gateway.creates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn delete_during_ambiguous_create_recovers_receipt_then_waits_for_absence() {
    let mut status = initial();
    let accepted = remote(&status, "Ready");
    let store = Store {
        durable: Mutex::new(status.clone()),
        fail: false,
    };
    let gateway = mock(&store);
    gateway
        .create_results
        .lock()
        .unwrap()
        .extend([Err(GatewayError::Uncertain), Ok(accepted.clone())]);
    advance(&mut status, false, NOW, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(
        advance(&mut status, true, NOW + 30, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Requeue
    );
    *gateway.current.lock().unwrap() = Some(accepted);
    advance(&mut status, true, NOW + 60, &gateway, &store)
        .await
        .unwrap();
    assert!(status.delete_accepted);
    assert_eq!(gateway.deletes.lock().unwrap().len(), 1);
    // Successful DeleteSandbox admission is not proof cleanup has completed.
    assert_eq!(
        advance(&mut status, true, NOW + 90, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Requeue
    );
    *gateway.current.lock().unwrap() = None;
    assert_eq!(
        advance(&mut status, true, NOW + 120, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Finalize
    );
}

#[tokio::test]
async fn collisions_and_replaced_ids_are_never_adopted_or_deleted() {
    let mut status = initial();
    let store = Store {
        durable: Mutex::new(status.clone()),
        fail: false,
    };
    let gateway = mock(&store);
    gateway
        .create_results
        .lock()
        .unwrap()
        .push_back(Err(GatewayError::Collision));
    advance(&mut status, false, NOW, &gateway, &store)
        .await
        .unwrap();
    advance(&mut status, true, NOW + 30, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(status.reason, "NameCollision");
    assert!(status.sandbox_id.is_none());
    status.sandbox_id = Some("original".into());
    *gateway.current.lock().unwrap() = Some(remote(&status, "Ready"));
    advance(&mut status, true, NOW + 60, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(status.reason, "IdentityMismatch");
    assert!(gateway.deletes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn accepted_stopped_or_missing_sandbox_is_not_recreated() {
    let mut status = initial();
    status.sandbox_id = Some("gateway-id".into());
    status.create_attempts = 1;
    let store = Store {
        durable: Mutex::new(status.clone()),
        fail: false,
    };
    let gateway = mock(&store);
    *gateway.current.lock().unwrap() = Some(remote(&status, "Stopped"));
    advance(&mut status, false, NOW, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(status.gateway_phase.as_deref(), Some("Stopped"));
    assert_ne!(status.phase, "Ready");
    *gateway.current.lock().unwrap() = None;
    advance(&mut status, false, NOW + 30, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(status.reason, "AcceptedSandboxMissing");
    assert!(gateway.creates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn expired_ambiguous_create_and_delete_fail_closed() {
    let mut status = initial();
    status.create_attempts = 1;
    let store = Store {
        durable: Mutex::new(status.clone()),
        fail: false,
    };
    let gateway = mock(&store);
    let expired = NOW + 24 * 60 * 60;
    assert_eq!(
        advance(&mut status, true, expired, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Requeue
    );
    assert_eq!(status.reason, "CreateReplayExhausted");
    assert!(gateway.creates.lock().unwrap().is_empty());
    status.sandbox_id = Some("gateway-id".into());
    status.delete_started_at = Some(NOW);
    status.delete_attempts = 1;
    *gateway.current.lock().unwrap() = Some(remote(&status, "Ready"));
    advance(&mut status, true, expired, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(status.reason, "DeleteReplayExhausted");
    assert!(gateway.deletes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn lost_delete_response_replays_same_request_and_never_early_finalizes() {
    let mut status = initial();
    status.sandbox_id = Some("gateway-id".into());
    status.create_attempts = 1;
    let store = Store {
        durable: Mutex::new(status.clone()),
        fail: false,
    };
    let mut gateway = mock(&store);
    gateway.delete_error = true;
    *gateway.current.lock().unwrap() = Some(remote(&status, "Ready"));
    advance(&mut status, true, NOW, &gateway, &store)
        .await
        .unwrap();
    let mut restarted = store.durable.lock().unwrap().clone();
    assert_eq!(
        advance(&mut restarted, true, NOW + 30, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Requeue
    );
    let ids = gateway.deletes.lock().unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
}

#[test]
fn generated_crd_uses_transition_cel_and_status_subresource() {
    let crd = crd().unwrap();
    let spec_schema =
        &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"];
    // Structural CRD schemas cannot combine properties with additionalProperties.
    assert!(spec_schema.get("additionalProperties").is_none());
    assert_eq!(crd["spec"]["names"]["kind"], "OpenShellSandbox");
    assert_eq!(
        crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["x-kubernetes-validations"]
            [0]["rule"],
        "self == oldSelf"
    );
    assert!(crd["spec"]["versions"][0]["subresources"]["status"].is_object());
}

#[test]
fn gateway_name_fits_routable_limit_without_truncating_operation_identity() {
    let status = initial();
    assert_eq!(status.binding.sandbox_name, "os-fd229996bc63413e");
    assert!(status.binding.sandbox_name.len() <= 19);
    assert_eq!(
        status.binding.source_uid,
        "fd229996-bc63-413e-9dc0-cc5f05abe5bd"
    );
    assert_eq!(status.binding.create_request_id, status.binding.source_uid);
}
