// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F08-T02 rollback + restart-recovery coverage.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{vision_bundle, Harness};
use std::sync::Arc;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::error::AgentError;
use tensorplate_agent::recovery;
use tensorplate_agent::state::StateStore;
use tensorplate_agent::worker::{AgentErrorKind, MockBehavior, MockWorkerControl};
use tensorplate_protocol::agent_control::RecoveryAction;
use tensorplate_protocol::deploy_transaction::DeployState;

#[test]
fn rollback_with_no_previous_active_returns_unavailable() {
    let h = Harness::new();
    let err = h.coord.rollback(None).expect_err("must reject");
    assert!(matches!(err, AgentError::Unavailable(_)));
}

#[test]
fn rollback_restores_previous_active() {
    let h = Harness::new();
    let b1 = vision_bundle(h.td.path(), "d1");
    h.coord
        .deploy("d1", &b1, Default::default(), None, None)
        .expect("ok");
    let b2 = vision_bundle(h.td.path(), "d2");
    h.coord
        .deploy("d2", &b2, Default::default(), None, None)
        .expect("ok");

    h.coord.rollback(None).expect("rollback ok");

    let snap = h.store.snapshot().expect("snap");
    assert_eq!(snap.active.as_ref().expect("a").deployment_id, "d1");
    assert_eq!(
        snap.previous_active.as_ref().expect("p").deployment_id,
        "d2"
    );
}

#[test]
fn failed_rollback_preserves_current_active() {
    let h = Harness::new();
    let b1 = vision_bundle(h.td.path(), "d1");
    h.coord
        .deploy("d1", &b1, Default::default(), None, None)
        .expect("ok");
    let b2 = vision_bundle(h.td.path(), "d2");
    h.coord
        .deploy("d2", &b2, Default::default(), None, None)
        .expect("ok");

    // Swap the worker for one that fails on prepare for the rollback.
    let behavior = MockBehavior {
        fail_prepare: Some(AgentErrorKind::LoadFailed("rollback prepare failed".into())),
        ..Default::default()
    };
    let bad_worker = Arc::new(MockWorkerControl::with_behavior(behavior));
    let bad_coord = Arc::new(Coordinator::new(
        h.config.clone(),
        h.store.clone(),
        bad_worker.clone(),
    ));

    let err = bad_coord.rollback(None).expect_err("must fail");
    assert!(matches!(err, AgentError::WorkerControl(_)));

    let snap = h.store.snapshot().expect("snap");
    // Active deployment unchanged: still d2.
    assert_eq!(snap.active.as_ref().expect("a").deployment_id, "d2");
    assert_eq!(
        snap.previous_active.as_ref().expect("p").deployment_id,
        "d1"
    );
}

#[test]
fn restart_after_active_deploy_recovers_via_state_file() {
    let h = Harness::new();
    let b1 = vision_bundle(h.td.path(), "d1");
    h.coord
        .deploy("d1", &b1, Default::default(), None, None)
        .expect("ok");

    // Drop the in-memory components and reopen the store from disk.
    drop(h.coord);
    let store = Arc::new(StateStore::open(h.config.state_dir.clone()).expect("reopen"));
    let snap = store.snapshot().expect("snap");
    assert_eq!(snap.active.as_ref().expect("a").deployment_id, "d1");
    // A fresh worker reports no active deployment; recovery should
    // recommend `RestoreActive` rather than panic.
    let worker = MockWorkerControl::new();
    let plan = recovery::plan_with_worker(&store, &worker).expect("plan");
    assert_eq!(plan.action, RecoveryAction::RestoreActive);
}

#[test]
fn restart_during_worker_side_phase_recommends_quarantine() {
    let h = Harness::new();
    // Inject an in-flight transaction stuck at `prepared` (a worker-
    // side phase) by mutating the durable store directly. The recovery
    // planner must recommend quarantining the candidate instead of
    // blindly replaying prepare.
    h.store
        .update(|s| {
            s.in_flight_transaction = Some(tensorplate_protocol::agent_state::TransactionRecord {
                transaction_id: "tx-1".into(),
                deployment_id: "d-stuck".into(),
                phase: DeployState::Prepared,
                kind: tensorplate_protocol::agent_state::TransactionKind::Deploy,
                bundle_digest: Some("sha256:cafe".into()),
                bundle_path: Some("/staging/d-stuck".into()),
                correlation_id: None,
                started_monotonic_ns: Some(1),
                last_transition_monotonic_ns: Some(1),
                failure: None,
            });
            Ok(())
        })
        .expect("update");

    let plan = recovery::plan_with_worker(&h.store, h.worker.as_ref()).expect("plan");
    assert_eq!(plan.action, RecoveryAction::QuarantineCandidate);
}

#[test]
fn restart_during_replayable_phase_recommends_resume() {
    let h = Harness::new();
    h.store
        .update(|s| {
            s.in_flight_transaction = Some(tensorplate_protocol::agent_state::TransactionRecord {
                transaction_id: "tx-1".into(),
                deployment_id: "d-r".into(),
                phase: DeployState::Verified,
                kind: tensorplate_protocol::agent_state::TransactionKind::Deploy,
                bundle_digest: Some("sha256:cafe".into()),
                bundle_path: Some("/bundles/d-r".into()),
                correlation_id: None,
                started_monotonic_ns: Some(1),
                last_transition_monotonic_ns: Some(1),
                failure: None,
            });
            Ok(())
        })
        .expect("update");

    let plan = recovery::plan_with_worker(&h.store, h.worker.as_ref()).expect("plan");
    assert_eq!(plan.action, RecoveryAction::ResumeStage);
}
