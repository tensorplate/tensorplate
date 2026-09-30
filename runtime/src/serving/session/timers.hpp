// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <optional>

#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {

enum class SessionExpiry { Idle, Heartbeat, MaxDuration };

class SessionTimers {
 public:
  SessionTimers(SessionLimits limits, SchedulerClock::TimePoint opened_at) noexcept;

  void on_client_activity(LogicalSessionEvent event, SchedulerClock::TimePoint now) noexcept;
  [[nodiscard]] bool heartbeat_due(SchedulerClock::TimePoint now) const noexcept;
  [[nodiscard]] std::optional<SessionExpiry> expired(SchedulerClock::TimePoint now,
                                                     LogicalSessionState state) const noexcept;
  [[nodiscard]] std::optional<SchedulerClock::TimePoint> next_deadline(
      LogicalSessionState state) const noexcept;
  [[nodiscard]] SchedulerClock::Duration duration_remaining(
      SchedulerClock::TimePoint now) const noexcept;

 private:
  SessionLimits limits_;
  SchedulerClock::TimePoint opened_at_;
  SchedulerClock::TimePoint last_activity_at_;
  SchedulerClock::TimePoint last_heartbeat_at_;
};
}  // namespace tensorplate::serving
