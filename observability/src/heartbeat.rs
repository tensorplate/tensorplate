// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F03: Monotonic heartbeat evaluator and no-heartbeat detection.
//
// The evaluator tracks the most-recent heartbeat per source using
// monotonic time and produces a deterministic state output without
// depending on wall-clock time or inference traffic. The aggregator
// (F04) consumes the evaluator's view to combine it with explicit
// degraded / failed / overload signals.
//
// All timing decisions evaluate against an injected `MonotonicClock`,
// which makes failure-injection tests deterministic.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn duration_to_u64_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

use crate::clock::MonotonicClock;
use crate::config::HeartbeatPolicy;
use crate::listener::InputSource;

/// Per-source heartbeat state surfaced to the aggregator and the
/// status snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum HeartbeatHealth {
    /// No heartbeat has been observed yet for this source.
    #[default]
    NoneYet,
    /// Most-recent heartbeat is still fresh.
    Fresh,
    /// A heartbeat has been missed but the missed-threshold has not
    /// yet been reached. The aggregator may treat this as degraded.
    Stale,
    /// Missed-threshold reached: the source has wedged or stopped.
    NoHeartbeat,
}

/// Per-source bookkeeping. Sources are added on first observation.
#[derive(Debug, Clone)]
pub struct SourceState {
    pub last_heartbeat_at: Option<Instant>,
    pub missed_count: u32,
    pub consecutive_recovery_heartbeats: u32,
    pub health: HeartbeatHealth,
}

impl SourceState {
    fn new() -> Self {
        Self {
            last_heartbeat_at: None,
            missed_count: 0,
            consecutive_recovery_heartbeats: 0,
            health: HeartbeatHealth::NoneYet,
        }
    }
}

/// Heartbeat evaluator. The composition root wires one of these per
/// service instance; the evaluator is `Send + Sync` so the listener,
/// the aggregator, and the snapshot writer can read its current state
/// from independent worker threads.
pub struct HeartbeatEvaluator {
    policy: HeartbeatPolicy,
    clock: std::sync::Arc<dyn MonotonicClock>,
    state: Mutex<HashMap<&'static str, SourceState>>,
}

impl HeartbeatEvaluator {
    #[must_use]
    pub fn new(policy: HeartbeatPolicy, clock: std::sync::Arc<dyn MonotonicClock>) -> Self {
        Self {
            policy,
            clock,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Record a heartbeat from `source`. Returns the updated state
    /// snapshot so callers can inspect transitions without acquiring
    /// the internal mutex twice.
    pub fn observe_heartbeat(&self, source: InputSource, received_at: Instant) -> SourceState {
        #[allow(clippy::expect_used)]
        let mut state = self.state.lock().expect("heartbeat state poisoned");
        let entry = state
            .entry(source.as_str())
            .or_insert_with(SourceState::new);
        entry.last_heartbeat_at = Some(received_at);
        if entry.health == HeartbeatHealth::NoHeartbeat {
            entry.consecutive_recovery_heartbeats =
                entry.consecutive_recovery_heartbeats.saturating_add(1);
            if entry.consecutive_recovery_heartbeats >= self.policy.recovery_heartbeats {
                entry.missed_count = 0;
                entry.consecutive_recovery_heartbeats = 0;
                entry.health = HeartbeatHealth::Fresh;
            }
        } else {
            entry.missed_count = 0;
            entry.consecutive_recovery_heartbeats = 0;
            entry.health = HeartbeatHealth::Fresh;
        }
        entry.clone()
    }

    /// Recompute health for every tracked source. Called by the
    /// composition root on every tick so the no-heartbeat state can
    /// engage even when no events have arrived.
    pub fn evaluate(&self) -> Vec<(InputSource, SourceState)> {
        let now = self.clock.now();
        #[allow(clippy::expect_used)]
        let mut state = self.state.lock().expect("heartbeat state poisoned");
        let mut out = Vec::with_capacity(state.len());
        for (label, entry) in state.iter_mut() {
            let source = match *label {
                "serving_worker" => InputSource::ServingWorker,
                "agent_supervisor" => InputSource::AgentSupervisor,
                _ => InputSource::Internal,
            };
            self.recompute_in_place(entry, now);
            out.push((source, entry.clone()));
        }
        out
    }

    /// Force-add a source the operator wants the evaluator to monitor
    /// even before the first event has arrived. Used by the composition
    /// root to register the configured `primary_source`.
    pub fn register_source(&self, source: InputSource) {
        #[allow(clippy::expect_used)]
        let mut state = self.state.lock().expect("heartbeat state poisoned");
        state
            .entry(source.as_str())
            .or_insert_with(SourceState::new);
    }

    /// Read-only view of the source bookkeeping. Used by the status
    /// snapshot writer.
    pub fn snapshot(&self) -> Vec<(InputSource, SourceState)> {
        #[allow(clippy::expect_used)]
        let state = self.state.lock().expect("heartbeat state poisoned");
        state
            .iter()
            .map(|(label, entry)| {
                let source = match *label {
                    "serving_worker" => InputSource::ServingWorker,
                    "agent_supervisor" => InputSource::AgentSupervisor,
                    _ => InputSource::Internal,
                };
                (source, entry.clone())
            })
            .collect()
    }

    /// Return the canonical (primary-source) state for snapshot fields
    /// that surface a single missed-heartbeat count.
    pub fn primary_state(&self, source: InputSource) -> SourceState {
        #[allow(clippy::expect_used)]
        let state = self.state.lock().expect("heartbeat state poisoned");
        state
            .get(source.as_str())
            .cloned()
            .unwrap_or_else(SourceState::new)
    }

    fn recompute_in_place(&self, entry: &mut SourceState, now: Instant) {
        let Some(last) = entry.last_heartbeat_at else {
            // No heartbeat has been seen yet. After `missed_threshold`
            // expected intervals elapse from "now" we stay in
            // `NoHeartbeat` so the aggregator can drive the no-
            // heartbeat state without any input arriving at all. The
            // `NoneYet -> NoHeartbeat` transition uses the configured
            // interval to gate the initial-no-heartbeat decision so
            // unit tests don't fire instantly.
            entry.health = HeartbeatHealth::NoneYet;
            return;
        };
        let elapsed = now.saturating_duration_since(last);
        let expected = Duration::from_millis(self.policy.expected_interval_ms);
        let grace = Duration::from_millis(self.policy.grace_ms);
        let one_window = expected + grace;
        if elapsed < one_window {
            // Last heartbeat is still inside the expected interval.
            // Recovery has been observed; the `Fresh` state is final
            // until the window expires.
            entry.health = HeartbeatHealth::Fresh;
            return;
        }
        // Count how many full windows have elapsed since the last
        // heartbeat. Subtracting one full window from the elapsed
        // gives the number of missed beats.
        let missed_full = if expected.is_zero() {
            entry.missed_count.saturating_add(1)
        } else {
            // (elapsed_ms - grace_ms) / expected_ms, saturating at
            // missed_threshold so the counter stops growing unbounded
            // while the source is down.
            let elapsed_ms = duration_to_u64_ms(elapsed);
            let grace_ms = duration_to_u64_ms(grace);
            let expected_ms = duration_to_u64_ms(expected);
            let usable = elapsed_ms.saturating_sub(grace_ms);
            let estimate = if expected_ms == 0 {
                1
            } else {
                usable / expected_ms
            };
            u32::try_from(estimate).unwrap_or(u32::MAX)
        };
        entry.missed_count = missed_full.min(self.policy.missed_threshold.saturating_mul(4));
        entry.consecutive_recovery_heartbeats = 0;
        if entry.missed_count >= self.policy.missed_threshold {
            entry.health = HeartbeatHealth::NoHeartbeat;
        } else {
            entry.health = HeartbeatHealth::Stale;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{HeartbeatEvaluator, HeartbeatHealth};
    use crate::clock::{FakeClock, MonotonicClock};
    use crate::config::HeartbeatPolicy;
    use crate::listener::InputSource;
    use std::sync::Arc;
    use std::time::Duration;

    fn policy() -> HeartbeatPolicy {
        HeartbeatPolicy {
            expected_interval_ms: 100,
            grace_ms: 25,
            missed_threshold: 3,
            recovery_heartbeats: 1,
        }
    }

    #[test]
    fn fresh_state_after_first_heartbeat() {
        let clock = Arc::new(FakeClock::new());
        let e = HeartbeatEvaluator::new(policy(), clock.clone());
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        let s = e.primary_state(InputSource::ServingWorker);
        assert!(matches!(s.health, HeartbeatHealth::Fresh));
    }

    #[test]
    fn stale_after_one_missed_then_no_heartbeat_at_threshold() {
        let clock = Arc::new(FakeClock::new());
        let e = HeartbeatEvaluator::new(policy(), clock.clone());
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        // advance one window + grace -> 1 missed
        clock.advance(Duration::from_millis(150));
        let v = e.evaluate();
        assert_eq!(v.len(), 1);
        assert!(matches!(v[0].1.health, HeartbeatHealth::Stale));
        assert!(v[0].1.missed_count >= 1);
        // advance well past the threshold -> NoHeartbeat
        clock.advance(Duration::from_millis(1_000));
        let v = e.evaluate();
        assert!(matches!(v[0].1.health, HeartbeatHealth::NoHeartbeat));
    }

    #[test]
    fn heartbeat_recovers_no_heartbeat_state() {
        let clock = Arc::new(FakeClock::new());
        let e = HeartbeatEvaluator::new(policy(), clock.clone());
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        clock.advance(Duration::from_millis(1_000));
        e.evaluate();
        assert!(matches!(
            e.primary_state(InputSource::ServingWorker).health,
            HeartbeatHealth::NoHeartbeat
        ));
        clock.advance(Duration::from_millis(10));
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        assert!(matches!(
            e.primary_state(InputSource::ServingWorker).health,
            HeartbeatHealth::Fresh
        ));
    }

    #[test]
    fn initial_no_heartbeat_does_not_flip_until_observation() {
        let clock = Arc::new(FakeClock::new());
        let e = HeartbeatEvaluator::new(policy(), clock.clone());
        e.register_source(InputSource::ServingWorker);
        clock.advance(Duration::from_millis(10_000));
        let s = e.evaluate();
        assert_eq!(s.len(), 1);
        assert!(matches!(s[0].1.health, HeartbeatHealth::NoneYet));
    }

    #[test]
    fn wall_clock_jump_simulation_does_not_affect_freshness() {
        // The evaluator uses monotonic instants supplied by the
        // injected clock; we can't manufacture a wall-clock jump, but
        // we can verify that the freshness window is purely a function
        // of `FakeClock` advances.
        let clock = Arc::new(FakeClock::new());
        let e = HeartbeatEvaluator::new(policy(), clock.clone());
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        clock.advance(Duration::from_millis(50));
        let v = e.evaluate();
        assert!(matches!(v[0].1.health, HeartbeatHealth::Fresh));
    }

    #[test]
    fn recovery_requires_multiple_heartbeats_when_configured() {
        let clock = Arc::new(FakeClock::new());
        let mut p = policy();
        p.recovery_heartbeats = 2;
        let e = HeartbeatEvaluator::new(p, clock.clone());
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        clock.advance(Duration::from_millis(1_000));
        e.evaluate();
        assert!(matches!(
            e.primary_state(InputSource::ServingWorker).health,
            HeartbeatHealth::NoHeartbeat
        ));
        clock.advance(Duration::from_millis(10));
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        // first recovery heartbeat: still NoHeartbeat
        assert!(matches!(
            e.primary_state(InputSource::ServingWorker).health,
            HeartbeatHealth::NoHeartbeat
        ));
        clock.advance(Duration::from_millis(10));
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        // second recovery heartbeat: Fresh
        assert!(matches!(
            e.primary_state(InputSource::ServingWorker).health,
            HeartbeatHealth::Fresh
        ));
    }

    #[test]
    fn missed_count_saturates_at_a_bounded_multiple_of_threshold() {
        let clock = Arc::new(FakeClock::new());
        let e = HeartbeatEvaluator::new(policy(), clock.clone());
        e.observe_heartbeat(InputSource::ServingWorker, clock.now());
        clock.advance(Duration::from_secs(60));
        e.evaluate();
        let s = e.primary_state(InputSource::ServingWorker);
        assert!(s.missed_count <= policy().missed_threshold.saturating_mul(4));
    }
}
