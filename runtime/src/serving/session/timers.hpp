// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <optional>

#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

#include "serving/session/credits.hpp"

namespace tensorplate::serving {

enum class SessionExpiry { Idle, Heartbeat, MaxDuration, SlowConsumer, FinalizeDeadline };

/// How long a session may owe a finalization or a drain: ten seconds to
/// finish a transcript, two minutes to synthesize and deliver accepted text.
[[nodiscard]] constexpr std::chrono::seconds finalize_deadline(SessionInputKind kind) noexcept {
  return kind == SessionInputKind::Audio ? std::chrono::seconds{10} : std::chrono::seconds{120};
}

class SessionTimers {
 public:
  SessionTimers(SessionLimits limits, SessionInputKind input_kind,
                SchedulerClock::TimePoint opened_at) noexcept;

  void on_client_activity(LogicalSessionEvent event, SchedulerClock::TimePoint now) noexcept;
  /// Starts the finalize deadline when an accepted event first leaves the
  /// session owing a finalization or a drain, and stops it once the session
  /// is active again or its drain has completed. Nothing in between moves
  /// it: not delivery progress, a further finalization or a half-close.
  void on_transition(LogicalSessionState before, LogicalSessionEvent event,
                     LogicalSessionState after, SchedulerClock::TimePoint now) noexcept;
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
  SchedulerClock::Duration finalize_deadline_;
  SchedulerClock::TimePoint opened_at_;
  SchedulerClock::TimePoint last_activity_at_;
  SchedulerClock::TimePoint last_heartbeat_at_;
  std::optional<SchedulerClock::TimePoint> finalize_started_at_;
};
}  // namespace tensorplate::serving
