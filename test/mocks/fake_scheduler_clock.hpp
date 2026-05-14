// SPDX-License-Identifier: Apache-2.0
//
// V01-E06: Deterministic fake clock for scheduler unit/integration
// tests. The fake is monotonic by construction (advance() must take a
// non-negative duration) and is the only clock injected into scheduler
// tests so deadline behavior is reproducible.

#pragma once

#include <chrono>
#include <mutex>

#include "tensorplate/scheduler/clock.hpp"

namespace tensorplate::testing {

/// Monotonic fake clock. Construction starts at an arbitrary monotonic
/// origin so tests do not accidentally depend on the system steady
/// clock's epoch.
class FakeSchedulerClock final : public SchedulerClock {
 public:
  /// Start the fake at `origin`. Defaults to a fixed offset 1 hour
  /// past `steady_clock`'s zero so the fake never returns zero (which
  /// some metric encoders treat as "missing").
  explicit FakeSchedulerClock(
      SchedulerClock::TimePoint origin = SchedulerClock::TimePoint{std::chrono::hours{1}}) noexcept
      : now_(origin) {}

  TimePoint now() const noexcept override {
    std::lock_guard<std::mutex> guard(mu_);
    return now_;
  }

  /// Advance the fake by `delta`. Negative deltas are clamped to zero
  /// (the clock is monotonic).
  void advance(SchedulerClock::Duration delta) noexcept {
    if (delta.count() < 0) {
      return;
    }
    std::lock_guard<std::mutex> guard(mu_);
    now_ += delta;
  }

  /// Convenience: advance by milliseconds.
  void advance_ms(std::chrono::milliseconds delta) noexcept {
    advance(std::chrono::duration_cast<SchedulerClock::Duration>(delta));
  }

  /// Set the clock to an absolute time. Caller must ensure `t >= now()`;
  /// values that would regress the clock are ignored.
  void set(TimePoint t) noexcept {
    std::lock_guard<std::mutex> guard(mu_);
    if (t > now_) {
      now_ = t;
    }
  }

 private:
  mutable std::mutex mu_;
  TimePoint now_;
};

}  // namespace tensorplate::testing
