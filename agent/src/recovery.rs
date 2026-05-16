// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F07: startup recovery planner.
//
// On startup the agent loads durable state, consults the worker's
// actual-active-deployment if reachable, and computes an explicit
// recovery action. Recovery never blindly replays the original
// transaction. It reasons from state alone:
//
//   - No in-flight transaction => action depends on whether desired and
//     actual active deployments agree.
//   - In-flight in a replayable phase (`received`, `verified`, `staged`,
//     `capacity_checked`) => safe to retry from the beginning.
//   - In-flight in a worker-side phase (`prepared`, `warmed`,
//     `promoted`) => quarantine the candidate; the worker may have
//     partial state.
//   - Terminal (`failed`, `rolled_back`) => no-op.
//
// The planner returns the action without applying it; the agent's main
// loop applies the action and reports the outcome through the status
// API.

use tensorplate_protocol::agent_control::{RecoveryAction, RecoverySummary};
use tensorplate_protocol::deploy_transaction::DeployState;

use crate::error::AgentResult;
use crate::state::StateStore;
use crate::transaction::{classify, PhaseClass};
use crate::worker::WorkerControl;

/// Compute the recovery action.
///
/// `worker_active_deployment_id` is best-effort: pass `None` when the
/// worker is unreachable (e.g. fresh boot before the worker has been
/// started). The planner still produces a useful recommendation from
/// durable state alone.
///
/// # Errors
///
/// Propagates errors from the state store.
pub fn plan(
    store: &StateStore,
    worker_active_deployment_id: Option<&str>,
) -> AgentResult<RecoverySummary> {
    let state = store.snapshot()?;
    let desired_active = state.active.as_ref().map(|a| a.deployment_id.clone());

    if let Some(tx) = state.in_flight_transaction.as_ref() {
        if tx.phase.is_terminal() {
            return Ok(action(
                RecoveryAction::NoOp,
                "in-flight transaction is terminal",
            ));
        }
        return Ok(match classify(tx.phase) {
            PhaseClass::Replayable => match tx.phase {
                DeployState::Received => action(
                    RecoveryAction::ResumeVerify,
                    "received: re-run verification",
                ),
                DeployState::Verified => {
                    action(RecoveryAction::ResumeStage, "verified: re-stage the bundle")
                }
                DeployState::Staged => action(
                    RecoveryAction::ResumePrepare,
                    "staged: re-run prepare on the worker",
                ),
                DeployState::CapacityChecked => action(
                    RecoveryAction::ResumePrepare,
                    "capacity_checked: re-run prepare on the worker",
                ),
                _ => action(
                    RecoveryAction::OperatorRequired,
                    "unexpected replayable phase",
                ),
            },
            PhaseClass::WorkerSideEffect => action(
                RecoveryAction::QuarantineCandidate,
                "worker-side phase interrupted; quarantining candidate",
            ),
            PhaseClass::Active => action(RecoveryAction::NoOp, "transaction is already active"),
            PhaseClass::Terminal => {
                action(RecoveryAction::NoOp, "transaction reached a terminal state")
            }
        });
    }

    // No in-flight transaction. Reconcile desired vs actual.
    match (desired_active.as_deref(), worker_active_deployment_id) {
        (Some(want), Some(have)) if want == have => Ok(action(
            RecoveryAction::NoOp,
            "desired and actual active deployments agree",
        )),
        (Some(want), Some(have)) => Ok(action(
            RecoveryAction::OperatorRequired,
            format!("active deployment mismatch: desired `{want}`, worker reports `{have}`"),
        )),
        (Some(_), None) => Ok(action(
            RecoveryAction::RestoreActive,
            "desired active deployment recorded; worker unreachable or empty",
        )),
        (None, Some(have)) => Ok(action(
            RecoveryAction::OperatorRequired,
            format!("worker reports `{have}` but no desired active deployment is recorded"),
        )),
        (None, None) => Ok(action(RecoveryAction::NoOp, "no deployment recorded")),
    }
}

/// Convenience that consults a [`WorkerControl`] for actual state and
/// returns the plan. A worker error degrades gracefully to
/// "actual unknown".
///
/// # Errors
///
/// Propagates state-store errors only. Worker errors are absorbed and
/// surface as "actual unknown" in the resulting plan.
pub fn plan_with_worker(
    store: &StateStore,
    worker: &dyn WorkerControl,
) -> AgentResult<RecoverySummary> {
    let actual = worker.active_deployment_id().ok().flatten();
    plan(store, actual.as_deref())
}

fn action(a: RecoveryAction, reason: impl Into<String>) -> RecoverySummary {
    RecoverySummary {
        action: a,
        reason: Some(reason.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::{plan, plan_with_worker};
    use crate::state::StateStore;
    use crate::worker::{MockBehavior, MockWorkerControl};
    use tempfile::TempDir;
    use tensorplate_protocol::agent_control::RecoveryAction;
    use tensorplate_protocol::agent_state::{DeploymentRecord, TransactionKind, TransactionRecord};
    use tensorplate_protocol::deploy_transaction::DeployState;

    fn record(id: &str) -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: id.into(),
            bundle_digest: "sha256:cafe".into(),
            bundle_name: "n".into(),
            bundle_version: "1".into(),
            backend_hint: "mock".into(),
            model_class: "vision".into(),
            staged_path: format!("/staging/{id}"),
            promoted_monotonic_ns: Some(1),
            labels: Default::default(),
        }
    }

    fn tx(id: &str, phase: DeployState) -> TransactionRecord {
        TransactionRecord {
            transaction_id: id.into(),
            deployment_id: "d".into(),
            phase,
            kind: TransactionKind::Deploy,
            bundle_digest: None,
            bundle_path: None,
            correlation_id: None,
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(1),
            failure: None,
        }
    }

    #[test]
    fn no_state_returns_no_op() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        let p = plan(&store, None).expect("plan");
        assert!(matches!(p.action, RecoveryAction::NoOp));
    }

    #[test]
    fn replayable_phases_resume_safely() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        for (phase, expected) in [
            (DeployState::Received, RecoveryAction::ResumeVerify),
            (DeployState::Verified, RecoveryAction::ResumeStage),
            (DeployState::Staged, RecoveryAction::ResumePrepare),
            (DeployState::CapacityChecked, RecoveryAction::ResumePrepare),
        ] {
            // Reset state by writing the transaction directly.
            store
                .update(|s| {
                    s.in_flight_transaction = Some(tx("tx", phase));
                    Ok(())
                })
                .expect("update");
            let p = plan(&store, None).expect("plan");
            assert_eq!(p.action, expected, "phase {phase:?}");
        }
    }

    #[test]
    fn worker_side_phases_quarantine() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        for phase in [
            DeployState::Prepared,
            DeployState::Warmed,
            DeployState::Promoted,
        ] {
            store
                .update(|s| {
                    s.in_flight_transaction = Some(tx("tx", phase));
                    Ok(())
                })
                .expect("update");
            let p = plan(&store, None).expect("plan");
            assert_eq!(p.action, RecoveryAction::QuarantineCandidate);
        }
    }

    #[test]
    fn desired_and_actual_agreement_is_no_op() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .update(|s| {
                s.active = Some(record("d-active"));
                Ok(())
            })
            .expect("update");
        let p = plan(&store, Some("d-active")).expect("plan");
        assert_eq!(p.action, RecoveryAction::NoOp);
    }

    #[test]
    fn desired_with_unreachable_worker_returns_restore_active() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .update(|s| {
                s.active = Some(record("d-active"));
                Ok(())
            })
            .expect("update");
        let worker = MockWorkerControl::with_behavior(MockBehavior {
            active_deployment_id: None,
            ..Default::default()
        });
        let p = plan_with_worker(&store, &worker).expect("plan");
        assert_eq!(p.action, RecoveryAction::RestoreActive);
    }

    #[test]
    fn worker_disagreement_requires_operator() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .update(|s| {
                s.active = Some(record("d-desired"));
                Ok(())
            })
            .expect("update");
        let p = plan(&store, Some("d-other")).expect("plan");
        assert_eq!(p.action, RecoveryAction::OperatorRequired);
    }
}
