// SPDX-License-Identifier: Apache-2.0
//
// V01-E10-F03: Monotonic clock abstraction for the observability service.
//
// Heartbeat freshness, no-heartbeat thresholds, and ROS 2 publish
// intervals MUST evaluate against monotonic time so wall-clock skew or
// NTP step adjustments cannot mask a wedged worker or trigger a false
// no-heartbeat transition.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Source of monotonic time. Implementations must be cheap, non-blocking,
/// and guaranteed never to go backwards.
pub trait MonotonicClock: Send + Sync {
    /// Current monotonic instant.
    fn now(&self) -> Instant;
}

/// Production clock backed by [`Instant::now`].
#[derive(Clone, Debug, Default)]
pub struct SystemMonotonicClock;

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock with explicit advancement. The base is irrelevant for
/// correctness because the evaluator only compares instants for ordering.
#[derive(Debug)]
pub struct FakeClock {
    inner: Mutex<Instant>,
}

impl FakeClock {
    /// Build a clock anchored at the current real-time `Instant`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Instant::now()),
        }
    }

    /// Advance the clock by `delta`. Saturates on overflow.
    #[allow(clippy::expect_used)]
    pub fn advance(&self, delta: Duration) {
        let mut guard = self.inner.lock().expect("fake clock poisoned");
        *guard = guard.checked_add(delta).unwrap_or(*guard);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for FakeClock {
    #[allow(clippy::expect_used)]
    fn now(&self) -> Instant {
        *self.inner.lock().expect("fake clock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration, FakeClock, MonotonicClock, SystemMonotonicClock};

    #[test]
    fn system_clock_is_non_decreasing() {
        let c = SystemMonotonicClock;
        let a = c.now();
        let b = c.now();
        assert!(b >= a);
    }

    #[test]
    fn fake_clock_advance_moves_now_forward() {
        let c = FakeClock::new();
        let a = c.now();
        c.advance(Duration::from_millis(50));
        let b = c.now();
        assert!(b >= a + Duration::from_millis(50));
    }
}
