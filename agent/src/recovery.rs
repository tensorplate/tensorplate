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

use std::path::Path;

use tensorplate_protocol::agent_control::{RecoveryAction, RecoverySummary};
use tensorplate_protocol::agent_state::{DeploymentRecord, ErrorRecord, TransactionKind};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::worker_control::CandidateRef;
use tensorplate_protocol::ErrorCode;

use crate::bundle::model_artifact_relative_path;
use crate::coordinator::{now_monotonic_ns, Coordinator};
use crate::error::{AgentError, AgentResult};
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
        let actual_matches_transaction =
            worker_active_deployment_id == Some(tx.deployment_id.as_str());
        if tx.phase.is_terminal() {
            return Ok(action(
                RecoveryAction::NoOp,
                "in-flight transaction is terminal",
            ));
        }
        if matches!(
            tx.phase,
            DeployState::Prepared | DeployState::Warmed | DeployState::Promoted
        ) && actual_matches_transaction
        {
            return Ok(action(
                RecoveryAction::FinalizePromotion,
                format!(
                    "{:?} transaction reached worker active deployment `{}`; finalizing durable state",
                    tx.kind, tx.deployment_id
                ),
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

/// Apply the startup recovery action before the agent accepts mutating
/// requests. The action is computed from durable state plus worker actual
/// state; replayable actions are driven through the coordinator so the
/// normal deploy/rollback safety gates still apply.
///
/// # Errors
///
/// Returns the first recovery error. The process entrypoint treats this
/// as a startup failure instead of opening the control socket with a
/// half-recovered transaction.
pub fn apply_startup(coordinator: &Coordinator) -> AgentResult<RecoverySummary> {
    let summary = plan_with_worker(
        coordinator.state().as_ref(),
        coordinator.worker_client().as_ref(),
    )?;
    match summary.action {
        RecoveryAction::NoOp | RecoveryAction::OperatorRequired => Ok(summary),
        RecoveryAction::QuarantineCandidate => {
            quarantine_in_flight(coordinator.state().as_ref(), summary.reason.clone())?;
            Ok(summary)
        }
        RecoveryAction::FinalizePromotion => {
            finalize_promoted(coordinator)?;
            Ok(summary)
        }
        RecoveryAction::RestoreActive => {
            restore_active(coordinator)?;
            Ok(summary)
        }
        RecoveryAction::ResumeVerify
        | RecoveryAction::ResumeStage
        | RecoveryAction::ResumePrepare
        | RecoveryAction::ResumeWarm => {
            resume_replayable(coordinator)?;
            Ok(summary)
        }
    }
}

fn quarantine_in_flight(store: &StateStore, reason: Option<String>) -> AgentResult<()> {
    let snapshot = store.snapshot()?;
    if snapshot.in_flight_transaction.is_none() {
        return Ok(());
    }
    let record = ErrorRecord::new(
        ErrorCode::Internal,
        reason.unwrap_or_else(|| "startup recovery quarantined candidate".into()),
    )
    .with_context("startup_recovery");
    store.quarantine_in_flight(record, now_monotonic_ns())
}

fn finalize_promoted(coordinator: &Coordinator) -> AgentResult<()> {
    let store = coordinator.state();
    let snapshot = store.snapshot()?;
    let tx = snapshot.in_flight_transaction.ok_or_else(|| {
        AgentError::Internal("finalize_promoted called without in-flight transaction".into())
    })?;
    match tx.kind {
        TransactionKind::Deploy => {
            if snapshot
                .active
                .as_ref()
                .is_some_and(|active| active.deployment_id == tx.deployment_id)
            {
                store.clear_transaction()?;
                store.set_last_error(None)?;
                return Ok(());
            }
            let Some(candidate) = snapshot.candidate.as_ref() else {
                return Err(AgentError::Internal(
                    "cannot finalize promoted deploy without candidate record".into(),
                ));
            };
            if candidate.deployment_id != tx.deployment_id {
                return Err(AgentError::Internal(format!(
                    "candidate `{}` does not match transaction `{}`",
                    candidate.deployment_id, tx.deployment_id
                )));
            }
            store.promote_candidate(now_monotonic_ns())?;
        }
        TransactionKind::Rollback => {
            if snapshot
                .active
                .as_ref()
                .is_some_and(|active| active.deployment_id == tx.deployment_id)
            {
                store.clear_transaction()?;
                store.set_last_error(None)?;
                return Ok(());
            }
            let Some(previous) = snapshot.previous_active.as_ref() else {
                return Err(AgentError::Unavailable(
                    "cannot finalize rollback without previous active deployment".into(),
                ));
            };
            if previous.deployment_id != tx.deployment_id {
                return Err(AgentError::Internal(format!(
                    "previous active `{}` does not match rollback transaction `{}`",
                    previous.deployment_id, tx.deployment_id
                )));
            }
            store.swap_active_with_previous(now_monotonic_ns())?;
        }
    }
    store.clear_transaction()?;
    store.set_last_error(None)
}

fn restore_active(coordinator: &Coordinator) -> AgentResult<()> {
    let active = coordinator
        .state()
        .snapshot()?
        .active
        .ok_or_else(|| AgentError::Unavailable("no active deployment to restore".into()))?;
    let candidate = match candidate_from_record(&active) {
        Ok(candidate) => candidate,
        Err(err) => {
            coordinator.state().set_last_error(Some(err.to_record()))?;
            return Err(err);
        }
    };
    let tx = format!("recovery-restore-{}", active.deployment_id);
    let prepare_timeout =
        std::time::Duration::from_millis(coordinator.config().worker.prepare_timeout_ms);
    let warm_timeout =
        std::time::Duration::from_millis(coordinator.config().worker.warm_timeout_ms);
    if let Err(err) = coordinator
        .worker_client()
        .prepare(&tx, &candidate, prepare_timeout)
    {
        coordinator.state().set_last_error(Some(err.to_record()))?;
        return Err(err);
    }
    match coordinator
        .worker_client()
        .warm(&tx, &candidate, warm_timeout)
    {
        Ok(readiness) if readiness.ready => {}
        Ok(_) => {
            let err = AgentError::WorkerNotReady;
            coordinator.state().set_last_error(Some(err.to_record()))?;
            return Err(err);
        }
        Err(err) => {
            coordinator.state().set_last_error(Some(err.to_record()))?;
            return Err(err);
        }
    }
    if let Err(err) = coordinator.worker_client().promote(&tx, &candidate) {
        coordinator.state().set_last_error(Some(err.to_record()))?;
        return Err(err);
    }
    coordinator.state().set_last_error(None)
}

fn resume_replayable(coordinator: &Coordinator) -> AgentResult<()> {
    let snapshot = coordinator.state().snapshot()?;
    let tx = snapshot.in_flight_transaction.ok_or_else(|| {
        AgentError::Internal("resume_replayable called without in-flight transaction".into())
    })?;
    let labels = snapshot
        .candidate
        .as_ref()
        .map(|candidate| candidate.labels.clone())
        .unwrap_or_default();
    coordinator.state().update(|state| {
        state.candidate = None;
        state.in_flight_transaction = None;
        Ok(())
    })?;
    match tx.kind {
        TransactionKind::Deploy => {
            let path = tx.bundle_path.ok_or_else(|| {
                AgentError::Unavailable("cannot resume deploy without bundle_path".into())
            })?;
            coordinator.deploy(
                &tx.deployment_id,
                std::path::Path::new(&path),
                labels,
                tx.correlation_id,
                None,
            )?;
        }
        TransactionKind::Rollback => {
            coordinator.rollback(tx.correlation_id)?;
        }
    }
    Ok(())
}

fn candidate_from_record(record: &DeploymentRecord) -> AgentResult<CandidateRef> {
    let artifact_relative_path = model_artifact_relative_path(Path::new(&record.staged_path))?;
    Ok(CandidateRef {
        deployment_id: record.deployment_id.clone(),
        staged_path: record.staged_path.clone(),
        bundle_digest: record.bundle_digest.clone(),
        backend_hint: record.backend_hint.clone(),
        model_class: record.model_class.clone(),
        bundle_name: Some(record.bundle_name.clone()),
        bundle_version: Some(record.bundle_version.clone()),
        artifact_relative_path: Some(artifact_relative_path),
    })
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
    use super::{apply_startup, candidate_from_record, plan, plan_with_worker};
    use crate::config::AgentConfig;
    use crate::coordinator::Coordinator;
    use crate::state::StateStore;
    use crate::worker::{MockBehavior, MockWorkerControl};
    use std::sync::Arc;
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

    fn staged_bundle(root: &std::path::Path, id: &str) -> String {
        use sha2::Digest;

        let dir = root.join(format!("staged-{id}"));
        std::fs::create_dir_all(&dir).expect("mkdir staged bundle");
        let body = b"engine-bytes";
        std::fs::write(dir.join("model.engine"), body).expect("write model");
        let mut h = sha2::Sha256::new();
        h.update(body);
        let digest = format!("sha256:{}", hex::encode(h.finalize()));
        let manifest = format!(
            r#"{{"schema_version":"{}","name":"n","version":"1","format_version":"0.1","model_class":"vision","backend_hint":"mock","artifacts":[{{"role":"model","path":"model.engine","digest":"{digest}"}}]}}"#,
            tensorplate_protocol::SCHEMA_VERSION
        );
        std::fs::write(dir.join("manifest.json"), manifest).expect("write manifest");
        dir.display().to_string()
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
            device_memory_bytes: Some(8 * 1024 * 1024 * 1024),
            device_family: Default::default(),
            worker: Default::default(),
            runtime_version: Some("0.1.0".into()),
        }
        .validate()
        .expect("valid")
    }

    #[test]
    fn candidate_from_record_reads_model_artifact_from_manifest() {
        let td = TempDir::new().expect("td");
        let mut record = record("d-active");
        record.staged_path = staged_bundle(td.path(), "d-active");

        let candidate = candidate_from_record(&record).expect("candidate");

        assert_eq!(
            candidate.artifact_relative_path.as_deref(),
            Some("model.engine")
        );
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
    fn promoted_worker_actual_finalizes_deploy_state() {
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
        let worker = Arc::new(MockWorkerControl::with_behavior(MockBehavior {
            active_deployment_id: Some("d-new".into()),
            ..Default::default()
        }));
        let coordinator = Coordinator::new(cfg, store.clone(), worker);

        let summary = apply_startup(&coordinator).expect("apply");
        assert_eq!(summary.action, RecoveryAction::FinalizePromotion);
        let snap = store.snapshot().expect("snap");
        assert_eq!(snap.active.as_ref().expect("active").deployment_id, "d-new");
        assert!(snap.in_flight_transaction.is_none());
    }

    #[test]
    fn promoted_worker_actual_finalizes_rollback_state() {
        let td = TempDir::new().expect("td");
        let cfg = config(td.path());
        let store = Arc::new(StateStore::open(&cfg.state_dir).expect("open"));
        let mut tx_record = tx("tx", DeployState::Promoted);
        tx_record.kind = TransactionKind::Rollback;
        tx_record.deployment_id = "d-prev".into();
        store
            .update(|s| {
                s.active = Some(record("d-current"));
                s.previous_active = Some(record("d-prev"));
                s.in_flight_transaction = Some(tx_record);
                Ok(())
            })
            .expect("update");
        let worker = Arc::new(MockWorkerControl::with_behavior(MockBehavior {
            active_deployment_id: Some("d-prev".into()),
            ..Default::default()
        }));
        let coordinator = Coordinator::new(cfg, store.clone(), worker);

        let summary = apply_startup(&coordinator).expect("apply");
        assert_eq!(summary.action, RecoveryAction::FinalizePromotion);
        let snap = store.snapshot().expect("snap");
        assert_eq!(
            snap.active.as_ref().expect("active").deployment_id,
            "d-prev"
        );
        assert_eq!(
            snap.previous_active.as_ref().expect("prev").deployment_id,
            "d-current"
        );
        assert!(snap.in_flight_transaction.is_none());
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
        let worker = Arc::new(MockWorkerControl::with_behavior(MockBehavior {
            active_deployment_id: Some("d-active".into()),
            ..Default::default()
        }));
        let coordinator = Coordinator::new(cfg, store.clone(), worker);

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
