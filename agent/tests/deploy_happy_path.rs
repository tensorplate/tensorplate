// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F08-T02 happy-path coverage.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{vision_bundle, Harness};
use tensorplate_protocol::deploy_transaction::DeployState;

#[test]
fn first_deploy_reaches_active() {
    let h = Harness::new();
    let bundle = vision_bundle(h.td.path(), "d1");
    let outcome = h
        .coord
        .deploy("d1", &bundle, Default::default(), None, None)
        .expect("deploy ok");
    assert_eq!(outcome.deployment_id, "d1");

    let snap = h.store.snapshot().expect("snap");
    assert_eq!(snap.active.as_ref().expect("active").deployment_id, "d1");
    assert!(snap.previous_active.is_none());
    assert!(snap.candidate.is_none());
    assert!(snap.in_flight_transaction.is_none());

    // Worker observed prepare -> warm -> promote in order.
    let calls = h.worker.calls().expect("calls");
    let ops: Vec<&'static str> = calls.iter().map(|c| c.op).collect();
    assert_eq!(ops, vec!["prepare", "warm", "promote"]);
}

#[test]
fn second_deploy_records_previous_active_and_unloads() {
    let h = Harness::new();
    let b1 = vision_bundle(h.td.path(), "d1");
    h.coord
        .deploy("d1", &b1, Default::default(), None, None)
        .expect("ok");

    let b2 = vision_bundle(h.td.path(), "d2");
    h.coord
        .deploy("d2", &b2, Default::default(), None, None)
        .expect("ok");

    let snap = h.store.snapshot().expect("snap");
    assert_eq!(snap.active.as_ref().expect("active").deployment_id, "d2");
    assert_eq!(
        snap.previous_active.as_ref().expect("prev").deployment_id,
        "d1"
    );

    let ops: Vec<&'static str> = h
        .worker
        .calls()
        .expect("calls")
        .iter()
        .map(|c| c.op)
        .collect();
    // prepare/warm/promote x2 plus the best-effort unload of d1.
    assert!(ops.contains(&"unload"));
}

#[test]
fn expected_bundle_digest_must_match() {
    let h = Harness::new();
    let bundle = vision_bundle(h.td.path(), "d1");

    let outcome = h
        .coord
        .deploy("d1", &bundle, Default::default(), None, Some("sha256:00ff"))
        .expect_err("digest mismatch");
    assert!(matches!(
        outcome,
        tensorplate_agent::error::AgentError::BundleIntegrity { .. }
    ));

    // Active state preserved (still empty here; no deploy ever succeeded).
    let snap = h.store.snapshot().expect("snap");
    assert!(snap.active.is_none());
    // Quarantine recorded the failed candidate.
    assert_eq!(snap.quarantined.len(), 1);
    assert_eq!(snap.quarantined[0].phase, DeployState::Received);
}
