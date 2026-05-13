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
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
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

}  // namespace

Result<void> ExecutionSession::load(const ModelSpec& spec) {
  if (state_ != SessionState::Unloaded) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::load requires Unloaded state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    return unexpected(std::move(err));
  }

  auto result = do_load(spec);
  if (!result) {
    auto err = std::move(result).error();
    model_.reset();
    state_ = SessionState::Failed;
    last_error_ = err;
    return unexpected(std::move(err));
  }

  model_ = spec;
  state_ = SessionState::Loaded;
  last_error_.reset();
  return Result<void>{};
}

Result<void> ExecutionSession::prime() {
  if (state_ != SessionState::Loaded) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::prime requires Loaded state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    return unexpected(std::move(err));
  }

  auto result = do_prime();
  if (!result) {
    auto err = std::move(result).error();
    state_ = prime_failure_state(err.code);
    last_error_ = err;
    return unexpected(std::move(err));
  }

  state_ = SessionState::Ready;
  last_error_.reset();
  return Result<void>{};
}

Result<InferResult> ExecutionSession::infer(const InferRequest& request) {
  // F02 gate only: full request/output validation lands in F03, and
  // monotonic timing + InferResult construction in F04. Sessions that
  // are not Ready must surface NotReady before any adapter dispatch.
  if (state_ != SessionState::Ready) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::infer requires Ready state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    return unexpected(std::move(err));
  }

  auto adapter_result = do_infer(request);
  if (!adapter_result) {
    auto err = std::move(adapter_result).error();
    last_error_ = err;
    return InferResult::create_failure(request.request_id(), std::move(err));
  }

  auto outputs = std::move(adapter_result).value();
  auto result = InferResult::create_success(request.request_id(), std::move(outputs));
  if (!result) {
    auto err = std::move(result).error();
    last_error_ = err;
    return InferResult::create_failure(request.request_id(), std::move(err));
  }

  last_error_.reset();
  return std::move(result).value();
}

Result<AsyncInferHandle> ExecutionSession::infer_async(const InferRequest& request) {
  // F02 gate only. The typed Unsupported shape comes from the default
  // do_infer_async; F05 adds the full async wrapper (validation, event
  // emission, and the Unsupported event distinct from generic failure).
  if (state_ != SessionState::Ready) {
    auto err = Error::make(Error::Code::NotReady,
                           "ExecutionSession::infer_async requires Ready state (current: " +
                               std::string(to_string(state_)) + ")");
    last_error_ = err;
    return unexpected(std::move(err));
  }

  auto adapter_result = do_infer_async(request);
  if (!adapter_result) {
    auto err = std::move(adapter_result).error();
    last_error_ = err;
    return unexpected(std::move(err));
  }

  last_error_.reset();
  return adapter_result;
}

Result<void> ExecutionSession::unload() {
  // Unload from Unloaded is a no-op success so cleanup paths that
  // call unload defensively don't need to query state first.
  if (state_ == SessionState::Unloaded) {
    last_error_.reset();
    return Result<void>{};
  }

  auto result = do_unload();
  if (!result) {
    auto err = std::move(result).error();
    state_ = SessionState::Failed;
    last_error_ = err;
    return unexpected(std::move(err));
  }

  state_ = SessionState::Unloaded;
  model_.reset();
  last_error_.reset();
  return Result<void>{};
}

}  // namespace tensorplate
