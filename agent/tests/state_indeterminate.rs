// SPDX-License-Identifier: Apache-2.0
//
// A durable state write that fails after the rename that commits it leaves
// the next start reading either state. The store then refuses every later
// write, and status reports the agent failed with an error that says so,
// instead of the stale state in memory.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::default_trait_access
)]

mod common;

use common::{vision_bundle, Harness};
use tensorplate_agent::error::AgentError;
use tensorplate_protocol::agent_control::AgentRunState;

#[test]
fn a_write_that_fails_after_its_commit_rename_fails_the_agent_until_restart() {
    let h = Harness::new();
    let b1 = vision_bundle(h.td.path(), "d1");
    h.coord
        .deploy("d1", &b1, Default::default(), None, None)
        .expect("first deploy");
    assert_eq!(
        h.coord.status().expect("status").agent_state,
        AgentRunState::Ready
    );

    // A 0.1 write renames state.json first; a directory where the backup's
    // temp file goes then fails the write after its commit rename.
    let state_dir = h.store.state_dir().to_path_buf();
    std::fs::create_dir(state_dir.join("state.json.bak.tmp")).expect("mkdir");
    let b2 = vision_bundle(h.td.path(), "d2");
    match h.coord.deploy("d2", &b2, Default::default(), None, None) {
        Err(AgentError::StateIndeterminate(_)) => {}
        other => panic!("expected StateIndeterminate, got {other:?}"),
    }
    assert!(h.store.is_indeterminate());

    let status = h.coord.status().expect("status");
    assert_eq!(status.agent_state, AgentRunState::Failed);
    let error = status.last_error.expect("last_error");
    assert!(
        error.message.contains("unknown outcome"),
        "{}",
        error.message
    );

    // Every later write is refused, even once the fault is gone.
    std::fs::remove_dir(state_dir.join("state.json.bak.tmp")).expect("rmdir");
    let b3 = vision_bundle(h.td.path(), "d3");
    match h.coord.deploy("d3", &b3, Default::default(), None, None) {
        Err(AgentError::StateIndeterminate(_)) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}
