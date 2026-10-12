// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F07: startup recovery.
//
// A start settles the transaction it finds and starts the committed record
// again, in one pass decided from the durable state alone:
//
//   - A transaction interrupted before its commit is ended as failed; the
//     records stay as they were before it.
//   - A transaction at or after its commit is cleared; the records it
//     wrote stand.
//   - The `active` record is then started again, unless the worker control
//     already serves it.
//
// Nothing of a transaction is run again at a start.

use std::path::Path;

use tensorplate_protocol::agent_control::{
    is_valid_deployment_id, RecoveryAction, RecoverySummary,
};
use tensorplate_protocol::agent_state::{
    AgentState, DeploymentRecord, ErrorRecord, TransactionKind, TransactionRecord,
};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::ErrorCode;

use crate::bundle::{bundle_digests_equal, model_artifact_relative_path};
use crate::coordinator::{now_monotonic_ns, serves, Coordinator};
use crate::descriptor_store::{another_bundle_staged, start_again, Start};
use crate::error::{AgentError, AgentResult};
use crate::state::StateStore;
use crate::worker::WorkerControl;

/// What a start does about the transaction it finds in flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Settle {
    /// None is in flight.
    Nothing,
    /// It reached its commit: it is cleared and its records stand.
    Forward,
    /// It was interrupted before its commit: it is ended as failed, with a
    /// quarantine record, and the records stay as they were before it.
    Ended,
    /// It was recorded failed and not cleared: it is cleared.
    Failed,
}

/// One start's pass: how the transaction is settled, whether the `active`
/// record is started again, and what is reported of both.
struct Pass {
    settle: Settle,
    restore: bool,
    summary: RecoverySummary,
}

/// Whether `tx` reached its commit, read from the records alone: the id the
/// worker control reports says nothing of it.
fn committed(state: &AgentState, tx: &TransactionRecord) -> bool {
    match tx.kind {
        // Only the commit takes the candidate record of a deploy that
        // reached `promoted`; before `staged` none is recorded yet.
        TransactionKind::Deploy => {
            matches!(tx.phase, DeployState::Promoted | DeployState::Active)
                && state.candidate.is_none()
        }
        TransactionKind::Rollback => {
            let Some(active) = state.active.as_ref() else {
                return false;
            };
            let is_its = |record: &DeploymentRecord| {
                let digest = tx.bundle_digest.as_deref();
                record.deployment_id == tx.deployment_id
                    && digest.is_some_and(|d| bundle_digests_equal(d, &record.bundle_digest))
            };
            match tx.descriptor_identity() {
                // Only the swap copies the generation a rollback named.
                Ok(Some(named)) => {
                    active.deployment_id == tx.deployment_id
                        && active.descriptor_identity() == Ok(Some(named))
                }
                // Two records of one id and one bundle read as not swapped:
                // either reading starts the same bundle.
                Ok(None) => is_its(active) && !state.previous_active.as_ref().is_some_and(is_its),
                Err(_) => false,
            }
        }
    }
}

/// The phase as status reports it.
fn label(phase: DeployState) -> String {
    let value = serde_json::to_value(phase).ok();
    let label = value.as_ref().and_then(|v| v.as_str());
    label.map_or_else(|| format!("{phase:?}"), str::to_owned)
}

fn describe(tx: &TransactionRecord) -> String {
    let kind = match tx.kind {
        TransactionKind::Deploy => "deploy",
        TransactionKind::Rollback => "rollback",
    };
    format!(
        "{kind} transaction `{}` of `{}`",
        tx.transaction_id, tx.deployment_id
    )
}

/// What a worker control serves, beside a record.
enum Served {
    Record,
    Another,
    Nothing,
}

/// What `worker` serves, beside `record`. A control that reports no serving
/// member is asked for the id it serves.
fn served(worker: &dyn WorkerControl, record: &DeploymentRecord) -> Served {
    let is_record = match worker.serving_member() {
        Ok(Some(serving)) => serves(&serving, record),
        _ => match worker.active_deployment_id() {
            Ok(Some(id)) => id == record.deployment_id,
            _ => return Served::Nothing,
        },
    };
    if is_record {
        Served::Record
    } else {
        Served::Another
    }
}

fn pass(state: &AgentState, worker: &dyn WorkerControl) -> Pass {
    let (settle, settled) = match state.in_flight_transaction.as_ref() {
        None => (Settle::Nothing, None),
        Some(tx) if tx.phase == DeployState::Failed => (
            Settle::Failed,
            Some(format!("{} was recorded failed: cleared", describe(tx))),
        ),
        Some(tx) if committed(state, tx) => (
            Settle::Forward,
            Some(format!("{} had committed: cleared", describe(tx))),
        ),
        Some(tx) => (
            Settle::Ended,
            Some(format!(
                "{} was interrupted at `{}`, before its commit: ended as failed",
                describe(tx),
                label(tx.phase)
            )),
        ),
    };
    let serving = state.active.as_ref().map(|active| served(worker, active));
    let restore = matches!(serving, Some(Served::Nothing | Served::Another));
    let mut action = match (restore, settle) {
        (true, _) => RecoveryAction::RestoreActive,
        (false, Settle::Ended) => RecoveryAction::QuarantineCandidate,
        (false, _) => RecoveryAction::NoOp,
    };
    // With no transaction in flight these are the reasons status has
    // always reported.
    let mut restored = match serving {
        None => "no deployment recorded",
        Some(Served::Record) => "desired and actual active deployments agree",
        Some(Served::Nothing) => "desired active deployment recorded; worker unreachable or empty",
        Some(Served::Another) => "desired active deployment recorded; the worker serves another",
    }
    .to_owned();
    // A deployment served that no record names is left alone and reported,
    // as before: no start meets it, and the pass has nothing to act on.
    if settle == Settle::Nothing && serving.is_none() {
        if let Ok(Some(have)) = worker.active_deployment_id() {
            action = RecoveryAction::OperatorRequired;
            restored =
                format!("worker reports `{have}` but no desired active deployment is recorded");
        }
    }
    let reason = match settled {
        Some(settled) => format!("{settled}; {restored}"),
        None => restored,
    };
    Pass {
        settle,
        restore,
        summary: RecoverySummary {
            action,
            reason: Some(reason),
        },
    }
}

/// What a start would do now, from the durable state and what `worker`
/// serves, without doing it: `restore_active` when the `active` record
/// would be started again, `quarantine_candidate` when a transaction would
/// be ended as failed and nothing started, and `no_op` otherwise.
/// `operator_required` is reported for a deployment served that no record
/// names; no other action is reported.
///
/// # Errors
///
/// Propagates state-store errors only. A worker control that cannot answer
/// is read as serving nothing.
pub fn plan_with_worker(
    store: &StateStore,
    worker: &dyn WorkerControl,
) -> AgentResult<RecoverySummary> {
    Ok(pass(&store.snapshot()?, worker).summary)
}

/// Run the start's one pass before the agent accepts requests: settle the
/// transaction in flight, in at most one write, then start the `active`
/// record again. Nothing of the transaction is run again.
///
/// # Errors
///
/// A state write that fails, and a restore whose worker does not come up;
/// the process entrypoint ends the start on either. A restore refused by
/// what is on disk is not an error: see [`restore_active`].
pub fn apply_startup(coordinator: &Coordinator) -> AgentResult<RecoverySummary> {
    let store = coordinator.state().as_ref();
    let state = store.snapshot()?;
    let pass = pass(&state, coordinator.worker_client().as_ref());
    if let Some(tx) = state.in_flight_transaction.as_ref() {
        match pass.settle {
            Settle::Nothing => {}
            Settle::Forward => store.end_transaction()?,
            Settle::Ended => {
                let interrupted = || {
                    let message = format!(
                        "{} was interrupted at `{}`, before its commit, and ended at the next start",
                        describe(tx),
                        label(tx.phase)
                    );
                    ErrorRecord::new(ErrorCode::Internal, message).with_context("startup_recovery")
                };
                // A failure it had recorded is why it ended.
                let error = tx.failure.clone().unwrap_or_else(interrupted);
                store.quarantine_in_flight(error, now_monotonic_ns())?;
            }
            Settle::Failed => {
                let unrecorded = || {
                    let message = format!("{} failed", describe(tx));
                    ErrorRecord::new(ErrorCode::Internal, message).with_context("startup_recovery")
                };
                store.fail_transaction(tx.failure.clone().unwrap_or_else(unrecorded))?;
            }
        }
    }
    if pass.restore {
        restore_active(coordinator)?;
    }
    Ok(pass.summary)
}

/// Whether `err` of a restore was decided by the files it reads, the staged
/// copy and the descriptor document, which no restart of the agent changes.
/// Every other error is the agent's own: its state store, or a document it
/// could not write.
fn decided_by_files(err: &AgentError) -> bool {
    matches!(
        err,
        AgentError::DeploymentDescriptor { .. }
            | AgentError::BundleManifest(_)
            | AgentError::BundleUnsupported(_)
            | AgentError::BundleIntegrity { .. }
            | AgentError::BundleMissing(_)
    )
}

/// Start the active record's deployment again. A restore refused by what
/// is on disk leaves the record not serving, with the refusal as
/// `last_error` and no error returned, so the operator can deploy or roll
/// back: an id that cannot name a member, another bundle staged under the
/// id, a staged copy that does not read, a descriptor document that is not
/// the record's, a re-stamp that is refused. A failed state write and a
/// worker that does not come up are returned.
fn restore_active(coordinator: &Coordinator) -> AgentResult<()> {
    let store = coordinator.state();
    let active = store
        .snapshot()?
        .active
        .ok_or_else(|| AgentError::Unavailable("no active deployment to restore".into()))?;
    let refuse = |mut record: ErrorRecord| {
        record.message = format!(
            "{}; `{}` is not serving until it is deployed again",
            record.message, active.deployment_id
        );
        store.set_last_error(Some(record))
    };
    // An id recorded before deployment ids were restricted cannot be started
    // as a member.
    if !is_valid_deployment_id(&active.deployment_id) {
        let err = AgentError::Config(format!(
            "recorded active deployment `{}` was not restored: its id cannot name a member (1 to 128 of ASCII letters, digits, `-`, `_` or `.`); deploy it again under such an id",
            active.deployment_id
        ));
        return store.set_last_error(Some(err.to_record()));
    }
    // Another bundle is staged under the id than the record names, as a
    // same-id update that did not complete leaves it.
    if let Ok(Some(refusal)) = another_bundle_staged(&active) {
        return refuse(refusal.to_record());
    }
    // Read before any state write: an unreadable manifest is reported by
    // its own error kind, which a failed state write shares.
    let artifact = match model_artifact_relative_path(Path::new(&active.staged_path)) {
        Ok(artifact) => artifact,
        Err(err) => return refuse(err.to_record()),
    };
    let worker = coordinator.worker_client().as_ref();
    let in_hand = match start_again(store, worker, &active, artifact, Start::Restore) {
        Ok(in_hand) => in_hand,
        Err(err) if decided_by_files(&err) => return refuse(err.to_record()),
        Err(err) => {
            store.set_last_error(Some(err.to_record()))?;
            return Err(err);
        }
    };
    let tx = format!("recovery-restore-{}", active.deployment_id);
    let prepare_timeout =
        std::time::Duration::from_millis(coordinator.config().worker.prepare_timeout_ms);
    let warm_timeout =
        std::time::Duration::from_millis(coordinator.config().worker.warm_timeout_ms);
    let (candidate, prepared) = in_hand.prepare(&tx, prepare_timeout);
    if let Err(err) = prepared {
        store.set_last_error(Some(err.to_record()))?;
        return Err(err);
    }
    match worker.warm(&tx, &candidate, warm_timeout) {
        Ok(readiness) if readiness.ready => {}
        Ok(_) => {
            let err = AgentError::WorkerNotReady;
            store.set_last_error(Some(err.to_record()))?;
            return Err(err);
        }
        Err(err) => {
            store.set_last_error(Some(err.to_record()))?;
            return Err(err);
        }
    }
    if let Err(err) = worker.promote(&tx, &candidate) {
        store.set_last_error(Some(err.to_record()))?;
        return Err(err);
    }
    store.set_last_error(None)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]
    use super::{apply_startup, plan_with_worker};
    use crate::config::AgentConfig;
    use crate::coordinator::Coordinator;
    use crate::state::StateStore;
    use crate::worker::{MockBehavior, MockWorkerControl};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tensorplate_protocol::agent_control::RecoveryAction;
    use tensorplate_protocol::agent_state::{
        DeploymentRecord, ErrorRecord, TransactionKind, TransactionRecord,
    };
    use tensorplate_protocol::deploy_transaction::DeployState;
    use tensorplate_protocol::ErrorCode;

    fn record(id: &str) -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: id.into(),
            bundle_digest: "sha256:cafe".into(),
            generation: None,
            descriptor_digest: None,
            configuration_digest: None,
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
            generation: None,
            descriptor_digest: None,
            configuration_digest: None,
            bundle_path: None,
            correlation_id: None,
            started_monotonic_ns: Some(1),
            last_transition_monotonic_ns: Some(1),
            failure: None,
        }
    }

    fn config(td: &std::path::Path) -> AgentConfig {
        AgentConfig {
            schema_version: tensorplate_protocol::SCHEMA_VERSION.to_string(),
            transport: crate::config::ControlTransport::UnixSocket,
            socket_path: Some(td.join("agent.sock")),
            tcp_bind_host: "127.0.0.1".into(),
            tcp_bind_port: 0,
            state_dir: td.join("state"),
            staging_dir: td.join("staging"),
            available_backends: vec!["mock".into()],
            backend_capabilities: Default::default(),
            memory_admission: None,
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: Default::default(),
            admission_posture: None,
            worker: Default::default(),
            supervision: None,
            runtime_version: Some("0.1.0".into()),
        }
        .validate()
        .expect("valid")
    }

    fn mock(serving: Option<&str>) -> MockWorkerControl {
        MockWorkerControl::with_behavior(MockBehavior {
            active_deployment_id: serving.map(str::to_owned),
            ..Default::default()
        })
    }

    #[test]
    fn no_state_returns_no_op() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        let p = plan_with_worker(&store, &mock(None)).expect("plan");
        assert!(matches!(p.action, RecoveryAction::NoOp));
    }

    #[test]
    fn a_transaction_found_before_its_commit_is_ended_whatever_its_phase_and_never_run_again() {
        use DeployState::{
            Active, CapacityChecked, Prepared, Promoted, Received, RolledBack, Staged, Verified,
            Warmed,
        };
        let phases = [
            Received,
            Verified,
            Staged,
            CapacityChecked,
            Prepared,
            Warmed,
            Promoted,
            Active,
            RolledBack,
        ];
        for kind in [TransactionKind::Deploy, TransactionKind::Rollback] {
            for phase in phases {
                let td = TempDir::new().expect("td");
                let store = StateStore::open(td.path()).expect("open");
                let mut record = tx("tx", phase);
                record.kind = kind;
                store
                    .update(|s| {
                        // A deploy's candidate is what keeps it uncommitted.
                        s.candidate = Some(self::record("d"));
                        s.in_flight_transaction = Some(record);
                        Ok(())
                    })
                    .expect("update");
                let nothing = plan_with_worker(&store, &mock(Some("d"))).expect("plan");
                assert_eq!(
                    nothing.action,
                    RecoveryAction::QuarantineCandidate,
                    "{kind:?} {phase:?}"
                );
                store
                    .update(|s| {
                        s.active = Some(self::record("d-active"));
                        Ok(())
                    })
                    .expect("update");
                let restores = plan_with_worker(&store, &mock(None)).expect("plan");
                assert_eq!(
                    restores.action,
                    RecoveryAction::RestoreActive,
                    "{kind:?} {phase:?}"
                );
                let reason = restores.reason.expect("reason");
                assert!(reason.contains("before its commit"), "{reason}");
            }
        }
    }

    #[test]
    fn a_deploy_is_read_as_committed_from_its_phase_and_its_records_together() {
        use DeployState::{
            Active, CapacityChecked, Prepared, Promoted, Received, Staged, Verified, Warmed,
        };
        // No candidate is recorded before `staged`: a deploy killed at
        // `received` has none, as one that committed has none.
        let before = [
            Received,
            Verified,
            Staged,
            CapacityChecked,
            Prepared,
            Warmed,
        ];
        let found = before.map(|phase| (phase, false));
        for (phase, committed) in found.into_iter().chain([(Promoted, true), (Active, true)]) {
            let td = TempDir::new().expect("td");
            let cfg = config(td.path());
            let store = Arc::new(StateStore::open(&cfg.state_dir).expect("open"));
            store
                .update(|s| {
                    s.active = Some(record("d"));
                    s.in_flight_transaction = Some(tx("tx", phase));
                    Ok(())
                })
                .expect("update");
            let coordinator = Coordinator::new(cfg, store.clone(), Arc::new(mock(Some("d"))));

            let summary = apply_startup(&coordinator).expect("apply");

            let snap = store.snapshot().expect("snap");
            let ended: Vec<_> = snap.quarantined.iter().map(|q| q.phase).collect();
            let (action, quarantined) = if committed {
                (RecoveryAction::NoOp, vec![])
            } else {
                (RecoveryAction::QuarantineCandidate, vec![phase])
            };
            assert_eq!((summary.action, ended), (action, quarantined), "{phase:?}");
            assert!(snap.in_flight_transaction.is_none(), "{phase:?}");
        }
    }

    #[test]
    fn a_start_reads_no_commit_from_the_deployment_the_worker_control_serves() {
        let td = TempDir::new().expect("td");
        let cfg = config(td.path());
        let store = Arc::new(StateStore::open(&cfg.state_dir).expect("open"));
        let mut tx_record = tx("tx", DeployState::Promoted);
        tx_record.deployment_id = "d-new".into();
        store
            .update(|s| {
                s.candidate = Some(record("d-new"));
                s.in_flight_transaction = Some(tx_record);
                Ok(())
            })
            .expect("update");
        let coordinator = Coordinator::new(cfg, store.clone(), Arc::new(mock(Some("d-new"))));

        let summary = apply_startup(&coordinator).expect("apply");

        assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
        let snap = store.snapshot().expect("snap");
        assert!(snap.active.is_none() && snap.candidate.is_none());
        assert!(snap.in_flight_transaction.is_none());
        assert_eq!(snap.quarantined.len(), 1);
        assert_eq!(snap.quarantined[0].phase, DeployState::Promoted);
    }

    #[test]
    fn a_transaction_that_recorded_its_failure_is_quarantined_with_that_failure() {
        let td = TempDir::new().expect("td");
        let cfg = config(td.path());
        let store = Arc::new(StateStore::open(&cfg.state_dir).expect("open"));
        let failure = ErrorRecord::new(ErrorCode::LoadFailed, "the runner refused the model");
        let mut tx_record = tx("tx", DeployState::Prepared);
        tx_record.failure = Some(failure.clone());
        store
            .update(|s| {
                s.candidate = Some(record("d"));
                s.in_flight_transaction = Some(tx_record);
                Ok(())
            })
            .expect("update");
        let coordinator = Coordinator::new(cfg, store.clone(), Arc::new(mock(None)));

        apply_startup(&coordinator).expect("apply");

        let snap = store.snapshot().expect("snap");
        assert_eq!(snap.quarantined[0].error, failure);
        assert_eq!(snap.last_error, Some(failure));
    }

    #[test]
    fn startup_quarantine_applies_unsafe_worker_phase() {
        let td = TempDir::new().expect("td");
        let cfg = config(td.path());
        let store = Arc::new(StateStore::open(&cfg.state_dir).expect("open"));
        let mut tx_record = tx("tx", DeployState::Prepared);
        tx_record.deployment_id = "d-new".into();
        store
            .update(|s| {
                s.active = Some(record("d-active"));
                s.candidate = Some(record("d-new"));
                s.in_flight_transaction = Some(tx_record);
                Ok(())
            })
            .expect("update");
        let coordinator = Coordinator::new(cfg, store.clone(), Arc::new(mock(Some("d-active"))));

        let summary = apply_startup(&coordinator).expect("apply");
        assert_eq!(summary.action, RecoveryAction::QuarantineCandidate);
        let snap = store.snapshot().expect("snap");
        assert_eq!(
            snap.active.as_ref().expect("active").deployment_id,
            "d-active"
        );
        assert!(snap.candidate.is_none());
        assert!(snap.in_flight_transaction.is_none());
        assert_eq!(snap.quarantined.len(), 1);
        let error = snap.last_error.expect("the quarantine's error");
        assert_eq!(error.context.as_deref(), Some("startup_recovery"));
        let says = "deploy transaction `tx` of `d-new` was interrupted at `prepared`";
        assert!(error.message.contains(says), "{}", error.message);
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
        let p = plan_with_worker(&store, &mock(Some("d-active"))).expect("plan");
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
        let p = plan_with_worker(&store, &mock(None)).expect("plan");
        assert_eq!(p.action, RecoveryAction::RestoreActive);
    }

    #[test]
    fn a_worker_control_that_serves_another_deployment_does_not_keep_the_record_from_starting() {
        let td = TempDir::new().expect("td");
        let store = StateStore::open(td.path()).expect("open");
        store
            .update(|s| {
                s.active = Some(record("d-desired"));
                Ok(())
            })
            .expect("update");
        let p = plan_with_worker(&store, &mock(Some("d-other"))).expect("plan");
        assert_eq!(p.action, RecoveryAction::RestoreActive);
        let reason = p.reason.expect("reason");
        assert!(reason.ends_with("the worker serves another"), "{reason}");
        // A deployment no record names is not the start's to act on.
        store
            .update(|s| {
                s.active = None;
                Ok(())
            })
            .expect("update");
        let p = plan_with_worker(&store, &mock(Some("d-other"))).expect("plan");
        assert_eq!(p.action, RecoveryAction::OperatorRequired);
        let reason = p.reason.expect("reason");
        assert!(reason.starts_with("worker reports `d-other`"), "{reason}");
    }
}
