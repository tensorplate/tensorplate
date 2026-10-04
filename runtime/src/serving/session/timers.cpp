// SPDX-License-Identifier: Apache-2.0
#include "serving/session/timers.hpp"

#include <algorithm>

#include "serving/session/credits.hpp"

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

std::optional<SessionTimers::Deadline> SessionTimers::earliest(
    LogicalSessionState state,
    std::optional<SchedulerClock::TimePoint> output_stalled_since) const noexcept {
  if (state == LogicalSessionState::Closed || state == LogicalSessionState::Failed ||
      state == LogicalSessionState::CancelRequested) {
    return std::nullopt;
  }
  // Ties go to the deadline listed first.
  Deadline first{opened_at_ + limits_.max_duration(), SessionExpiry::MaxDuration};
  const auto consider = [&first](SchedulerClock::TimePoint at, SessionExpiry expiry) {
    if (at < first.at) {
      first = Deadline{at, expiry};
    }
  };
  if (output_stalled_since) {
    consider(*output_stalled_since + SessionBudgets::kOutputNoProgressTimeout,
             SessionExpiry::SlowConsumer);
  }
  if (state != LogicalSessionState::Draining) {
    consider(last_heartbeat_at_ + limits_.liveness_timeout(), SessionExpiry::Heartbeat);
    consider(last_activity_at_ + limits_.idle_timeout(), SessionExpiry::Idle);
  }
  return first;
}

std::optional<SessionExpiry> SessionTimers::expired(
    SchedulerClock::TimePoint now, LogicalSessionState state,
    std::optional<SchedulerClock::TimePoint> output_stalled_since) const noexcept {
  const auto first = earliest(state, output_stalled_since);
  if (!first || now < first->at) {
    return std::nullopt;
  }
  return first->expiry;
}

std::optional<SchedulerClock::TimePoint> SessionTimers::next_deadline(
    LogicalSessionState state,
    std::optional<SchedulerClock::TimePoint> output_stalled_since) const noexcept {
  const auto first = earliest(state, output_stalled_since);
  return first ? std::optional{first->at} : std::nullopt;
}

SchedulerClock::Duration SessionTimers::duration_remaining(
    SchedulerClock::TimePoint now) const noexcept {
  return std::max(SchedulerClock::Duration::zero(),
                  std::chrono::duration_cast<SchedulerClock::Duration>(
                      opened_at_ + limits_.max_duration() - now));
}
}  // namespace tensorplate::serving
