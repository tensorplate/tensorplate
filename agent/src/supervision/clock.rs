// SPDX-License-Identifier: Apache-2.0
//
// V01-E09: Monotonic clock abstraction.
//
// Supervision policy (backoff, crash-loop windows, readiness timeouts)
// MUST use monotonic time so wall-clock adjustments cannot trigger
// spurious restarts or hide a crash-loop. The agent runs against a
// process-monotonic `Instant`-based clock in production; tests inject a
// fake clock through this trait so all timing decisions are deterministic.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Source of monotonic time for the supervisor and its policy
/// components. Implementations must be cheap, non-blocking, and guaranteed
/// to never go backwards.
pub trait MonotonicClock: Send + Sync {
    /// Current monotonic instant.
    fn now(&self) -> Instant;
}

/// Production clock backed by [`Instant::now`]. Cheap; the supervisor
/// stores one of these behind an `Arc`.
#[derive(Clone, Debug, Default)]
pub struct SystemMonotonicClock;

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock with explicit advancement. Constructed at a fixed base and
/// advanced through [`FakeClock::advance`] so unit tests can drive backoff
/// and crash-loop windows without sleeping.
#[derive(Debug)]
pub struct FakeClock {
    inner: Mutex<Instant>,
}

impl FakeClock {
    /// Build a clock anchored at the current real-time `Instant`. The
    /// base is irrelevant for correctness because the supervisor only
    /// compares instants for ordering.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Instant::now()),
        }
    }

    /// Advance the fake clock by `delta`. Panics in tests are acceptable
    /// because the mutex cannot be poisoned under single-threaded test
    /// drivers; production code never sees `FakeClock`.
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
    fn system_clock_is_monotonic_non_decreasing() {
        let c = SystemMonotonicClock;
        let a = c.now();
        let b = c.now();
        assert!(b >= a);
    }

    #[test]
    fn fake_clock_advance_moves_now_forward() {
        let c = FakeClock::new();
        let a = c.now();
        c.advance(Duration::from_millis(100));
        let b = c.now();
        assert!(b >= a + Duration::from_millis(100));
    }
}
