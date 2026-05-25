// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F04 / F05 / F06: Deploy and rollback transaction coordinator.
//
// The coordinator orchestrates one deploy or rollback transaction at a
// time. It:
//
//   1. Verifies the bundle through the V01-E08-F03 verifier.
//   2. Stages the bundle into `staging_dir/<deployment_id>/` by copying
//      verified artifacts plus the manifest. Staging is content-addressed
//      so a successful prepare keeps the previous active deployment's
//      files untouched.
//   3. Walks the F04 state machine, persisting each phase through the
//      F02 state store before the next phase begins.
//   4. Calls the V01-E08-F05 [`WorkerControl`] for prepare / warm /
//      promote. A failure at any worker-side phase quarantines the
//      candidate and preserves the active deployment.
//   5. Records previous-active metadata atomically with promotion so the
//      F06 rollback path can restore it later.
//
// Rollback is the same coordinator with the source/destination swapped:
// the previous active deployment becomes the candidate and goes back
// through prepare / warm / promote.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tensorplate_protocol::agent_control::{
    AgentRunState, AgentStatus, DeployFailureSummary, DeployStatus, DeploymentSummary,
    QuarantineSummary, ResponseError, SupervisionStatusSummary,
};
use tensorplate_protocol::agent_state::{DeploymentRecord, TransactionKind, TransactionRecord};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::supervision_event::SupervisionAgentState;
use tensorplate_protocol::worker_control::CandidateRef;

use std::collections::BTreeMap;

use crate::backend_detection::BackendProbeReport;
use crate::bundle::{
    capacity_check, model_artifact_relative_path, verify_with_probes, VerifiedBundle,
};
use crate::config::AgentConfig;
use crate::error::{AgentError, AgentResult};
use crate::state::StateStore;
use crate::supervision::supervisor::{DesiredWorker, WorkerSupervisor};
use crate::transaction::is_permitted;
use crate::worker::{WorkerControl, WorkerEvent};

/// Caller-supplied event sink. The coordinator emits one event per
/// significant state transition; production sinks ship them to structured
/// logs and (in V01-E10) the observability service.
pub type EventSink = dyn Fn(&WorkerEvent) + Send + Sync;

/// V01-E08 coordinator. Optionally extended by V01-E09 with a worker
/// supervisor; when a supervisor is attached the coordinator forwards
/// desired-active changes to it on every successful promote/rollback and
/// asks it to reset crash-loop state after a deploy or rollback (the
/// documented recovery trigger in V01-E09-F06-T02).
pub struct Coordinator {
    config: AgentConfig,
    store: Arc<StateStore>,
    worker: Arc<dyn WorkerControl>,
    sink: Option<Arc<EventSink>>,
    supervisor: Option<Arc<WorkerSupervisor>>,
    backend_probes: BTreeMap<String, BackendProbeReport>,
}

/// Result of a successful deploy/rollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeployOutcome {
    pub transaction_id: String,
    pub deployment_id: String,
    pub bundle_digest: String,
}

impl Coordinator {
    /// Build a coordinator. `worker` is moved in as an `Arc` so the
    /// coordinator can be cloned cheaply across threads.
    #[must_use]
    pub fn new(
        config: AgentConfig,
        store: Arc<StateStore>,
        worker: Arc<dyn WorkerControl>,
    ) -> Self {
        Self {
            config,
            store,
            worker,
            sink: None,
            supervisor: None,
            backend_probes: BTreeMap::new(),
        }
    }

    /// Attach V01-E14-F05 backend probe reports. The agent main calls
    /// this once at startup with the cached probe outcomes for every
    /// backend listed in `config.available_backends`. The coordinator
    /// hands the map to [`crate::bundle::verify_with_probes`] so a
    /// deploy of a non-runnable backend (e.g. `python_pytorch` with no
    /// PyTorch installed) is rejected before staging.
    #[must_use]
    pub fn with_backend_probes(mut self, probes: BTreeMap<String, BackendProbeReport>) -> Self {
        self.backend_probes = probes;
        self
    }

    /// Install an event sink. The sink is called on every worker / state
    /// transition; it must not block on inference work.
    #[must_use]
    pub fn with_event_sink(mut self, sink: Arc<EventSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// Attach a V01-E09 supervisor. The coordinator will:
    ///
    ///   - install the new active deployment as the supervisor's desired
    ///     state after every successful promote
    ///   - reset crash-loop counters through
    ///     [`WorkerSupervisor::recover_after_operator_action`] on every
    ///     successful deploy or rollback (the documented operator-action
    ///     recovery trigger from V01-E09-F06-T02)
    ///
    /// The supervisor itself never promotes a candidate; the coordinator
    /// remains the only mutator of durable state.
    #[must_use]
    pub fn with_supervisor(mut self, supervisor: Arc<WorkerSupervisor>) -> Self {
        self.supervisor = Some(supervisor);
        self
    }

    /// Read-only handle on the durable store. Useful for status endpoints.
    #[must_use]
    pub fn state(&self) -> &Arc<StateStore> {
        &self.store
    }

    /// Read-only handle on the validated agent config.
    #[must_use]
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Read-only handle on the worker client. Reserved for tests.
    #[must_use]
    pub fn worker_client(&self) -> &Arc<dyn WorkerControl> {
        &self.worker
    }

    /// Drive a deploy transaction end-to-end.
    ///
    /// # Errors
    ///
    /// Returns the typed [`AgentError`] of the first phase that fails.
    /// On failure the candidate is quarantined and the active deployment
    /// is preserved.
    #[allow(clippy::too_many_lines)]
    pub fn deploy(
        &self,
        deployment_id: &str,
        bundle_path: &Path,
        labels: std::collections::BTreeMap<String, String>,
        correlation_id: Option<String>,
        expected_bundle_digest: Option<&str>,
    ) -> AgentResult<DeployOutcome> {
        if deployment_id.is_empty() {
            return Err(AgentError::Config("deployment_id must be non-empty".into()));
        }

        let transaction_id = new_transaction_id();
        let tx_record = TransactionRecord {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
            phase: DeployState::Received,
            kind: TransactionKind::Deploy,
            bundle_digest: None,
            bundle_path: Some(bundle_path.display().to_string()),
            correlation_id,
            started_monotonic_ns: Some(now_monotonic_ns()),
            last_transition_monotonic_ns: Some(now_monotonic_ns()),
            failure: None,
        };
        self.store.begin_transaction(tx_record)?;

        // Phase: verified.
        let verified = match verify_with_probes(bundle_path, &self.config, &self.backend_probes) {
            Ok(v) => v,
            Err(err) => {
                return self.fail(&transaction_id, deployment_id, DeployState::Received, err)
            }
        };
        if let Some(expected) = expected_bundle_digest {
            if !crate::bundle::bundle_digests_equal(expected, &verified.manifest_digest) {
                let err = AgentError::BundleIntegrity {
                    path: "manifest.json".into(),
                    reason: format!(
                        "expected_bundle_digest mismatch: caller `{expected}` computed `{}`",
                        verified.manifest_digest
                    ),
                };
                return self.fail(&transaction_id, deployment_id, DeployState::Received, err);
            }
        }
        self.advance(
            &transaction_id,
            DeployState::Received,
            DeployState::Verified,
        )?;
        self.store.update(|s| {
            if let Some(tx) = s.in_flight_transaction.as_mut() {
                tx.bundle_digest = Some(verified.manifest_digest.clone());
            }
            Ok(())
        })?;

        // Phase: staged.
        let staged_path = match self.stage_bundle(deployment_id, &verified) {
            Ok(p) => p,
            Err(err) => {
                return self.fail(&transaction_id, deployment_id, DeployState::Verified, err);
            }
        };
        let candidate_record = DeploymentRecord {
            deployment_id: deployment_id.to_string(),
            bundle_digest: verified.manifest_digest.clone(),
            bundle_name: verified.manifest.name.clone(),
            bundle_version: verified.manifest.version.clone(),
            backend_hint: verified.manifest.backend_hint.clone(),
            model_class: verified.manifest.model_class.as_str().to_string(),
            staged_path: staged_path.display().to_string(),
            promoted_monotonic_ns: None,
            labels,
        };
        self.store.record_candidate(candidate_record.clone())?;
        self.advance(&transaction_id, DeployState::Verified, DeployState::Staged)?;

        // Phase: capacity_checked.
        if let Err(err) = capacity_check(&verified, &self.config) {
            return self.fail(&transaction_id, deployment_id, DeployState::Staged, err);
        }
        self.advance(
            &transaction_id,
            DeployState::Staged,
            DeployState::CapacityChecked,
        )?;

        // Build the candidate ref handed to the worker.
        let candidate_ref = CandidateRef {
            deployment_id: deployment_id.to_string(),
            staged_path: staged_path.display().to_string(),
            bundle_digest: verified.manifest_digest.clone(),
            backend_hint: verified.manifest.backend_hint.clone(),
            model_class: verified.manifest.model_class.as_str().to_string(),
            bundle_name: Some(verified.manifest.name.clone()),
            bundle_version: Some(verified.manifest.version.clone()),
            artifact_relative_path: verified.manifest.model_artifact().map(|a| a.path.clone()),
        };

        // Phase: prepared.
        self.emit(&WorkerEvent::PrepareStart {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
        });
        let prepare_timeout = Duration::from_millis(self.config.worker.prepare_timeout_ms);
        if let Err(err) = self
            .worker
            .prepare(&transaction_id, &candidate_ref, prepare_timeout)
        {
            self.emit(&WorkerEvent::Failure {
                transaction_id: transaction_id.clone(),
                deployment_id: deployment_id.to_string(),
                reason: err.to_string(),
            });
            return self.fail(
                &transaction_id,
                deployment_id,
                DeployState::CapacityChecked,
                err,
            );
        }
        self.advance(
            &transaction_id,
            DeployState::CapacityChecked,
            DeployState::Prepared,
        )?;
        self.emit(&WorkerEvent::PrepareEnd {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
        });

        // Phase: warmed.
        self.emit(&WorkerEvent::WarmStart {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
        });
        let warm_timeout = Duration::from_millis(self.config.worker.warm_timeout_ms);
        let warmed = match self
            .worker
            .warm(&transaction_id, &candidate_ref, warm_timeout)
        {
            Ok(r) => r,
            Err(err) => {
                self.emit(&WorkerEvent::Failure {
                    transaction_id: transaction_id.clone(),
                    deployment_id: deployment_id.to_string(),
                    reason: err.to_string(),
                });
                return self.fail(&transaction_id, deployment_id, DeployState::Prepared, err);
            }
        };
        if !warmed.ready {
            let err = AgentError::WorkerNotReady;
            self.emit(&WorkerEvent::Failure {
                transaction_id: transaction_id.clone(),
                deployment_id: deployment_id.to_string(),
                reason: err.to_string(),
            });
            return self.fail(&transaction_id, deployment_id, DeployState::Prepared, err);
        }
        self.advance(&transaction_id, DeployState::Prepared, DeployState::Warmed)?;
        self.emit(&WorkerEvent::WarmEnd {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
        });

        // Phase: promoted -> active.
        if let Err(err) = self.worker.promote(&transaction_id, &candidate_ref) {
            self.emit(&WorkerEvent::Failure {
                transaction_id: transaction_id.clone(),
                deployment_id: deployment_id.to_string(),
                reason: err.to_string(),
            });
            return self.fail(&transaction_id, deployment_id, DeployState::Warmed, err);
        }
        self.advance(&transaction_id, DeployState::Warmed, DeployState::Promoted)?;
        let monotonic_ns = now_monotonic_ns();
        self.store.promote_candidate(monotonic_ns)?;
        self.advance(&transaction_id, DeployState::Promoted, DeployState::Active)?;
        self.emit(&WorkerEvent::Promote {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
        });

        // Optionally unload previous active. Best-effort; never blocks
        // success.
        let previous_active = {
            let s = self.store.snapshot()?;
            s.previous_active.as_ref().map(|d| d.deployment_id.clone())
        };
        if let Some(prev) = previous_active.as_deref() {
            self.worker.unload(prev);
            self.emit(&WorkerEvent::Unload {
                deployment_id: prev.to_string(),
            });
        }

        // Clear the in-flight transaction now that it has reached Active.
        self.store.clear_transaction()?;
        self.store.set_last_error(None)?;

        // V01-E09-F06-T02: hand the new active over to the supervisor.
        // A successful deploy is also the documented recovery trigger
        // that exits crash-loop state.
        self.notify_supervisor_promotion(deployment_id, verified.manifest.backend_hint.as_str());

        Ok(DeployOutcome {
            transaction_id,
            deployment_id: deployment_id.to_string(),
            bundle_digest: verified.manifest_digest,
        })
    }

    /// Drive a rollback transaction. Uses the same prepare/warm/promote
    /// path as deploy; refuses when no previous active deployment exists.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Unavailable`] when rollback is not possible,
    /// and the typed [`AgentError`] of whichever phase fails otherwise.
    #[allow(clippy::too_many_lines)]
    pub fn rollback(&self, correlation_id: Option<String>) -> AgentResult<DeployOutcome> {
        let prev = {
            let s = self.store.snapshot()?;
            s.previous_active
                .clone()
                .ok_or_else(|| AgentError::Unavailable("no previous active deployment".into()))?
        };

        // Revalidate that the previous bundle files are still on disk
        // before we touch the worker.
        let staged = PathBuf::from(&prev.staged_path);
        if !staged.is_dir() {
            return Err(AgentError::Unavailable(format!(
                "previous active staged path `{}` is missing",
                staged.display()
            )));
        }
        let manifest_path = staged.join("manifest.json");
        if !manifest_path.is_file() {
            return Err(AgentError::Unavailable(format!(
                "previous active manifest missing at `{}`",
                manifest_path.display()
            )));
        }
        let artifact_relative_path = model_artifact_relative_path(&staged).map_err(|err| {
            AgentError::Unavailable(format!(
                "previous active manifest invalid at `{}`: {err}",
                manifest_path.display()
            ))
        })?;

        let transaction_id = new_transaction_id();
        let tx_record = TransactionRecord {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
            phase: DeployState::Received,
            kind: TransactionKind::Rollback,
            bundle_digest: Some(prev.bundle_digest.clone()),
            bundle_path: Some(staged.display().to_string()),
            correlation_id,
            started_monotonic_ns: Some(now_monotonic_ns()),
            last_transition_monotonic_ns: Some(now_monotonic_ns()),
            failure: None,
        };
        self.store.begin_transaction(tx_record)?;

        self.advance(
            &transaction_id,
            DeployState::Received,
            DeployState::Verified,
        )?;
        self.advance(&transaction_id, DeployState::Verified, DeployState::Staged)?;
        self.advance(
            &transaction_id,
            DeployState::Staged,
            DeployState::CapacityChecked,
        )?;

        let candidate_ref = CandidateRef {
            deployment_id: prev.deployment_id.clone(),
            staged_path: prev.staged_path.clone(),
            bundle_digest: prev.bundle_digest.clone(),
            backend_hint: prev.backend_hint.clone(),
            model_class: prev.model_class.clone(),
            bundle_name: Some(prev.bundle_name.clone()),
            bundle_version: Some(prev.bundle_version.clone()),
            artifact_relative_path: Some(artifact_relative_path),
        };

        self.emit(&WorkerEvent::PrepareStart {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
        });
        let prepare_timeout = Duration::from_millis(self.config.worker.prepare_timeout_ms);
        if let Err(err) = self
            .worker
            .prepare(&transaction_id, &candidate_ref, prepare_timeout)
        {
            return self.fail_rollback(&transaction_id, &prev.deployment_id, err);
        }
        self.advance(
            &transaction_id,
            DeployState::CapacityChecked,
            DeployState::Prepared,
        )?;
        self.emit(&WorkerEvent::PrepareEnd {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
        });

        self.emit(&WorkerEvent::WarmStart {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
        });
        let warm_timeout = Duration::from_millis(self.config.worker.warm_timeout_ms);
        let warmed = match self
            .worker
            .warm(&transaction_id, &candidate_ref, warm_timeout)
        {
            Ok(r) => r,
            Err(err) => return self.fail_rollback(&transaction_id, &prev.deployment_id, err),
        };
        if !warmed.ready {
            return self.fail_rollback(
                &transaction_id,
                &prev.deployment_id,
                AgentError::WorkerNotReady,
            );
        }
        self.advance(&transaction_id, DeployState::Prepared, DeployState::Warmed)?;
        self.emit(&WorkerEvent::WarmEnd {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
        });

        if let Err(err) = self.worker.promote(&transaction_id, &candidate_ref) {
            return self.fail_rollback(&transaction_id, &prev.deployment_id, err);
        }
        self.advance(&transaction_id, DeployState::Warmed, DeployState::Promoted)?;
        let monotonic_ns = now_monotonic_ns();
        self.store.swap_active_with_previous(monotonic_ns)?;
        // Rollback's terminal state is RolledBack rather than Active.
        self.advance(
            &transaction_id,
            DeployState::Promoted,
            DeployState::RolledBack,
        )?;
        self.emit(&WorkerEvent::Promote {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
        });
        self.store.clear_transaction()?;
        self.store.set_last_error(None)?;

        // V01-E09-F06-T02: hand the rolled-back active over to the
        // supervisor and reset crash-loop state.
        self.notify_supervisor_promotion(prev.deployment_id.as_str(), prev.backend_hint.as_str());

        Ok(DeployOutcome {
            transaction_id,
            deployment_id: prev.deployment_id,
            bundle_digest: prev.bundle_digest,
        })
    }

    /// Project a [`AgentStatus`] suitable for the control API.
    ///
    /// # Errors
    ///
    /// Propagates errors from the underlying state store.
    pub fn status(&self) -> AgentResult<AgentStatus> {
        let s = self.store.snapshot()?;
        let mut agent_state = if s.last_error.is_some() {
            AgentRunState::Degraded
        } else {
            AgentRunState::Ready
        };
        let to_summary = |d: &DeploymentRecord| DeploymentSummary {
            deployment_id: d.deployment_id.clone(),
            bundle_digest: d.bundle_digest.clone(),
            bundle_name: Some(d.bundle_name.clone()),
            bundle_version: Some(d.bundle_version.clone()),
            backend_hint: Some(d.backend_hint.clone()),
            model_class: Some(d.model_class.clone()),
            staged_path: Some(d.staged_path.clone()),
            promoted_monotonic_ns: d.promoted_monotonic_ns,
            serving_url: None,
        };
        let mut active = s.active.as_ref().map(to_summary);
        if let Some(summary) = active.as_mut() {
            summary.serving_url = self.worker.active_serving_url()?;
        }
        let previous = s.previous_active.as_ref().map(to_summary);
        let candidate = s.candidate.as_ref().map(to_summary);
        let in_flight = s.in_flight_transaction.as_ref().map(|t| DeployStatus {
            phase: t.phase,
            transaction_id: Some(t.transaction_id.clone()),
            deployment_id: Some(t.deployment_id.clone()),
            bundle_digest: t.bundle_digest.clone(),
            started_monotonic_ns: t.started_monotonic_ns,
            last_transition_monotonic_ns: t.last_transition_monotonic_ns,
            failure: t.failure.as_ref().map(|f| DeployFailureSummary {
                error_code: f.code,
                message: Some(f.message.clone()),
                recoverable: f.recoverable,
            }),
        });
        let last_error = s.last_error.as_ref().map(|e| ResponseError {
            code: e.code,
            message: e.message.clone(),
            context: e.context.clone(),
        });
        let quarantined = s
            .quarantined
            .iter()
            .map(|q| QuarantineSummary {
                transaction_id: q.transaction_id.clone(),
                deployment_id: q.deployment_id.clone(),
                bundle_digest: q.bundle_digest.clone(),
                phase: q.phase,
                error_code: q.error.code,
                message: Some(q.error.message.clone()),
                quarantined_monotonic_ns: q.quarantined_monotonic_ns,
            })
            .collect();
        let supervision = self.supervisor.as_ref().map(|sup| {
            let status = sup.status();
            agent_state = merge_agent_state(agent_state, status.agent_state);
            SupervisionStatusSummary {
                serving_state: status.serving_state,
                agent_state: status.agent_state,
                desired_active: status.desired_active,
                actual_active: status.actual_active,
                backend: status.backend,
                restart_count: u64::from(status.restart_count),
                crash_loop_threshold: u64::from(status.crash_loop_threshold),
                crash_loop: status.crash_loop,
                launch_sequence: status.launch_sequence,
                last_failure_code: status.last_failure_code,
                last_failure_message: status.last_failure_message,
                next_restart_delay_ms: status.next_restart_delay_ms,
                stable_uptime_ms: status.stable_uptime_ms,
            }
        });
        Ok(AgentStatus {
            agent_state,
            active,
            previous_active: previous,
            candidate,
            in_flight_transaction: in_flight,
            last_error,
            quarantined,
            recovery: None,
            supervision,
        })
    }

    fn advance(&self, tx: &str, from: DeployState, to: DeployState) -> AgentResult<()> {
        if !is_permitted(from, to) {
            return Err(AgentError::InvalidTransition(format!("{from:?} -> {to:?}")));
        }
        self.store.record_phase(tx, to, now_monotonic_ns())
    }

    fn fail(
        &self,
        transaction_id: &str,
        deployment_id: &str,
        last_successful: DeployState,
        err: AgentError,
    ) -> AgentResult<DeployOutcome> {
        let record = err.to_record();
        // Stamp the in-flight transaction with the last-successful phase
        // so the quarantine record reflects how far the candidate got
        // before failing. This is what recovery / status surfaces read.
        let _ = self.store.update(|s| {
            if let Some(tx) = s.in_flight_transaction.as_mut() {
                tx.phase = last_successful;
                tx.failure = Some(record.clone());
                tx.last_transition_monotonic_ns = Some(now_monotonic_ns());
            }
            Ok(())
        });
        self.emit(&WorkerEvent::Failure {
            transaction_id: transaction_id.to_string(),
            deployment_id: deployment_id.to_string(),
            reason: err.to_string(),
        });
        self.store
            .quarantine_in_flight(record, now_monotonic_ns())?;
        Err(err)
    }

    fn fail_rollback(
        &self,
        transaction_id: &str,
        deployment_id: &str,
        err: AgentError,
    ) -> AgentResult<DeployOutcome> {
        let record = err.to_record();
        let _ = self.store.update(|s| {
            if let Some(tx) = s.in_flight_transaction.as_mut() {
                tx.phase = DeployState::Failed;
                tx.failure = Some(record.clone());
                tx.last_transition_monotonic_ns = Some(now_monotonic_ns());
            }
            Ok(())
        });
        self.emit(&WorkerEvent::Failure {
            transaction_id: transaction_id.to_string(),
            deployment_id: deployment_id.to_string(),
            reason: err.to_string(),
        });
        // Rollback failures do not quarantine the previous-active record:
        // it is still a valid deployment, just temporarily unreachable.
        // Clear the in-flight transaction and surface last_error.
        self.store.set_last_error(Some(record))?;
        self.store.clear_transaction()?;
        Err(err)
    }

    fn stage_bundle(&self, deployment_id: &str, verified: &VerifiedBundle) -> AgentResult<PathBuf> {
        let dest = self.config.staging_dir.join(deployment_id);
        fs::create_dir_all(&dest)?;
        // Manifest first.
        fs::copy(
            verified.root_path.join("manifest.json"),
            dest.join("manifest.json"),
        )?;
        // All declared artifacts.
        for art in &verified.manifest.artifacts {
            let source = verified.root_path.join(&art.path);
            let destination = dest.join(&art.path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source, &destination)?;
        }
        Ok(dest)
    }

    fn emit(&self, event: &WorkerEvent) {
        if let Some(sink) = self.sink.as_ref() {
            sink(event);
        }
    }

    /// Forward the new active deployment to the attached supervisor and
    /// reset crash-loop counters. Best-effort: a poisoned supervisor
    /// mutex never blocks deploy progress because the coordinator owns
    /// the source of truth (the durable state store).
    fn notify_supervisor_promotion(&self, deployment_id: &str, backend_hint: &str) {
        let Some(sup) = self.supervisor.as_ref() else {
            return;
        };
        let desired = DesiredWorker {
            deployment_id: deployment_id.to_string(),
            backend: backend_hint.to_string(),
        };
        let _ = sup.set_desired_active(Some(desired));
        let _ = sup.recover_after_operator_action();
    }
}

fn merge_agent_state(base: AgentRunState, supervision: SupervisionAgentState) -> AgentRunState {
    match supervision {
        SupervisionAgentState::Failed => AgentRunState::Failed,
        SupervisionAgentState::Degraded => {
            if matches!(base, AgentRunState::Failed) {
                AgentRunState::Failed
            } else {
                AgentRunState::Degraded
            }
        }
        SupervisionAgentState::Ready | SupervisionAgentState::Unknown => base,
    }
}

/// Generate a new transaction id. UUID v4 is opaque, globally unique, and
/// safe to use as a stable correlation key across restarts.
#[must_use]
pub fn new_transaction_id() -> String {
    format!("tx-{}", uuid::Uuid::new_v4())
}

/// Read the wall-clock-anchored monotonic-ish timestamp. The agent does
/// **not** persist wall-clock time as a deadline; this value is only
/// stored for diagnostic ordering. Tests use it to assert that promotion
/// recorded a non-zero timestamp.
#[must_use]
pub fn now_monotonic_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
