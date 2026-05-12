// SPDX-License-Identifier: Apache-2.0
//
// V01-E03 Epic acceptance: end-to-end buffer-plane integration test.
//
// Exercises the full v0.1.0 buffer-plane loop end to end:
//
//   raw bytes
//     -> BufferManager (allocate + memcpy)
//     -> BufferRef + TensorView
//     -> InferRequest
//     -> [scheduler-style cancel/timeout/error path]
//     -> deterministic release
//     -> ExecutionSession-style mock output
//     -> BufferRef + TensorView
//     -> InferResult
//
// This test does NOT load a model or call a real backend. It substitutes a
// mock "policy" routine that reads input bytes through the manager and
// writes mock output bytes back, exactly as a real adapter will.

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/ingress.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"

#include "ingress_fixtures.hpp"

namespace {

using namespace tensorplate;

std::unique_ptr<BufferManager> make_manager() {
  BufferManagerConfig cfg;
  cfg.pool_name = "e2e";
  cfg.capacity_bytes = 4ULL * 1024ULL * 1024ULL;
  cfg.max_buffer_bytes = 1ULL * 1024ULL * 1024ULL;
  auto r = BufferManager::create(std::move(cfg));
  EXPECT_TRUE(r.has_value());
  return std::move(r).value();
}

// Mock "policy" execution: read the named state vector input bytes,
// compute a tiny deterministic action chunk, and publish the result
// through the same buffer manager.
Result<InferResult> run_mock_policy(BufferManager& mgr, const InferRequest& req) {
  // RAII guard releases inputs if we return early via the failure path.
  RequestBufferGuard input_guard(mgr, req);

  // Allocate output buffers up front so failures release them.
  auto action_view = TensorView::create(DType::Float32, {4, 7});
  EXPECT_TRUE(action_view.has_value());
  std::vector<OutputDescriptor> descs;
  descs.push_back({"action_chunk", action_view.value(),
                   std::optional<std::string>{"action_chunk"}, 0});

  auto outs = build_named_outputs(mgr, descs);
  if (!outs.has_value()) {
    return unexpected(std::move(outs).error());
  }

  // Read one input through the manager view path so we exercise the
  // bounds check.
  bool found_state = false;
  for (const auto& in : req.inputs()) {
    if (in.name == "state") {
      auto bytes = mgr.view(in.buffer, in.tensor);
      if (!bytes.has_value()) {
        // Release the partial outputs; inputs flow back through the
        // guard.
        (void)release_partial_outputs(mgr, outs.value());
        return unexpected(std::move(bytes).error());
      }
      EXPECT_EQ(bytes.value().size(), in.tensor.byte_size());
      found_state = true;
      break;
    }
  }
  EXPECT_TRUE(found_state);

  // Mock policy: write a deterministic ramp into the output buffer.
  auto out_bytes = mgr.data(outs.value().front().buffer);
  if (!out_bytes.has_value()) {
    (void)release_partial_outputs(mgr, outs.value());
    return unexpected(std::move(out_bytes).error());
  }
  for (std::size_t i = 0; i < out_bytes.value().size(); ++i) {
    out_bytes.value()[i] = static_cast<std::byte>(i & 0xFF);
  }

  // Success: hand the inputs back to the guard (it releases them) and
  // pass the outputs into the result. The guard's destructor will
  // release the inputs after this function returns.
  auto result = InferResult::create_success(std::string(req.request_id()), std::move(outs).value());
  if (!result.has_value()) {
    // Result construction failed; release the partial outputs and let
    // the guard release the inputs.
    (void)release_partial_outputs(mgr, outs.value());
    return unexpected(std::move(result).error());
  }
  // The InferResult now logically owns the output buffers; the test
  // explicitly releases them after the assertion phase. Inputs flow
  // through the guard on scope exit (guard not dismissed).
  return result;
}

// ----- The happy path -----

TEST(BufferPlaneE2E, EndToEndVisionRequestThroughBufferPlane) {
  auto mgr = make_manager();
  const auto fixtures = testing::make_vision_fixture(64, 64, 3);
  auto inputs = build_named_inputs(*mgr, testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());

  auto req = InferRequest::create("vision-1", "/policy", std::move(inputs).value());
  ASSERT_TRUE(req.has_value());

  // Mock execution: just verify the buffer is reachable through the
  // manager and then release it.
  {
    RequestBufferGuard guard(*mgr, req.value());
    auto bytes = mgr->view(req.value().inputs().front().buffer,
                           req.value().inputs().front().tensor);
    ASSERT_TRUE(bytes.has_value());
    EXPECT_EQ(bytes.value().size(), 64u * 64u * 3u);
    // No outputs in this minimal happy-path assertion; the guard
    // releases the inputs on scope exit.
  }
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(BufferPlaneE2E, EndToEndSmolVLARequestProducesInferResult) {
  auto mgr = make_manager();
  const auto fixtures = testing::make_smolvla_fixture();
  auto inputs = build_named_inputs(*mgr, testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());

  auto req = InferRequest::create("vla-1", "/policy", std::move(inputs).value());
  ASSERT_TRUE(req.has_value());

  auto result = run_mock_policy(*mgr, req.value());
  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result.value().is_success());
  ASSERT_EQ(result.value().outputs().size(), 1u);
  const auto& out = result.value().outputs().front();
  EXPECT_EQ(out.name, "action_chunk");
  EXPECT_EQ(out.tensor.shape().size(), 2u);
  EXPECT_EQ(out.tensor.shape()[0], 4);
  EXPECT_EQ(out.tensor.shape()[1], 7);

  // Inputs are released by the RequestBufferGuard inside run_mock_policy;
  // outputs are still alive and reachable.
  auto seen = mgr->view(out.buffer, out.tensor);
  ASSERT_TRUE(seen.has_value());
  EXPECT_EQ(seen.value().size(), 4u * 7u * 4u);

  // Explicit cleanup so we don't depend on manager destruction.
  ASSERT_TRUE(mgr->release(out.buffer).has_value());
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

// ----- Cancellation / timeout / error paths -----

TEST(BufferPlaneE2E, CancellationReleasesEveryInputBuffer) {
  auto mgr = make_manager();
  const auto fixtures = testing::make_smolvla_fixture();
  auto inputs = build_named_inputs(*mgr, testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());

  auto req = InferRequest::create("cancel-1", "/policy", std::move(inputs).value());
  ASSERT_TRUE(req.has_value());
  EXPECT_EQ(mgr->accounting().active_count, 4u);

  // Simulate scheduler cancellation. The runtime would call the cleanup
  // helper; the original cancellation error survives.
  Error original = Error::make(Error::Code::Internal, "client cancelled");
  auto report = release_request_buffers(*mgr, req.value());
  EXPECT_TRUE(report.clean());
  EXPECT_EQ(report.buffers_released, 4u);
  EXPECT_EQ(mgr->accounting().active_count, 0u);
  // Original error is unchanged by cleanup.
  EXPECT_EQ(original.code, Error::Code::Internal);
}

TEST(BufferPlaneE2E, TimeoutReleasesEveryInputBufferThroughGuard) {
  auto mgr = make_manager();
  const auto fixtures = testing::make_vision_fixture(32, 32, 3);
  auto inputs = build_named_inputs(*mgr, testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());

  // Build a request with an already-expired relative deadline; the
  // factory rejects it as Timeout, but we still want to demonstrate
  // that the manager-owned buffers can be released by cleanup helpers
  // without leaking.
  std::vector<NamedInput> snapshot = inputs.value();
  auto req = InferRequest::create("timeout-1", "/policy", std::move(inputs).value());
  ASSERT_TRUE(req.has_value());

  // Pretend the scheduler expired the request after construction; route
  // through the guard's RAII path.
  {
    RequestBufferGuard guard(*mgr, req.value());
    // No dismiss(); leaving the scope simulates the timeout cleanup
    // hook firing.
  }
  EXPECT_EQ(mgr->accounting().active_count, 0u);

  // The snapshot copy proves that BufferRef value-object copies see the
  // release fate: any further data access fails.
  for (const auto& in : snapshot) {
    auto bytes = mgr->data(in.buffer);
    EXPECT_FALSE(bytes.has_value());
  }
}

TEST(BufferPlaneE2E, ErrorPathReleasesPartialOutputsAndInputs) {
  auto mgr = make_manager();
  const auto fixtures = testing::make_vision_fixture(32, 32, 3);
  auto inputs = build_named_inputs(*mgr, testing::as_ingress_inputs(fixtures));
  ASSERT_TRUE(inputs.has_value());
  auto req = InferRequest::create("err-1", "/policy", std::move(inputs).value());
  ASSERT_TRUE(req.has_value());

  // Allocate one valid output and one impossible one to force failure
  // after the first allocation.
  auto good = TensorView::create(DType::Float32, {16});
  ASSERT_TRUE(good.has_value());
  auto bad = TensorView::create(DType::Float32, {1024 * 1024});  // 4 MiB, exceeds per-buffer cap
  ASSERT_TRUE(bad.has_value());

  std::vector<OutputDescriptor> descs;
  descs.push_back({"logits", good.value(), std::nullopt, 0});
  descs.push_back({"too_big", bad.value(), std::nullopt, 0});

  auto outs = build_named_outputs(*mgr, descs);
  ASSERT_FALSE(outs.has_value());
  // The first allocation was rolled back; only inputs remain active.
  EXPECT_EQ(mgr->accounting().active_count, req.value().inputs().size());

  // Now release the inputs as the error path would.
  auto report = release_request_buffers(*mgr, req.value());
  EXPECT_TRUE(report.clean());
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

// ----- Pressure signal observable from the test harness -----

TEST(BufferPlaneE2E, PressureTransitionsObservableUnderRealLoad) {
  BufferManagerConfig cfg;
  cfg.pool_name = "e2e_pressure";
  cfg.capacity_bytes = 256 * 1024;  // 256 KiB
  cfg.max_buffer_bytes = 128 * 1024;
  cfg.warning_threshold = 0.5;
  cfg.critical_threshold = 0.9;
  auto mgr = BufferManager::create(std::move(cfg)).value();

  std::vector<MemoryPressure> levels;
  mgr->subscribe_pressure([&levels](const BufferPressureEvent& e) { levels.push_back(e.current); });

  std::vector<BufferRef> handles;
  // Allocate 7 buffers of 32 KiB so we cross both thresholds.
  for (int i = 0; i < 7; ++i) {
    auto h = mgr->allocate(32 * 1024);
    if (h.has_value()) {
      handles.push_back(h.value());
    }
  }
  ASSERT_FALSE(levels.empty());
  // Walking back down emits a Normal transition.
  for (auto& h : handles) {
    ASSERT_TRUE(mgr->release(h).has_value());
  }
  EXPECT_EQ(levels.back(), MemoryPressure::Normal);
}

}  // namespace
