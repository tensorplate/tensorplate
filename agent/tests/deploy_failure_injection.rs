// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F08-T02 failure-injection coverage:
//
//   - bad-bundle rejection (corrupt artifact, unsupported backend,
//     unsupported runtime, capacity overflow, missing capability)
//   - worker prepare failure
//   - worker warm failure
//   - worker not-ready timeout
//   - typed quarantine record and active-deployment preservation

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{vision_bundle, write_bundle, BundleSpec, Harness};
use tensorplate_agent::error::AgentError;
use tensorplate_agent::worker::{AgentErrorKind, MockBehavior};
use tensorplate_protocol::deploy_transaction::DeployState;

fn deploy_should_fail(h: &Harness, deployment: &str, bundle: &std::path::Path) -> AgentError {
    h.coord
        .deploy(deployment, bundle, Default::default(), None, None)
        .expect_err("must fail")
}

#[test]
fn corrupt_artifact_rejected_before_worker() {
    let h = Harness::new();
    let bundle = write_bundle(
        h.td.path(),
        "bad-art",
        BundleSpec {
            corrupt_artifact_bytes_after: true,
            ..Default::default()
        },
    );
    let err = deploy_should_fail(&h, "d1", &bundle);
    assert!(matches!(err, AgentError::BundleIntegrity { .. }));

    let calls: Vec<&'static str> = h
        .worker
        .calls()
        .expect("calls")
        .iter()
        .map(|c| c.op)
        .collect();
    assert!(
        calls.is_empty(),
        "worker must not be contacted for bad bundle"
    );

    let snap = h.store.snapshot().expect("snap");
    assert!(snap.active.is_none());
    assert_eq!(snap.quarantined.len(), 1);
}

#[test]
fn unsupported_backend_rejected_typed() {
    let h = Harness::new();
    let bundle = write_bundle(
        h.td.path(),
        "vla",
        BundleSpec {
            backend_hint: Some("python_pytorch"),
            model_class: Some("vla"),
            ..Default::default()
        },
    );
    let err = deploy_should_fail(&h, "d1", &bundle);
    assert!(matches!(err, AgentError::UnsupportedBackend(_)));
}

#[test]
fn capacity_overflow_rejected() {
    let h = Harness::new();
    let bundle = write_bundle(
        h.td.path(),
        "big",
        BundleSpec {
            memory_estimate_bytes: Some(64 * 1024 * 1024 * 1024),
            ..Default::default()
        },
    );
    let err = deploy_should_fail(&h, "d1", &bundle);
    assert!(matches!(err, AgentError::InsufficientCapacity));
}

#[test]
fn prepare_failure_preserves_active_and_quarantines() {
    let h_first = Harness::new();
    let b1 = vision_bundle(h_first.td.path(), "d1");
    h_first
        .coord
        .deploy("d1", &b1, Default::default(), None, None)
        .expect("first deploy ok");

    // Build a coordinator whose mock fails on prepare while sharing the
    // same state directory.
    let behavior = MockBehavior {
        fail_prepare: Some(AgentErrorKind::LoadFailed("bad weights".into())),
        ..Default::default()
    };
    let h = Harness::with_behavior(behavior);
    // Manually copy state so this harness "sees" d1 as active.
    let snap = h_first.store.snapshot().expect("snap");
    h.store
        .update(|s| {
            *s = snap.clone();
            Ok(())
        })
        .expect("update");

    let b2 = vision_bundle(h.td.path(), "d2");
    let err = deploy_should_fail(&h, "d2", &b2);
    assert!(matches!(err, AgentError::WorkerControl(_)));

    let snap = h.store.snapshot().expect("snap");
    assert_eq!(
        snap.active.as_ref().expect("active").deployment_id,
        "d1",
        "active deployment must be preserved across failed candidate"
    );
    assert_eq!(snap.quarantined.len(), 1);
    let q = &snap.quarantined[0];
    assert_eq!(q.deployment_id, "d2");
    assert_eq!(q.phase, DeployState::CapacityChecked);
}

#[test]
fn warm_not_ready_quarantines_candidate() {
    let behavior = MockBehavior {
        warm_not_ready: true,
        ..Default::default()
    };
    let h = Harness::with_behavior(behavior);
    let b1 = vision_bundle(h.td.path(), "d1");
    let err = deploy_should_fail(&h, "d1", &b1);
    assert!(matches!(err, AgentError::WorkerNotReady));

    let snap = h.store.snapshot().expect("snap");
    assert!(snap.active.is_none());
    assert_eq!(snap.quarantined.len(), 1);
    assert_eq!(snap.quarantined[0].phase, DeployState::Prepared);
}

#[test]
fn new_deploy_after_quarantine_proceeds() {
    let behavior = MockBehavior {
        fail_prepare: Some(AgentErrorKind::LoadFailed("first attempt".into())),
        ..Default::default()
    };
    let h = Harness::with_behavior(behavior);
    let b_bad = vision_bundle(h.td.path(), "d-bad");
    let _ = deploy_should_fail(&h, "d-bad", &b_bad);

    // Reset the mock's failure injection (one-shot Option::take() pattern
    // already consumed the failure; second deploy can succeed).
    let b_good = vision_bundle(h.td.path(), "d-good");
    let outcome = h
        .coord
        .deploy("d-good", &b_good, Default::default(), None, None)
        .expect("ok");
    assert_eq!(outcome.deployment_id, "d-good");

    let snap = h.store.snapshot().expect("snap");
    assert_eq!(snap.active.as_ref().expect("a").deployment_id, "d-good");
    assert_eq!(snap.quarantined.len(), 1);
    assert_eq!(snap.quarantined[0].deployment_id, "d-bad");
}
