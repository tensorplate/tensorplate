// SPDX-License-Identifier: Apache-2.0
//
// V01-E08-F04 / F05 / F06: Deploy and rollback transaction coordinator.
//
// The coordinator orchestrates one deploy or rollback transaction at a
// time. It:
//
//   1. Verifies the bundle through the V01-E08-F03 verifier.
//   2. Stages the bundle into a tree of its own,
//      `staging_dir/<deployment_id>/<transaction id>/`, by copying the
//      manifest plus the verified artifacts. No tree a record names is
//      written again, so the previous active deployment's files stay as
//      they were (see `crate::staging`).
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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tensorplate_platform::PlatformRegistry;
use tensorplate_protocol::agent_control::{
    is_valid_deployment_id, singleton_slots, AgentRunState, AgentStatus, ContactState,
    DeployFailureSummary, DeployStatus, DeploymentSummary, MemberStatus, ProcessState,
    QuarantineSummary, Readiness, ResidentSetStatus, ResponseError, SupervisionStatusSummary,
};
use tensorplate_protocol::agent_state::{
    AgentState, DeploymentRecord, TransactionKind, TransactionRecord,
};
use tensorplate_protocol::deploy_transaction::DeployState;
use tensorplate_protocol::supervision_event::SupervisionAgentState;
use tensorplate_protocol::worker_control::CandidateRef;
use tensorplate_protocol::ErrorCode;

use std::collections::BTreeMap;

use crate::backend_detection::BackendProbeReport;
use crate::bundle::{
    capacity_check, model_artifact_relative_path, verify_before_staging, DeployFacts,
};
use crate::config::AgentConfig;
use crate::error::{AgentError, AgentResult};
use crate::platform_admission::PlatformAdmission;
use crate::state::StateStore;
use crate::supervision::supervisor::{DesiredWorker, WorkerSupervisor};
use crate::transaction::is_permitted;
use crate::worker::{
    LiveMember, MemberEvent, MemberRole, ServingMember, WorkerControl, WorkerEvent,
};

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
    platform_registry: Option<PlatformRegistry>,
    platform_admission: Option<PlatformAdmission>,
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
            platform_registry: None,
            platform_admission: None,
        }
    }

    /// Attach the platform support registry the agent loaded at startup.
    ///
    /// Loaded once and held, rather than re-read per query: re-reading
    /// would let two answers in one agent lifetime disagree because a
    /// package upgrade landed in between.
    #[must_use]
    pub fn with_platform_registry(mut self, registry: PlatformRegistry) -> Self {
        self.platform_registry = Some(registry);
        self
    }

    /// The platform support registry, or `None` when it could not be
    /// loaded at startup.
    ///
    /// `None` is not "no platform is supported" — it means the agent has
    /// no basis to judge, and callers must treat the two differently.
    #[must_use]
    pub fn platform_registry(&self) -> Option<&PlatformRegistry> {
        self.platform_registry.as_ref()
    }

    /// Attach the platform outcome evaluated at startup.
    #[must_use]
    pub fn with_platform_admission(mut self, admission: PlatformAdmission) -> Self {
        self.platform_admission = Some(admission);
        self
    }

    /// The cached platform admission outcome, when this platform has an
    /// accelerator probe wired for deploy admission.
    #[must_use]
    pub fn platform_admission(&self) -> Option<&PlatformAdmission> {
        self.platform_admission.as_ref()
    }

    /// Attach packaging backend probe reports. The agent main calls
    /// this once at startup with the cached probe outcomes for every
    /// backend listed in `config.available_backends`. The coordinator
    /// hands the map to [`crate::bundle::verify_before_staging`] so a
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

    /// The worker client: the agent's loop supervises and stops the serving
    /// workers through it.
    #[must_use]
    pub fn worker_client(&self) -> &Arc<dyn WorkerControl> {
        &self.worker
    }

    /// One turn of the agent's loop over the serving members.
    pub fn supervise(&self) -> Vec<MemberEvent> {
        self.worker.supervise()
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
        if !is_valid_deployment_id(deployment_id) {
            return Err(AgentError::Config(
                "deployment_id must be 1 to 128 bytes and contain only ASCII letters, digits, `-`, `_`, or `.`; `.` and `..` are reserved".into(),
            ));
        }

        self.refuse_while_stopping()?;
        let transaction_id = new_transaction_id();
        let tx_record = TransactionRecord {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
            phase: DeployState::Received,
            kind: TransactionKind::Deploy,
            bundle_digest: None,
            generation: None,
            descriptor_digest: None,
            configuration_digest: None,
            bundle_path: Some(bundle_path.display().to_string()),
            correlation_id,
            started_monotonic_ns: Some(now_monotonic_ns()),
            last_transition_monotonic_ns: Some(now_monotonic_ns()),
            failure: None,
        };
        self.store.begin_transaction(tx_record)?;
        // No other deploy stages while this one's transaction is in flight.
        match crate::staging::prune(&self.config.staging_dir, &self.store) {
            Ok(0) => {}
            Ok(removed) => {
                eprintln!("staged bundles: {removed} removed that no state record names");
            }
            Err(err) => eprintln!("staged bundles: not all unnamed copies were removed: {err}"),
        }

        if let Some(admission) = &self.platform_admission {
            if let Err(err) = admission.ensure_supported() {
                return self.fail(&transaction_id, deployment_id, err);
            }
        }

        // Phase: verified.
        let verification = self.store.snapshot().and_then(|state| {
            let facts = DeployFacts {
                probes: &self.backend_probes,
                registry: self.platform_registry.as_ref(),
                admission: self.platform_admission.as_ref(),
                state: Some(&state),
            };
            verify_before_staging(bundle_path, &self.config, &facts)
        });
        let verified = match verification {
            Ok(v) => v,
            Err(err) => return self.fail(&transaction_id, deployment_id, err),
        };
        if let (Some(admission), Some(registry)) =
            (&self.platform_admission, self.platform_registry.as_ref())
        {
            if let Err(err) =
                admission.admit_backend(registry, verified.manifest.backend_hint.as_str())
            {
                return self.fail(&transaction_id, deployment_id, err);
            }
        }
        if let Some(expected) = expected_bundle_digest {
            if !crate::bundle::bundle_digests_equal(expected, &verified.manifest_digest) {
                let err = AgentError::BundleIntegrity {
                    path: "manifest.json".into(),
                    reason: format!(
                        "expected_bundle_digest mismatch: caller `{expected}` computed `{}`",
                        verified.manifest_digest
                    ),
                };
                return self.fail(&transaction_id, deployment_id, err);
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
        let staging_dir = &self.config.staging_dir;
        let staged = crate::staging::stage(staging_dir, deployment_id, &transaction_id, &verified);
        let staged_path = match staged {
            Ok(p) => p,
            Err(err) => {
                return self.fail(&transaction_id, deployment_id, err);
            }
        };
        let candidate_record = DeploymentRecord {
            deployment_id: deployment_id.to_string(),
            bundle_digest: verified.manifest_digest.clone(),
            generation: None,
            descriptor_digest: None,
            configuration_digest: None,
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
            return self.fail(&transaction_id, deployment_id, err);
        }
        self.advance(
            &transaction_id,
            DeployState::Staged,
            DeployState::CapacityChecked,
        )?;

        // The reference handed to the worker. Just before `prepare`: a format
        // 0.2 bundle's generation is taken here, under a hold `prepare` ends.
        let first = crate::descriptor_store::Deploy {
            transaction_id: &transaction_id,
            record: &candidate_record,
            artifact_relative_path: verified.manifest.model_artifact().map(|a| a.path.clone()),
            format_version: &verified.manifest.format_version,
            probes: &self.backend_probes,
        };
        let worker = self.worker.as_ref();
        let in_hand = match crate::descriptor_store::start_deploy(&self.store, worker, &first) {
            Ok(in_hand) => in_hand,
            Err(err) => {
                return self.fail(&transaction_id, deployment_id, err);
            }
        };

        // Phase: prepared.
        self.emit(&WorkerEvent::PrepareStart {
            transaction_id: transaction_id.clone(),
            deployment_id: deployment_id.to_string(),
        });
        let prepare_timeout = Duration::from_millis(self.config.worker.prepare_timeout_ms);
        let (candidate_ref, prepared) = in_hand.prepare(&transaction_id, prepare_timeout);
        if let Err(err) = prepared {
            self.emit(&WorkerEvent::Failure {
                transaction_id: transaction_id.clone(),
                deployment_id: deployment_id.to_string(),
                reason: err.to_string(),
            });
            return self.fail(&transaction_id, deployment_id, err);
        }
        let mut prepared = Prepared::new(self.worker.as_ref(), &candidate_ref);
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
                return self.fail(&transaction_id, deployment_id, err);
            }
        };
        if !warmed.ready {
            let err = AgentError::WorkerNotReady;
            self.emit(&WorkerEvent::Failure {
                transaction_id: transaction_id.clone(),
                deployment_id: deployment_id.to_string(),
                reason: err.to_string(),
            });
            return self.fail(&transaction_id, deployment_id, err);
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
            return self.fail(&transaction_id, deployment_id, err);
        }
        prepared.promoted = true;
        // The commit: the records move and the phase becomes `promoted` in
        // one write. No later phase is written to a transaction in flight.
        self.store.promote_candidate(now_monotonic_ns())?;
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
        // A deployment replaced under its own id was retired at promotion;
        // unloading that id now would stop the worker just promoted.
        if let Some(prev) = previous_active
            .as_deref()
            .filter(|prev| *prev != deployment_id)
        {
            self.worker.unload(prev);
            self.emit(&WorkerEvent::Unload {
                deployment_id: prev.to_string(),
            });
        }

        self.store.end_transaction()?;

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
        self.refuse_while_stopping()?;
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
            generation: None,
            descriptor_digest: None,
            configuration_digest: None,
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

        // Just before `prepare`: a described record's generation is taken
        // here, under a hold `prepare` ends.
        let start = crate::descriptor_store::Start::Rollback(&transaction_id);
        let (store, worker) = (&self.store, self.worker.as_ref());
        let again = crate::descriptor_store::start_again(
            store,
            worker,
            &prev,
            artifact_relative_path,
            start,
        );
        let in_hand = match again {
            Ok(in_hand) => in_hand,
            Err(err) => return self.fail_rollback(&transaction_id, &prev.deployment_id, err),
        };

        self.emit(&WorkerEvent::PrepareStart {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
        });
        let prepare_timeout = Duration::from_millis(self.config.worker.prepare_timeout_ms);
        let (candidate_ref, prepared) = in_hand.prepare(&transaction_id, prepare_timeout);
        if let Err(err) = prepared {
            return self.fail_rollback(&transaction_id, &prev.deployment_id, err);
        }
        let mut prepared = Prepared::new(self.worker.as_ref(), &candidate_ref);
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
        prepared.promoted = true;
        // The commit, as a deploy's: the swap and `promoted` in one write.
        self.store.swap_active_with_previous(now_monotonic_ns())?;
        self.emit(&WorkerEvent::Promote {
            transaction_id: transaction_id.clone(),
            deployment_id: prev.deployment_id.clone(),
        });
        self.store.end_transaction()?;

        // V01-E09-F06-T02: hand the rolled-back active over to the
        // supervisor and reset crash-loop state.
        self.notify_supervisor_promotion(prev.deployment_id.as_str(), prev.backend_hint.as_str());

        Ok(DeployOutcome {
            transaction_id,
            deployment_id: prev.deployment_id,
            bundle_digest: prev.bundle_digest,
        })
    }

    /// Project the durable state and the supervisor onto [`AgentStatus`].
    /// A store that refuses writes after a write with an unknown outcome
    /// reports the agent failed: nothing can change until it restarts, and
    /// its durable `last_error` cannot record why.
    ///
    /// # Errors
    ///
    /// Propagates errors from the underlying state store.
    pub fn status(&self) -> AgentResult<AgentStatus> {
        let mut status = self.durable_status()?;
        if self.store.is_indeterminate() {
            status.agent_state = AgentRunState::Failed;
            status.last_error = Some(ResponseError::new(
                ErrorCode::Internal,
                "a durable state write ended with an unknown outcome; the agent refuses state changes until it restarts",
            ));
        }
        Ok(status)
    }

    /// The deployment slots and the resident set of `state`, with what the
    /// worker control reports of the processes behind them.
    fn deployments(&self, state: &AgentState) -> AgentResult<Deployments> {
        let mut active = state.active.as_ref().map(deployment_summary);
        // Without an active record nothing is read from the serving member,
        // so a registry that cannot answer for it does not fail the status.
        if let (Some(recorded), Some(summary)) = (state.active.as_ref(), active.as_mut()) {
            if let Some(serving) = self.worker.serving_member()? {
                // A worker is promoted before the record that names it is
                // written, and the worker it replaces is stopped in between:
                // the member serving may be another deployment's, or another
                // bundle deployed under the same id.
                if serves(&serving, recorded) {
                    summary.generation = Some(serving.member.generation);
                }
                summary.serving_url = Some(serving.serving_url);
            }
        }
        // Nothing serves the retained record: its generation is the one it
        // was last started as, where it names a descriptor.
        let mut previous = state.previous_active.as_ref().map(|retained| {
            let generation = retained.generation;
            DeploymentSummary {
                generation,
                ..deployment_summary(retained)
            }
        });
        let live = self.worker.live_members();
        // Beside a durable resident set the singleton slots are empty; a
        // size-1 serving set projects into them for clients that read only
        // `active` (with a `serving_url` only for a loopback unary endpoint).
        let resident_set = state.resident_set.as_ref().map(|set| {
            if active.is_none() && previous.is_none() {
                (active, previous) = singleton_slots(set);
            }
            let mut status = ResidentSetStatus::from_committed(set);
            if let Some(live) = live.as_deref() {
                for member in &mut status.members {
                    report_process(member, live);
                }
            }
            status
        });
        let mut candidate = state.candidate.as_ref().map(deployment_summary);
        if let (Some(summary), Some(live)) = (candidate.as_mut(), live.as_deref()) {
            summary.generation = prepared_generation(live, &summary.deployment_id);
        }
        Ok(Deployments {
            active,
            previous,
            candidate,
            resident_set,
        })
    }

    /// The status the durable state and the supervisor describe, before
    /// [`Self::status`] accounts for a store that refuses writes.
    fn durable_status(&self) -> AgentResult<AgentStatus> {
        let s = self.store.snapshot()?;
        // A member whose restarts are used up is why nothing serves, so it
        // is reported ahead of the last transaction's error.
        let last_error = self
            .worker
            .member_failure()
            .or_else(|| s.last_error.clone());
        let mut agent_state = if last_error.is_some() {
            AgentRunState::Degraded
        } else {
            AgentRunState::Ready
        };
        if self
            .platform_admission
            .as_ref()
            .is_some_and(PlatformAdmission::telemetry_degraded)
        {
            agent_state = AgentRunState::Degraded;
        }
        let Deployments {
            active,
            previous,
            candidate,
            resident_set,
        } = self.deployments(&s)?;
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
        let last_error = last_error.map(|e| ResponseError {
            code: e.code,
            message: e.message,
            context: e.context,
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
                desired_device_index: status.desired_device_index,
                actual_device_index: status.actual_device_index,
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
            resident_set,
            // This agent executes neither `set_operation` `add` nor a member
            // rollback, so it lists no control feature.
            control_features: Vec::new(),
            agent_state,
            active,
            previous_active: previous,
            candidate,
            in_flight_transaction: in_flight,
            last_error,
            quarantined,
            recovery: None,
            supervision,
            platform_telemetry: self
                .platform_admission
                .as_ref()
                .and_then(PlatformAdmission::telemetry_status),
        })
    }

    /// Once the agent is stopping no transaction begins and none is
    /// recorded failed: one in flight stays so, as after a kill, and the
    /// next start's recovery decides it.
    fn refuse_while_stopping(&self) -> AgentResult<()> {
        if self.worker.stopping() {
            return Err(AgentError::Stopping);
        }
        Ok(())
    }

    fn advance(&self, tx: &str, from: DeployState, to: DeployState) -> AgentResult<()> {
        if !is_permitted(from, to) {
            return Err(AgentError::InvalidTransition(format!("{from:?} -> {to:?}")));
        }
        self.store.record_phase(tx, to, now_monotonic_ns())
    }

    /// End a deploy that failed, in one write: its quarantine record
    /// carries the phase the transaction had recorded and the failure.
    fn fail(
        &self,
        transaction_id: &str,
        deployment_id: &str,
        err: AgentError,
    ) -> AgentResult<DeployOutcome> {
        self.refuse_while_stopping()?;
        self.emit(&WorkerEvent::Failure {
            transaction_id: transaction_id.to_string(),
            deployment_id: deployment_id.to_string(),
            reason: err.to_string(),
        });
        self.store
            .quarantine_in_flight(err.to_record(), now_monotonic_ns())?;
        Err(err)
    }

    fn fail_rollback(
        &self,
        transaction_id: &str,
        deployment_id: &str,
        err: AgentError,
    ) -> AgentResult<DeployOutcome> {
        self.refuse_while_stopping()?;
        self.emit(&WorkerEvent::Failure {
            transaction_id: transaction_id.to_string(),
            deployment_id: deployment_id.to_string(),
            reason: err.to_string(),
        });
        // Rollback failures do not quarantine the previous-active record:
        // it is still a valid deployment, just temporarily unreachable.
        // One write clears the transaction and records why.
        self.store.fail_transaction(err.to_record())?;
        Err(err)
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
            // Unpinned: one deployment, one worker, whatever the host
            // exposes. A pin here needs an allocator deciding which device
            // each replica gets, and nothing allocates yet -- rows claim
            // one device and the manifest refuses to ask for more.
            device_index: None,
        };
        let _ = sup.set_desired_active(Some(desired));
        let _ = sup.recover_after_operator_action();
    }
}

/// A candidate the worker control prepared. Unless it is promoted, it is
/// abandoned when the transaction returns, whichever way it returns.
struct Prepared<'a> {
    worker: &'a dyn WorkerControl,
    candidate: &'a CandidateRef,
    promoted: bool,
}

impl<'a> Prepared<'a> {
    fn new(worker: &'a dyn WorkerControl, candidate: &'a CandidateRef) -> Self {
        Self {
            worker,
            candidate,
            promoted: false,
        }
    }
}

impl Drop for Prepared<'_> {
    fn drop(&mut self) {
        if !self.promoted {
            self.worker.abandon(self.candidate);
        }
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

/// What status reports of the recorded deployments.
struct Deployments {
    active: Option<DeploymentSummary>,
    previous: Option<DeploymentSummary>,
    candidate: Option<DeploymentSummary>,
    resident_set: Option<ResidentSetStatus>,
}

/// Whether `serving` is the deployment `recorded` describes: the same id,
/// started from the same bundle and, when the record names a descriptor,
/// from that descriptor. A record that names none cannot tell a redeploy
/// of its own bundle under its own id from the deployment it records.
pub(crate) fn serves(serving: &ServingMember, recorded: &DeploymentRecord) -> bool {
    let started_from = |recorded: &Option<String>, started_from: &Option<String>| {
        recorded.is_none() || recorded == started_from
    };
    serving.member.deployment_id == recorded.deployment_id
        && crate::bundle::bundle_digests_equal(&serving.bundle_digest, &recorded.bundle_digest)
        && started_from(&recorded.descriptor_digest, &serving.descriptor_digest)
        && started_from(
            &recorded.configuration_digest,
            &serving.configuration_digest,
        )
}

/// The generation a deploy of `deployment_id` has prepared and not promoted.
/// A candidate an earlier deploy abandoned is not this deploy's, whatever
/// its id.
fn prepared_generation(live: &[LiveMember], deployment_id: &str) -> Option<u64> {
    live.iter()
        .find(|held| {
            held.role == MemberRole::Candidate && held.member.deployment_id == deployment_id
        })
        .map(|held| held.member.generation)
}

/// Report how the worker process of `member`'s committed generation stands,
/// from what the worker control holds. A generation it holds no process for
/// is stopped.
fn report_process(member: &mut MemberStatus, live: &[LiveMember]) {
    let held = live.iter().find(|held| {
        held.member.deployment_id == member.deployment_id
            && held.member.generation == member.generation
    });
    let running = held.is_some_and(|held| held.running);
    member.process = Some(if running {
        ProcessState::Running
    } else {
        ProcessState::Stopped
    });
    member.readiness = Some(if running && held.is_some_and(|held| held.ready) {
        Readiness::Ready
    } else {
        Readiness::NotReady
    });
    member.draining = Some(held.is_some_and(|held| held.role == MemberRole::Draining));
    member.contact = held
        .filter(|held| held.running)
        .and_then(|held| held.in_contact)
        .map(|in_contact| {
            if in_contact {
                ContactState::InContact
            } else {
                ContactState::OutOfContact
            }
        });
}

/// The status summary of a singleton deployment record, without a serving
/// URL.
fn deployment_summary(d: &DeploymentRecord) -> DeploymentSummary {
    DeploymentSummary {
        deployment_id: d.deployment_id.clone(),
        generation: None,
        bundle_digest: d.bundle_digest.clone(),
        bundle_name: Some(d.bundle_name.clone()),
        bundle_version: Some(d.bundle_version.clone()),
        backend_hint: Some(d.backend_hint.clone()),
        model_class: Some(d.model_class.clone()),
        staged_path: Some(d.staged_path.clone()),
        promoted_monotonic_ns: d.promoted_monotonic_ns,
        serving_url: None,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{prepared_generation, report_process, serves};
    use crate::worker::{LiveMember, MemberRole, ServingMember};
    use tensorplate_protocol::agent_control::{
        MemberStatus, ProcessState, Readiness, ResidentSetStatus,
    };
    use tensorplate_protocol::agent_state::decode_agent_state;
    use tensorplate_protocol::worker_control::MemberRef;

    fn held(id: &str, generation: u64, role: MemberRole) -> LiveMember {
        LiveMember {
            member: MemberRef::new(id, generation),
            role,
            running: true,
            ready: true,
            in_contact: Some(true),
            http_port: 18_080,
            stream_port: None,
        }
    }

    /// The member a state fixture commits: `vision-v2`, generation 2.
    fn committed() -> MemberStatus {
        let state = decode_agent_state(include_str!(
            "../../protocol/rust/tests/fixtures/agent_state_0_2_restore_step.json"
        ))
        .expect("fixture decodes");
        let set = state.resident_set.expect("the fixture records a set");
        ResidentSetStatus::from_committed(&set).members.remove(0)
    }

    #[test]
    fn a_process_is_the_members_only_under_its_deployment_id_and_its_generation() {
        for (id, generation, expected) in [
            ("vision-v2", 2, ProcessState::Running),
            ("vision-v2", 3, ProcessState::Stopped),
            ("vision-v3", 2, ProcessState::Stopped),
        ] {
            let mut member = committed();
            report_process(&mut member, &[held(id, generation, MemberRole::Serving)]);
            assert_eq!(
                member.process,
                Some(expected),
                "{id} generation {generation}"
            );
        }
    }

    #[test]
    fn a_running_process_is_ready_only_once_the_agent_judged_it_so() {
        let mut loading = held("vision-v2", 2, MemberRole::Serving);
        loading.ready = false;
        let mut member = committed();
        report_process(&mut member, &[loading]);
        assert_eq!(
            (member.process, member.readiness),
            (Some(ProcessState::Running), Some(Readiness::NotReady))
        );
    }

    #[test]
    fn a_record_that_names_a_descriptor_is_served_only_by_a_member_started_from_it() {
        let state = decode_agent_state(include_str!(
            "../../protocol/rust/tests/fixtures/agent_state_0_2_descriptor_identity.json"
        ))
        .expect("fixture decodes");
        let recorded = state.active.expect("the fixture records one");
        let (described, configured) = (
            recorded.descriptor_digest.clone().expect("it names one"),
            recorded.configuration_digest.clone().expect("it names one"),
        );
        let serving = |descriptor: Option<&str>, configuration: Option<&str>| ServingMember {
            member: MemberRef::new(recorded.deployment_id.clone(), 3),
            bundle_digest: recorded.bundle_digest.clone(),
            descriptor_digest: descriptor.map(Into::into),
            configuration_digest: configuration.map(Into::into),
            serving_url: String::new(),
        };
        let other = "sha256:0000";

        assert!(serves(
            &serving(Some(&described), Some(&configured)),
            &recorded
        ));
        for (descriptor, configuration) in [
            (Some(other), Some(configured.as_str())),
            (Some(described.as_str()), Some(other)),
            (None, Some(configured.as_str())),
            (Some(described.as_str()), None),
        ] {
            assert!(
                !serves(&serving(descriptor, configuration), &recorded),
                "{descriptor:?} {configuration:?}"
            );
        }

        // A record that names none cannot tell: the id and the bundle decide.
        let mut unnamed = recorded.clone();
        (unnamed.generation, unnamed.descriptor_digest) = (None, None);
        unnamed.configuration_digest = None;
        assert!(serves(&serving(Some(other), Some(other)), &unnamed));
        assert!(serves(&serving(None, None), &unnamed));
    }

    #[test]
    fn the_prepared_generation_is_the_candidate_of_that_deployment_and_nothing_else() {
        let live = [
            held("stt", 4, MemberRole::Serving),
            held("stt", 5, MemberRole::Draining),
            held("tts", 6, MemberRole::Candidate),
        ];
        assert_eq!(prepared_generation(&live, "tts"), Some(6));
        assert_eq!(prepared_generation(&live, "stt"), None);
        assert_eq!(prepared_generation(&[], "tts"), None);
    }
}
