// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <atomic>
#include <chrono>
#include <limits>

namespace tensorplate::serving {

/// The one drain window of a worker's shutdown. The first caller begins it and
/// every later caller is given the same end, so whatever drains after another
/// drain has what that one left and no more.
///
/// Thread-safe. The caller supplies `now`: the window reads no clock.
class DrainWindow {
 public:
  using TimePoint = std::chrono::steady_clock::time_point;

  /// Begins the window at `now`, `length` long, unless it has begun.
  /// @return when the window ends.
  TimePoint begin(TimePoint now, std::chrono::milliseconds length) noexcept {
    const TimePoint end = now + length;
    auto begun = kNotBegun;
    if (end_.compare_exchange_strong(begun, end.time_since_epoch().count())) {
      return end;
    }
    return TimePoint{TimePoint::duration{begun}};
  }

 private:
  static constexpr TimePoint::rep kNotBegun = std::numeric_limits<TimePoint::rep>::min();
  std::atomic<TimePoint::rep> end_{kNotBegun};
};

}  // namespace tensorplate::serving
