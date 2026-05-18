// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F04 / F06 / F07: WorkerSupervisor — the agent-owned worker
// lifecycle state machine.
//
// Design rules:
//
//   1. The supervisor is *driven*: callers invoke `tick(now)` to advance
//      state. `tick` is non-blocking and idempotent. The agent main loop
//      calls `tick` on every iteration; tests inject a fake clock and
//      drive `tick` manually for deterministic backoff behavior.
//
//   2. The supervisor uses monotonic time exclusively. Wall-clock is
//      never consulted; restart and crash-loop decisions are stable
//      under wall-clock adjustments.
//
//   3. Process I/O is delegated to [`WorkerProcess`] and readiness I/O
//      to [`ReadinessProbe`]; both are trait objects so tests can mock
//      them out and the production code can plug real implementations
//      in without changing the state machine.
//
//   4. Supervision events are emitted at every state transition through
//      the bounded [`SupervisionEventSink`]. The supervisor never blocks
//      on the sink; a missing or slow consumer cannot stall restart or
//      stop progress (V01-E09-F05).
//
//   5. Deploy / rollback transactions interact with the supervisor
//      through the documented control surface (`request_stop`,
//      `set_desired_active`, `notify_external_promotion`,
//      `recover_after_operator_action`). Crash inside an in-flight
//      transaction is reported back via [`tick`] returning a typed
//      [`SupervisionFault`] the V01-E08 coordinator maps to a candidate
//      failure (V01-E09-F06-T02). The supervisor itself never promotes
//      a candidate.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::supervision_event::{SupervisionAgentState, SupervisionEventKind};

use crate::error::{AgentError, AgentResult};

use super::clock::MonotonicClock;
use super::config::{RestartPolicyKind, SupervisorConfig};
use super::event::{NoopEventSink, SupervisionEventPayload, SupervisionEventSink};
use super::policy::{BackoffDecision, BackoffScheduler, FailureClass};
use super::process::{ExitStatus, PollOutcome, WorkerHandle, WorkerProcess};
use super::readiness::{ReadinessProbe, ReadinessSample};
use super::state::{SupervisionPhase, SupervisionState, SupervisionStatus};

/// External fault surfaced by [`WorkerSupervisor::tick`]. Returned at the
/// instant the supervisor finalizes a worker failure so the calling
/// coordinator can map it onto an in-flight deploy transaction (V01-E09-F06-T02).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisionFault {
    pub class: FailureClass,
    pub error_code: ErrorCode,
    pub message: String,
    pub launch_sequence: u64,
    pub deployment_id: Option<String>,
}

/// `tick` outcome. Idle ticks return `Continue`. The supervisor returns
/// `Fault` once per failure so consumers can react without polling
/// status repeatedly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TickOutcome {
    Continue,
    /// The worker entered a terminal supervision state. Callers should
    /// inspect status and (optionally) notify operators / observability.
    Terminal(SupervisionPhase),
    /// A failure was finalized this tick.
    Fault(SupervisionFault),
}

/// Desired-state input. Installed by `Coordinator` after a successful
/// promote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredWorker {
    pub deployment_id: String,
    pub backend: String,
}

/// Public V01-E09 worker supervisor.
pub struct WorkerSupervisor {
    cfg: SupervisorConfig,
    process: Arc<dyn WorkerProcess>,
    probe: Arc<dyn ReadinessProbe>,
    clock: Arc<dyn MonotonicClock>,
    sink: Arc<dyn SupervisionEventSink>,
    inner: Mutex<Inner>,
    anchor: Instant,
}

struct Inner {
    state: SupervisionState,
    backoff: BackoffScheduler,
    desired: Option<DesiredWorker>,
    handle: Option<WorkerHandle>,
    launched_at: Option<Instant>,
    after_ready: bool,
    pending_faults: VecDeque<SupervisionFault>,
    stop_requested: bool,
    stop_started_at: Option<Instant>,
    force_terminate_at: Option<Instant>,
    next_sequence: u64,
}

impl WorkerSupervisor {
    /// Build a supervisor. The caller wires concrete process / probe /
    /// clock / sink implementations through the builder methods.
    pub fn new(
        cfg: SupervisorConfig,
        process: Arc<dyn WorkerProcess>,
        probe: Arc<dyn ReadinessProbe>,
        clock: Arc<dyn MonotonicClock>,
    ) -> AgentResult<Self> {
        let cfg = cfg.validate()?;
        let now = clock.now();
        let backoff = BackoffScheduler::new(cfg.restart_policy.clone());
        Ok(Self {
            cfg,
            process,
            probe,
            clock,
            sink: Arc::new(NoopEventSink),
            inner: Mutex::new(Inner {
                state: SupervisionState::fresh(),
                backoff,
                desired: None,
                handle: None,
                launched_at: None,
                after_ready: false,
                pending_faults: VecDeque::new(),
                stop_requested: false,
                stop_started_at: None,
                force_terminate_at: None,
                next_sequence: 0,
            }),
            anchor: now,
        })
    }

    /// Install an event sink. The default sink is a no-op; production
    /// callers install the bounded ring sink, tests inject a
    /// `RingEventSink` to capture transitions.
    #[must_use]
    pub fn with_event_sink(mut self, sink: Arc<dyn SupervisionEventSink>) -> Self {
        self.sink = sink;
        self
    }

    /// Read-only access to the validated config.
    #[must_use]
    pub fn config(&self) -> &SupervisorConfig {
        &self.cfg
    }

    /// Snapshot the current supervision status.
    #[allow(clippy::expect_used)]
    #[must_use]
    pub fn status(&self) -> SupervisionStatus {
        let inner = self.inner.lock().expect("supervisor mutex poisoned");
        let now = self.clock.now();
        inner.state.status(now)
    }

    /// Install / clear the desired active deployment.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the supervisor mutex is
    /// poisoned.
    #[allow(clippy::needless_pass_by_value)]
    pub fn set_desired_active(&self, desired: Option<DesiredWorker>) -> AgentResult<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor mutex poisoned: {e}")))?;
        inner.desired.clone_from(&desired);
        inner.state.set_desired_active(
            desired.as_ref().map(|d| d.deployment_id.clone()),
            desired.as_ref().map(|d| d.backend.clone()),
        );
        if desired.is_none() {
            // Drop any in-flight backoff schedule. Stopping is requested
            // explicitly through `request_stop`.
            inner.state.next_restart_at = None;
        }
        Ok(())
    }

    /// Request a graceful stop of the current worker. The supervisor
    /// transitions through `Stopping` -> `Stopped`.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the supervisor mutex is
    /// poisoned.
    pub fn request_stop(&self) -> AgentResult<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor mutex poisoned: {e}")))?;
        if inner.handle.is_some() {
            inner.stop_requested = true;
        }
        Ok(())
    }

    /// Reset crash-loop counters after operator action (recovery,
    /// rollback, deploy of a new bundle). V01-E09-F06-T02 documents this
    /// as the only path out of crash-loop terminal state.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the supervisor mutex is
    /// poisoned.
    pub fn recover_after_operator_action(&self) -> AgentResult<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor mutex poisoned: {e}")))?;
        inner.backoff.reset();
        inner.state.counters = inner.backoff.counters(self.clock.now());
        inner.state.last_failure = None;
        inner.state.next_restart_at = None;
        if matches!(
            inner.state.phase,
            SupervisionPhase::Failed | SupervisionPhase::CrashLoop
        ) {
            inner.state.phase = if inner.desired.is_some() {
                SupervisionPhase::Starting
            } else {
                SupervisionPhase::NoActiveDeployment
            };
        }
        Ok(())
    }

    /// Drain pending faults the supervisor produced since the last call.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Internal`] if the mutex is poisoned.
    pub fn drain_faults(&self) -> AgentResult<Vec<SupervisionFault>> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor mutex poisoned: {e}")))?;
        Ok(inner.pending_faults.drain(..).collect())
    }

    /// Advance the supervisor by one tick. Callers invoke this on every
    /// iteration of the agent loop (or in deterministic order under a
    /// fake clock in tests). One call may perform multiple side effects:
    /// poll the worker, launch a replacement, sample readiness, escalate
    /// stops.
    ///
    /// # Errors
    ///
    /// Propagates fatal [`AgentError`] values from the process layer.
    /// Transient process / readiness errors are converted into
    /// supervision events and counted against the restart policy.
    pub fn tick(&self) -> AgentResult<TickOutcome> {
        let now = self.clock.now();
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| AgentError::Internal(format!("supervisor mutex poisoned: {e}")))?;

        // Step 1: poll for worker exit if a worker is running.
        if let Some(handle) = inner.handle.clone() {
            match self.process.poll(&handle)? {
                PollOutcome::Running => {}
                PollOutcome::Exited(status) => {
                    return self.on_worker_exit(&mut inner, &handle, status, now);
                }
            }
        }

        // Step 2: drive stop sequence escalation.
        if inner.stop_requested {
            return self.drive_stop(&mut inner, now);
        }

        // Step 3: in terminal phases, nothing left to do.
        if inner.state.phase.is_terminal() {
            return Ok(TickOutcome::Continue);
        }

        // Step 4: launch when desired-state requires a worker.
        if inner.handle.is_none() && inner.desired.is_some() {
            // Honor backoff if we have a scheduled restart in the future.
            if let Some(next) = inner.state.next_restart_at {
                if now < next {
                    return Ok(TickOutcome::Continue);
                }
            }
            return self.launch_worker(&mut inner, now);
        }

        // Step 5: probe readiness while the worker is running but not
        // yet ready.
        if let Some(handle) = inner.handle.clone() {
            if matches!(
                inner.state.phase,
                SupervisionPhase::Starting | SupervisionPhase::Running
            ) {
                self.probe_readiness(&mut inner, &handle, now)?;
            } else if matches!(
                inner.state.phase,
                SupervisionPhase::Ready | SupervisionPhase::Degraded
            ) {
                // Steady-state polling. The supervisor downgrades state
                // when the worker reports degraded/failed.
                self.steady_state_poll(&mut inner, &handle, now)?;
            }
        }

        Ok(TickOutcome::Continue)
    }

    fn launch_worker(&self, inner: &mut Inner, now: Instant) -> AgentResult<TickOutcome> {
        let desired = inner
            .desired
            .clone()
            .ok_or_else(|| AgentError::Internal("launch_worker without desired".into()))?;
        match self.process.launch(&desired.deployment_id) {
            Ok(handle) => {
                inner.handle = Some(handle.clone());
                inner.launched_at = Some(now);
                inner.after_ready = false;
                inner.state.launch_sequence = handle.launch_sequence;
                inner.state.phase = SupervisionPhase::Starting;
                inner.state.next_restart_at = None;
                inner.state.actual_active = Some(handle.deployment_id.clone());
                self.emit(
                    inner,
                    SupervisionEventKind::WorkerStarted,
                    now,
                    None,
                    None,
                    None,
                    None,
                    None,
                );
                Ok(TickOutcome::Continue)
            }
            Err(err) => {
                let class = FailureClass::ExitBeforeReady;
                let fault = SupervisionFault {
                    class,
                    error_code: err.code(),
                    message: err.to_string(),
                    launch_sequence: inner.state.launch_sequence,
                    deployment_id: Some(desired.deployment_id.clone()),
                };
                self.record_fault(inner, &fault, now);
                self.schedule_after_failure(inner, class, now)
            }
        }
    }

    fn probe_readiness(
        &self,
        inner: &mut Inner,
        handle: &WorkerHandle,
        now: Instant,
    ) -> AgentResult<()> {
        let started = inner.launched_at.unwrap_or(now);
        let deadline = started
            .checked_add(Duration::from_millis(self.cfg.startup_timeout_ms))
            .unwrap_or(started);

        // Move to Running before the first probe so observers see the
        // phase advance even when probes return early.
        if matches!(inner.state.phase, SupervisionPhase::Starting) {
            inner.state.phase = SupervisionPhase::Running;
        }

        let sample = self
            .probe
            .sample(&handle.deployment_id)
            .unwrap_or_else(|_| {
                // Transient probe error inside the startup window is
                // ignored; the deadline is the authoritative gate.
                ReadinessSample::unknown()
            });
        if sample.failed {
            self.finalize_unready(inner, handle, now, FailureClass::HealthFailed)?;
            return Ok(());
        }
        if sample.ready
            && sample.active_deployment.as_deref() == Some(handle.deployment_id.as_str())
        {
            inner.state.phase = SupervisionPhase::Ready;
            inner.state.actual_active = sample.active_deployment;
            inner.state.backend = sample.backend.or(inner.state.backend.clone());
            inner.state.last_ready_at = Some(now);
            inner.state.last_failure = None;
            inner.after_ready = true;
            inner.backoff.on_ready(now);
            inner.state.counters = inner.backoff.counters(now);
            self.emit(
                inner,
                SupervisionEventKind::WorkerReady,
                now,
                None,
                None,
                None,
                None,
                None,
            );
            return Ok(());
        }
        if now >= deadline {
            self.finalize_unready(inner, handle, now, FailureClass::NotReadyTimeout)?;
        }
        Ok(())
    }

    fn steady_state_poll(
        &self,
        inner: &mut Inner,
        handle: &WorkerHandle,
        now: Instant,
    ) -> AgentResult<()> {
        let Ok(sample) = self.probe.sample(&handle.deployment_id) else {
            return Ok(());
        };
        if sample.failed {
            self.finalize_unready(inner, handle, now, FailureClass::HealthFailed)?;
            return Ok(());
        }
        if sample.degraded && !matches!(inner.state.phase, SupervisionPhase::Degraded) {
            inner.state.phase = SupervisionPhase::Degraded;
            self.emit(
                inner,
                SupervisionEventKind::WorkerDegraded,
                now,
                None,
                None,
                None,
                None,
                None,
            );
        } else if sample.ready && matches!(inner.state.phase, SupervisionPhase::Degraded) {
            inner.state.phase = SupervisionPhase::Ready;
        }
        Ok(())
    }

    fn finalize_unready(
        &self,
        inner: &mut Inner,
        handle: &WorkerHandle,
        now: Instant,
        class: FailureClass,
    ) -> AgentResult<()> {
        let _ = self.process.force_terminate(handle);
        inner.handle = None;
        inner.state.actual_active = None;
        let message = format!(
            "worker `{}` failed readiness ({:?})",
            handle.deployment_id, class
        );
        let fault = SupervisionFault {
            class,
            error_code: match class {
                FailureClass::NotReadyTimeout => ErrorCode::Timeout,
                FailureClass::HealthFailed => ErrorCode::InferenceFailed,
                _ => ErrorCode::NotReady,
            },
            message,
            launch_sequence: handle.launch_sequence,
            deployment_id: Some(handle.deployment_id.clone()),
        };
        self.record_fault(inner, &fault, now);
        self.emit(
            inner,
            SupervisionEventKind::WorkerNotReady,
            now,
            None,
            None,
            None,
            Some(fault.error_code),
            Some(fault.message.clone()),
        );
        // Discard the outcome — the caller of `tick` is interested in
        // the on-exit branch's outcome only; readiness escalation just
        // schedules a restart.
        let _ = self.schedule_after_failure(inner, class, now)?;
        Ok(())
    }

    #[allow(clippy::needless_pass_by_value)]
    fn on_worker_exit(
        &self,
        inner: &mut Inner,
        handle: &WorkerHandle,
        status: ExitStatus,
        now: Instant,
    ) -> AgentResult<TickOutcome> {
        inner.handle = None;
        inner.state.actual_active = None;
        let after_ready = inner.after_ready;
        inner.after_ready = false;
        let class = if after_ready {
            FailureClass::ExitAfterReady
        } else {
            FailureClass::ExitBeforeReady
        };
        let mut exit_message = match (status.code, status.signal) {
            (Some(code), _) => format!("worker exited with code {code}"),
            (_, Some(sig)) => format!("worker terminated by signal {sig}"),
            _ => "worker exited (unknown cause)".to_string(),
        };
        if inner.stop_requested {
            // Treat a stop-induced exit as a graceful stop.
            inner.stop_requested = false;
            inner.stop_started_at = None;
            inner.force_terminate_at = None;
            inner.state.phase = SupervisionPhase::Stopped;
            inner.state.actual_active = None;
            exit_message = "worker stopped cleanly".to_string();
            let mut payload = self.build_payload(inner, SupervisionEventKind::WorkerStopped, now);
            payload.exit_code = status.code;
            payload.exit_signal = status.signal;
            payload.after_ready = Some(after_ready);
            payload.message = Some(exit_message);
            self.sink.emit(&payload);
            inner.next_sequence = inner.next_sequence.saturating_add(1);
            return Ok(TickOutcome::Continue);
        }
        let fault = SupervisionFault {
            class,
            error_code: ErrorCode::InferenceFailed,
            message: exit_message.clone(),
            launch_sequence: handle.launch_sequence,
            deployment_id: Some(handle.deployment_id.clone()),
        };
        self.record_fault(inner, &fault, now);
        let mut payload = self.build_payload(inner, SupervisionEventKind::WorkerExit, now);
        payload.exit_code = status.code;
        payload.exit_signal = status.signal;
        payload.after_ready = Some(after_ready);
        payload.failure_class = Some(class);
        payload.error_code = Some(fault.error_code);
        payload.message = Some(fault.message.clone());
        self.sink.emit(&payload);
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        self.schedule_after_failure(inner, class, now)?;
        Ok(TickOutcome::Fault(fault))
    }

    fn drive_stop(&self, inner: &mut Inner, now: Instant) -> AgentResult<TickOutcome> {
        let Some(handle) = inner.handle.clone() else {
            inner.stop_requested = false;
            inner.state.phase = SupervisionPhase::Stopped;
            return Ok(TickOutcome::Continue);
        };
        if inner.stop_started_at.is_none() {
            self.emit(
                inner,
                SupervisionEventKind::WorkerStopping,
                now,
                None,
                None,
                None,
                None,
                None,
            );
            self.process.graceful_stop(&handle)?;
            inner.stop_started_at = Some(now);
            inner.force_terminate_at = Some(
                now.checked_add(Duration::from_millis(self.cfg.graceful_stop_timeout_ms))
                    .unwrap_or(now),
            );
            inner.state.phase = SupervisionPhase::Stopping;
            return Ok(TickOutcome::Continue);
        }
        if let Some(deadline) = inner.force_terminate_at {
            if now >= deadline {
                self.process.force_terminate(&handle)?;
                inner.force_terminate_at = None;
            }
        }
        Ok(TickOutcome::Continue)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn schedule_after_failure(
        &self,
        inner: &mut Inner,
        class: FailureClass,
        now: Instant,
    ) -> AgentResult<TickOutcome> {
        let decision = inner.backoff.on_failure(now, class);
        inner.state.counters = inner.backoff.counters(now);
        match decision {
            BackoffDecision::Restart { at, delay } => {
                inner.state.phase = SupervisionPhase::AwaitingRestart;
                inner.state.next_restart_at = Some(at);
                let mut payload =
                    self.build_payload(inner, SupervisionEventKind::RestartScheduled, now);
                payload.next_restart_delay_ms =
                    Some(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
                self.sink.emit(&payload);
                inner.next_sequence = inner.next_sequence.saturating_add(1);
                Ok(TickOutcome::Continue)
            }
            BackoffDecision::EnterCrashLoop { reason } => {
                inner.state.phase = SupervisionPhase::CrashLoop;
                inner.state.next_restart_at = None;
                let mut payload =
                    self.build_payload(inner, SupervisionEventKind::CrashLoopEntered, now);
                payload.message = Some(reason);
                self.sink.emit(&payload);
                inner.next_sequence = inner.next_sequence.saturating_add(1);
                Ok(TickOutcome::Terminal(SupervisionPhase::CrashLoop))
            }
            BackoffDecision::PolicyDisabled => {
                inner.state.phase = SupervisionPhase::Stopped;
                inner.state.next_restart_at = None;
                Ok(TickOutcome::Continue)
            }
        }
    }

    #[allow(clippy::unused_self)]
    fn record_fault(&self, inner: &mut Inner, fault: &SupervisionFault, now: Instant) {
        inner.state.last_failure = Some(super::state::LastFailure {
            class: fault.class,
            error_code: fault.error_code,
            message: fault.message.clone(),
            at: now,
        });
        inner.pending_faults.push_back(fault.clone());
        // Bound the fault queue to avoid unbounded growth if no consumer
        // calls `drain_faults`.
        while inner.pending_faults.len() > 32 {
            inner.pending_faults.pop_front();
        }
    }

    #[allow(clippy::unused_self)]
    fn build_payload(
        &self,
        inner: &Inner,
        kind: SupervisionEventKind,
        now: Instant,
    ) -> SupervisionEventPayload {
        SupervisionEventPayload {
            kind,
            sequence: inner.next_sequence,
            timestamp: now,
            agent_state: inner.state.phase.to_agent_state(),
            serving_state: inner.state.phase.to_serving_state(),
            active_deployment: inner
                .desired
                .as_ref()
                .map(|d| d.deployment_id.clone())
                .or_else(|| inner.state.actual_active.clone())
                .unwrap_or_default(),
            backend: inner
                .desired
                .as_ref()
                .map(|d| d.backend.clone())
                .or_else(|| inner.state.backend.clone())
                .unwrap_or_default(),
            restart_count: inner.state.counters.rolling_count,
            next_restart_delay_ms: None,
            exit_code: None,
            exit_signal: None,
            after_ready: None,
            error_code: None,
            message: None,
            failure_class: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        inner: &mut Inner,
        kind: SupervisionEventKind,
        now: Instant,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
        after_ready: Option<bool>,
        error_code: Option<ErrorCode>,
        message: Option<String>,
    ) {
        let mut payload = self.build_payload(inner, kind, now);
        payload.exit_code = exit_code;
        payload.exit_signal = exit_signal;
        payload.after_ready = after_ready;
        payload.error_code = error_code;
        payload.message = message;
        self.sink.emit(&payload);
        inner.next_sequence = inner.next_sequence.saturating_add(1);
    }

    /// Anchor instant used by serialized supervision events.
    #[must_use]
    pub fn anchor(&self) -> Instant {
        self.anchor
    }

    /// True if the configured restart policy is disabled.
    #[must_use]
    pub fn restart_policy_is_disabled(&self) -> bool {
        matches!(self.cfg.restart_policy.kind, RestartPolicyKind::Disabled)
    }
}

/// Convert an [`AgentError`] to a typed [`SupervisionFault`] for tests
/// that need to drive the supervisor by hand without going through
/// `tick`. Production callers always observe faults through `tick`.
#[must_use]
pub fn fault_from_error(
    err: &AgentError,
    deployment_id: &str,
    launch_sequence: u64,
    class: FailureClass,
) -> SupervisionFault {
    SupervisionFault {
        class,
        error_code: err.code(),
        message: err.to_string(),
        launch_sequence,
        deployment_id: Some(deployment_id.to_string()),
    }
}

/// Test helper: convert a fault's wire-stable agent state name. Useful
/// for assertions in V01-E09-F07 tests.
#[must_use]
pub fn fault_agent_state(_fault: &SupervisionFault) -> SupervisionAgentState {
    // Faults imply the agent is degraded or worse; the exact mapping is
    // driven by phase, not the fault itself.
    SupervisionAgentState::Degraded
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::default_trait_access
    )]

    use super::super::clock::FakeClock;
    use super::super::config::{
        BackoffConfig, EventSinkConfig, RestartPolicy, RestartPolicyKind, SupervisorConfig,
        WorkerStdioMode,
    };
    use super::super::event::RingEventSink;
    use super::super::process::{ExitStatus, MockProcessBehavior, MockWorkerProcess};
    use super::super::readiness::{MockReadinessProbe, ReadinessSample};
    use super::{DesiredWorker, SupervisionEventKind, TickOutcome, WorkerSupervisor};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    fn supervisor_config() -> SupervisorConfig {
        SupervisorConfig {
            binary_path: PathBuf::from("/usr/local/bin/tensorplate-serving"),
            args: vec![],
            env_allowlist: BTreeSet::new(),
            working_dir: PathBuf::from("/var/lib/tensorplate"),
            serving_config_path: PathBuf::from("/var/lib/tensorplate/serving.json"),
            control_host: "127.0.0.1".into(),
            control_port: 18080,
            stdio_mode: WorkerStdioMode::Inherit,
            startup_timeout_ms: 50,
            graceful_stop_timeout_ms: 10,
            kill_timeout_ms: 10,
            status_poll_interval_ms: 5,
            restart_policy: RestartPolicy {
                kind: RestartPolicyKind::BoundedBackoff,
                backoff: BackoffConfig {
                    initial_delay_ms: 5,
                    multiplier_hundredths: 200,
                    max_delay_ms: 80,
                    window_ms: 10_000,
                    threshold: 3,
                    stable_reset_ms: 5_000,
                },
            },
            event_sink: EventSinkConfig::default(),
        }
    }

    fn build_supervisor(
        process: Arc<MockWorkerProcess>,
        probe: Arc<MockReadinessProbe>,
        clock: Arc<FakeClock>,
        sink: Arc<RingEventSink>,
    ) -> WorkerSupervisor {
        WorkerSupervisor::new(supervisor_config(), process, probe, clock as Arc<_>)
            .expect("supervisor")
            .with_event_sink(sink as Arc<_>)
    }

    fn ready_sample(id: &str) -> ReadinessSample {
        ReadinessSample {
            ready: true,
            active_deployment: Some(id.to_string()),
            backend: Some("mock".into()),
            queue_depth: Some(0),
            ..ReadinessSample::unknown()
        }
    }

    #[test]
    fn happy_path_starts_and_reaches_ready() {
        let process = Arc::new(MockWorkerProcess::new());
        let probe = Arc::new(MockReadinessProbe::new());
        probe.script(vec![ready_sample("d-1")]);
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        supervisor
            .set_desired_active(Some(DesiredWorker {
                deployment_id: "d-1".into(),
                backend: "mock".into(),
            }))
            .expect("set desired");
        // Tick 1: launch.
        assert!(matches!(
            supervisor.tick().expect("tick1"),
            TickOutcome::Continue
        ));
        // Tick 2: probe sees ready.
        assert!(matches!(
            supervisor.tick().expect("tick2"),
            TickOutcome::Continue
        ));
        let status = supervisor.status();
        assert!(matches!(
            status.serving_state,
            tensorplate_protocol::supervision_event::SupervisionServingState::Ready
        ));
        let kinds: Vec<_> = sink.drain().into_iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&SupervisionEventKind::WorkerStarted));
        assert!(kinds.contains(&SupervisionEventKind::WorkerReady));
    }

    #[test]
    fn exit_before_ready_schedules_restart() {
        let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
            exit_at_poll: Some(1),
            exit_code: Some(2),
            ..Default::default()
        }));
        let probe = Arc::new(MockReadinessProbe::new());
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        supervisor
            .set_desired_active(Some(DesiredWorker {
                deployment_id: "d-1".into(),
                backend: "mock".into(),
            }))
            .expect("set desired");
        // Tick 1: launch
        let _ = supervisor.tick().expect("tick1");
        // Tick 2: poll detects exit -> Fault, schedules restart
        let outcome = supervisor.tick().expect("tick2");
        match outcome {
            TickOutcome::Fault(f) => {
                assert_eq!(f.class, super::super::policy::FailureClass::ExitBeforeReady)
            }
            other => panic!("expected fault, got {other:?}"),
        }
        let status = supervisor.status();
        assert!(matches!(
            status.serving_state,
            tensorplate_protocol::supervision_event::SupervisionServingState::AwaitingRestart
        ));
        assert!(status.next_restart_delay_ms.unwrap_or(0) > 0);
    }

    #[test]
    fn repeated_crashes_enter_crash_loop() {
        let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
            exit_at_poll: Some(1),
            exit_code: Some(2),
            ..Default::default()
        }));
        let probe = Arc::new(MockReadinessProbe::new());
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        supervisor
            .set_desired_active(Some(DesiredWorker {
                deployment_id: "d-1".into(),
                backend: "mock".into(),
            }))
            .expect("set desired");
        for _ in 0..6 {
            let _ = supervisor.tick().expect("tick");
            clock.advance(Duration::from_millis(100));
        }
        let status = supervisor.status();
        assert!(status.crash_loop);
        assert!(matches!(
            status.serving_state,
            tensorplate_protocol::supervision_event::SupervisionServingState::CrashLoop
        ));
        let drained = sink.drain();
        assert!(drained
            .iter()
            .any(|p| matches!(p.kind, SupervisionEventKind::CrashLoopEntered)));
    }

    #[test]
    fn recover_after_operator_action_clears_crash_loop() {
        let process = Arc::new(MockWorkerProcess::with_behavior(MockProcessBehavior {
            exit_at_poll: Some(1),
            exit_code: Some(2),
            ..Default::default()
        }));
        let probe = Arc::new(MockReadinessProbe::new());
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        supervisor
            .set_desired_active(Some(DesiredWorker {
                deployment_id: "d-1".into(),
                backend: "mock".into(),
            }))
            .expect("set desired");
        for _ in 0..6 {
            let _ = supervisor.tick().expect("tick");
            clock.advance(Duration::from_millis(100));
        }
        assert!(supervisor.status().crash_loop);
        supervisor.recover_after_operator_action().expect("recover");
        let status = supervisor.status();
        assert!(!status.crash_loop);
    }

    #[test]
    fn graceful_stop_transitions_through_stopping() {
        let process = Arc::new(MockWorkerProcess::new());
        let probe = Arc::new(MockReadinessProbe::new());
        probe.script(vec![ready_sample("d-1")]);
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        supervisor
            .set_desired_active(Some(DesiredWorker {
                deployment_id: "d-1".into(),
                backend: "mock".into(),
            }))
            .expect("set desired");
        // Reach ready.
        let _ = supervisor.tick().expect("t1");
        let _ = supervisor.tick().expect("t2");
        supervisor.request_stop().expect("stop");
        // Tick advances stop sequence and graceful_stop returns; next
        // poll detects exit and transitions to Stopped.
        let _ = supervisor.tick().expect("t3");
        let _ = supervisor.tick().expect("t4");
        let status = supervisor.status();
        assert!(matches!(
            status.serving_state,
            tensorplate_protocol::supervision_event::SupervisionServingState::Stopped
        ));
        let kinds: Vec<_> = sink.drain().into_iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&SupervisionEventKind::WorkerStopping));
        assert!(kinds.contains(&SupervisionEventKind::WorkerStopped));
    }

    #[test]
    fn not_ready_timeout_finalizes_unready_and_restarts() {
        let process = Arc::new(MockWorkerProcess::new());
        let probe = Arc::new(MockReadinessProbe::new());
        // Persistently not ready.
        probe.script(vec![ReadinessSample::unknown()]);
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        supervisor
            .set_desired_active(Some(DesiredWorker {
                deployment_id: "d-1".into(),
                backend: "mock".into(),
            }))
            .expect("set desired");
        let _ = supervisor.tick().expect("launch"); // launch
                                                    // Push past the startup window (50ms).
        clock.advance(Duration::from_millis(60));
        let _ = supervisor.tick().expect("readiness expire");
        let status = supervisor.status();
        // Not-ready timeout transitions to AwaitingRestart, with at
        // least one failure recorded.
        assert!(status.restart_count >= 1);
        assert!(matches!(
            status.serving_state,
            tensorplate_protocol::supervision_event::SupervisionServingState::AwaitingRestart
                | tensorplate_protocol::supervision_event::SupervisionServingState::Starting
        ));
    }

    #[test]
    fn no_desired_active_keeps_supervisor_idle() {
        let process = Arc::new(MockWorkerProcess::new());
        let probe = Arc::new(MockReadinessProbe::new());
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        let _ = supervisor.tick().expect("tick");
        let status = supervisor.status();
        assert!(matches!(
            status.serving_state,
            tensorplate_protocol::supervision_event::SupervisionServingState::NoActiveDeployment
        ));
        assert!(sink.drain().is_empty());
    }

    #[test]
    fn exit_after_ready_records_after_ready_flag() {
        let process = Arc::new(MockWorkerProcess::new());
        let probe = Arc::new(MockReadinessProbe::new());
        probe.script(vec![ready_sample("d-1")]);
        let clock = Arc::new(FakeClock::new());
        let sink = Arc::new(RingEventSink::new(&EventSinkConfig::default()));
        let supervisor =
            build_supervisor(process.clone(), probe.clone(), clock.clone(), sink.clone());
        supervisor
            .set_desired_active(Some(DesiredWorker {
                deployment_id: "d-1".into(),
                backend: "mock".into(),
            }))
            .expect("set desired");
        let _ = supervisor.tick().expect("launch");
        let _ = supervisor.tick().expect("ready");
        // Now have the worker exit after ready.
        let handles_seq = supervisor.status().launch_sequence;
        assert!(handles_seq > 0);
        process.exit_now(ExitStatus {
            code: Some(1),
            signal: None,
            after_ready: true,
        });
        match supervisor.tick().expect("post-exit tick") {
            TickOutcome::Fault(f) => {
                assert_eq!(f.class, super::super::policy::FailureClass::ExitAfterReady)
            }
            other => panic!("expected fault, got {other:?}"),
        }
    }
}
