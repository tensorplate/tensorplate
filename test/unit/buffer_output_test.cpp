// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F05-T01 / T02 unit coverage for session output buffer helpers.

#include <gtest/gtest.h>

#include <cstddef>
#include <memory>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_result.hpp"

namespace {

using tensorplate::BufferManager;
using tensorplate::BufferManagerConfig;
using tensorplate::build_named_output;
using tensorplate::build_named_outputs;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::Layout;
using tensorplate::NamedOutput;
using tensorplate::OutputDescriptor;
using tensorplate::release_partial_outputs;
using tensorplate::TensorView;

std::unique_ptr<BufferManager> make_manager() {
  BufferManagerConfig cfg;
  cfg.pool_name = "output_test";
  cfg.capacity_bytes = 1024 * 1024;
  cfg.max_buffer_bytes = 256 * 1024;
  auto r = BufferManager::create(std::move(cfg));
  EXPECT_TRUE(r.has_value());
  return std::move(r).value();
}

TEST(OutputAllocation, SingleVisionDetectorOutput) {
  auto mgr = make_manager();
  // Class scores for a small image classifier: float32, 1000-way.
  auto tv = TensorView::create(DType::Float32, {1000});
  ASSERT_TRUE(tv.has_value());
  OutputDescriptor d{"logits", tv.value(), std::optional<std::string>{"logits"}, 0};

  auto out = build_named_output(*mgr, d);
  ASSERT_TRUE(out.has_value());
  EXPECT_EQ(out.value().name, "logits");
  EXPECT_EQ(out.value().buffer.size_bytes(), 1000u * 4u);
  EXPECT_EQ(mgr->accounting().active_count, 1u);
  ASSERT_TRUE(mgr->release(out.value().buffer).has_value());
}

TEST(OutputAllocation, ChunkShapedVLAActionOutput) {
  auto mgr = make_manager();
  // SmolVLA action chunk: [chunk_size=8, action_dim=14] float32.
  auto tv = TensorView::create(DType::Float32, {8, 14});
  ASSERT_TRUE(tv.has_value());
  OutputDescriptor d{"action_chunk", tv.value(), std::optional<std::string>{"action_chunk"}, 0};

  auto out = build_named_output(*mgr, d);
  ASSERT_TRUE(out.has_value());
  EXPECT_EQ(out.value().buffer.size_bytes(), 8u * 14u * 4u);
  EXPECT_EQ(out.value().tensor.shape().size(), 2u);
  ASSERT_TRUE(mgr->release(out.value().buffer).has_value());
}

TEST(OutputAllocation, MultipleDistinctNamedOutputs) {
  auto mgr = make_manager();
  std::vector<OutputDescriptor> descs;
  {
    auto tv = TensorView::create(DType::Float32, {1000});
    ASSERT_TRUE(tv.has_value());
    descs.push_back({"logits", tv.value(), std::nullopt, 0});
  }
  {
    auto tv = TensorView::create(DType::Float32, {8, 14});
    ASSERT_TRUE(tv.has_value());
    descs.push_back({"action_chunk", tv.value(), std::nullopt, 0});
  }

  auto outs = build_named_outputs(*mgr, descs);
  ASSERT_TRUE(outs.has_value());
  ASSERT_EQ(outs.value().size(), 2u);
  EXPECT_NE(outs.value()[0].buffer.id(), outs.value()[1].buffer.id());
  EXPECT_EQ(mgr->accounting().active_count, 2u);

  auto report = release_partial_outputs(*mgr, outs.value());
  EXPECT_TRUE(report.clean());
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(OutputAllocation, RejectsViewWindowLargerThanRequestedBuffer) {
  auto mgr = make_manager();
  // Declared view describes 4096 floats but buffer_size_bytes asks for
  // less than that.
  auto tv = TensorView::create(DType::Float32, {4096});
  ASSERT_TRUE(tv.has_value());
  OutputDescriptor d{"y", tv.value(), std::nullopt, /*buffer_size_bytes=*/1024};

  auto out = build_named_output(*mgr, d);
  ASSERT_FALSE(out.has_value());
  EXPECT_EQ(out.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(OutputAllocation, AllowsOverAllocationBeyondViewWindow) {
  auto mgr = make_manager();
  auto tv = TensorView::create(DType::Float32, {16}, Layout::RowMajor, /*byte_offset=*/0,
                               /*byte_size=*/64);  // exact view footprint
  ASSERT_TRUE(tv.has_value());
  // Adapter wants 256 bytes of headroom to align writes.
  OutputDescriptor d{"y", tv.value(), std::nullopt, /*buffer_size_bytes=*/256};

  auto out = build_named_output(*mgr, d);
  ASSERT_TRUE(out.has_value());
  EXPECT_EQ(out.value().buffer.size_bytes(), 256u);
  ASSERT_TRUE(mgr->release(out.value().buffer).has_value());
}

TEST(OutputAllocation, BuildNamedOutputsRollsBackOnDuplicateName) {
  auto mgr = make_manager();
  auto tv = TensorView::create(DType::Float32, {16});
  ASSERT_TRUE(tv.has_value());
  std::vector<OutputDescriptor> descs;
  descs.push_back({"y", tv.value(), std::nullopt, 0});
  descs.push_back({"y", tv.value(), std::nullopt, 0});

  auto outs = build_named_outputs(*mgr, descs);
  ASSERT_FALSE(outs.has_value());
  EXPECT_EQ(outs.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(mgr->accounting().active_count, 0u);
}

TEST(OutputAllocation, BuildNamedOutputsRollsBackOnLaterAllocationFailure) {
  // Make the manager tight enough that the second allocation fails.
  BufferManagerConfig cfg;
  cfg.pool_name = "tight";
  cfg.capacity_bytes = 256;
  cfg.max_buffer_bytes = 256;
  auto r = BufferManager::create(std::move(cfg));
  ASSERT_TRUE(r.has_value());
  auto mgr = std::move(r).value();

  auto tv_a = TensorView::create(DType::Float32, {50});  // 200 bytes
  auto tv_b = TensorView::create(DType::Float32, {50});  // 200 bytes
  ASSERT_TRUE(tv_a.has_value());
  ASSERT_TRUE(tv_b.has_value());

  std::vector<OutputDescriptor> descs;
  descs.push_back({"a", tv_a.value(), std::nullopt, 0});
  descs.push_back({"b", tv_b.value(), std::nullopt, 0});

  auto outs = build_named_outputs(*mgr, descs);
  ASSERT_FALSE(outs.has_value());
  EXPECT_EQ(outs.error().code, Error::Code::OOMError);
  // The first allocation must have been rolled back.
  EXPECT_EQ(mgr->accounting().active_count, 0u);
  EXPECT_EQ(mgr->accounting().in_use_bytes, 0u);
}

TEST(OutputAllocation, NamedOutputBufferDoesNotTouchInputCleanup) {
  // Sanity: F03 partial-output cleanup releases only the outputs passed
  // in. Document this end-to-end with a small fixture.
  auto mgr = make_manager();
  auto tv = TensorView::create(DType::Float32, {16});
  ASSERT_TRUE(tv.has_value());

  // One "input" buffer that the test pretends came from F04 ingress.
  auto fake_input = mgr->allocate(128);
  ASSERT_TRUE(fake_input.has_value());

  auto out = build_named_output(*mgr, {"y", tv.value(), std::nullopt, 0});
  ASSERT_TRUE(out.has_value());

  std::vector<NamedOutput> outs{out.value()};
  auto report = release_partial_outputs(*mgr, outs);
  EXPECT_EQ(report.buffers_released, 1u);
  EXPECT_EQ(mgr->accounting().active_count, 1u);  // input untouched

  ASSERT_TRUE(mgr->release(fake_input.value()).has_value());
}

}  // namespace
