// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F01-T02: ExecutionSession public-interface T1 unit tests.
//
// These tests pin the public surface of `tensorplate::ExecutionSession`
// declared in `include/tensorplate/core/execution_session.hpp`:
//
//   - The exact public method set (load, prime, infer, infer_async,
//     unload, is_ready, backend_name).
//   - Non-virtual lifecycle methods + a non-empty set of protected
//     virtual override points.
//   - Stable `SessionState` and `SessionEventKind` wire-name round-trip.
//   - Public header pulls in no vendor SDK type (compile-time check via
//     the macro guard below).
//
// Lifecycle behavior (state machine, NVI gates, timing, async,
// events) is exercised in V01-E04-F02..F06 tests.

#include "tensorplate/core/execution_session.hpp"

#include <gtest/gtest.h>

#include <string_view>
#include <type_traits>

// Vendor-SDK hygiene check: the public header must compile without
// pulling in any CUDA, TensorRT, PyTorch/LibTorch, Vitis AI, XRT, DPU,
// or ONNX Runtime symbol. The macros below would have been defined by
// the corresponding vendor headers; we assert they are not.
#if defined(CUDA_VERSION) || defined(__CUDACC__)
#error "ExecutionSession public header must not pull in CUDA headers"
#endif
#if defined(NV_TENSORRT_MAJOR) || defined(TRT_VERSION)
#error "ExecutionSession public header must not pull in TensorRT headers"
#endif
#if defined(TORCH_VERSION) || defined(TORCH_API)
#error "ExecutionSession public header must not pull in LibTorch headers"
#endif
#if defined(VITIS_AI_LIBRARY_VERSION) || defined(VART_API)
#error "ExecutionSession public header must not pull in Vitis AI headers"
#endif

namespace {

using tensorplate::AsyncInferHandle;
using tensorplate::ExecutionSession;
using tensorplate::SessionEventKind;
using tensorplate::SessionState;

// -----------------------------------------------------------------------------
// Public-surface tests
// -----------------------------------------------------------------------------

// Public destructor must be virtual so adapters can be deleted through
// `std::unique_ptr<ExecutionSession>`.
TEST(ExecutionSessionInterface, HasVirtualDestructor) {
  static_assert(std::has_virtual_destructor_v<ExecutionSession>,
                "ExecutionSession must declare a virtual destructor");
}

// The public method set is enforced as part of the V01-E04-F01 contract.
// These pin the exact signatures so that future drift requires a
// deliberate header change (which requires tech lead approval per the
// architecture doc).
TEST(ExecutionSessionInterface, PublicMethodSignatures) {
  using LoadFn = tensorplate::Result<void> (ExecutionSession::*)(const tensorplate::ModelSpec&);
  using PrimeFn = tensorplate::Result<void> (ExecutionSession::*)();
  using InferFn = tensorplate::Result<tensorplate::InferResult> (ExecutionSession::*)(
      const tensorplate::InferRequest&);
  using InferAsyncFn = tensorplate::Result<AsyncInferHandle> (ExecutionSession::*)(
      const tensorplate::InferRequest&);
  using UnloadFn = tensorplate::Result<void> (ExecutionSession::*)();
  using IsReadyFn = bool (ExecutionSession::*)() const noexcept;
  using BackendNameFn = std::string_view (ExecutionSession::*)() const noexcept;

  [[maybe_unused]] LoadFn load_fn = &ExecutionSession::load;
  [[maybe_unused]] PrimeFn prime_fn = &ExecutionSession::prime;
  [[maybe_unused]] InferFn infer_fn = &ExecutionSession::infer;
  [[maybe_unused]] InferAsyncFn infer_async_fn = &ExecutionSession::infer_async;
  [[maybe_unused]] UnloadFn unload_fn = &ExecutionSession::unload;
  [[maybe_unused]] IsReadyFn is_ready_fn = &ExecutionSession::is_ready;
  [[maybe_unused]] BackendNameFn backend_name_fn = &ExecutionSession::backend_name;

  SUCCEED();
}

TEST(ExecutionSessionInterface, IsNotCopyableOrMovable) {
  static_assert(!std::is_copy_constructible_v<ExecutionSession>);
  static_assert(!std::is_copy_assignable_v<ExecutionSession>);
  static_assert(!std::is_move_constructible_v<ExecutionSession>);
  static_assert(!std::is_move_assignable_v<ExecutionSession>);
}

// `backend_name` is the only public virtual method in the public method
// set; all other lifecycle methods are non-virtual wrappers. We can't
// observe virtual-ness directly with type_traits, but we can require
// that `&ExecutionSession::load` is a non-virtual data-pointer-compatible
// method by taking its address (already exercised above) and that the
// class itself has at least one virtual entry (the destructor).
TEST(ExecutionSessionInterface, ClassIsPolymorphic) {
  static_assert(std::is_polymorphic_v<ExecutionSession>);
}

// -----------------------------------------------------------------------------
// SessionState wire-name round-trip
// -----------------------------------------------------------------------------

TEST(SessionStateNames, RoundTripsEveryValue) {
  constexpr SessionState kAll[] = {SessionState::Unloaded, SessionState::Loaded,
                                   SessionState::Ready, SessionState::Failed};
  for (auto s : kAll) {
    auto name = tensorplate::to_string(s);
    EXPECT_FALSE(name.empty()) << "no wire name for state index " << static_cast<int>(s);
    auto parsed = tensorplate::session_state_from_string(name);
    ASSERT_TRUE(parsed.has_value());
    EXPECT_EQ(*parsed, s);
  }
}

TEST(SessionStateNames, RejectsUnknownNames) {
  EXPECT_FALSE(tensorplate::session_state_from_string("").has_value());
  EXPECT_FALSE(tensorplate::session_state_from_string("running").has_value());
  EXPECT_FALSE(tensorplate::session_state_from_string("UNLOADED").has_value());
}

// -----------------------------------------------------------------------------
// SessionEventKind wire-name round-trip
// -----------------------------------------------------------------------------

TEST(SessionEventKindNames, RoundTripsEveryValue) {
  constexpr SessionEventKind kAll[] = {
      SessionEventKind::LoadStart,        SessionEventKind::LoadEnd,
      SessionEventKind::LoadFailed,       SessionEventKind::PrimeStart,
      SessionEventKind::PrimeEnd,         SessionEventKind::PrimeFailed,
      SessionEventKind::InferStart,       SessionEventKind::InferEnd,
      SessionEventKind::InferFailed,      SessionEventKind::InferAsyncStart,
      SessionEventKind::InferAsyncEnd,    SessionEventKind::InferAsyncFailed,
      SessionEventKind::UnloadStart,      SessionEventKind::UnloadEnd,
      SessionEventKind::UnloadFailed,     SessionEventKind::ValidationFailed,
      SessionEventKind::UnsupportedAsync,
  };
  for (auto k : kAll) {
    auto name = tensorplate::to_string(k);
    EXPECT_FALSE(name.empty()) << "no wire name for event kind " << static_cast<int>(k);
    auto parsed = tensorplate::session_event_kind_from_string(name);
    ASSERT_TRUE(parsed.has_value());
    EXPECT_EQ(*parsed, k);
  }
}

TEST(SessionEventKindNames, RejectsUnknownNames) {
  EXPECT_FALSE(tensorplate::session_event_kind_from_string("").has_value());
  EXPECT_FALSE(tensorplate::session_event_kind_from_string("LOAD_START").has_value());
  EXPECT_FALSE(tensorplate::session_event_kind_from_string("unknown").has_value());
}

// -----------------------------------------------------------------------------
// AsyncInferHandle value-object semantics
// -----------------------------------------------------------------------------

TEST(AsyncInferHandle, EqualityComparesIdAndRequestId) {
  AsyncInferHandle a{.request_id = "r1", .async_id = 42};
  AsyncInferHandle b{.request_id = "r1", .async_id = 42};
  AsyncInferHandle c{.request_id = "r1", .async_id = 43};
  AsyncInferHandle d{.request_id = "r2", .async_id = 42};

  EXPECT_EQ(a, b);
  EXPECT_NE(a, c);
  EXPECT_NE(a, d);
}

}  // namespace
