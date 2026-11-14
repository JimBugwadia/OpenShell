// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::VecDeque, sync::Mutex};

use async_trait::async_trait;
use openshell_k8s_controller::{
    Binding, ConditionStatus, Gateway, GatewayError, Outcome, Persistence, PersistenceError,
    RemoteSandbox, SandboxStatus, advance, crd, sync_conditions,
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
        assert!(
            durable.journal.create_attempts > 0,
            "intent must precede RPC"
        );
        assert_eq!(&durable.journal.binding, binding);
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
        assert!(durable.journal.delete_attempts > 0);
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
        name: status.journal.binding.sandbox_name.clone(),
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
    assert!(status.journal.delete_accepted);
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
    assert_eq!(status.reason, "NameCollision");
    assert!(!store.durable.lock().unwrap().journal.create_may_exist);
    // A first-attempt collision proves no record of ours exists, so deletion
    // completes without adopting or deleting the colliding record.
    assert_eq!(
        advance(&mut status, true, NOW + 30, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Finalize
    );
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
    status.journal.create_attempts = 1;
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
    status.journal.create_attempts = 1;
    status.journal.create_may_exist = true;
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
    status.journal.delete_started_at = Some(NOW);
    status.journal.delete_attempts = 1;
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
    status.journal.create_attempts = 1;
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
    assert_eq!(status.journal.binding.sandbox_name, "os-fd229996bc63413e");
    assert!(status.journal.binding.sandbox_name.len() <= 19);
    assert_eq!(
        status.journal.binding.source_uid,
        "fd229996-bc63-413e-9dc0-cc5f05abe5bd"
    );
    assert_eq!(
        status.journal.binding.create_request_id,
        status.journal.binding.source_uid
    );
}

#[test]
fn generated_crd_has_no_unsigned_formats_and_exposes_printer_columns() {
    let crd = crd().unwrap();
    let text = serde_json::to_string(&crd).unwrap();
    assert!(
        !text.contains("uint32"),
        "Kubernetes does not recognize uint32"
    );
    assert!(!text.contains("\"format\":\"uint"));
    assert_eq!(
        crd["spec"]["names"]["shortNames"],
        serde_json::json!(["ossb"])
    );
    let columns: Vec<_> = crd["spec"]["versions"][0]["additionalPrinterColumns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(columns, ["Phase", "Reason", "Gateway Phase", "Age"]);
    let status = &crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]["properties"];
    assert_eq!(status["conditions"]["x-kubernetes-list-type"], "map");
    assert_eq!(
        status["journal"]["properties"]["retryAfter"]["format"],
        "date-time"
    );
}

#[test]
fn status_serializes_rfc3339_times_and_separates_journal() {
    let mut status = initial();
    status.journal.retry_after = Some(NOW + 30);
    sync_conditions(&mut status, NOW);
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(value["sandboxName"], "os-fd229996bc63413e");
    assert_eq!(
        value["journal"]["binding"]["createStartedAt"],
        "1970-01-01T02:46:40Z"
    );
    assert_eq!(value["journal"]["retryAfter"], "1970-01-01T02:47:10Z");
    // Merge patches must clear stale values, so None serializes as null.
    assert!(value["journal"]["deleteStartedAt"].is_null());
    assert!(value["journal"].get("deleteStartedAt").is_some());
    assert!(value.get("createAttempts").is_none());
    assert_eq!(
        value["conditions"][0]["lastTransitionTime"],
        "1970-01-01T02:46:40Z"
    );
    let decoded: SandboxStatus = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, status);
}

#[test]
fn ready_condition_tracks_phase_and_only_moves_transition_time_on_change() {
    let mut status = initial();
    sync_conditions(&mut status, NOW);
    let ready = &status.conditions[0];
    assert_eq!(ready.type_, "Ready");
    assert_eq!(ready.status, ConditionStatus::False);
    assert_eq!(ready.reason, "PendingSubmission");
    assert_eq!(ready.last_transition_time, NOW);

    status.reason = "Accepted".into();
    sync_conditions(&mut status, NOW + 10);
    assert_eq!(status.conditions[0].reason, "Accepted");
    assert_eq!(status.conditions[0].last_transition_time, NOW);

    status.phase = "Ready".into();
    status.reason = "GatewayReady".into();
    status.observed_generation = 2;
    sync_conditions(&mut status, NOW + 20);
    assert_eq!(status.conditions.len(), 1);
    assert_eq!(status.conditions[0].status, ConditionStatus::True);
    assert_eq!(status.conditions[0].last_transition_time, NOW + 20);
    assert_eq!(status.conditions[0].observed_generation, 2);

    status.phase = "Blocked".into();
    status.reason = "ObservationUnavailable".into();
    sync_conditions(&mut status, NOW + 30);
    assert_eq!(status.conditions[0].status, ConditionStatus::Unknown);
    assert_eq!(status.conditions[0].last_transition_time, NOW + 30);
    // Messages are fixed text selected by reason, never gateway error text.
    status.reason = "SomeFutureReason".into();
    sync_conditions(&mut status, NOW + 40);
    assert_eq!(
        status.conditions[0].message,
        "The sandbox is blocked; investigate the status reason."
    );
}

#[tokio::test]
async fn rejected_create_blocks_without_retry_and_deletes_cleanly() {
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
        .push_back(Err(GatewayError::Rejected));
    advance(&mut status, false, NOW, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Blocked", "CreateRejected")
    );
    assert!(!store.durable.lock().unwrap().journal.create_may_exist);
    advance(&mut status, false, NOW + 3600, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(gateway.creates.lock().unwrap().len(), 1);
    assert_eq!(
        advance(&mut status, true, NOW + 3630, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Finalize
    );
}

#[tokio::test]
async fn rejection_after_uncertain_attempt_keeps_replaying_and_blocks_deletion() {
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
        .extend([Err(GatewayError::Uncertain), Err(GatewayError::Rejected)]);
    advance(&mut status, false, NOW, &gateway, &store)
        .await
        .unwrap();
    advance(&mut status, false, NOW + 30, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Progressing", "CreateUncertain")
    );
    assert!(status.journal.create_may_exist);
    assert_eq!(
        advance(&mut status, true, NOW + 60, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Requeue
    );
}

#[tokio::test]
async fn crash_after_durable_intent_never_finalizes_without_receipt() {
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
        .push_back(Err(GatewayError::Uncertain));
    advance(&mut status, false, NOW, &gateway, &store)
        .await
        .unwrap();
    // The intent record written before the RPC already marks possible creation.
    let mut journal = store.durable.lock().unwrap().clone();
    journal.reason = "Submitting".into();
    assert!(journal.journal.create_may_exist);
    gateway
        .create_results
        .lock()
        .unwrap()
        .push_back(Err(GatewayError::Uncertain));
    assert_eq!(
        advance(&mut journal, true, NOW + 30, &gateway, &store)
            .await
            .unwrap(),
        Outcome::Requeue
    );
}

#[tokio::test]
async fn uncertain_creates_back_off_exponentially_and_report_progressing() {
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
        .extend(std::iter::repeat_n(Err(GatewayError::Uncertain), 10));
    // Attempt n waits 30 * 2^(n-1) seconds, capped at one hour.
    let mut at = NOW;
    for delay in [30, 60, 120, 240, 480, 960, 1920, 3600, 3600] {
        advance(&mut status, false, at, &gateway, &store)
            .await
            .unwrap();
        assert_eq!(status.phase, "Progressing");
        let attempts = gateway.creates.lock().unwrap().len();
        advance(&mut status, false, at + delay - 1, &gateway, &store)
            .await
            .unwrap();
        assert_eq!(gateway.creates.lock().unwrap().len(), attempts);
        at += delay;
    }
    advance(&mut status, false, at, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(gateway.creates.lock().unwrap().len(), 10);
    // Ten attempts span over three hours, not five minutes.
    assert!(at - NOW > 3 * 60 * 60);
    advance(&mut status, false, at + 3600, &gateway, &store)
        .await
        .unwrap();
    assert_eq!(
        (status.phase.as_str(), status.reason.as_str()),
        ("Blocked", "CreateReplayExhausted")
    );
}

#[test]
fn journals_without_create_may_exist_fail_closed() {
    let mut value = serde_json::to_value(initial()).unwrap();
    value["journal"]
        .as_object_mut()
        .unwrap()
        .remove("createMayExist");
    let status: SandboxStatus = serde_json::from_value(value).unwrap();
    assert!(status.journal.create_may_exist);
}

#[test]
fn settled_sandboxes_are_observed_rarely_and_changing_ones_promptly() {
    use openshell_k8s_controller::{
        ACTIVE_OBSERVATION_SECONDS, SETTLED_OBSERVATION_SECONDS, next_observation,
    };
    let within = |value: i64, base: i64| value >= base && value <= base + base / 10;

    let mut status = initial();
    assert!(within(
        next_observation(&status, NOW, false),
        ACTIVE_OBSERVATION_SECONDS
    ));
    status.phase = "Ready".into();
    assert!(within(
        next_observation(&status, NOW, false),
        SETTLED_OBSERVATION_SECONDS
    ));
    // Deletion waits for absence promptly, even for a healthy sandbox.
    assert!(within(
        next_observation(&status, NOW, true),
        ACTIVE_OBSERVATION_SECONDS
    ));
    // A pending retry is observed when it becomes due.
    status.journal.retry_after = Some(NOW + 120);
    assert!(within(next_observation(&status, NOW, false), 120));
    // The jitter is stable for one resource.
    assert_eq!(
        next_observation(&status, NOW, false),
        next_observation(&status, NOW, false)
    );
}
