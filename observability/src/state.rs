// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F04: Health state aggregation, transition table, and local
// safe-state event.
//
// The aggregator consumes the heartbeat evaluator's per-source state
// and the recent normalised inputs from the listener to produce one
// observability state plus, when the state changes or the configured
// periodic interval elapses, a [`SafeStateEvent`] for the local sink.
//
// The transition table is intentionally narrow:
//
//   * `no-heartbeat` is the highest-precedence state because it is the
//     "wedged worker" signal V01-E10 must surface without depending on
//     the agent.
//   * `failed` is next: explicit failed signals from the serving worker
//     or supervisor mean the worker is down even if a heartbeat is
//     still flowing.
//   * `degraded` covers backlog / overload / not-ready / restart-loop.
//   * `ready` is everything else.

use std::sync::Mutex;
use std::time::{Duration, Instant};

fn duration_to_u64_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

use tensorplate_protocol::error::ErrorCode;
use tensorplate_protocol::worker_status::ComponentState;

use crate::config::HeartbeatPolicy;
use crate::heartbeat::{HeartbeatHealth, SourceState};
use crate::listener::{HealthInput, InputKind, InputSource};

/// Discrete observability state surfaced by the service. Maps directly
/// to the ROS 2 `diagnostic_msgs::DiagnosticStatus` level required by
/// V01-E10-F05.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum ObservabilityState {
    #[default]
    Ready,
    Degraded,
    Failed,
    NoHeartbeat,
}

impl ObservabilityState {
    /// Stable wire name used by snapshots, ROS 2 key-values, and the
    /// safe-state event payload.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ObservabilityState::Ready => "ready",
            ObservabilityState::Degraded => "degraded",
            ObservabilityState::Failed => "failed",
            ObservabilityState::NoHeartbeat => "no_heartbeat",
        }
    }
}

/// Local safe-state event. Emitted on transitions into degraded,
/// failed, or no-heartbeat, and optionally on a configured periodic
/// interval so a downstream consumer that started after the transition
/// can still see the current state.
#[derive(Clone, Debug, PartialEq)]
pub struct SafeStateEvent {
    pub state: ObservabilityState,
    pub previous_state: ObservabilityState,
    pub reason: SafeStateReason,
    pub agent_state: ComponentState,
    pub serving_state: ComponentState,
    pub active_deployment: String,
    pub backend: String,
    pub missed_heartbeat_count: u64,
    pub missed_deadline_rate: f64,
    pub queue_depth: u64,
    pub last_error_code: Option<ErrorCode>,
    pub monotonic_age_ms: u64,
}

/// Discrete reason for the safe-state event. Lets downstream consumers
/// filter without parsing free-text messages.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum SafeStateReason {
    #[default]
    Transition,
    Periodic,
    HeartbeatMissing,
    ServingFailed,
    ServingDegraded,
    CrashLoop,
    WorkerExit,
    Overload,
    Recovery,
}

impl SafeStateReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SafeStateReason::Transition => "transition",
            SafeStateReason::Periodic => "periodic",
            SafeStateReason::HeartbeatMissing => "heartbeat_missing",
            SafeStateReason::ServingFailed => "serving_failed",
            SafeStateReason::ServingDegraded => "serving_degraded",
            SafeStateReason::CrashLoop => "crash_loop",
            SafeStateReason::WorkerExit => "worker_exit",
            SafeStateReason::Overload => "overload",
            SafeStateReason::Recovery => "recovery",
        }
    }
}

/// Aggregated state shared with the snapshot writer and the ROS 2
/// publisher. The aggregator updates this in place on every input;
/// callers read it without locking the listener.
#[derive(Clone, Debug, PartialEq)]
pub struct AggregateState {
    pub state: ObservabilityState,
    pub previous_state: ObservabilityState,
    pub agent_state: ComponentState,
    pub serving_state: ComponentState,
    pub active_deployment: String,
    pub backend: String,
    pub missed_heartbeat_count: u64,
    pub missed_deadline_rate: f64,
    pub queue_depth: u64,
    pub last_error_code: Option<ErrorCode>,
    pub last_transition_at: Option<Instant>,
    pub last_event_sequence: Option<u64>,
    pub last_heartbeat_age_ms: Option<u64>,
    pub last_periodic_emit_at: Option<Instant>,
}

impl Default for AggregateState {
    fn default() -> Self {
        Self {
            state: ObservabilityState::Ready,
            previous_state: ObservabilityState::Ready,
            agent_state: ComponentState::Unknown,
            serving_state: ComponentState::Unknown,
            active_deployment: String::new(),
            backend: String::new(),
            missed_heartbeat_count: 0,
            missed_deadline_rate: 0.0,
            queue_depth: 0,
            last_error_code: None,
            last_transition_at: None,
            last_event_sequence: None,
            last_heartbeat_age_ms: None,
            last_periodic_emit_at: None,
        }
    }
}

/// Aggregator. Composition root constructs exactly one and threads it
/// to the listener consumer, the snapshot writer, and the ROS 2
/// publisher.
pub struct Aggregator {
    heartbeat_policy: HeartbeatPolicy,
    primary_source: InputSource,
    state: Mutex<AggregateState>,
}

impl Aggregator {
    #[must_use]
    pub fn new(heartbeat_policy: HeartbeatPolicy, primary_source: InputSource) -> Self {
        Self {
            heartbeat_policy,
            primary_source,
            state: Mutex::new(AggregateState::default()),
        }
    }

    /// Public view used by the snapshot writer and the ROS 2
    /// publisher.
    pub fn current(&self) -> AggregateState {
        #[allow(clippy::expect_used)]
        self.state
            .lock()
            .expect("aggregator state poisoned")
            .clone()
    }

    /// Replace the aggregator state outright. Used by tests; production
    /// code mutates via [`apply_inputs`].
    #[cfg(test)]
    pub fn replace(&self, next: AggregateState) {
        #[allow(clippy::expect_used)]
        let mut guard = self.state.lock().expect("aggregator state poisoned");
        *guard = next;
    }

    /// Ingest a batch of normalised inputs together with the current
    /// heartbeat-evaluator output, then update the state and return any
    /// safe-state events that should be emitted.
    pub fn apply(
        &self,
        inputs: &[HealthInput],
        heartbeat: &[(InputSource, SourceState)],
        now: Instant,
        periodic_interval: Option<Duration>,
    ) -> Vec<SafeStateEvent> {
        #[allow(clippy::expect_used)]
        let mut guard = self.state.lock().expect("aggregator state poisoned");

        let mut events = Vec::new();
        let explicit_no_heartbeat = inputs
            .iter()
            .any(|input| matches!(input.kind, InputKind::NoHeartbeat));

        // 1. Fold inputs into the running aggregate. The listener
        //    drained inputs in arrival order so the latest serving /
        //    agent state wins.
        for input in inputs {
            absorb_input(&mut guard, input);
        }

        // 2. Update heartbeat-derived bookkeeping from the evaluator.
        let primary = heartbeat
            .iter()
            .find(|(s, _)| *s == self.primary_source)
            .map(|(_, st)| st);
        let primary_health = primary.map(|s| s.health).unwrap_or_default();
        guard.missed_heartbeat_count = u64::from(primary.map_or(0, |s| s.missed_count));
        guard.last_heartbeat_age_ms = primary.and_then(|s| {
            s.last_heartbeat_at
                .map(|h| duration_to_u64_ms(now.saturating_duration_since(h)))
        });

        // 3. Apply the precedence table.
        let next = compute_state(&guard, primary_health, explicit_no_heartbeat);
        if next != guard.state {
            let reason = transition_reason(next, primary_health, &guard);
            let event = build_event(&guard, next, reason, now);
            guard.previous_state = guard.state;
            guard.state = next;
            guard.last_transition_at = Some(now);
            events.push(event);
        }

        // 4. Periodic emission for non-ready states. The aggregator
        //    surfaces the *current* state again so consumers that
        //    started late can synchronise. We skip the periodic emit
        //    when a transition already fired during this tick so a
        //    single tick never produces two events for the same state.
        if let Some(interval) = periodic_interval {
            let due = guard
                .last_periodic_emit_at
                .map_or(true, |t| now.saturating_duration_since(t) >= interval);
            if due && events.is_empty() && !matches!(guard.state, ObservabilityState::Ready) {
                let event = build_event(&guard, guard.state, SafeStateReason::Periodic, now);
                guard.last_periodic_emit_at = Some(now);
                events.push(event);
            }
        }

        // 5. Bound the missed-heartbeat counter to a multiple of the
        //    threshold so retained snapshots don't keep counting up.
        let cap = u64::from(self.heartbeat_policy.missed_threshold).saturating_mul(4);
        if guard.missed_heartbeat_count > cap {
            guard.missed_heartbeat_count = cap;
        }

        events
    }
}

fn absorb_input(state: &mut AggregateState, input: &HealthInput) {
    if let Some(agent) = input.agent_state {
        state.agent_state = agent;
    }
    if let Some(serving) = input.serving_state {
        state.serving_state = serving;
    }
    if !input.active_deployment.is_empty() {
        state.active_deployment.clone_from(&input.active_deployment);
    }
    if !input.backend.is_empty() {
        state.backend.clone_from(&input.backend);
    }
    if let Some(queue_depth) = input.queue_depth {
        state.queue_depth = queue_depth;
    }
    if let Some(missed_deadline_rate) = input.missed_deadline_rate {
        state.missed_deadline_rate = missed_deadline_rate;
    }
    if let Some(code) = input.error_code {
        state.last_error_code = Some(code);
    }
    if let Some(seq) = input.sequence {
        state.last_event_sequence = Some(seq);
    }
    // Recover from explicit `ready` / `recovery` signals: the
    // serving_state is already updated above; we just clear stale
    // failure context so the aggregator can transition back to ready.
    match input.kind {
        InputKind::Ready | InputKind::Recovery => {
            if matches!(state.serving_state, ComponentState::Ready) {
                state.last_error_code = None;
            }
        }
        InputKind::NoHeartbeat | InputKind::Heartbeat => {}
        InputKind::CrashLoop | InputKind::WorkerExit | InputKind::Failed => {
            state.serving_state = ComponentState::Failed;
        }
        InputKind::WorkerNotReady => {
            state.serving_state = ComponentState::Degraded;
        }
        InputKind::Degraded | InputKind::Overload | InputKind::MissedDeadline => {
            if matches!(
                state.serving_state,
                ComponentState::Ready | ComponentState::Unknown
            ) {
                state.serving_state = ComponentState::Degraded;
            }
        }
    }
}

fn compute_state(
    state: &AggregateState,
    heartbeat: HeartbeatHealth,
    explicit_no_heartbeat: bool,
) -> ObservabilityState {
    if explicit_no_heartbeat || matches!(heartbeat, HeartbeatHealth::NoHeartbeat) {
        return ObservabilityState::NoHeartbeat;
    }
    if matches!(state.serving_state, ComponentState::Failed)
        || matches!(state.agent_state, ComponentState::Failed)
    {
        return ObservabilityState::Failed;
    }
    if matches!(state.serving_state, ComponentState::Degraded)
        || matches!(state.agent_state, ComponentState::Degraded)
        || matches!(heartbeat, HeartbeatHealth::Stale)
    {
        return ObservabilityState::Degraded;
    }
    if matches!(heartbeat, HeartbeatHealth::NoneYet)
        && !matches!(state.serving_state, ComponentState::Ready)
    {
        return ObservabilityState::Degraded;
    }
    ObservabilityState::Ready
}

fn transition_reason(
    next: ObservabilityState,
    heartbeat: HeartbeatHealth,
    state: &AggregateState,
) -> SafeStateReason {
    match next {
        ObservabilityState::NoHeartbeat => SafeStateReason::HeartbeatMissing,
        ObservabilityState::Failed => {
            if matches!(state.last_error_code, Some(ErrorCode::Internal)) {
                SafeStateReason::CrashLoop
            } else if matches!(heartbeat, HeartbeatHealth::NoHeartbeat) {
                SafeStateReason::HeartbeatMissing
            } else {
                SafeStateReason::ServingFailed
            }
        }
        ObservabilityState::Degraded => {
            if state.missed_deadline_rate > 0.0 {
                SafeStateReason::Overload
            } else if matches!(heartbeat, HeartbeatHealth::Stale) {
                SafeStateReason::HeartbeatMissing
            } else {
                SafeStateReason::ServingDegraded
            }
        }
        ObservabilityState::Ready => SafeStateReason::Recovery,
    }
}

fn build_event(
    state: &AggregateState,
    next: ObservabilityState,
    reason: SafeStateReason,
    now: Instant,
) -> SafeStateEvent {
    let monotonic_age_ms = state
        .last_transition_at
        .map_or(0, |t| duration_to_u64_ms(now.saturating_duration_since(t)));
    SafeStateEvent {
        state: next,
        previous_state: state.state,
        reason,
        agent_state: state.agent_state,
        serving_state: state.serving_state,
        active_deployment: state.active_deployment.clone(),
        backend: state.backend.clone(),
        missed_heartbeat_count: state.missed_heartbeat_count,
        missed_deadline_rate: state.missed_deadline_rate,
        queue_depth: state.queue_depth,
        last_error_code: state.last_error_code,
        monotonic_age_ms,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Aggregator, ObservabilityState, SafeStateReason};
    use crate::config::HeartbeatPolicy;
    use crate::heartbeat::{HeartbeatHealth, SourceState};
    use crate::listener::{HealthInput, InputKind, InputSource};
    use std::time::Instant;
    use tensorplate_protocol::error::ErrorCode;
    use tensorplate_protocol::worker_status::ComponentState;

    fn policy() -> HeartbeatPolicy {
        HeartbeatPolicy {
            expected_interval_ms: 100,
            grace_ms: 25,
            missed_threshold: 3,
            recovery_heartbeats: 1,
        }
    }

    fn source_state(health: HeartbeatHealth, missed: u32, now: Instant) -> SourceState {
        SourceState {
            registered_at: now,
            last_heartbeat_at: Some(now),
            missed_count: missed,
            consecutive_recovery_heartbeats: 0,
            health,
        }
    }

    #[test]
    fn ready_after_first_heartbeat() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let inputs = vec![HealthInput::heartbeat(InputSource::ServingWorker, now)];
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::Fresh, 0, now),
        )];
        let events = agg.apply(&inputs, &hb, now, None);
        // First transition (Ready -> Ready) emits nothing because the
        // aggregator starts in Ready already.
        assert!(events.is_empty());
        assert_eq!(agg.current().state, ObservabilityState::Ready);
    }

    #[test]
    fn no_heartbeat_state_emits_safe_state_event() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::NoHeartbeat, 5, now),
        )];
        let events = agg.apply(&[], &hb, now, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::NoHeartbeat);
        assert_eq!(events[0].reason, SafeStateReason::HeartbeatMissing);
        assert_eq!(events[0].missed_heartbeat_count, 5);
    }

    #[test]
    fn explicit_no_heartbeat_input_emits_no_heartbeat_state() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let mut input = HealthInput::heartbeat(InputSource::ServingWorker, now);
        input.kind = InputKind::NoHeartbeat;
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::Fresh, 0, now),
        )];
        let events = agg.apply(&[input], &hb, now, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::NoHeartbeat);
        assert_eq!(events[0].reason, SafeStateReason::HeartbeatMissing);
    }

    #[test]
    fn failed_state_takes_precedence_over_degraded() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let mut failed = HealthInput::heartbeat(InputSource::ServingWorker, now);
        failed.kind = InputKind::Failed;
        let mut degraded = HealthInput::heartbeat(InputSource::ServingWorker, now);
        degraded.kind = InputKind::Degraded;
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::Fresh, 0, now),
        )];
        let events = agg.apply(&[degraded, failed], &hb, now, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::Failed);
    }

    #[test]
    fn crash_loop_input_drives_failed_with_crash_loop_reason_when_error_internal() {
        let agg = Aggregator::new(policy(), InputSource::AgentSupervisor);
        let now = Instant::now();
        let mut crash = HealthInput::heartbeat(InputSource::AgentSupervisor, now);
        crash.kind = InputKind::CrashLoop;
        crash.error_code = Some(ErrorCode::Internal);
        let hb = vec![(
            InputSource::AgentSupervisor,
            source_state(HeartbeatHealth::Fresh, 0, now),
        )];
        let events = agg.apply(&[crash], &hb, now, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::Failed);
        assert_eq!(events[0].reason, SafeStateReason::CrashLoop);
    }

    #[test]
    fn degraded_state_emits_event_and_clears_on_recovery() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let mut degraded = HealthInput::heartbeat(InputSource::ServingWorker, now);
        degraded.kind = InputKind::Degraded;
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::Fresh, 0, now),
        )];
        let events = agg.apply(&[degraded], &hb, now, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::Degraded);

        let mut recovery = HealthInput::heartbeat(InputSource::ServingWorker, now);
        recovery.kind = InputKind::Ready;
        recovery.serving_state = Some(ComponentState::Ready);
        let events = agg.apply(&[recovery], &hb, now, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::Ready);
        assert_eq!(events[0].reason, SafeStateReason::Recovery);
    }

    #[test]
    fn periodic_emit_only_fires_when_not_ready() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::NoHeartbeat, 5, now),
        )];
        let mut events = agg.apply(&[], &hb, now, Some(std::time::Duration::from_millis(10)));
        assert_eq!(events.len(), 1);
        events.clear();
        let later = now + std::time::Duration::from_millis(20);
        let events = agg.apply(&[], &hb, later, Some(std::time::Duration::from_millis(10)));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, SafeStateReason::Periodic);

        // When the source recovers the periodic emit stops.
        let hb_ready = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::Fresh, 0, later),
        )];
        let later2 = later + std::time::Duration::from_millis(20);
        let events = agg.apply(
            &[],
            &hb_ready,
            later2,
            Some(std::time::Duration::from_millis(10)),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::Ready);

        let later3 = later2 + std::time::Duration::from_millis(20);
        let events = agg.apply(
            &[],
            &hb_ready,
            later3,
            Some(std::time::Duration::from_millis(10)),
        );
        assert!(
            events.is_empty(),
            "ready state should not periodically emit"
        );
    }

    #[test]
    fn overload_input_sets_degraded_with_overload_reason() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let mut overload = HealthInput::heartbeat(InputSource::ServingWorker, now);
        overload.kind = InputKind::Overload;
        overload.missed_deadline_rate = Some(0.1);
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::Fresh, 0, now),
        )];
        let events = agg.apply(&[overload], &hb, now, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ObservabilityState::Degraded);
        assert_eq!(events[0].reason, SafeStateReason::Overload);
    }

    #[test]
    fn missed_heartbeat_counter_saturates() {
        let agg = Aggregator::new(policy(), InputSource::ServingWorker);
        let now = Instant::now();
        let hb = vec![(
            InputSource::ServingWorker,
            source_state(HeartbeatHealth::NoHeartbeat, 50, now),
        )];
        agg.apply(&[], &hb, now, None);
        let s = agg.current();
        assert!(s.missed_heartbeat_count <= u64::from(policy().missed_threshold).saturating_mul(4));
    }
}
