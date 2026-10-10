// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstdint>
#include <memory>
#include <string>

#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {

class SessionDispatch;

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
};

/// The gRPC SessionService of one worker: one call per logical session, each
/// driving the SessionManager it owns and the dispatch behind it.
///
/// Uses the callback API: no thread per call; gRPC's own threads run the
/// reactions, which never wait on a job. Without a dispatch every Open is
/// refused with `backend_unsupported_capability`.
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

  /// Refuses every later Open with `admission_closed` and drains every live
  /// session with `worker_shutdown`. Does not wait.
  void stop_admission() noexcept;

  /// stop_admission(), then waits until `deadline` for every session to
  /// close, cancels the calls still open, stops the dispatch and releases
  /// the manager. Safe to repeat.
  void stop(std::chrono::steady_clock::time_point deadline) noexcept;

  /// Sessions holding a slot; after stop(), those that still held one when it
  /// released the manager.
  [[nodiscard]] std::size_t live_sessions() const noexcept;

  struct Impl;

 private:
  explicit StreamListener(std::unique_ptr<Impl> impl) noexcept;
  std::unique_ptr<Impl> impl_;
};

}  // namespace tensorplate::serving
