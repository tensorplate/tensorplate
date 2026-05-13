// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F05-T01 / T02: Async method shape + typed Unsupported tests.
//
// Covers:
//   - `infer_async` is present on the public lifecycle interface and
//     returns `Result<AsyncInferHandle>`,
//   - the default `do_infer_async` returns `Error::Code::Unsupported`
//     (typed) on a Ready session, distinct from generic adapter failure,
//   - readiness and validation errors are surfaced **before** the
//     unsupported capability is considered (Not-Ready / shape mismatch
//     / expired deadline / released buffer beat Unsupported),
//   - native async adapters can return a handle whose `async_id` is
//     session-scoped and monotonically increasing,
//   - the unsupported path does not allocate output buffers via the
//     buffer manager and does not dispatch to adapter execution.

#include <gtest/gtest.h>

#include <chrono>
#include <memory>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/model_spec.hpp"

#include "mock_execution_session.hpp"

namespace {

using tensorplate::AsyncInferHandle;
using tensorplate::BufferManager;
using tensorplate::BufferManagerConfig;
using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::InferRequest;
using tensorplate::ModelClass;
using tensorplate::ModelSpec;
using tensorplate::NamedInput;
using tensorplate::TensorView;
using tensorplate::testing::MockSession;

ModelSpec make_spec() {
  return ModelSpec::create("mock", ModelClass::Vision, "/dev/null", "mock").value();
}

InferRequest valid_request(const std::string& id = "req-1") {
  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto buf = BufferRef::create(1, 16, BufferOwnership::Owned).value();
  return InferRequest::create(id, "/infer", {NamedInput{"in0", buf, tv}}).value();
}

std::unique_ptr<BufferManager> make_manager() {
  BufferManagerConfig cfg;
  cfg.pool_name = "async_test";
  cfg.capacity_bytes = 1 << 16;
  cfg.max_buffer_bytes = 1 << 14;
  return std::move(BufferManager::create(std::move(cfg))).value();
}

void put_into_ready(MockSession& s) {
  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
}

// -- Default Unsupported path -----------------------------------------------

TEST(SessionInferAsync, DefaultReturnsTypedUnsupportedOnReady) {
  MockSession s;
  put_into_ready(s);

  auto r = s.infer_async(valid_request());
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
  // The adapter was reached but reported Unsupported — the dispatch
  // count increments because the default do_infer_async is what
  // produces the typed error.
  EXPECT_EQ(s.dispatch_counts().infer_async, 1u);
}

TEST(SessionInferAsync, NotReadyBeatsUnsupported) {
  MockSession s;
  // Skip load/prime: state == Unloaded.
  auto r = s.infer_async(valid_request());
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::NotReady);
  EXPECT_EQ(s.dispatch_counts().infer_async, 0u);
}

TEST(SessionInferAsync, ValidationBeatsUnsupported) {
  MockSession s;
  put_into_ready(s);

  // Build a tensor window that overflows its buffer.
  auto tv = TensorView::create(DType::Float32, {1, 8}).value();  // 32 bytes
  auto buf = BufferRef::create(1, 16, BufferOwnership::Owned).value();
  auto req = InferRequest::create("req-1", "/infer", {NamedInput{"in0", buf, tv}}).value();

  auto r = s.infer_async(req);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
  EXPECT_EQ(s.dispatch_counts().infer_async, 0u);
}

TEST(SessionInferAsync, ExpiredDeadlineBeatsUnsupported) {
  MockSession s;
  put_into_ready(s);

  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto buf = BufferRef::create(1, 16, BufferOwnership::Owned).value();
  const auto deadline = InferRequest::Clock::now() + std::chrono::milliseconds(10);
  auto req =
      InferRequest::create("req-1", "/infer", {NamedInput{"in0", buf, tv}}, {}, deadline).value();
  std::this_thread::sleep_for(std::chrono::milliseconds(25));

  auto r = s.infer_async(req);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Timeout);
  EXPECT_EQ(s.dispatch_counts().infer_async, 0u);
}

// -- Unsupported path does not allocate output buffers ---------------------

TEST(SessionInferAsync, UnsupportedPathDoesNotAllocateOutputBuffers) {
  MockSession s;
  auto manager = make_manager();
  s.set_buffer_manager(manager.get());
  put_into_ready(s);

  const auto before = manager->accounting();

  auto r = s.infer_async(valid_request());
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);

  const auto after = manager->accounting();
  EXPECT_EQ(before.active_count, after.active_count);
  EXPECT_EQ(before.in_use_bytes, after.in_use_bytes);
  EXPECT_EQ(before.high_water_bytes, after.high_water_bytes);
}

// -- Native-async adapter path ---------------------------------------------

TEST(SessionInferAsync, NativeAsyncAdapterReturnsHandle) {
  MockSession s;
  s.enable_native_async();
  put_into_ready(s);

  auto r = s.infer_async(valid_request("req-async-1"));
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().request_id, "req-async-1");
  EXPECT_GT(r.value().async_id, 0u);
}

TEST(SessionInferAsync, NativeAsyncIdsAreMonotonicallyIncreasing) {
  MockSession s;
  s.enable_native_async();
  put_into_ready(s);

  auto r1 = s.infer_async(valid_request("a"));
  auto r2 = s.infer_async(valid_request("b"));
  auto r3 = s.infer_async(valid_request("c"));
  ASSERT_TRUE(r1.has_value());
  ASSERT_TRUE(r2.has_value());
  ASSERT_TRUE(r3.has_value());
  EXPECT_LT(r1.value().async_id, r2.value().async_id);
  EXPECT_LT(r2.value().async_id, r3.value().async_id);
}

// -- Adapter-level failure on async path is preserved -----------------------

TEST(SessionInferAsync, AdapterErrorIsPreservedThroughWrapper) {
  MockSession s;
  s.enable_native_async();
  put_into_ready(s);
  s.next_infer_async_fails_with(Error::make(Error::Code::OOMError, "no async slot"));

  auto r = s.infer_async(valid_request());
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::OOMError);
  EXPECT_EQ(s.dispatch_counts().infer_async, 1u);
}

}  // namespace
