// SPDX-License-Identifier: Apache-2.0
#include "serving/session/timers.hpp"

#include <algorithm>

namespace tensorplate::serving {

SessionTimers::SessionTimers(SessionLimits limits, SchedulerClock::TimePoint opened_at) noexcept
    : limits_(limits),
      opened_at_(opened_at),
      last_activity_at_(opened_at),
      last_heartbeat_at_(opened_at) {}

void SessionTimers::on_client_activity(LogicalSessionEvent event,
                                       SchedulerClock::TimePoint now) noexcept {
  switch (event) {
    case LogicalSessionEvent::Data:
    case LogicalSessionEvent::Finalize:
    case LogicalSessionEvent::Ping:
    case LogicalSessionEvent::StatusRequest:
      last_activity_at_ = now;
      if (event == LogicalSessionEvent::Ping) {
        last_heartbeat_at_ = now;
      }
      break;
    default:
      break;
  }
}

bool SessionTimers::heartbeat_due(SchedulerClock::TimePoint now) const noexcept {
  return now >= last_heartbeat_at_ + limits_.heartbeat_interval();
}

std::optional<SessionExpiry> SessionTimers::expired(SchedulerClock::TimePoint now,
                                                    LogicalSessionState state) const noexcept {
  if (state == LogicalSessionState::Closed || state == LogicalSessionState::Failed ||
      state == LogicalSessionState::CancelRequested) {
    return std::nullopt;
  }
  const auto maximum = opened_at_ + limits_.max_duration();
  if (state == LogicalSessionState::Draining) {
    return now >= maximum ? std::optional{SessionExpiry::MaxDuration} : std::nullopt;
  }
  const auto heartbeat = last_heartbeat_at_ + limits_.liveness_timeout();
  const auto idle = last_activity_at_ + limits_.idle_timeout();
  if (now < std::min({maximum, heartbeat, idle})) {
    return std::nullopt;
  }
  if (maximum <= heartbeat && maximum <= idle) {
    return SessionExpiry::MaxDuration;
  }
  if (heartbeat <= idle) {
    return SessionExpiry::Heartbeat;
  }
  return SessionExpiry::Idle;
}

std::optional<SchedulerClock::TimePoint> SessionTimers::next_deadline(
    LogicalSessionState state) const noexcept {
  if (state == LogicalSessionState::Closed || state == LogicalSessionState::Failed ||
      state == LogicalSessionState::CancelRequested) {
    return std::nullopt;
  }
  auto next = opened_at_ + limits_.max_duration();
  if (state != LogicalSessionState::Draining) {
    next = std::min(next, last_activity_at_ + limits_.idle_timeout());
    next = std::min(next, last_heartbeat_at_ + limits_.liveness_timeout());
  }
  return next;
}

SchedulerClock::Duration SessionTimers::duration_remaining(
    SchedulerClock::TimePoint now) const noexcept {
  return std::max(SchedulerClock::Duration::zero(),
                  std::chrono::duration_cast<SchedulerClock::Duration>(
                      opened_at_ + limits_.max_duration() - now));
}
}  // namespace tensorplate::serving
