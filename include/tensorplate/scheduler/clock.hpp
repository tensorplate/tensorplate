// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F01-T01 / V01-E06-F03-T01: Monotonic clock abstraction used by
// the scheduler for deadline-aware admission, queued-expiry sweeps, and
// wait-time metrics.
//
// The contract is monotonic-only: implementations must be backed by
// std::chrono::steady_clock or another monotonic source. Wall-clock
// time must never be used because deadline decisions must be immune to
// system clock adjustments (NTP, DST, manual edits). Tests inject a
// FakeSchedulerClock to drive deterministic scenarios; production code
// uses SystemSchedulerClock.

#pragma once

#include <chrono>
#include <memory>

namespace tensorplate {

/// Monotonic clock interface used by the scheduler. Implementations
/// must be safe to call from any thread.
class SchedulerClock {
 public:
  using TimePoint = std::chrono::steady_clock::time_point;
  using Duration = std::chrono::nanoseconds;

  virtual ~SchedulerClock() = default;

  /// Monotonic timestamp. Must never regress.
  [[nodiscard]] virtual TimePoint now() const noexcept = 0;
};

/// Production clock backed by std::chrono::steady_clock.
class SystemSchedulerClock final : public SchedulerClock {
 public:
  [[nodiscard]] TimePoint now() const noexcept override {
    return std::chrono::steady_clock::now();
  }

  /// Convenience factory.
  [[nodiscard]] static std::unique_ptr<SchedulerClock> create() {
    return std::make_unique<SystemSchedulerClock>();
  }
};

}  // namespace tensorplate
