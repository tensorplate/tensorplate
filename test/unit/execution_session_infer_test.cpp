// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F04-T01 / T02: Sync inference path tests.
//
// Covers:
//   - successful infer wraps adapter outputs into an InferResult with
//     `execution_latency` populated using `std::chrono::steady_clock`,
//   - adapter failure produces a failure InferResult that still carries
//     `execution_latency` (failed-call timing requirement),
//   - readiness / validation failures do NOT call the adapter and
//     surface as `Result::error` (not as a failure InferResult),
//   - adapter outputs with released buffers, out-of-bounds tensor
//     windows, or empty/duplicate names are rejected before success is
//     returned and partial outputs are released through the buffer
//     manager when one is wired,
//   - single-output and multi-output success paths.

#include <gtest/gtest.h>

#include <chrono>
#include <memory>
#include <thread>
#include <utility>
#include <vector>

#include "mock_execution_session.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/model_spec.hpp"

namespace {

using tensorplate::BufferManager;
using tensorplate::BufferManagerConfig;
using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::InferRequest;
using tensorplate::InferResult;
using tensorplate::ModelClass;
using tensorplate::ModelSpec;
using tensorplate::NamedInput;
using tensorplate::NamedOutput;
using tensorplate::OutputDescriptor;
using tensorplate::TensorView;
using tensorplate::testing::MockSession;

ModelSpec make_spec() {
  return ModelSpec::create("mock", ModelClass::Vision, "/dev/null", "mock").value();
}

InferRequest valid_request() {
  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto buf = BufferRef::create(1, 16, BufferOwnership::Owned).value();
  std::vector<NamedInput> inputs{NamedInput{"in0", buf, tv}};
  return InferRequest::create("req-1", "/infer", std::move(inputs)).value();
}

std::unique_ptr<BufferManager> make_manager() {
  BufferManagerConfig cfg;
  cfg.pool_name = "infer_test";
  cfg.capacity_bytes = 1 << 16;
  cfg.max_buffer_bytes = 1 << 14;
  auto r = BufferManager::create(std::move(cfg));
  return std::move(r).value();
}

void put_into_ready(MockSession& s) {
  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
}

// -- Successful sync inference -----------------------------------------------

TEST(SessionInfer, SuccessPopulatesExecutionLatencyAndOutputs) {
  MockSession s;
  put_into_ready(s);

  auto out_tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto out_buf = BufferRef::create(99, 16, BufferOwnership::Owned).value();
  s.set_next_infer_outputs({NamedOutput{"out0", out_buf, out_tv, std::nullopt}});

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  const InferResult& result = r.value();
  EXPECT_TRUE(result.is_success());
  ASSERT_EQ(result.outputs().size(), 1u);
  EXPECT_EQ(result.outputs()[0].name, "out0");
  ASSERT_TRUE(result.timing().execution_latency.has_value());
  EXPECT_GE(result.timing().execution_latency->count(), 0);
}

TEST(SessionInfer, MultiOutputSuccessPreservesOrderAndNames) {
  MockSession s;
  put_into_ready(s);

  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto a = BufferRef::create(1001, 16, BufferOwnership::Owned).value();
  auto b = BufferRef::create(1002, 16, BufferOwnership::Owned).value();
  s.set_next_infer_outputs(
      {NamedOutput{"logits", a, tv, std::nullopt},
       NamedOutput{"action_chunk", b, tv, std::optional<std::string>{"action_chunk"}}});

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  ASSERT_EQ(r.value().outputs().size(), 2u);
  EXPECT_EQ(r.value().outputs()[0].name, "logits");
  EXPECT_EQ(r.value().outputs()[1].name, "action_chunk");
  ASSERT_TRUE(r.value().outputs()[1].semantic_tag.has_value());
  EXPECT_EQ(*r.value().outputs()[1].semantic_tag, "action_chunk");
}

// -- Adapter-failure timing requirement --------------------------------------

TEST(SessionInfer, AdapterFailureStillStampsExecutionLatency) {
  MockSession s;
  put_into_ready(s);

  s.next_infer_fails_with(
      Error::make(Error::Code::InferenceFailed, "kernel launch failed"));

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());  // Adapter failures surface as failure InferResult.
  const InferResult& result = r.value();
  EXPECT_TRUE(result.is_failure());
  EXPECT_EQ(result.error().code, Error::Code::InferenceFailed);
  ASSERT_TRUE(result.timing().execution_latency.has_value());
  EXPECT_GE(result.timing().execution_latency->count(), 0);
}

TEST(SessionInfer, AdapterFailureTimingApproximatesAdapterDuration) {
  // Sanity check: a deliberately slow adapter call should produce a
  // measurable execution_latency. Tolerant assertion only — avoid
  // brittle sleeps by checking a generous lower bound.
  class SlowSession final : public tensorplate::ExecutionSession {
   public:
    std::string_view backend_name() const noexcept override { return "slow_mock"; }

   protected:
    tensorplate::Result<void> do_load(const ModelSpec&) override {
      return tensorplate::Result<void>{};
    }
    tensorplate::Result<std::vector<NamedOutput>> do_infer(const InferRequest&) override {
      std::this_thread::sleep_for(std::chrono::milliseconds(5));
      return tensorplate::unexpected(Error::Code::InferenceFailed, "slow but failed");
    }
  };

  SlowSession s;
  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  ASSERT_TRUE(r.value().timing().execution_latency.has_value());
  EXPECT_GE(r.value().timing().execution_latency->count(),
            std::chrono::nanoseconds(std::chrono::milliseconds(1)).count());
}

// -- Validation failures bypass the adapter and do not stamp timing ----------

TEST(SessionInfer, NotReadyReturnsResultErrorNotFailureInferResult) {
  MockSession s;
  // No load/prime: state == Unloaded.
  auto r = s.infer(valid_request());
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::NotReady);
  EXPECT_EQ(s.dispatch_counts().infer, 0u);
}

TEST(SessionInfer, ValidationFailureReturnsResultErrorNotFailureInferResult) {
  MockSession s;
  put_into_ready(s);

  // Build a request whose tensor window does not fit.
  auto tv = TensorView::create(DType::Float32, {1, 8}).value();  // 32 bytes
  auto buf = BufferRef::create(1, 16, BufferOwnership::Owned).value();
  auto req = InferRequest::create("req-1", "/infer",
                                  {NamedInput{"in0", buf, tv}})
                 .value();

  auto r = s.infer(req);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
  EXPECT_EQ(s.dispatch_counts().infer, 0u);
}

// -- Output validation: adapter outputs with bad geometry -------------------

TEST(SessionInfer, AdapterOutputWithReleasedBufferRejected) {
  MockSession s;
  put_into_ready(s);

  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  BufferRef released;  // released sentinel.
  s.set_next_infer_outputs({NamedOutput{"out0", released, tv, std::nullopt}});

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_failure());
  EXPECT_EQ(r.value().error().code, Error::Code::InferenceFailed);
}

TEST(SessionInfer, AdapterOutputOutOfBoundsRejected) {
  MockSession s;
  put_into_ready(s);

  auto out_tv = TensorView::create(DType::Float32, {1, 8}).value();  // 32 bytes
  auto out_buf = BufferRef::create(99, 16, BufferOwnership::Owned).value();
  s.set_next_infer_outputs({NamedOutput{"out0", out_buf, out_tv, std::nullopt}});

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_failure());
  EXPECT_EQ(r.value().error().code, Error::Code::ShapeMismatch);
}

TEST(SessionInfer, AdapterOutputDuplicateNameRejected) {
  MockSession s;
  put_into_ready(s);

  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto a = BufferRef::create(101, 16, BufferOwnership::Owned).value();
  auto b = BufferRef::create(102, 16, BufferOwnership::Owned).value();
  s.set_next_infer_outputs(
      {NamedOutput{"out0", a, tv, std::nullopt}, NamedOutput{"out0", b, tv, std::nullopt}});

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_failure());
  EXPECT_EQ(r.value().error().code, Error::Code::InferenceFailed);
}

TEST(SessionInfer, AdapterEmptyOutputsRejected) {
  MockSession s;
  put_into_ready(s);
  s.set_next_infer_outputs({});

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_failure());
  EXPECT_EQ(r.value().error().code, Error::Code::InferenceFailed);
}

// -- Partial outputs released through the buffer manager --------------------

TEST(SessionInfer, BadOutputReleasesPartialBuffersThroughManager) {
  MockSession s;
  auto manager = make_manager();
  s.set_buffer_manager(manager.get());
  put_into_ready(s);

  // Build two real outputs through the buffer manager. The second view
  // overflows its buffer, so the wrapper must release both.
  auto good_tv = TensorView::create(DType::Float32, {1, 4}).value();    // 16 bytes
  auto bad_tv = TensorView::create(DType::Float32, {1, 16}).value();    // 64 bytes
  auto good = tensorplate::build_named_output(
      *manager, OutputDescriptor{.name = "good", .tensor = good_tv})
                  .value();
  // Allocate a buffer too small for `bad_tv` so output validation
  // rejects it. Allocate manually so the bounds check fires inside the
  // session wrapper rather than at allocation time.
  auto bad_buf = manager->allocate(16).value();  // 16 bytes
  NamedOutput bad{"bad", bad_buf, bad_tv, std::nullopt};

  const auto before = manager->accounting().active_count;
  s.set_next_infer_outputs({good, bad});

  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_failure());
  EXPECT_EQ(r.value().error().code, Error::Code::ShapeMismatch);

  // Both adapter-published outputs must be released by the wrapper.
  EXPECT_EQ(manager->accounting().active_count, before - 2);
}

}  // namespace
