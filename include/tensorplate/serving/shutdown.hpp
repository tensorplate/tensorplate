// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F07: Graceful shutdown controller.
//
// The shutdown controller is a small state machine shared by the
// composition root, the request router, the serving pipeline, and
// the HTTP server. It transitions through:
//
//   Running   -> Stopping   (set when shutdown() is called)
//             -> Draining   (set after the HTTP listener closes)
//             -> Stopped    (set after every in-flight request
//                            has completed or been cancelled)
//
// Producers (composition root) drive transitions; consumers (router,
// pipeline, HTTP server) read the state and reject new admission
// once Stopping is set.

#pragma once

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <mutex>
#include <optional>
#include <string>

#include "tensorplate/serving/health.hpp"

namespace tensorplate::serving {

enum class ShutdownPhase : std::uint8_t {
  Running = 0,
  Stopping = 1,
  Draining = 2,
  Stopped = 3,
};

[[nodiscard]] std::string_view to_string(ShutdownPhase phase) noexcept;

class ShutdownController {
 public:
  ShutdownController();
  ~ShutdownController() = default;
  ShutdownController(const ShutdownController&) = delete;
  ShutdownController& operator=(const ShutdownController&) = delete;
  ShutdownController(ShutdownController&&) = delete;
  ShutdownController& operator=(ShutdownController&&) = delete;

  /// Request shutdown. Idempotent. The reason is captured for logs
  /// and /health.
  void request(std::string reason) noexcept;

  /// True if shutdown has been requested (state >= Stopping).
  [[nodiscard]] bool is_stopping() const noexcept;

  /// Phase observer.
  [[nodiscard]] ShutdownPhase phase() const noexcept;

  /// Transition Stopping -> Draining. Called by the composition
  /// root after the HTTP listener has stopped.
  void enter_draining() noexcept;

  /// Transition to Stopped. Called by the composition root after
  /// the scheduler shutdown completes.
  void enter_stopped() noexcept;

  /// Capture the reason recorded by `request`.
  [[nodiscard]] std::optional<std::string> reason() const;

  /// Map the current phase to a `ServingState` for /health.
  [[nodiscard]] ServingState serving_state() const noexcept;

  /// Block until shutdown is requested. Used by `serve_forever`.
  void wait_for_request() noexcept;

  /// Signal whoever is waiting in `wait_for_request`. Tests / signal
  /// handlers call this directly.
  void notify_request() noexcept;

 private:
  std::atomic<ShutdownPhase> phase_{ShutdownPhase::Running};
  mutable std::mutex mutex_;
  std::optional<std::string> reason_;
  std::condition_variable cv_;
};

}  // namespace tensorplate::serving
