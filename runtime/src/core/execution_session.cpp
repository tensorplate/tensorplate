// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F02: ExecutionSession lifecycle state machine.
//
// Builds on the V01-E04-F01 public interface by wiring the lifecycle
// transitions (unloaded -> loaded -> primed/ready, plus failed and
// recovery via unload) into the non-virtual public methods. Request
// validation, output validation, monotonic timing, async unsupported
// path, and event emission land in V01-E04-F03 / F04 / F05 / F06; the
// public lifecycle gates ("infer before prime returns NotReady") are
// already enforced here.

#include "tensorplate/core/execution_session.hpp"

#include <array>
#include <chrono>
#include <cstddef>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_set>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

// -- Enum / wire-name tables. -------------------------------------------------

namespace {

struct SessionStateName {
  SessionState state;
  std::string_view name;
};

constexpr std::array<SessionStateName, 4> kSessionStateNames{{
    {SessionState::Unloaded, "unloaded"},
    {SessionState::Loaded, "loaded"},
    {SessionState::Ready, "ready"},
    {SessionState::Failed, "failed"},
}};

struct SessionEventKindName {
  SessionEventKind kind;
  std::string_view name;
};

constexpr std::array<SessionEventKindName, 17> kSessionEventKindNames{{
    {SessionEventKind::LoadStart, "load_start"},
    {SessionEventKind::LoadEnd, "load_end"},
    {SessionEventKind::LoadFailed, "load_failed"},
    {SessionEventKind::PrimeStart, "prime_start"},
    {SessionEventKind::PrimeEnd, "prime_end"},
    {SessionEventKind::PrimeFailed, "prime_failed"},
    {SessionEventKind::InferStart, "infer_start"},
    {SessionEventKind::InferEnd, "infer_end"},
    {SessionEventKind::InferFailed, "infer_failed"},
    {SessionEventKind::InferAsyncStart, "infer_async_start"},
    {SessionEventKind::InferAsyncEnd, "infer_async_end"},
    {SessionEventKind::InferAsyncFailed, "infer_async_failed"},
    {SessionEventKind::UnloadStart, "unload_start"},
    {SessionEventKind::UnloadEnd, "unload_end"},
    {SessionEventKind::UnloadFailed, "unload_failed"},
    {SessionEventKind::ValidationFailed, "validation_failed"},
    {SessionEventKind::UnsupportedAsync, "unsupported_async"},
}};

}  // namespace

std::string_view to_string(SessionState state) noexcept {
  for (const auto& entry : kSessionStateNames) {
    if (entry.state == state) {
      return entry.name;
    }
  }
  return "failed";
}

std::optional<SessionState> session_state_from_string(std::string_view name) noexcept {
  for (const auto& entry : kSessionStateNames) {
    if (entry.name == name) {
      return entry.state;
    }
  }
  return std::nullopt;
}

std::string_view to_string(SessionEventKind kind) noexcept {
  for (const auto& entry : kSessionEventKindNames) {
    if (entry.kind == kind) {
      return entry.name;
    }
  }
  return "validation_failed";
}

std::optional<SessionEventKind> session_event_kind_from_string(std::string_view name) noexcept {
  for (const auto& entry : kSessionEventKindNames) {
    if (entry.name == name) {
      return entry.kind;
    }
  }
  return std::nullopt;
}

// -- Default `do_*` implementations. -----------------------------------------

Result<void> ExecutionSession::do_prime() {
  return Result<void>{};
}

Result<AsyncInferHandle> ExecutionSession::do_infer_async(const InferRequest& /*request*/) {
  return unexpected(Error::Code::Unsupported,
                    "this ExecutionSession does not implement native async inference");
}

Result<void> ExecutionSession::do_unload() {
  return Result<void>{};
}

// -- Lifecycle state machine (V01-E04-F02). -----------------------------------

namespace {

/// Map an adapter-reported error into the post-prime state transition.
/// `ConfigInvalid` is treated as recoverable: the host can adjust
/// configuration and retry `prime`. Every other failure transitions the
/// session into `Failed` so the host knows recovery requires `unload`.
SessionState prime_failure_state(Error::Code code) noexcept {
  return code == Error::Code::ConfigInvalid ? SessionState::Loaded : SessionState::Failed;
}

/// True iff `view.byte_offset() + view.byte_size()` fits inside `buffer`
/// without overflow. Returns false for released buffers as a safety net;
/// the F03 wrapper rejects released buffers before reaching this check.
bool buffer_window_fits(const BufferRef& buffer, const TensorView& view) noexcept {
  if (!buffer.is_valid()) {
    return false;
  }
  const std::size_t offset = view.byte_offset();
  const std::size_t size = view.byte_size();
  const std::size_t end = offset + size;
  if (end < offset) {  // overflow guard
    return false;
  }
  return end <= buffer.size_bytes();
}

}  // namespace

// -- NVI request validation (V01-E04-F03). ------------------------------------

Result<void> ExecutionSession::validate_request_for_infer(const InferRequest& request) const {
  // `InferRequest::create` already rejects most of these at construction.
  // The NVI wrapper re-validates so adapter-side callers that bypass the
  // factory (e.g. test fixtures, future codecs) still fail *before*
  // adapter dispatch.
  if (request.request_id().empty()) {
    return unexpected(Error::Code::ConfigInvalid, "InferRequest.request_id is empty");
  }
  if (request.endpoint().empty()) {
    return unexpected(Error::Code::ConfigInvalid, "InferRequest.endpoint is empty");
  }
  if (request.inputs().empty()) {
    return unexpected(Error::Code::ConfigInvalid, "InferRequest has no inputs");
  }

  std::unordered_set<std::string> seen_names;
  seen_names.reserve(request.inputs().size());

  for (const auto& input : request.inputs()) {
    if (input.name.empty()) {
      return unexpected(Error::Code::ConfigInvalid, "InferRequest input has empty name");
    }
    auto [_, inserted] = seen_names.insert(input.name);
    if (!inserted) {
      return unexpected(Error::Code::ConfigInvalid,
                        "InferRequest has duplicate input name `" + input.name + "`");
    }

    // Released / missing input buffers are rejected before adapter dispatch.
    if (!input.buffer.is_valid()) {
      return unexpected(Error::Code::ConfigInvalid,
                        "InferRequest input `" + input.name + "` carries a released buffer");
    }

    // Tensor byte windows must fit inside their owning buffers. A
    // mismatch surfaces as ShapeMismatch so callers can distinguish a
    // tensor/buffer geometry bug from generic config invalidity.
    if (!buffer_window_fits(input.buffer, input.tensor)) {
      return unexpected(
          Error::Code::ShapeMismatch,
          "InferRequest input `" + input.name +
              "` tensor window does not fit inside its buffer (offset + size > buffer size)");
    }
  }

  if (request.is_expired()) {
    return unexpected(Error::Code::Timeout, "InferRequest deadline already expired at dispatch");
  }

  return Result<void>{};
}

// -- NVI event emission (V01-E04-F06). ----------------------------------------

void ExecutionSession::emit_event(SessionEventKind kind,
                                  const std::optional<std::string>& request_id,
                                  std::optional<Error::Code> error_code,
                                  SessionEvent::Duration duration) noexcept {
  if (event_sink_ == nullptr) {
    return;
  }

  SessionEvent ev;
  ev.kind = kind;
  // `backend_name()` is the only public virtual that adapters implement;
  // copy into a std::string so the event record stays valid past
  // adapter teardown.
  ev.backend_name = std::string(this->backend_name());
  if (model_.has_value()) {
    ev.model_id = model_->model_id();
  }
  ev.request_id = request_id;
  ev.error_code = error_code;
  ev.duration = duration;
  ev.state_after = state_;

  // A misbehaving sink must never corrupt session state. The
  // SessionEventSink::on_event contract is noexcept; catch defensively
  // anyway so a sink that violates the contract cannot break the
  // inference path.
  try {
    event_sink_->on_event(ev);
  } catch (...) {  // NOLINT(bugprone-empty-catch): intentional, sink contract is noexcept.
  }
}

Result<void> ExecutionSession::validate_outputs_for_infer(
    const std::vector<NamedOutput>& outputs) const {
  if (outputs.empty()) {
    return unexpected(Error::Code::InferenceFailed,
                      "adapter `do_infer` returned an empty outputs vector");
  }

  std::unordered_set<std::string> seen_names;
  seen_names.reserve(outputs.size());

  for (const auto& output : outputs) {
    if (output.name.empty()) {
      return unexpected(Error::Code::InferenceFailed, "adapter output has empty name");
    }
    auto [_, inserted] = seen_names.insert(output.name);
    if (!inserted) {
      return unexpected(Error::Code::InferenceFailed,
                        "adapter outputs have duplicate name `" + output.name + "`");
    }
    if (!output.buffer.is_valid()) {
      return unexpected(Error::Code::InferenceFailed,
                        "adapter output `" + output.name + "` has a released buffer");
    }
    if (!buffer_window_fits(output.buffer, output.tensor)) {
      return unexpected(Error::Code::ShapeMismatch,
                        "adapter output `" + output.name +
                            "` tensor window does not fit inside its buffer");
    }
  }

  return Result<void>{};
}

Result<void> ExecutionSession::load(const ModelSpec& spec) {
  emit_event(SessionEventKind::LoadStart, std::nullopt, std::nullopt, SessionEvent::Duration{0});

  if (state_ != SessionState::Unloaded) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::load requires Unloaded state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    emit_event(SessionEventKind::LoadFailed, std::nullopt, err.code, SessionEvent::Duration{0});
    return unexpected(std::move(err));
  }

  const auto start = Clock::now();
  auto result = do_load(spec);
  const auto duration =
      std::chrono::duration_cast<SessionEvent::Duration>(Clock::now() - start);

  if (!result) {
    auto err = std::move(result).error();
    model_.reset();
    state_ = SessionState::Failed;
    last_error_ = err;
    emit_event(SessionEventKind::LoadFailed, std::nullopt, err.code, duration);
    return unexpected(std::move(err));
  }

  model_ = spec;
  state_ = SessionState::Loaded;
  last_error_.reset();
  emit_event(SessionEventKind::LoadEnd, std::nullopt, std::nullopt, duration);
  return Result<void>{};
}

Result<void> ExecutionSession::prime() {
  emit_event(SessionEventKind::PrimeStart, std::nullopt, std::nullopt, SessionEvent::Duration{0});

  if (state_ != SessionState::Loaded) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::prime requires Loaded state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    emit_event(SessionEventKind::PrimeFailed, std::nullopt, err.code, SessionEvent::Duration{0});
    return unexpected(std::move(err));
  }

  const auto start = Clock::now();
  auto result = do_prime();
  const auto duration =
      std::chrono::duration_cast<SessionEvent::Duration>(Clock::now() - start);

  if (!result) {
    auto err = std::move(result).error();
    state_ = prime_failure_state(err.code);
    last_error_ = err;
    emit_event(SessionEventKind::PrimeFailed, std::nullopt, err.code, duration);
    return unexpected(std::move(err));
  }

  state_ = SessionState::Ready;
  last_error_.reset();
  emit_event(SessionEventKind::PrimeEnd, std::nullopt, std::nullopt, duration);
  return Result<void>{};
}

Result<InferResult> ExecutionSession::infer(const InferRequest& request) {
  // F03 readiness + validation gates run before adapter dispatch.
  // F04 wraps the adapter call in monotonic timing and validates the
  // adapter-published outputs before returning success.
  // F06 emits paired lifecycle events around every path.
  std::optional<std::string> req_id;
  if (!request.request_id().empty()) {
    req_id = request.request_id();
  }

  emit_event(SessionEventKind::InferStart, req_id, std::nullopt, SessionEvent::Duration{0});

  if (state_ != SessionState::Ready) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::infer requires Ready state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    emit_event(SessionEventKind::ValidationFailed, req_id, err.code, SessionEvent::Duration{0});
    return unexpected(std::move(err));
  }

  auto validation = validate_request_for_infer(request);
  if (!validation) {
    auto err = std::move(validation).error();
    last_error_ = err;
    emit_event(SessionEventKind::ValidationFailed, req_id, err.code, SessionEvent::Duration{0});
    return unexpected(std::move(err));
  }

  // Monotonic latency stamping around adapter dispatch. Uses
  // `std::chrono::steady_clock` so the timing field is unaffected by
  // wall-clock changes. Both successful and failed adapter calls have
  // `execution_latency` populated.
  const auto start = Clock::now();
  auto adapter_result = do_infer(request);
  const auto end = Clock::now();
  const auto exec_latency =
      std::chrono::duration_cast<InferenceTiming::Duration>(end - start);
  const auto event_duration =
      std::chrono::duration_cast<SessionEvent::Duration>(end - start);

  InferenceTiming timing;
  timing.execution_latency = exec_latency;

  if (!adapter_result) {
    auto err = std::move(adapter_result).error();
    last_error_ = err;
    emit_event(SessionEventKind::InferFailed, req_id, err.code, event_duration);
    return InferResult::create_failure(request.request_id(), std::move(err), timing);
  }

  auto outputs = std::move(adapter_result).value();

  // Output validation: released buffers, out-of-bounds windows, empty /
  // duplicate names all fail before success is published. Partial
  // outputs (allocated by the adapter) are released through the buffer
  // plane when a manager is wired.
  auto output_validation = validate_outputs_for_infer(outputs);
  if (!output_validation) {
    auto err = std::move(output_validation).error();
    last_error_ = err;
    if (buffer_manager_ != nullptr) {
      (void)release_partial_outputs(*buffer_manager_, outputs);
    }
    emit_event(SessionEventKind::InferFailed, req_id, err.code, event_duration);
    return InferResult::create_failure(request.request_id(), std::move(err), timing);
  }

  auto result = InferResult::create_success(request.request_id(), std::move(outputs), timing);
  if (!result) {
    auto err = std::move(result).error();
    last_error_ = err;
    emit_event(SessionEventKind::InferFailed, req_id, err.code, event_duration);
    return InferResult::create_failure(request.request_id(), std::move(err), timing);
  }

  last_error_.reset();
  emit_event(SessionEventKind::InferEnd, req_id, std::nullopt, event_duration);
  return std::move(result).value();
}

Result<AsyncInferHandle> ExecutionSession::infer_async(const InferRequest& request) {
  // F03 readiness + validation gates run before adapter dispatch. F05
  // differentiates the typed Unsupported event variant from generic
  // adapter failure; F06 emits paired lifecycle events around every
  // path.
  std::optional<std::string> req_id;
  if (!request.request_id().empty()) {
    req_id = request.request_id();
  }

  emit_event(SessionEventKind::InferAsyncStart, req_id, std::nullopt,
             SessionEvent::Duration{0});

  if (state_ != SessionState::Ready) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::infer_async requires Ready state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    emit_event(SessionEventKind::ValidationFailed, req_id, err.code,
               SessionEvent::Duration{0});
    return unexpected(std::move(err));
  }

  auto validation = validate_request_for_infer(request);
  if (!validation) {
    auto err = std::move(validation).error();
    last_error_ = err;
    emit_event(SessionEventKind::ValidationFailed, req_id, err.code,
               SessionEvent::Duration{0});
    return unexpected(std::move(err));
  }

  const auto start = Clock::now();
  auto adapter_result = do_infer_async(request);
  const auto duration =
      std::chrono::duration_cast<SessionEvent::Duration>(Clock::now() - start);

  if (!adapter_result) {
    auto err = std::move(adapter_result).error();
    last_error_ = err;
    // The Unsupported variant is reported as a distinct event so
    // observability can count it separately from genuine adapter
    // failure (OOMError, Internal, InferenceFailed, ...).
    if (err.code == Error::Code::Unsupported) {
      emit_event(SessionEventKind::UnsupportedAsync, req_id, err.code, duration);
    } else {
      emit_event(SessionEventKind::InferAsyncFailed, req_id, err.code, duration);
    }
    return unexpected(std::move(err));
  }

  last_error_.reset();
  emit_event(SessionEventKind::InferAsyncEnd, req_id, std::nullopt, duration);
  return adapter_result;
}

Result<void> ExecutionSession::unload() {
  emit_event(SessionEventKind::UnloadStart, std::nullopt, std::nullopt, SessionEvent::Duration{0});

  // Unload from Unloaded is a no-op success so cleanup paths that
  // call unload defensively don't need to query state first.
  if (state_ == SessionState::Unloaded) {
    last_error_.reset();
    emit_event(SessionEventKind::UnloadEnd, std::nullopt, std::nullopt,
               SessionEvent::Duration{0});
    return Result<void>{};
  }

  const auto start = Clock::now();
  auto result = do_unload();
  const auto duration =
      std::chrono::duration_cast<SessionEvent::Duration>(Clock::now() - start);

  if (!result) {
    auto err = std::move(result).error();
    state_ = SessionState::Failed;
    last_error_ = err;
    emit_event(SessionEventKind::UnloadFailed, std::nullopt, err.code, duration);
    return unexpected(std::move(err));
  }

  state_ = SessionState::Unloaded;
  model_.reset();
  last_error_.reset();
  emit_event(SessionEventKind::UnloadEnd, std::nullopt, std::nullopt, duration);
  return Result<void>{};
}

}  // namespace tensorplate
