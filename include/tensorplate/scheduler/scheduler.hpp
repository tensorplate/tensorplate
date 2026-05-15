// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F01-T01: Public InferScheduler interface.
//
// The scheduler sits between the input adapter / serving-worker and the
// execution session. It is the single point that:
//
//   1. Admits requests against capacity, memory, and monotonic-deadline
//      feasibility (F01 + F03 + F06).
//   2. Holds an ordered queue of admitted-but-undispatched requests
//      (F02).
//   3. Hands the next request to the executor via next() (F02).
//   4. Records completion to remove in-flight accounting (F04).
//   5. Cancels and expires requests, releasing BufferRef payloads via
//      the V01-E03 cleanup helpers (F04).
//   6. Surfaces queue/in-flight/wait/admission/expiry/cancellation
//      counters and emits bounded events for telemetry (F05).
//   7. Accepts memory- and thermal-pressure signals (F06).
//
// The interface is **non-templated and runtime-polymorphic** so
// serving-worker code can hold an `InferScheduler*` and never branch on
// the concrete scheduler type. The Strategy Pattern selects the
// concrete implementation through `make_scheduler` / the factory in
// `factory.hpp`. SmolVLA-style async chunk requests and stale-request
// cancellation are expressible through the same interface used by
// synchronous vision requests (F07).
//
// No vendor SDK type appears in this header.

#pragma once

#include <chrono>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/scheduler/pressure.hpp"
#include "tensorplate/scheduler/scheduler_request.hpp"

namespace tensorplate {

class BufferManager;

// -- Configuration -------------------------------------------------------

/// Configuration for any InferScheduler implementation. Validated by
/// the factory; invalid values surface as Error::Code::ConfigInvalid.
struct SchedulerConfig {
  /// Stable, low-cardinality policy key. v0.1.0 supports "fifo".
  /// Unknown values are rejected by the factory.
  std::string policy = "fifo";

  /// Maximum number of admitted-but-undispatched requests retained in
  /// the queue at once. Admission past this returns
  /// Error::Code::OOMError (overload). Must be > 0.
  std::size_t queue_capacity = 64;

  /// Maximum number of concurrently in-flight requests (dispatched but
  /// not yet completed). Must be > 0. v0.1.0 defaults to 1 to reflect
  /// the single-session serving model; the scheduler does not assume
  /// any particular concurrency model above this number.
  std::size_t in_flight_capacity = 1;

  /// Deadline-margin: the scheduler rejects new admission whose
  /// estimated completion exceeds `deadline + margin`. Equivalently,
  /// the scheduler will accept a deadline that is at most `margin`
  /// later than the estimated completion. Must be >= 0. v0.1.0
  /// defaults to 5 ms, a conservative robotics-friendly margin.
  std::chrono::milliseconds deadline_margin{5};

  /// Service-time estimate used when a request carries no per-request
  /// estimate. Must be >= 0. The default (10 ms) is a placeholder
  /// large enough that vision/VLA workloads typically come in under
  /// it but small enough that the deadline rejection path is
  /// exercisable by tests.
  std::chrono::milliseconds default_service_estimate{10};

  /// Severity threshold at which pressure-signal admission rejection
  /// kicks in. Severities >= this value cause `admit()` to return
  /// Error::Code::OOMError until the most recent signal drops back
  /// below the threshold. Defaults to Critical so Warning-level
  /// signals are recorded but do not reject new work.
  PressureSeverity pressure_reject_threshold = PressureSeverity::Critical;
};

// -- Event types ---------------------------------------------------------

/// Kind tag for `SchedulerEvent`. Stable lowercase snake_case wire
/// names (see `to_string(SchedulerEventKind)`).
enum class SchedulerEventKind : std::uint8_t {
  Admitted = 0,
  AdmissionRejected = 1,
  Dispatched = 2,
  Completed = 3,
  Cancelled = 4,
  Expired = 5,
  MemoryPressure = 6,
  ThermalPressure = 7,
};

[[nodiscard]] std::string_view to_string(SchedulerEventKind kind) noexcept;
[[nodiscard]] std::optional<SchedulerEventKind> scheduler_event_kind_from_string(
    std::string_view name) noexcept;

/// Outcome status recorded by `on_completion`.
enum class CompletionStatus : std::uint8_t {
  /// Adapter returned a successful InferResult.
  Success = 0,
  /// Adapter surfaced a typed failure InferResult.
  Failure = 1,
};

[[nodiscard]] std::string_view to_string(CompletionStatus status) noexcept;

/// Reason recorded by `cancel`. The value is echoed on the
/// SchedulerEvent so consumers can attribute cancellations.
enum class CancellationReason : std::uint8_t {
  /// Caller-initiated cancellation (e.g. HTTP client disconnect, CLI).
  ClientRequest = 0,
  /// LeRobot-style stale-sequence cancellation; queued sequences whose
  /// `action_chunk_sequence` is <= the active `stale_after_sequence`
  /// are cancelled with this reason.
  StaleSequence = 1,
  /// Cancellation forced by graceful-shutdown drain.
  Shutdown = 2,
  /// Cancellation forced by a pressure-based eviction policy. Not
  /// emitted by v0.1.0 baseline policy but reserved.
  Pressure = 3,
};

[[nodiscard]] std::string_view to_string(CancellationReason reason) noexcept;

/// Bounded scheduler event record. The runtime emits one per state
/// transition (admit/reject/dispatch/complete/cancel/expire/pressure).
/// Labels are explicit fields so downstream observability (V01-E12)
/// can scrape them without parsing free text.
struct SchedulerEvent {
  SchedulerEventKind kind = SchedulerEventKind::Admitted;

  /// Originating request id. Empty for some pressure events.
  std::string request_id;

  /// Routing identity from the request envelope.
  std::string endpoint;

  /// Backend that owns or would own the request. Stable label.
  std::string backend_name;

  /// Model identifier from the request envelope. Stable label.
  std::string model_id;

  /// Stable policy label (e.g. "fifo"). Echoed on every event.
  std::string policy;

  /// Optional bounded error code carried on rejection/expiry/
  /// cancellation events. nullopt on success paths.
  std::optional<Error::Code> error_code;

  /// Outcome status, populated for Completed events.
  std::optional<CompletionStatus> completion_status;

  /// Cancellation reason, populated for Cancelled events.
  std::optional<CancellationReason> cancellation_reason;

  /// Pressure source, populated for MemoryPressure / ThermalPressure
  /// events. Mirrors the dispatched signal's source.
  std::optional<PressureSource> pressure_source;

  /// Pressure severity, populated for MemoryPressure / ThermalPressure
  /// events.
  std::optional<PressureSeverity> pressure_severity;

  /// Wait-time in the queue. Populated on Dispatched, Cancelled, and
  /// Expired events for requests that were queued; zero otherwise.
  SchedulerClock::Duration wait_time{0};

  /// Monotonic timestamp at which the event was recorded.
  SchedulerClock::TimePoint timestamp{};
};

/// Sink for scheduler events. Implementations must be safe to call from
/// the scheduler hot path: emission must be non-blocking and must not
/// throw. The scheduler invokes `on_event` inside a defensive
/// `try { ... } catch (...) {}`; a misbehaving sink will not corrupt
/// scheduler state, but it will be observably slower.
class SchedulerEventSink {
 public:
  virtual ~SchedulerEventSink() = default;
  virtual void on_event(const SchedulerEvent& event) = 0;
};

// -- Metrics snapshot ----------------------------------------------------

/// Snapshot of scheduler counters and aggregates. Cheap to capture
/// (single mutex acquisition in the canonical implementation); safe
/// to share. The snapshot never carries pointers to scheduler state.
struct SchedulerMetrics {
  /// Policy label echoed back from the scheduler config.
  std::string policy;

  /// Current admitted-but-undispatched depth.
  std::size_t queue_depth = 0;
  /// Largest queue depth observed since scheduler construction.
  std::size_t queue_depth_high_water = 0;
  /// Current dispatched-but-not-completed count.
  std::size_t in_flight = 0;
  /// Largest in-flight count observed since scheduler construction.
  std::size_t in_flight_high_water = 0;

  /// Monotonic counters.
  std::uint64_t admitted_total = 0;
  std::uint64_t admission_rejected_overload = 0;
  std::uint64_t admission_rejected_deadline = 0;
  std::uint64_t admission_rejected_pressure = 0;
  std::uint64_t expired_total = 0;
  std::uint64_t cancelled_queued = 0;
  std::uint64_t cancelled_in_flight = 0;
  std::uint64_t completed_success = 0;
  std::uint64_t completed_failure = 0;
  std::uint64_t pressure_events_memory = 0;
  std::uint64_t pressure_events_thermal = 0;

  /// Wait-time aggregation across requests that were dispatched,
  /// cancelled, or expired (i.e. removed from the queue). Mean is
  /// derivable from sum / samples.
  SchedulerClock::Duration wait_time_sum{0};
  std::uint64_t wait_time_samples = 0;
  SchedulerClock::Duration wait_time_max{0};

  /// Last observed memory- and thermal-pressure severity. v0.1.0
  /// stores only the most recent severity per source; older values
  /// are not retained.
  PressureSeverity last_memory_severity = PressureSeverity::Normal;
  PressureSeverity last_thermal_severity = PressureSeverity::Normal;
};

// -- Runtime hooks -------------------------------------------------------

/// Runtime hooks for scheduler construction.
///
/// `event_sink`     : fire-and-forget event emission. nullptr disables.
/// `buffer_manager` : used by cancel/expire/shutdown paths to release
///                    the input BufferRefs of removed requests through
///                    `release_request_buffers` (V01-E03). nullptr
///                    disables that cleanup; tests that pass nullptr
///                    must release buffers themselves.
/// `clock`          : monotonic clock. nullptr selects the system
///                    clock; tests inject a FakeSchedulerClock.
struct SchedulerRuntimeHooks {
  SchedulerEventSink* event_sink = nullptr;
  BufferManager* buffer_manager = nullptr;
  const SchedulerClock* clock = nullptr;
};

// -- The scheduler contract ----------------------------------------------

/// Public InferScheduler interface. Implementations follow the
/// Strategy Pattern (see `factory.hpp` and `docs/architecture/
/// scheduler.md`). Executor / serving-worker code must hold the
/// scheduler through an `InferScheduler*` (or `unique_ptr`) and never
/// branch on the concrete subtype.
///
/// **Threading.** All methods are safe to call concurrently from
/// multiple threads. Implementations document any per-method
/// granularity (the v0.1.0 FIFO scheduler uses a single mutex over
/// queue + in-flight state).
///
/// **Monotonic time.** All deadline and wait-time decisions use the
/// injected `SchedulerClock`. Wall-clock time is never consulted.
class InferScheduler {
 public:
  InferScheduler() noexcept = default;
  virtual ~InferScheduler() = default;

  InferScheduler(const InferScheduler&) = delete;
  InferScheduler& operator=(const InferScheduler&) = delete;
  InferScheduler(InferScheduler&&) = delete;
  InferScheduler& operator=(InferScheduler&&) = delete;

  /// Admit `request` to the scheduler. Returns Ok on admission;
  /// returns a typed error on rejection. Successful admission
  /// implicitly enqueues the request; the scheduler now owns the
  /// SchedulerRequest (and its BufferRefs) until `next` removes it.
  ///
  /// Typed rejection codes:
  ///   - Error::Code::OOMError      : queue capacity exceeded, or
  ///                                  active pressure severity
  ///                                  is at/above the configured
  ///                                  rejection threshold.
  ///   - Error::Code::Timeout       : request's monotonic deadline is
  ///                                  already past, or estimated
  ///                                  completion exceeds deadline +
  ///                                  margin.
  ///   - Error::Code::ConfigInvalid : malformed envelope (empty
  ///                                  request_id, etc.).
  ///
  /// On rejection, the scheduler releases the request's input buffers
  /// through `BufferManager::release_if_owned` if a buffer manager
  /// was provided in runtime hooks; otherwise the caller retains
  /// ownership of the rejected request via the moved-out
  /// SchedulerRequest passed in, which is destroyed at function
  /// return.
  ///
  /// On success the scheduler emits a `SchedulerEventKind::Admitted`
  /// event; on rejection it emits `AdmissionRejected` with the typed
  /// error code populated.
  virtual Result<void> admit(SchedulerRequest request) = 0;

  /// Dispatch the next admitted request, if any. Returns std::nullopt
  /// if the queue is empty, if `in_flight >= in_flight_capacity`, or
  /// if every queued request is currently stale/expired (the
  /// scheduler removes them through the expire path before this
  /// method returns).
  ///
  /// On success the scheduler moves the request out of the queue,
  /// records it as in-flight, emits a `Dispatched` event with
  /// `wait_time` populated, and returns the SchedulerRequest. The
  /// caller is responsible for invoking `on_completion` once
  /// inference finishes (success or failure).
  [[nodiscard]] virtual std::optional<SchedulerRequest> next() = 0;

  /// Record completion of an in-flight request. `status` distinguishes
  /// success and adapter-side failure. Duplicate completion of the
  /// same request id (e.g. an adapter that double-completes) returns
  /// `Error::Code::Internal` and does not change accounting.
  ///
  /// Output BufferRef cleanup is *not* the scheduler's job: the
  /// session NVI wrapper publishes the InferResult and the consumer
  /// owns the returned BufferRefs. `on_completion` only updates
  /// in-flight accounting and emits the completion event.
  virtual Result<void> on_completion(std::string_view request_id, CompletionStatus status,
                                     std::optional<Error::Code> error_code = std::nullopt) = 0;

  /// Cancel a request by id. Targets queued and in-flight requests.
  ///
  /// Behavior:
  ///   - Queued: the SchedulerRequest is removed from the queue. If a
  ///     buffer manager was provided, the request's input buffers are
  ///     released via `release_request_buffers`. Emits `Cancelled`.
  ///   - In-flight: the cancellation intent is recorded so adapters
  ///     that poll cancellation can drop work, and a Cancelled event
  ///     is emitted. The actual buffer release for in-flight requests
  ///     remains the executor's responsibility because the scheduler
  ///     no longer holds the SchedulerRequest. The scheduler still
  ///     clears in-flight accounting on this call so a separately
  ///     racing on_completion is a typed no-op.
  ///   - Unknown id: returns `Error::Code::NotReady`. Tests that
  ///     cancel a missing or already-completed id receive a typed
  ///     no-op response, not a corruption.
  virtual Result<void> cancel(std::string_view request_id, CancellationReason reason) = 0;

  /// Sweep the queue and remove every request whose monotonic deadline
  /// has passed or whose estimated completion now exceeds deadline plus
  /// the configured margin. Each removed request emits `Expired` with
  /// the wait time populated and the request's input buffers are
  /// released through the buffer manager when one is configured.
  ///
  /// Returns the number of requests removed.
  virtual std::size_t expire_due() = 0;

  /// Deliver a pressure signal. The scheduler records the signal,
  /// emits the appropriate Memory/Thermal pressure event, and updates
  /// the active severity used by `admit()`. v0.1.0 baseline policy
  /// never cancels or evicts queued work as a direct result of a
  /// pressure signal; severity above the configured threshold only
  /// affects new admission decisions.
  virtual void on_pressure(const PressureSignal& signal) = 0;

  /// Cancel every queued and in-flight request, releasing buffers
  /// deterministically. Used by serving-worker graceful shutdown
  /// (V01-E07). Returns the number of requests cancelled. After
  /// shutdown the scheduler rejects further admission with
  /// `Error::Code::NotReady`.
  virtual std::size_t shutdown() = 0;

  /// Snapshot of current metrics.
  [[nodiscard]] virtual SchedulerMetrics metrics() const = 0;

  /// Stable policy label echoed on every event.
  [[nodiscard]] virtual std::string_view policy_name() const noexcept = 0;
};

// -- Concept (compile-time interface check) ------------------------------

/// Concept used by tests and by adapter-side helpers to assert that a
/// concrete implementation type satisfies the scheduler contract at
/// compile time without exposing the concrete type to executor code.
///
/// Concrete schedulers should add a static_assert at the bottom of
/// their definition:
///
///   static_assert(InferSchedulerConcept<FifoScheduler>);
// clang-format off
template <typename T>
concept InferSchedulerConcept = std::derived_from<T, InferScheduler> && requires(
    T t,
    SchedulerRequest req,
    std::string_view id,
    CompletionStatus status,
    CancellationReason reason,
    std::optional<Error::Code> err,
    const PressureSignal& sig) {
  { t.admit(std::move(req)) } -> std::same_as<Result<void>>;
  { t.next() } -> std::same_as<std::optional<SchedulerRequest>>;
  { t.on_completion(id, status, err) } -> std::same_as<Result<void>>;
  { t.cancel(id, reason) } -> std::same_as<Result<void>>;
  { t.expire_due() } -> std::same_as<std::size_t>;
  { t.on_pressure(sig) } -> std::same_as<void>;
  { t.shutdown() } -> std::same_as<std::size_t>;
  { t.metrics() } -> std::same_as<SchedulerMetrics>;
  { t.policy_name() } -> std::same_as<std::string_view>;
};
// clang-format on

}  // namespace tensorplate
