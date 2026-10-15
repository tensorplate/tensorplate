// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstdint>
#include <mutex>
#include <optional>

namespace tensorplate::serving {

/// Why a worker drains: the cause its stream sessions end with.
enum class DrainCause : std::uint8_t { WorkerShutdown, DeploymentRetired };

/// The one drain window of a worker's shutdown. The first caller begins it,
/// for its cause, and every later caller is given the same end, so whatever
/// drains after another drain has what that one left and no more.
///
/// Thread-safe. The caller supplies `now`: the window reads no clock.
class DrainWindow {
 public:
  using TimePoint = std::chrono::steady_clock::time_point;

  /// Begins the window at `now`, `length` long, for `cause`, unless it has
  /// begun.
  /// @return when the window ends.
  TimePoint begin(TimePoint now, std::chrono::milliseconds length,
                  DrainCause cause = DrainCause::WorkerShutdown) noexcept {
    const std::lock_guard guard(mu_);
    if (!begun_) {
      begun_ = Begun{now + length, cause};
    }
    return begun_->end;
  }

  /// The first caller's cause; WorkerShutdown before the window begins.
  [[nodiscard]] DrainCause cause() const noexcept {
    const std::lock_guard guard(mu_);
    return begun_ ? begun_->cause : DrainCause::WorkerShutdown;
  }

 private:
  struct Begun {
    TimePoint end;
    DrainCause cause;
  };
  // The end and the cause are set together, under the lock.
  mutable std::mutex mu_;
  std::optional<Begun> begun_;
};

}  // namespace tensorplate::serving
