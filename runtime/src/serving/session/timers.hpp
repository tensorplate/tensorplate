// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <optional>

#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {

enum class SessionExpiry { Idle, Heartbeat, MaxDuration, SlowConsumer };

class SessionTimers {
 public:
  SessionTimers(SessionLimits limits, SchedulerClock::TimePoint opened_at) noexcept;

  void on_client_activity(LogicalSessionEvent event, SchedulerClock::TimePoint now) noexcept;
  [[nodiscard]] bool heartbeat_due(SchedulerClock::TimePoint now) const noexcept;
  /// `output_stalled_since` is the output queue's stall clock: undelivered
  /// output ends a session that is live or draining once it has made no
  /// delivery progress for the no-progress limit.
  [[nodiscard]] std::optional<SessionExpiry> expired(
      SchedulerClock::TimePoint now, LogicalSessionState state,
      std::optional<SchedulerClock::TimePoint> output_stalled_since = std::nullopt) const noexcept;
  [[nodiscard]] std::optional<SchedulerClock::TimePoint> next_deadline(
      LogicalSessionState state,
      std::optional<SchedulerClock::TimePoint> output_stalled_since = std::nullopt) const noexcept;
  [[nodiscard]] SchedulerClock::Duration duration_remaining(
      SchedulerClock::TimePoint now) const noexcept;

 private:
  struct Deadline {
    SchedulerClock::TimePoint at;
    SessionExpiry expiry;
  };
  [[nodiscard]] std::optional<Deadline> earliest(
      LogicalSessionState state,
      std::optional<SchedulerClock::TimePoint> output_stalled_since) const noexcept;

  SessionLimits limits_;
  SchedulerClock::TimePoint opened_at_;
  SchedulerClock::TimePoint last_activity_at_;
  SchedulerClock::TimePoint last_heartbeat_at_;
};
}  // namespace tensorplate::serving
