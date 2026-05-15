// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F06: Serving-worker health state.
//
// The serving worker publishes a coarse-grained ready/degraded/failed/
// stopping/draining/stopped state through `/health`. The state is
// computed from:
//
//   - composition-root startup readiness (config + components built),
//   - active session readiness (`ExecutionSession::is_ready`),
//   - the most recent typed error from the session, scheduler, or
//     adapter,
//   - the shutdown controller's drain state.
//
// State transitions are driven by the composition root and the
// scheduler/session event sinks; clients should treat the state as a
// snapshot taken at the moment `/health` was served.
//
// The state schema mirrors `protocol/schemas/serving_health.json`. The
// names below are part of the wire contract.

#pragma once

#include <chrono>
#include <cstdint>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>

#include "tensorplate/core/error.hpp"

namespace tensorplate {

/// Serving-worker health state. Stable lowercase snake_case wire names.
///
///   Starting     The process is wiring components. `/health` returns
///                503 with state `starting`.
///   Ready        Active session is `Ready`. Sync `/infer` and async
///                policy routes accept work. HTTP returns 200.
///   Degraded     Active session loaded but not yet primed, or a
///                non-fatal pressure / cancellation backlog was
///                observed. HTTP returns 200 so liveness probes do
///                not flap; agents are expected to read the
///                discriminator field, not just the HTTP status.
///   Failed       Active session is in the `Failed` state, or the
///                adapter/session lifecycle reported a fatal error.
///                HTTP returns 503.
///   Stopping     Graceful shutdown began but draining has not yet
///                completed. Admission is closed. HTTP returns 503.
///   Draining     Admission is closed and remaining in-flight work
///                is finishing. HTTP returns 503.
///   Stopped     Shutdown completed; the worker is about to exit.
///                HTTP returns 503.
enum class ServingState : std::uint8_t {
  Starting = 0,
  Ready = 1,
  Degraded = 2,
  Failed = 3,
  Stopping = 4,
  Draining = 5,
  Stopped = 6,
};

[[nodiscard]] std::string_view to_string(ServingState state) noexcept;
[[nodiscard]] std::optional<ServingState> serving_state_from_string(std::string_view name) noexcept;

/// Health snapshot emitted by `HealthState::snapshot()`. Used both by
/// `/health` JSON serialization and by structured logs.
struct HealthSnapshot {
  ServingState state = ServingState::Starting;
  std::string endpoint;
  std::string backend;
  std::optional<std::string> active_model_id;
  /// Most recent typed error code observed by the serving pipeline.
  std::optional<Error::Code> last_error_code;
  std::optional<std::string> last_error_message;
  /// Scheduler queue depth at the moment of the snapshot.
  std::size_t queue_depth = 0;
  /// Scheduler in-flight count at the moment of the snapshot.
  std::size_t in_flight = 0;
  /// Monotonic time the worker entered its current state, in
  /// nanoseconds since process steady-clock epoch.
  std::int64_t state_since_steady_ns = 0;
};

/// Thread-safe health-state container. Producers (the composition
/// root, the scheduler/session event sinks, and the shutdown
/// controller) drive state transitions through dedicated methods so
/// the wire-contract semantics live in one place. Consumers obtain a
/// `HealthSnapshot` and serialize it through `serialize_health_json`.
class HealthState {
 public:
  HealthState() = default;
  ~HealthState() = default;
  HealthState(const HealthState&) = delete;
  HealthState& operator=(const HealthState&) = delete;
  HealthState(HealthState&&) = delete;
  HealthState& operator=(HealthState&&) = delete;

  /// Capture the active endpoint / backend / model labels. Safe to
  /// call from the composition root at startup.
  void set_identity(std::string endpoint, std::string backend,
                    std::optional<std::string> model_id) noexcept;

  /// Transition to a new state. Records the monotonic transition
  /// timestamp; concurrent transitions resolve last-writer-wins.
  void set_state(ServingState next) noexcept;

  /// Record a typed error for the snapshot. Useful for surfacing the
  /// most recent adapter / scheduler failure without losing the
  /// state machine.
  void record_error(Error error) noexcept;

  /// Record live scheduler accounting. Cheap; intended to be called
  /// from a scheduler-event-sink adapter that already runs off the
  /// hot path.
  void record_queue_state(std::size_t queue_depth, std::size_t in_flight) noexcept;

  [[nodiscard]] ServingState state() const noexcept;
  [[nodiscard]] HealthSnapshot snapshot() const;

 private:
  mutable std::mutex mutex_;
  HealthSnapshot snap_{};
};

/// Serialize a health snapshot to JSON. The schema is documented in
/// `protocol/schemas/serving_health.json` and is stable for v0.1.0.
[[nodiscard]] std::string serialize_health_json(const HealthSnapshot& snap);

/// Map a serving state to the HTTP status used by `/health`. Ready /
/// degraded return 200; everything else returns 503. Agents should
/// rely on the `state` discriminator rather than the HTTP status to
/// distinguish degraded from ready.
[[nodiscard]] int health_http_status(ServingState state) noexcept;

}  // namespace tensorplate
