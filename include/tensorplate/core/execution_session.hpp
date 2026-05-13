// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F01-T02: Public ExecutionSession lifecycle interface.
//
// `tensorplate::ExecutionSession` is the backend-neutral execution
// contract that every adapter (TensorRT, LibTorch, Python/PyTorch
// sidecar, and a future Vitis AI adapter) implements and that higher
// runtime layers call.
//
// The public lifecycle methods are **non-virtual** wrappers (NVI). They
// enforce readiness, validation, monotonic latency stamping, and event
// emission, then delegate to **protected** `do_*` implementation methods
// that adapters override. Callers cannot bypass the NVI guarantees.
//
// Public method set (exact, see docs/architecture/execution-session.md
// for the canonical-name decision):
//
//   load          - load the model artifact described by ModelSpec
//   prime         - adapter readiness / fixed-shape binding / warmup
//   infer         - synchronous inference, returns InferResult
//   infer_async   - method shape always present; may return Unsupported
//   unload        - release session-owned state
//   is_ready      - observer
//   backend_name  - observer
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
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

class BufferManager;

/// Session lifecycle state. Stable, lowercase, snake_case wire names; see
/// `to_string(SessionState)` and the table in
/// `docs/architecture/execution-session.md`.
enum class SessionState : std::uint8_t {
  /// Initial state. No model loaded.
  Unloaded = 0,
  /// `load` succeeded. `prime` has not yet been called.
  Loaded = 1,
  /// `prime` succeeded. `infer` is permitted.
  Ready = 2,
  /// Last lifecycle call failed. Recover with `unload`.
  Failed = 3,
};

[[nodiscard]] std::string_view to_string(SessionState state) noexcept;
[[nodiscard]] std::optional<SessionState> session_state_from_string(std::string_view name) noexcept;

/// Async-inference handle returned by `infer_async`.
///
/// The handle carries the originating `request_id` plus a session-scoped
/// monotonically increasing `async_id` so the scheduler (V01-E06) and
/// LeRobot-compatible serving (V01-E07) can correlate cancellation and
/// completion observation without changing the public interface. It does
/// not assume threads, CUDA streams, Python futures, or backend-specific
/// handles. v0.1.0 adapters that do not implement native async return
/// `Error::Code::Unsupported` instead of a handle.
struct AsyncInferHandle {
  /// Mirror of the originating `InferRequest::request_id`.
  std::string request_id;

  /// Session-scoped identifier. Monotonically increasing; never zero.
  std::uint64_t async_id = 0;

  friend bool operator==(const AsyncInferHandle& lhs, const AsyncInferHandle& rhs) noexcept {
    return lhs.request_id == rhs.request_id && lhs.async_id == rhs.async_id;
  }
  friend bool operator!=(const AsyncInferHandle& lhs, const AsyncInferHandle& rhs) noexcept {
    return !(lhs == rhs);
  }
};

/// Kind tag for `SessionEvent`. Stable lowercase snake_case wire names; see
/// `to_string(SessionEventKind)`.
enum class SessionEventKind : std::uint8_t {
  LoadStart = 0,
  LoadEnd = 1,
  LoadFailed = 2,
  PrimeStart = 3,
  PrimeEnd = 4,
  PrimeFailed = 5,
  InferStart = 6,
  InferEnd = 7,
  InferFailed = 8,
  InferAsyncStart = 9,
  InferAsyncEnd = 10,
  InferAsyncFailed = 11,
  UnloadStart = 12,
  UnloadEnd = 13,
  UnloadFailed = 14,
  /// Emitted when the NVI wrapper rejects a request before adapter
  /// dispatch (released buffer, out-of-bounds tensor window, etc).
  ValidationFailed = 15,
  /// Emitted by the wrapper when adapters do not report native async
  /// support.
  UnsupportedAsync = 16,
};

[[nodiscard]] std::string_view to_string(SessionEventKind kind) noexcept;
[[nodiscard]] std::optional<SessionEventKind> session_event_kind_from_string(
    std::string_view name) noexcept;

/// In-process session event record. Bounded fields; no raw payload bytes.
///
/// Field semantics:
///   - `kind`           : event kind tag.
///   - `backend_name`   : stable adapter label echoed from `backend_name()`.
///   - `model_id`       : ModelSpec model_id if a model is loaded.
///   - `request_id`     : populated on infer/async events.
///   - `error_code`     : populated on `*_Failed`, `ValidationFailed`, and
///                        `UnsupportedAsync` events.
///   - `duration`       : monotonic duration of the wrapped operation.
///                        Zero on `*_Start` events.
///   - `state_after`    : session state after the event is recorded.
struct SessionEvent {
  using Duration = std::chrono::nanoseconds;

  SessionEventKind kind = SessionEventKind::ValidationFailed;
  std::string backend_name;
  std::optional<std::string> model_id;
  std::optional<std::string> request_id;
  std::optional<Error::Code> error_code;
  Duration duration{0};
  SessionState state_after = SessionState::Unloaded;
};

/// Sink for `SessionEvent` records. Implementations must be safe to call
/// from the NVI wrapper hot path: emission must be non-blocking and must
/// not allocate unbounded memory. The NVI wrapper invokes `on_event`
/// inside a defensive `try { ... } catch (...) {}`; a sink that throws
/// will not corrupt session state, but it will still be observably
/// slower than a well-behaved sink. Prefer noexcept implementations in
/// production.
class SessionEventSink {
 public:
  virtual ~SessionEventSink() = default;
  virtual void on_event(const SessionEvent& event) = 0;
};

/// Runtime hooks supplied by adapter factories or test mocks when a
/// concrete session is constructed. Kept separate from the public
/// `ExecutionSession` method set so callers see only the lifecycle
/// contract.
struct ExecutionSessionRuntimeHooks {
  /// Optional fire-and-forget event sink. nullptr disables emission.
  SessionEventSink* event_sink = nullptr;

  /// Optional buffer manager used to release partial adapter outputs
  /// after output validation fails. nullptr disables that cleanup path.
  BufferManager* buffer_manager = nullptr;
};

/// Canonical public execution-session lifecycle interface.
///
/// **Non-virtual interface (NVI).** Public lifecycle methods are
/// non-virtual; adapters override the protected `do_*` methods. Public
/// methods enforce readiness, validation, monotonic timing, and event
/// emission before delegating to adapter code.
///
/// **Threading.** Public lifecycle methods are *not* safe to call
/// concurrently on the same session. Concurrency is supplied by the
/// scheduler (V01-E06), which serializes lifecycle calls per session.
/// Const observers (`is_ready`, `backend_name`) are safe to call from
/// any thread without external locking.
///
/// **Ownership.** `ExecutionSession` does not own a `BufferManager` or
/// `SessionEventSink`. Concrete adapters receive optional runtime hooks
/// from their factory and pass them to the protected constructor.
/// Without a manager, the NVI wrapper still validates BufferRef
/// ownership state and TensorView byte windows, but cannot release
/// partial adapter outputs through the buffer plane.
///
/// **Vendor neutrality.** This header pulls in no CUDA, TensorRT,
/// PyTorch/LibTorch, ONNX Runtime, Vitis AI, XRT, or DPU types.
class ExecutionSession {
 public:
  using Clock = std::chrono::steady_clock;

  ExecutionSession() noexcept = default;
  virtual ~ExecutionSession() = default;

  ExecutionSession(const ExecutionSession&) = delete;
  ExecutionSession& operator=(const ExecutionSession&) = delete;
  ExecutionSession(ExecutionSession&&) = delete;
  ExecutionSession& operator=(ExecutionSession&&) = delete;

  // -- Public lifecycle methods (non-virtual). -------------------------------

  /// Load the model artifact described by `spec`. Permitted only from
  /// `Unloaded`. On success, transitions to `Loaded`. On failure,
  /// transitions to `Failed` and the previous model is dropped.
  ///
  /// Errors:
  ///   - `NotReady`: session is not in `Unloaded` state.
  ///   - any typed error returned by the adapter `do_load`.
  Result<void> load(const ModelSpec& spec);

  /// Prime the session for inference. Permitted only from `Loaded`. On
  /// success, transitions to `Ready`. On failure, the state machine
  /// stays in `Loaded` if the adapter reports a recoverable error
  /// (`ConfigInvalid`); otherwise it transitions to `Failed`.
  ///
  /// Errors:
  ///   - `NotReady`: session is not in `Loaded` state.
  ///   - any typed error returned by the adapter `do_prime`.
  Result<void> prime();

  /// Run synchronous inference. Permitted only from `Ready`. Returns an
  /// `InferResult` (success or failure) with `execution_latency` stamped
  /// using `std::chrono::steady_clock`. The session state is not changed
  /// by infer failures; the typed error is returned in the result.
  ///
  /// Returns a fallible `Result<InferResult>` for callers that observe
  /// readiness/validation errors distinctly from adapter failures:
  ///   - `Result::error` carries readiness or validation errors that
  ///     never reached the adapter (e.g. `NotReady`,
  ///     `ConfigInvalid`, `ShapeMismatch` from the NVI wrapper).
  ///   - `Result::value()` is an `InferResult` whose own `error()` field
  ///     carries an adapter-side `InferenceFailed` / `OOMError` /
  ///     `Timeout` / `Internal` typed code with timing populated.
  ///
  /// In all cases, `execution_latency` is recorded around the call to
  /// `do_infer` and event emission is paired.
  Result<InferResult> infer(const InferRequest& request);

  /// Schedule asynchronous inference. The method shape is always
  /// present from v0.1.0. Adapters that do not implement native async
  /// return `Error::Code::Unsupported` and the NVI wrapper does not
  /// allocate output buffers or dispatch to adapter execution. Readiness
  /// and validation errors are surfaced *before* the unsupported check.
  Result<AsyncInferHandle> infer_async(const InferRequest& request);

  /// Release session-owned state and return the session to `Unloaded`.
  /// Permitted from `Loaded`, `Ready`, or `Failed`. `unload` on
  /// `Unloaded` is a no-op success.
  ///
  /// On failure, transitions to `Failed` and surfaces the typed error.
  Result<void> unload();

  // -- Observers. ------------------------------------------------------------

  /// True iff the session can serve `infer` immediately (state `Ready`).
  [[nodiscard]] bool is_ready() const noexcept { return state_ == SessionState::Ready; }

  /// Stable adapter label. Adapter-defined; low cardinality (e.g.
  /// "mock", "tensorrt", "libtorch", "python_pytorch"). Implemented by
  /// the adapter so the NVI wrapper can echo it on every event.
  [[nodiscard]] virtual std::string_view backend_name() const noexcept = 0;

 protected:
  explicit ExecutionSession(ExecutionSessionRuntimeHooks hooks) noexcept
      : event_sink_(hooks.event_sink), buffer_manager_(hooks.buffer_manager) {}

  // -- Adapter override points (protected, virtual). -------------------------

  /// Called by `load` after readiness checks. The wrapper passes the
  /// validated spec; adapters may safely take a copy.
  ///
  /// Adapters must return typed errors for backend-specific failures
  /// (`LoadFailed`, `ConfigInvalid`, `Unsupported`, `OOMError`). They
  /// must not throw exceptions; the wrapper does not unwind exceptions.
  virtual Result<void> do_load(const ModelSpec& spec) = 0;

  /// Called by `prime` after readiness checks. Default is success no-op
  /// so adapters that have no separate priming step can skip it.
  virtual Result<void> do_prime();

  /// Called by `infer` after readiness + validation. Adapters publish a
  /// vector of `NamedOutput` value objects; the wrapper validates each
  /// output buffer + tensor view, stamps timing, and wraps it in an
  /// `InferResult`. Adapters that fail return a typed error; the wrapper
  /// converts that to a failure `InferResult` with timing.
  virtual Result<std::vector<NamedOutput>> do_infer(const InferRequest& request) = 0;

  /// Called by `infer_async` after readiness + validation only when
  /// `supports_native_async()` is true. Default returns
  /// `Error::Code::Unsupported` as a safety net for adapters that opt
  /// into native async incorrectly.
  virtual Result<AsyncInferHandle> do_infer_async(const InferRequest& request);

  /// True when the adapter implements native async execution and wants
  /// the NVI wrapper to dispatch to `do_infer_async`. The default false
  /// path returns typed `Unsupported` without adapter dispatch.
  [[nodiscard]] virtual bool supports_native_async() const noexcept { return false; }

  /// Called by `unload`. Default is success no-op so adapters that have
  /// no teardown can skip it.
  virtual Result<void> do_unload();

  // -- Adapter helpers. ------------------------------------------------------

  /// Adapters call this to claim the next session-scoped async id. Used
  /// only by adapters that implement native async; the wrapper-level
  /// Unsupported path does not call this.
  [[nodiscard]] std::uint64_t next_async_id() noexcept { return next_async_id_++; }

  /// Protected diagnostics for tests and adapter-owned instrumentation.
  /// These are intentionally not part of the public lifecycle contract.
  [[nodiscard]] SessionState current_state_for_diagnostics() const noexcept { return state_; }
  [[nodiscard]] const std::optional<ModelSpec>& loaded_model_for_diagnostics() const noexcept {
    return model_;
  }
  [[nodiscard]] const std::optional<Error>& last_error_for_diagnostics() const noexcept {
    return last_error_;
  }

 private:
  /// Validate an InferRequest before adapter dispatch. Rejects empty
  /// request_id / endpoint / inputs, empty input names, duplicate input
  /// names, released or missing input buffers, tensor byte windows that
  /// do not fit inside their owning buffers, and requests whose
  /// monotonic deadline has already expired (V01-E04-F03).
  ///
  /// Static because the gate is a pure function of the request value
  /// object; no session state is consulted.
  [[nodiscard]] static Result<void> validate_request_for_infer(const InferRequest& request);

  /// Validate an adapter-published outputs vector before wrapping it in
  /// an `InferResult`. Rejects empty outputs vectors, empty / duplicate
  /// names, released or missing output buffers, and tensor byte windows
  /// that do not fit inside their owning buffers (V01-E04-F04).
  ///
  /// Static for the same reason as `validate_request_for_infer`.
  [[nodiscard]] static Result<void> validate_outputs_for_infer(
      const std::vector<NamedOutput>& outputs);

  /// Emit a `SessionEvent` through the registered sink. Fire-and-forget:
  /// a missing sink is a no-op, and any exception thrown by a
  /// misbehaving sink is swallowed so it cannot corrupt session state
  /// (V01-E04-F06).
  void emit_event(SessionEventKind kind, const std::optional<std::string>& request_id,
                  std::optional<Error::Code> error_code, SessionEvent::Duration duration) noexcept;

  SessionState state_ = SessionState::Unloaded;
  std::optional<ModelSpec> model_;
  std::optional<Error> last_error_;
  SessionEventSink* event_sink_ = nullptr;
  BufferManager* buffer_manager_ = nullptr;
  std::uint64_t next_async_id_ = 1;
};

}  // namespace tensorplate
