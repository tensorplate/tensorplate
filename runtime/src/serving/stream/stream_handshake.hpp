// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <algorithm>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <mutex>
#include <optional>
#include <utility>
#include <vector>

#include "tensorplate/scheduler/clock.hpp"

#include "serving/stream/stream_output.hpp"

namespace tensorplate::serving::stream {

/// How long a call may take to send its first message, on the listener's
/// clock.
inline constexpr std::chrono::seconds kOpenHandshakeTimeout{5};
/// Calls a listener waits on at one time for their first message.
inline constexpr std::size_t kMaxOpenHandshakes = 32;
/// Refused calls a listener keeps at one time over all its connections, each
/// from its refusal until it is done. A bound on memory: a refused call that
/// is not done was measured at about 23.6 KB, so 4,096 of them hold roughly
/// 100 MB. What bounds one connection is its 64 concurrent streams.
inline constexpr std::size_t kMaxRefusedCalls = 4'096;

/// The calls of one listener whose first message has not been read: at most
/// kMaxOpenHandshakes, each until kOpenHandshakeTimeout after it began, and
/// none once closed. A call leaves once, and whoever takes it out answers it:
/// its reader, or the bound that refuses it. A call is waited on only with
/// its first read started, and a call given no place starts none.
///
/// Thread-safe. The caller supplies `now`: nothing here reads a clock.
class OpenHandshakes {
 public:
  using Call = std::shared_ptr<CallOutput>;
  enum class Begun : std::uint8_t { Yes, Full, Closed };

  /// Begins the wait for `call`'s first message at `now`, and only then runs
  /// `start_read`, before a bound can take the call. `start_read` must not
  /// call back into this object.
  template <typename StartRead>
  [[nodiscard]] Begun begin(Call call, SchedulerClock::TimePoint now, StartRead&& start_read) {
    const std::lock_guard guard(mu_);
    if (closed_) {
      return Begun::Closed;
    }
    if (waiting_.size() >= kMaxOpenHandshakes) {
      return Begun::Full;
    }
    waiting_.push_back({std::move(call), now + kOpenHandshakeTimeout});
    std::forward<StartRead>(start_read)();
    return Begun::Yes;
  }

  /// The call's first read was answered.
  /// @return false when a bound took the call first: it has been refused.
  [[nodiscard]] bool end(const CallOutput& call) {
    const std::lock_guard guard(mu_);
    const auto it = std::find_if(waiting_.begin(), waiting_.end(), [&](const Waiting& waiting) {
      return waiting.call.get() == &call;
    });
    if (it == waiting_.end()) {
      return false;
    }
    waiting_.erase(it);
    return true;
  }

  /// Takes the calls whose first message is overdue at `now`, in the order
  /// they began.
  [[nodiscard]] std::vector<Call> take_overdue(SchedulerClock::TimePoint now) {
    const std::lock_guard guard(mu_);
    std::vector<Call> overdue;
    std::vector<Waiting> kept;
    for (auto& waiting : waiting_) {
      if (now < waiting.due) {
        kept.push_back(std::move(waiting));
      } else {
        overdue.push_back(std::move(waiting.call));
      }
    }
    waiting_ = std::move(kept);
    return overdue;
  }

  /// When the next call still waited on becomes overdue.
  [[nodiscard]] std::optional<SchedulerClock::TimePoint> next_due() const {
    const std::lock_guard guard(mu_);
    std::optional<SchedulerClock::TimePoint> next;
    for (const auto& waiting : waiting_) {
      if (!next || waiting.due < *next) {
        next = waiting.due;
      }
    }
    return next;
  }

  /// Takes every call, in the order they began, and begins no wait from now
  /// on.
  [[nodiscard]] std::vector<Call> close() {
    const std::lock_guard guard(mu_);
    closed_ = true;
    std::vector<Call> taken;
    taken.reserve(waiting_.size());
    for (auto& waiting : waiting_) {
      taken.push_back(std::move(waiting.call));
    }
    waiting_.clear();
    return taken;
  }

  [[nodiscard]] std::size_t size() const {
    const std::lock_guard guard(mu_);
    return waiting_.size();
  }

 private:
  struct Waiting {
    Call call;
    SchedulerClock::TimePoint due;
  };

  mutable std::mutex mu_;
  std::vector<Waiting> waiting_;
  bool closed_ = false;
};

/// The places of one listener for calls it refused: a refused call lasts
/// until the transport has sent its status, which a peer that does not read
/// puts off for as long as it likes. A call keeps its place from its refusal
/// until it is done, whether or not its peer reads; CallOutput::refuse()
/// cancels a call refused with none left.
///
/// Thread-safe.
class RefusedCalls {
 public:
  explicit RefusedCalls(std::size_t places = kMaxRefusedCalls) noexcept : places_(places) {}

  /// Takes a place. False when none is left.
  [[nodiscard]] bool enter() {
    const std::lock_guard guard(mu_);
    if (held_ >= places_) {
      return false;
    }
    ++held_;
    return true;
  }

  /// Gives back a place enter() took.
  void leave() {
    const std::lock_guard guard(mu_);
    --held_;
  }

  [[nodiscard]] std::size_t size() const {
    const std::lock_guard guard(mu_);
    return held_;
  }

 private:
  mutable std::mutex mu_;
  std::size_t places_;
  std::size_t held_ = 0;
};

}  // namespace tensorplate::serving::stream
