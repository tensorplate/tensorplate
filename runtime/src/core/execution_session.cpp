// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F01-T02: ExecutionSession enum wire-name tables and default
// `do_*` implementations.
//
// The lifecycle state machine, NVI readiness/validation gates, sync
// inference path with timing, async method shape, and event emission
// land in V01-E04-F02 / F03 / F04 / F05 / F06 respectively. Until those
// arrive, the public lifecycle methods route through the default
// `do_*` virtual implementations defined below; concrete adapters
// override them as usual.

#include "tensorplate/core/execution_session.hpp"

#include <array>
#include <optional>
#include <string_view>

#include "tensorplate/core/error.hpp"
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
//
// V01-E04-F02 introduces the lifecycle state machine that drives the
// public methods. Until then, the public methods are not yet wired to
// these defaults; concrete adapters can override `do_load` / `do_infer`
// and rely on the default `do_prime` / `do_infer_async` / `do_unload`
// once the wrappers land.

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

// -- Public lifecycle methods (V01-E04-F02..F06). ----------------------------
//
// Stubs returning NotReady until the lifecycle wrapper lands in
// V01-E04-F02. These ensure the public symbol set is linkable so that
// downstream targets compile against the header.

Result<void> ExecutionSession::load(const ModelSpec& /*spec*/) {
  return unexpected(Error::Code::NotReady,
                    "ExecutionSession::load wrapper not yet implemented (V01-E04-F02)");
}

Result<void> ExecutionSession::prime() {
  return unexpected(Error::Code::NotReady,
                    "ExecutionSession::prime wrapper not yet implemented (V01-E04-F02)");
}

Result<InferResult> ExecutionSession::infer(const InferRequest& /*request*/) {
  return unexpected(Error::Code::NotReady,
                    "ExecutionSession::infer wrapper not yet implemented (V01-E04-F03/F04)");
}

Result<AsyncInferHandle> ExecutionSession::infer_async(const InferRequest& /*request*/) {
  return unexpected(Error::Code::NotReady,
                    "ExecutionSession::infer_async wrapper not yet implemented (V01-E04-F05)");
}

Result<void> ExecutionSession::unload() {
  return unexpected(Error::Code::NotReady,
                    "ExecutionSession::unload wrapper not yet implemented (V01-E04-F02)");
}

}  // namespace tensorplate
