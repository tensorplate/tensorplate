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

/// Monotonic fake clock. Construction starts past real
/// `std::chrono::steady_clock::now()` so deadlines computed from
/// `now()` are also valid against real-clock validation paths inside
/// value objects (e.g. InferRequest::create).
class FakeSchedulerClock final : public SchedulerClock {
 public:
  /// Default origin: real steady_clock now + 1 hour so any deadline
  /// derived from this fake clock is unambiguously "in the future"
  /// against either clock domain. Tests that need a specific origin
  /// pass one explicitly.
  explicit FakeSchedulerClock(SchedulerClock::TimePoint origin = std::chrono::steady_clock::now() +
                                                                 std::chrono::hours{1}) noexcept
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
