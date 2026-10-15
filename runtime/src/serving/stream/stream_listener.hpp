// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>

#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {

class SessionDispatch;

/// What stop() keeps of the drain window for the terminal events of the
/// sessions it fails: gRPC cancels a call at the deadline whether or not its
/// last event was written.
inline constexpr std::chrono::milliseconds kTerminalEventReserve{250};

struct StreamListenerOptions {
  /// A loopback literal unless `allow_non_loopback`, as for the HTTP bind.
  std::string host = "127.0.0.1";
  /// 0 requests an ephemeral port.
  std::uint16_t port = 0;
  bool allow_non_loopback = false;
  /// The deployment an Open must name; the listener serves no generation and
  /// no descriptor digest.
  std::string deployment_id;
  /// Session timers and the session count; the worker serves one at a time.
  SessionLimits limits = SessionLimits::defaults();
  /// Places for refused calls that are not done; the listener's own 4,096
  /// when unset, which the worker keeps. Set by tests only.
  std::optional<std::size_t> max_refused_calls;
};

/// The gRPC SessionService of one worker: one call per logical session, each
/// driving the SessionManager it owns and the dispatch behind it.
///
/// Uses the callback API: no thread per call; gRPC's own threads run the
/// reactions, which never wait on a job. One thread of its own refuses a call
/// whose Open is overdue and cancels a call that outruns the no-progress limit
/// after its session has ended. Without a dispatch every Open is refused with
/// `backend_unsupported_capability`. A connection carries at most 64 calls at
/// one time: gRPC refuses a stream beyond them before the listener sees it.
class StreamListener {
 public:
  /// Binds and starts serving. `dispatch`, when given, is attached here, is
  /// stopped by stop() and must outlive the listener. `clock` must too.
  [[nodiscard]] static Result<std::unique_ptr<StreamListener>> start(StreamListenerOptions options,
                                                                     SessionDispatch* dispatch,
                                                                     const SchedulerClock& clock);

  /// stop() with no time left for the drain.
  ~StreamListener();
  StreamListener(const StreamListener&) = delete;
  StreamListener& operator=(const StreamListener&) = delete;

  [[nodiscard]] std::uint16_t bound_port() const noexcept;

  /// Refuses every call that has not sent its Open, and every later call
  /// when it arrives, with `admission_closed`, and drains every live session
  /// with `worker_shutdown`. Does not wait.
  void stop_admission() noexcept;

  /// stop_admission(), then waits for every session to close until
  /// kTerminalEventReserve before `deadline`, fails those still open with
  /// `worker_shutdown` unless their drain completed, waits for the calls
  /// until `deadline`, cancels those still open, stops the dispatch and
  /// releases the manager. Safe to repeat.
  void stop(std::chrono::steady_clock::time_point deadline) noexcept;

  /// Sessions holding a slot; after stop(), those that still held one when it
  /// released the manager.
  [[nodiscard]] std::size_t live_sessions() const noexcept;

  /// Calls not yet over on this side: a session's call outlives its slot
  /// until its last event is written or the call is cancelled.
  [[nodiscard]] std::size_t open_calls() const noexcept;

  /// Calls whose first message has not been read: at most 32, each for at
  /// most the open handshake timeout.
  [[nodiscard]] std::size_t awaited_calls() const noexcept;

  /// Refused calls that are not done and hold a place: at most 4,096 over
  /// all connections. A call refused beyond them is cancelled and holds none.
  [[nodiscard]] std::size_t refused_calls() const noexcept;

  /// Says that the clock was moved by hand. The watch thread sleeps until an
  /// instant of the clock, which it takes as the steady clock's: a clock that
  /// jumps says so here, and the system clock never has to.
  void notify_clock_advanced() noexcept;

  struct Impl;

 private:
  explicit StreamListener(std::unique_ptr<Impl> impl) noexcept;
  std::unique_ptr<Impl> impl_;
};

}  // namespace tensorplate::serving
