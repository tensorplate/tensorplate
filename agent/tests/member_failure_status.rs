// SPDX-License-Identifier: Apache-2.0
//
// Agent status while the worker control reports a member whose restarts are
// used up.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::Harness;
use tensorplate_agent::coordinator::Coordinator;
use tensorplate_agent::error::AgentResult;
use tensorplate_agent::worker::{MockWorkerControl, WorkerControl, WorkerReadiness};
use tensorplate_protocol::agent_control::AgentRunState;
use tensorplate_protocol::agent_state::ErrorRecord;
use tensorplate_protocol::worker_control::CandidateRef;
use tensorplate_protocol::ErrorCode;

/// A worker control whose serving member has failed for good.
struct FailedMember {
    inner: MockWorkerControl,
    failure: ErrorRecord,
}

impl WorkerControl for FailedMember {
    fn prepare(&self, tx: &str, candidate: &CandidateRef, timeout: Duration) -> AgentResult<()> {
        self.inner.prepare(tx, candidate, timeout)
    }

    fn warm(
        &self,
        tx: &str,
        candidate: &CandidateRef,
        timeout: Duration,
    ) -> AgentResult<WorkerReadiness> {
        self.inner.warm(tx, candidate, timeout)
    }

    fn promote(&self, tx: &str, candidate: &CandidateRef) -> AgentResult<()> {
        self.inner.promote(tx, candidate)
    }

    fn hold(&self) {
        self.inner.hold();
    }

    fn release(&self) {
        self.inner.release();
    }

    fn unload(&self, deployment_id: &str) {
        self.inner.unload(deployment_id);
    }

    fn active_deployment_id(&self) -> AgentResult<Option<String>> {
        Ok(None)
    }

    fn member_failure(&self) -> Option<ErrorRecord> {
        Some(self.failure.clone())
    }
}

#[test]
fn status_reports_a_failed_member_ahead_of_the_last_transaction_error_and_keeps_it_out_of_the_state_file(
) {
    let h = Harness::new();
    let earlier = ErrorRecord::new(ErrorCode::Timeout, "an earlier deploy timed out");
    h.store
        .set_last_error(Some(earlier.clone()))
        .expect("record the earlier error");
    let failure = ErrorRecord::new(ErrorCode::LoadFailed, "`stt` is not restarted")
        .with_context("exit status: 1");
    let failed = Coordinator::new(
        h.coord.config().clone(),
        h.store.clone(),
        Arc::new(FailedMember {
            inner: MockWorkerControl::new(),
            failure: failure.clone(),
        }),
    );

    let status = failed.status().expect("status");

    assert_eq!(status.agent_state, AgentRunState::Degraded);
    let reported = status.last_error.expect("the failure is reported");
    assert_eq!(
        (reported.code, reported.message, reported.context),
        (failure.code, failure.message, failure.context)
    );
    // The same state through a worker control with no failed member.
    let reported = h.coord.status().expect("status").last_error.expect("error");
    assert_eq!(reported.message, earlier.message);
    assert_eq!(h.store.snapshot().expect("state").last_error, Some(earlier));
}

#[test]
fn a_failed_member_degrades_an_agent_with_no_recorded_error() {
    let h = Harness::new();
    let failure = ErrorRecord::new(ErrorCode::LoadFailed, "`stt` is not restarted");
    let failed = Coordinator::new(
        h.coord.config().clone(),
        h.store.clone(),
        Arc::new(FailedMember {
            inner: MockWorkerControl::new(),
            failure,
        }),
    );

    assert_eq!(
        h.coord.status().expect("status").agent_state,
        AgentRunState::Ready
    );
    assert_eq!(
        failed.status().expect("status").agent_state,
        AgentRunState::Degraded
    );
    assert_eq!(h.store.snapshot().expect("state").last_error, None);
}
