// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <optional>
#include <string>
#include <utility>

#include "tensorplate/core/error.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {

/// What is remembered of a session after its slot was released.
struct SessionTombstone {
  std::uint64_t session_key = 0;
  std::uint64_t generation = 0;
  /// Closed or Failed.
  LogicalSessionState state = LogicalSessionState::Closed;
  /// Code and reason of the end cause; none after a client half-close.
  std::optional<Error::Code> code;
  std::string reason;
  SchedulerClock::TimePoint ended_at;
};

/// The ended sessions of one worker, for diagnostics and for telling a late
/// report about an ended session from an unknown key. Bounded in count, age
/// and bytes per entry. Not thread-safe; its owner serializes access.
class SessionTombstones {
 public:
  static constexpr std::size_t kCapacity = 1024;
  static constexpr std::chrono::seconds kLifetime{60};
  static constexpr std::size_t kMaxReasonBytes = 64;

  /// Remembers an ended session, dropping expired entries and then the
  /// oldest one if the history is full. `now` never decreases across calls.
  void record(std::uint64_t session_key, std::uint64_t generation, LogicalSessionState state,
              const std::optional<Error>& cause, SchedulerClock::TimePoint now) {
    while (!entries_.empty() && expired(entries_.front(), now)) {
      entries_.pop_front();
    }
    if (entries_.size() == kCapacity) {
      entries_.pop_front();
    }
    SessionTombstone tombstone{session_key, generation, state, std::nullopt, {}, now};
    if (cause) {
      tombstone.code = cause->code;
      if (cause->context) {
        tombstone.reason = bounded(*cause->context);
      }
    }
    entries_.push_back(std::move(tombstone));
  }

  /// The tombstone of `session_key`, unless it has expired.
  [[nodiscard]] std::optional<SessionTombstone> find(std::uint64_t session_key,
                                                     SchedulerClock::TimePoint now) const {
    for (const auto& entry : entries_) {
      if (entry.session_key == session_key && !expired(entry, now)) {
        return entry;
      }
    }
    return std::nullopt;
  }

  /// Entries held, expired ones not yet dropped included.
  [[nodiscard]] std::size_t size() const noexcept { return entries_.size(); }

 private:
  [[nodiscard]] static bool expired(const SessionTombstone& entry,
                                    SchedulerClock::TimePoint now) noexcept {
    return now - entry.ended_at >= kLifetime;
  }

  /// At most kMaxReasonBytes of `reason`, cut on a UTF-8 character boundary.
  [[nodiscard]] static std::string bounded(const std::string& reason) {
    if (reason.size() <= kMaxReasonBytes) {
      return reason;
    }
    std::size_t cut = kMaxReasonBytes;
    while (cut > 0 && (static_cast<unsigned char>(reason[cut]) & 0xC0U) == 0x80U) {
      --cut;
    }
    return reason.substr(0, cut);
  }

  std::deque<SessionTombstone> entries_;
};
}  // namespace tensorplate::serving
