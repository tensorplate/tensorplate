// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F04: Supervision state + status projection.
//
// The supervisor's authoritative state lives here. `SupervisionPhase`
// uses the names called out by the planning document and the
// V01-E11 status command; both are stable wire identifiers.
//
// The supervision state is *agent-local*: it is not persisted in the
// V01-E08 durable store because crash-loop counters MUST use monotonic
// time and a process restart is a natural reset. The desired active
// deployment, by contrast, is sourced from `StateStore`.

use std::time::Instant;

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::supervision_event::{SupervisionAgentState, SupervisionServingState};

use super::policy::{FailureClass, RestartCounters};

/// Supervision-phase enum. The string form is the public stable name
/// observed by V01-E11 status and the V01-E10 observability service.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SupervisionPhase {
    NoActiveDeployment,
    Starting,
    Running,
    Ready,
    Degraded,
    Failed,
    Stopping,
    Stopped,
    AwaitingRestart,
    CrashLoop,
}

impl SupervisionPhase {
    /// Project to the wire-stable serving-state name shared with the
    /// V01-E10 observability service and the V01-E11 CLI.
    #[must_use]
    pub fn to_serving_state(self) -> SupervisionServingState {
        match self {
            Self::NoActiveDeployment => SupervisionServingState::NoActiveDeployment,
            Self::Starting => SupervisionServingState::Starting,
            Self::Running => SupervisionServingState::Running,
            Self::Ready => SupervisionServingState::Ready,
            Self::Degraded => SupervisionServingState::Degraded,
            Self::Failed => SupervisionServingState::Failed,
            Self::Stopping => SupervisionServingState::Stopping,
            Self::Stopped => SupervisionServingState::Stopped,
            Self::AwaitingRestart => SupervisionServingState::AwaitingRestart,
            Self::CrashLoop => SupervisionServingState::CrashLoop,
        }
    }

    /// Aggregate the agent-level run state implied by this supervision
    /// phase. The agent process itself is `ready` when the worker is at
    /// `ready` and `degraded`/`failed` only when the supervisor cannot
    /// keep the desired worker running.
    #[must_use]
    pub fn to_agent_state(self) -> SupervisionAgentState {
        match self {
            Self::Ready | Self::NoActiveDeployment | Self::Stopped => SupervisionAgentState::Ready,
            Self::Starting | Self::Running | Self::Stopping | Self::AwaitingRestart => {
                SupervisionAgentState::Ready
            }
            Self::Degraded => SupervisionAgentState::Degraded,
            Self::Failed | Self::CrashLoop => SupervisionAgentState::Failed,
        }
    }

    /// True when the phase is terminal — the supervisor will not move
    /// out of it without operator action.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::CrashLoop)
    }
}

/// Reason a supervision launch ended. Used by status projection and
/// supervision events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LastFailure {
    pub class: FailureClass,
    pub error_code: ErrorCode,
    pub message: String,
    pub at: Instant,
}

/// Agent-local supervision state. The supervisor mutates this under its
/// own mutex; the status server consults the projected
/// [`SupervisionStatus`] (which is `Send + Clone`).
#[derive(Clone, Debug)]
pub struct SupervisionState {
    pub phase: SupervisionPhase,
    pub desired_active: Option<String>,
    pub actual_active: Option<String>,
    pub backend: Option<String>,
    pub launch_sequence: u64,
    pub last_ready_at: Option<Instant>,
    pub last_failure: Option<LastFailure>,
    pub next_restart_at: Option<Instant>,
    pub counters: RestartCounters,
}

impl SupervisionState {
    /// Build the initial state for a fresh supervisor. The phase is
    /// `NoActiveDeployment` until [`SupervisionState::set_desired_active`]
    /// installs one.
    #[must_use]
    pub fn fresh() -> Self {
        Self {
            phase: SupervisionPhase::NoActiveDeployment,
            desired_active: None,
            actual_active: None,
            backend: None,
            launch_sequence: 0,
            last_ready_at: None,
            last_failure: None,
            next_restart_at: None,
            counters: RestartCounters::default(),
        }
    }

    /// Install a new desired active deployment. The supervisor transitions
    /// the phase to `Starting` if the worker is not already serving it.
    #[allow(clippy::needless_pass_by_value)]
    pub fn set_desired_active(&mut self, deployment_id: Option<String>, backend: Option<String>) {
        self.desired_active.clone_from(&deployment_id);
        self.backend = backend;
        if deployment_id.is_none() {
            self.phase = SupervisionPhase::NoActiveDeployment;
        } else if matches!(self.phase, SupervisionPhase::NoActiveDeployment) {
            self.phase = SupervisionPhase::Starting;
        }
    }

    /// Convenience: project a status snapshot suitable for the V01-E11
    /// status command. Tests assert against this; the agent's
    /// `Coordinator::status` glues it onto the `AgentStatus` projection.
    #[must_use]
    pub fn status(&self, now: Instant) -> SupervisionStatus {
        let stable_uptime_ms = self.last_ready_at.map_or(0, |t| {
            u64::try_from(now.saturating_duration_since(t).as_millis()).unwrap_or(u64::MAX)
        });
        SupervisionStatus {
            serving_state: self.phase.to_serving_state(),
            agent_state: self.phase.to_agent_state(),
            desired_active: self.desired_active.clone(),
            actual_active: self.actual_active.clone(),
            backend: self.backend.clone(),
            restart_count: self.counters.rolling_count,
            crash_loop_threshold: self.counters.threshold,
            crash_loop: matches!(self.phase, SupervisionPhase::CrashLoop),
            launch_sequence: self.launch_sequence,
            last_failure_code: self.last_failure.as_ref().map(|f| f.error_code),
            last_failure_message: self.last_failure.as_ref().map(|f| f.message.clone()),
            next_restart_delay_ms: self
                .next_restart_at
                .map(|t| u64::try_from(t.saturating_duration_since(now).as_millis()).unwrap_or(0)),
            stable_uptime_ms,
        }
    }
}

/// Public status projection consumed by `AgentStatus`, the V01-E11 CLI,
/// and the V01-E10 observability service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisionStatus {
    pub serving_state: SupervisionServingState,
    pub agent_state: SupervisionAgentState,
    pub desired_active: Option<String>,
    pub actual_active: Option<String>,
    pub backend: Option<String>,
    pub restart_count: u32,
    pub crash_loop_threshold: u32,
    pub crash_loop: bool,
    pub launch_sequence: u64,
    pub last_failure_code: Option<ErrorCode>,
    pub last_failure_message: Option<String>,
    pub next_restart_delay_ms: Option<u64>,
    pub stable_uptime_ms: u64,
}

impl SupervisionStatus {
    /// Status equivalent of "no supervision configured yet".
    #[must_use]
    pub fn no_active_deployment() -> Self {
        Self {
            serving_state: SupervisionServingState::NoActiveDeployment,
            agent_state: SupervisionAgentState::Ready,
            desired_active: None,
            actual_active: None,
            backend: None,
            restart_count: 0,
            crash_loop_threshold: 0,
            crash_loop: false,
            launch_sequence: 0,
            last_failure_code: None,
            last_failure_message: None,
            next_restart_delay_ms: None,
            stable_uptime_ms: 0,
        }
    }
}

/// Result of reconciling durable desired state against actual worker
/// state on startup (V01-E09-F04-T02).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisionReconcileAction {
    /// No desired active deployment; supervisor stays idle.
    Idle,
    /// Desired deployment matches actual worker deployment; just monitor.
    MonitorRunning,
    /// Desired deployment exists but no worker is running; supervisor
    /// must launch one.
    Launch,
    /// Worker is running a different deployment than desired; the
    /// supervisor reports the mismatch and asks the operator to take
    /// action (typically rollback or deploy).
    Mismatch { desired: String, actual: String },
    /// Crash-loop state recorded from a previous process; the next
    /// supervisor must wait for an operator-triggered recovery.
    HoldForOperator,
}

/// Plan the action a freshly-started supervisor should take.
#[must_use]
pub fn plan_reconcile(
    desired: Option<&str>,
    actual: Option<&str>,
    last_phase: SupervisionPhase,
) -> SupervisionReconcileAction {
    if last_phase == SupervisionPhase::CrashLoop {
        return SupervisionReconcileAction::HoldForOperator;
    }
    match (desired, actual) {
        (None, _) => SupervisionReconcileAction::Idle,
        (Some(want), Some(have)) if want == have => SupervisionReconcileAction::MonitorRunning,
        (Some(_), None) => SupervisionReconcileAction::Launch,
        (Some(want), Some(have)) => SupervisionReconcileAction::Mismatch {
            desired: want.to_string(),
            actual: have.to_string(),
        },
    }
}

/// Helper: convert a backoff scheduling instant + clock to a millisecond
/// delay for status projection.
#[must_use]
pub fn pending_delay_ms(now: Instant, scheduled: Option<Instant>) -> Option<u64> {
    scheduled.map(|t| u64::try_from(t.saturating_duration_since(now).as_millis()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::{
        plan_reconcile, SupervisionPhase, SupervisionReconcileAction, SupervisionServingState,
        SupervisionState,
    };

    #[test]
    fn fresh_supervision_state_is_idle() {
        let s = SupervisionState::fresh();
        assert_eq!(s.phase, SupervisionPhase::NoActiveDeployment);
        let status = s.status(std::time::Instant::now());
        assert_eq!(
            status.serving_state,
            SupervisionServingState::NoActiveDeployment
        );
    }

    #[test]
    fn set_desired_active_transitions_to_starting() {
        let mut s = SupervisionState::fresh();
        s.set_desired_active(Some("d-1".into()), Some("mock".into()));
        assert_eq!(s.phase, SupervisionPhase::Starting);
        assert_eq!(s.desired_active.as_deref(), Some("d-1"));
    }

    #[test]
    fn clear_desired_active_goes_idle() {
        let mut s = SupervisionState::fresh();
        s.set_desired_active(Some("d-1".into()), Some("mock".into()));
        s.set_desired_active(None, None);
        assert_eq!(s.phase, SupervisionPhase::NoActiveDeployment);
    }

    #[test]
    fn reconcile_no_desired_is_idle() {
        let action = plan_reconcile(None, Some("d-other"), SupervisionPhase::Stopped);
        assert!(matches!(action, SupervisionReconcileAction::Idle));
    }

    #[test]
    fn reconcile_match_is_monitor() {
        let action = plan_reconcile(Some("d-1"), Some("d-1"), SupervisionPhase::Ready);
        assert!(matches!(action, SupervisionReconcileAction::MonitorRunning));
    }

    #[test]
    fn reconcile_no_worker_is_launch() {
        let action = plan_reconcile(Some("d-1"), None, SupervisionPhase::Stopped);
        assert!(matches!(action, SupervisionReconcileAction::Launch));
    }

    #[test]
    fn reconcile_mismatch_records_both() {
        let action = plan_reconcile(Some("d-1"), Some("d-2"), SupervisionPhase::Stopped);
        match action {
            SupervisionReconcileAction::Mismatch { desired, actual } => {
                assert_eq!(desired, "d-1");
                assert_eq!(actual, "d-2");
            }
            other => panic!("expected mismatch, got {other:?}"),
        }
    }

    #[test]
    fn reconcile_after_crash_loop_holds_for_operator() {
        let action = plan_reconcile(Some("d-1"), None, SupervisionPhase::CrashLoop);
        assert!(matches!(
            action,
            SupervisionReconcileAction::HoldForOperator
        ));
    }
}
