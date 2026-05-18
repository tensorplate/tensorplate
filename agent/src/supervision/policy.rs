// SPDX-License-Identifier: Apache-2.0
//
// V01-E09-F03: Bounded backoff scheduler + crash-loop detector.
//
// Both components are pure: they consume a monotonic `Instant` and a
// failure classification, return the next decision, and never call into
// the runtime. The supervisor's state machine drives them under its own
// lock. Tests pin time through `FakeClock` so the policy is fully
// deterministic.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::config::{BackoffConfig, RestartPolicy, RestartPolicyKind};

/// Why this iteration restarted. The policy treats some failures
/// (`not_ready_timeout`, `health_failed`) more aggressively than others
/// (`exit_after_ready`) so an isolated post-ready crash does not enter
/// crash-loop on the very next event.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FailureClass {
    /// Worker exited with no successful ready transition during this
    /// launch.
    ExitBeforeReady,
    /// Worker exited after reaching ready.
    ExitAfterReady,
    /// Readiness probe never returned `ready` inside the startup window.
    NotReadyTimeout,
    /// Worker reported a failed health state.
    HealthFailed,
    /// Worker reported a degraded health state. The supervisor logs this
    /// but does not auto-restart unless escalation policy fires.
    HealthDegraded,
}

impl FailureClass {
    /// Weight this failure contributes to the rolling restart counter.
    /// Each class currently contributes one unit in v0.1.0; the
    /// classification is preserved on the wire so the V01-E10 service
    /// can distinguish them without re-deriving from exit codes.
    #[must_use]
    pub const fn weight(self) -> u32 {
        match self {
            Self::ExitBeforeReady
            | Self::NotReadyTimeout
            | Self::HealthFailed
            | Self::ExitAfterReady
            | Self::HealthDegraded => 1,
        }
    }
}

/// Action recommended by [`BackoffScheduler::on_failure`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackoffDecision {
    /// Schedule a restart at `at_instant`. The supervisor must wait until
    /// the monotonic clock crosses this point before launching.
    Restart { at: Instant, delay: Duration },
    /// Stop restarting. Crash-loop threshold reached; the supervisor
    /// transitions to a terminal supervision state.
    EnterCrashLoop { reason: String },
    /// Restart policy is disabled. The supervisor stays in `stopped`
    /// until operator action triggers a fresh launch.
    PolicyDisabled,
}

/// Stable view of the current restart counter and threshold, exposed for
/// status projection. The supervisor copies this into its
/// [`super::state::SupervisionStatus`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RestartCounters {
    pub rolling_count: u32,
    pub threshold: u32,
    pub current_delay_ms: u64,
    pub stable_uptime_ms: u64,
}

/// Bounded exponential-backoff scheduler. Records up to `threshold`
/// failure instants inside a rolling window and computes the next
/// schedule deterministically.
pub struct BackoffScheduler {
    policy: RestartPolicy,
    failures: VecDeque<Instant>,
    last_attempt: Option<Instant>,
    last_ready_at: Option<Instant>,
    current_delay_ms: u64,
}

impl BackoffScheduler {
    /// Build a fresh scheduler from the supervisor's restart policy.
    #[must_use]
    pub fn new(policy: RestartPolicy) -> Self {
        let initial = policy.backoff.initial_delay_ms;
        Self {
            policy,
            failures: VecDeque::new(),
            last_attempt: None,
            last_ready_at: None,
            current_delay_ms: initial,
        }
    }

    fn cfg(&self) -> &BackoffConfig {
        &self.policy.backoff
    }

    /// Record a successful ready transition. The supervisor calls this
    /// from `Running -> Ready`; once `stable_reset_ms` of ready uptime
    /// elapses without an intervening failure the failure counter
    /// decays to zero.
    pub fn on_ready(&mut self, now: Instant) {
        self.last_ready_at = Some(now);
    }

    fn maybe_reset_after_stable_ready(&mut self, now: Instant) {
        let Some(ready_at) = self.last_ready_at else {
            return;
        };
        let reset = Duration::from_millis(self.cfg().stable_reset_ms);
        if now.saturating_duration_since(ready_at) >= reset {
            self.failures.clear();
            self.current_delay_ms = self.cfg().initial_delay_ms;
            // Stable-ready uptime also discards the prior attempt
            // bookkeeping so the next failure starts at the initial
            // delay rather than the multiplied delay.
            self.last_attempt = None;
        }
    }

    fn prune_window(&mut self, now: Instant) {
        let window = Duration::from_millis(self.cfg().window_ms);
        while let Some(front) = self.failures.front().copied() {
            if now.saturating_duration_since(front) > window {
                self.failures.pop_front();
            } else {
                break;
            }
        }
    }

    /// Snapshot of the counter state suitable for status projection.
    #[must_use]
    pub fn counters(&self, now: Instant) -> RestartCounters {
        let cfg = self.cfg();
        let stable_uptime_ms = self.last_ready_at.map_or(0, |t| {
            u64::try_from(now.saturating_duration_since(t).as_millis()).unwrap_or(u64::MAX)
        });
        RestartCounters {
            rolling_count: u32::try_from(self.failures.len()).unwrap_or(u32::MAX),
            threshold: cfg.threshold,
            current_delay_ms: self.current_delay_ms,
            stable_uptime_ms,
        }
    }

    /// Record a failure and compute the next decision.
    pub fn on_failure(&mut self, now: Instant, class: FailureClass) -> BackoffDecision {
        if matches!(self.policy.kind, RestartPolicyKind::Disabled) {
            return BackoffDecision::PolicyDisabled;
        }
        self.maybe_reset_after_stable_ready(now);
        self.prune_window(now);
        for _ in 0..class.weight() {
            self.failures.push_back(now);
        }
        if u32::try_from(self.failures.len()).unwrap_or(u32::MAX) >= self.cfg().threshold {
            return BackoffDecision::EnterCrashLoop {
                reason: format!(
                    "{:?}: {} failures in last {} ms (threshold={})",
                    class,
                    self.failures.len(),
                    self.cfg().window_ms,
                    self.cfg().threshold
                ),
            };
        }
        let next_delay_ms = self.next_delay_ms();
        self.current_delay_ms = next_delay_ms;
        self.last_attempt = Some(now);
        let delay = Duration::from_millis(next_delay_ms);
        BackoffDecision::Restart {
            at: now.checked_add(delay).unwrap_or(now),
            delay,
        }
    }

    fn next_delay_ms(&self) -> u64 {
        let cfg = self.cfg();
        let base = if self.last_attempt.is_some() {
            // Exponential growth: current * multiplier / 100.
            let scaled = u128::from(self.current_delay_ms)
                .saturating_mul(u128::from(cfg.multiplier_hundredths));
            let div = scaled / 100;
            u64::try_from(div).unwrap_or(cfg.max_delay_ms)
        } else {
            cfg.initial_delay_ms
        };
        base.min(cfg.max_delay_ms).max(cfg.initial_delay_ms)
    }

    /// Reset all counters. Used by operator-triggered recovery and by a
    /// successful deploy/rollback transaction after crash-loop.
    pub fn reset(&mut self) {
        self.failures.clear();
        self.current_delay_ms = self.cfg().initial_delay_ms;
        self.last_attempt = None;
        self.last_ready_at = None;
    }

    /// True if the scheduler has previously declared crash-loop without a
    /// subsequent reset; tests assert against this via the supervisor's
    /// projected status.
    #[must_use]
    pub fn is_at_threshold(&self) -> bool {
        u32::try_from(self.failures.len()).unwrap_or(u32::MAX) >= self.cfg().threshold
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used,
        clippy::cast_possible_truncation,
        clippy::default_trait_access
    )]

    use super::super::clock::{FakeClock, MonotonicClock};
    use super::super::config::{BackoffConfig, RestartPolicy, RestartPolicyKind};
    use super::{BackoffDecision, BackoffScheduler, FailureClass};
    use std::time::Duration;

    fn quick_policy() -> RestartPolicy {
        RestartPolicy {
            kind: RestartPolicyKind::BoundedBackoff,
            backoff: BackoffConfig {
                initial_delay_ms: 100,
                multiplier_hundredths: 200, // 2x
                max_delay_ms: 1_600,
                window_ms: 60_000,
                threshold: 4,
                stable_reset_ms: 30_000,
            },
        }
    }

    #[test]
    fn first_failure_schedules_initial_delay() {
        let clock = FakeClock::new();
        let mut sched = BackoffScheduler::new(quick_policy());
        match sched.on_failure(clock.now(), FailureClass::ExitBeforeReady) {
            BackoffDecision::Restart { delay, .. } => {
                assert_eq!(delay, Duration::from_millis(100));
            }
            other => panic!("expected restart, got {other:?}"),
        }
    }

    #[test]
    fn delay_grows_exponentially_up_to_max() {
        let clock = FakeClock::new();
        let mut sched = BackoffScheduler::new(quick_policy());
        let delays: Vec<u64> = (0..3)
            .map(|_| {
                let d = match sched.on_failure(clock.now(), FailureClass::ExitBeforeReady) {
                    BackoffDecision::Restart { delay, .. } => delay.as_millis() as u64,
                    other => panic!("expected restart, got {other:?}"),
                };
                clock.advance(Duration::from_millis(1));
                d
            })
            .collect();
        // 100, 200, 400
        assert_eq!(delays, vec![100, 200, 400]);
    }

    #[test]
    fn crash_loop_fires_at_threshold() {
        let clock = FakeClock::new();
        let mut sched = BackoffScheduler::new(quick_policy());
        // Threshold is 4; the fourth failure must hit crash-loop.
        for _ in 0..3 {
            let decision = sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
            assert!(matches!(decision, BackoffDecision::Restart { .. }));
            clock.advance(Duration::from_millis(1));
        }
        let final_decision = sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        assert!(matches!(
            final_decision,
            BackoffDecision::EnterCrashLoop { .. }
        ));
        assert!(sched.is_at_threshold());
    }

    #[test]
    fn stable_uptime_resets_counters() {
        let clock = FakeClock::new();
        let mut sched = BackoffScheduler::new(quick_policy());
        sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        clock.advance(Duration::from_millis(50));
        sched.on_ready(clock.now());
        clock.advance(Duration::from_millis(30_500));
        // Another failure should restart at initial delay, not the prior
        // growth.
        let decision = sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        match decision {
            BackoffDecision::Restart { delay, .. } => {
                assert_eq!(delay, Duration::from_millis(100));
            }
            other => panic!("expected restart, got {other:?}"),
        }
    }

    #[test]
    fn rolling_window_drops_stale_failures() {
        let clock = FakeClock::new();
        let mut sched = BackoffScheduler::new(quick_policy());
        for _ in 0..3 {
            sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
            clock.advance(Duration::from_millis(1));
        }
        // Advance past the window so all entries fall off.
        clock.advance(Duration::from_secs(61));
        let decision = sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        assert!(matches!(decision, BackoffDecision::Restart { .. }));
        assert!(!sched.is_at_threshold());
    }

    #[test]
    fn disabled_policy_returns_policy_disabled() {
        let clock = FakeClock::new();
        let mut policy = quick_policy();
        policy.kind = RestartPolicyKind::Disabled;
        let mut sched = BackoffScheduler::new(policy);
        let decision = sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        assert!(matches!(decision, BackoffDecision::PolicyDisabled));
    }

    #[test]
    fn reset_restores_initial_delay_and_clears_counters() {
        let clock = FakeClock::new();
        let mut sched = BackoffScheduler::new(quick_policy());
        sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        sched.on_failure(clock.now(), FailureClass::ExitBeforeReady);
        sched.reset();
        let counters = sched.counters(clock.now());
        assert_eq!(counters.rolling_count, 0);
        assert_eq!(counters.current_delay_ms, 100);
    }
}
