// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F01-T02: Public composition root for `tensorplate-serving`.
//
// `ServingWorker` owns the runtime components built from a validated
// `ServingConfig`: a `BufferManager`, a `BackendRegistry`-resolved
// `ExecutionSession`, a `SchedulerPolicyRegistry`-resolved
// `InferScheduler`, the request router, the loopback HTTP server,
// metrics, health, and the shutdown controller. The composition root
// is intentionally narrow: it wires components in a single order and
// exposes a minimal start/stop surface.
//
// The public lifecycle is:
//
//   1. ServingWorker::create(config)  - validates config, builds
//      components, returns a unique_ptr.
//   2. worker->start()                - opens the HTTP listener,
//      transitions HealthState to Ready / Degraded.
//   3. worker->serve_forever()        - blocks until shutdown.
//   4. worker->shutdown(reason)       - asynchronous shutdown.
//   5. worker->stop()                 - blocks until drain completes.
//
// Tests usually call start() + stop() inline; production runs use
// serve_forever() with a signal-driven shutdown.

#pragma once

#include <atomic>
#include <chrono>
#include <cstdint>
#include <memory>
#include <string>
#include <string_view>

#include "tensorplate/core/result.hpp"
#include "tensorplate/serving/config.hpp"

namespace tensorplate {

class BufferManager;
class InferScheduler;
class BackendRegistry;

namespace serving {

class HttpServer;
class RequestRouter;
class ServingPipeline;
class AsyncPolicyStore;
class ShutdownController;

}  // namespace serving

class HealthState;
class ServingMetrics;

/// Exit-code policy. Stable across v0.1.0; surfaced from `main`.
enum class ServingExitCode : int {
  /// Normal shutdown after start.
  Ok = 0,
  /// Configuration parse / validation failure.
  ConfigError = 64,
  /// Component build / load failure (registry, session, scheduler).
  LoadError = 65,
  /// Listener bind / accept failure.
  ServeError = 66,
  /// Catch-all internal error.
  Internal = 70,
};

/// Composition root for the serving worker. See file header.
class ServingWorker {
 public:
  /// Validate the config and construct every owned runtime component.
  /// On failure, returns the typed error from validation or component
  /// initialization. The returned worker is not yet listening; call
  /// `start()` to open the HTTP listener.
  [[nodiscard]] static Result<std::unique_ptr<ServingWorker>> create(ServingConfig config);

  /// Alternate factory that accepts a caller-owned `BackendRegistry`.
  /// Tests use this overload to install mock backends without
  /// touching the process-wide global. The registry must outlive
  /// every session this worker creates; in practice this means the
  /// caller owns the registry for the entire worker lifetime.
  [[nodiscard]] static Result<std::unique_ptr<ServingWorker>> create(
      ServingConfig config, BackendRegistry& backend_registry);

  ~ServingWorker();

  ServingWorker(const ServingWorker&) = delete;
  ServingWorker& operator=(const ServingWorker&) = delete;
  ServingWorker(ServingWorker&&) = delete;
  ServingWorker& operator=(ServingWorker&&) = delete;

  /// Open the HTTP listener and start serving. Idempotent: a second
  /// call returns Ok if the worker is already serving.
  [[nodiscard]] Result<void> start();

  /// Block until shutdown is requested and drained.
  /// Returns the exit code the binary should use.
  [[nodiscard]] ServingExitCode serve_forever();

  /// Begin graceful shutdown. Non-blocking. Use `stop()` to wait for
  /// the drain to complete.
  void shutdown(std::string_view reason) noexcept;

  /// Block until shutdown completes. Safe to call multiple times.
  /// Returns the final exit code.
  [[nodiscard]] ServingExitCode stop();

  /// Return the port the HTTP listener bound to. Useful when the
  /// config requested an ephemeral port (port = 0); returns 0 until
  /// `start()` succeeds.
  [[nodiscard]] std::uint16_t bound_port() const noexcept;

  /// Live config snapshot. Used by `/health` and tests.
  [[nodiscard]] const ServingConfig& config() const noexcept;

  /// Live accessors used by tests and by the HTTP route handlers.
  [[nodiscard]] HealthState& health() noexcept;
  [[nodiscard]] ServingMetrics& metrics() noexcept;
  [[nodiscard]] BufferManager& buffer_manager() noexcept;
  [[nodiscard]] InferScheduler& scheduler() noexcept;
  [[nodiscard]] serving::AsyncPolicyStore& async_store() noexcept;
  [[nodiscard]] serving::RequestRouter& router() noexcept;

 private:
  struct Impl;
  explicit ServingWorker(std::unique_ptr<Impl> impl) noexcept;
  std::unique_ptr<Impl> impl_;
};

}  // namespace tensorplate
