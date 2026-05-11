// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F03-T01 / T02 / T03 unit coverage for tensorplate::InferRequest.

#include "tensorplate/core/infer_request.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace {

using std::chrono::milliseconds;
using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::InferRequest;
using tensorplate::Layout;
using tensorplate::NamedInput;
using tensorplate::RequestMetadata;
using tensorplate::TensorView;

NamedInput vision_input() {
  auto buf = BufferRef::create(1, 1 * 3 * 224 * 224 * 2, BufferOwnership::Owned);
  auto view = TensorView::create(DType::Float16, {1, 3, 224, 224});
  return NamedInput{"image", buf.value(), view.value()};
}

NamedInput chunk_state_input() {
  auto buf = BufferRef::create(2, 7 * 4, BufferOwnership::Owned);
  auto view = TensorView::create(DType::Float32, {7});
  return NamedInput{"state", buf.value(), view.value()};
}

TEST(InferRequest, SingleInputVisionConstructionPreservesFields) {
  auto r = InferRequest::create("req-1", "yolov8n", {vision_input()});
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().request_id(), "req-1");
  EXPECT_EQ(r.value().endpoint(), "yolov8n");
  EXPECT_EQ(r.value().inputs().size(), 1u);
  EXPECT_EQ(r.value().inputs()[0].name, "image");
  EXPECT_FALSE(r.value().deadline().has_value());
}

TEST(InferRequest, SmolVLAStyleMultiInputValidates) {
  std::vector<NamedInput> inputs;
  for (auto* name : {"image_front", "image_wrist", "state", "instruction"}) {
    auto buf = BufferRef::create(static_cast<std::uint64_t>(inputs.size() + 1), 64,
                                 BufferOwnership::Owned);
    auto view = TensorView::create(DType::Float32, {16});
    inputs.push_back(NamedInput{name, buf.value(), view.value()});
  }

  auto r = InferRequest::create("req-2", "smolvla-450m", std::move(inputs));
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().inputs().size(), 4u);
  EXPECT_EQ(r.value().inputs().front().name, "image_front");
  EXPECT_EQ(r.value().inputs().back().name, "instruction");
}

TEST(InferRequest, MetadataPreservesLeRobotAsyncFields) {
  RequestMetadata md;
  md.correlation_id = "corr-42";
  md.action_chunk_id = "chunk-7";
  md.action_chunk_sequence = 7;
  md.stale_after_sequence = 5;
  md.extra["episode"] = "20240501-1";

  auto r = InferRequest::create("req-3", "smolvla-450m", {chunk_state_input()}, md);
  ASSERT_TRUE(r.has_value());
  const auto& got = r.value().metadata();
  EXPECT_EQ(got.correlation_id.value_or(""), "corr-42");
  EXPECT_EQ(got.action_chunk_id.value_or(""), "chunk-7");
  EXPECT_EQ(got.action_chunk_sequence.value_or(0), 7);
  EXPECT_EQ(got.stale_after_sequence.value_or(0), 5);
  EXPECT_EQ(got.extra.at("episode"), "20240501-1");
}

TEST(InferRequest, RejectsEmptyRequestId) {
  auto r = InferRequest::create("", "yolov8n", {vision_input()});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, RejectsEmptyEndpoint) {
  auto r = InferRequest::create("req-1", "", {vision_input()});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, RejectsEmptyInputs) {
  auto r = InferRequest::create("req-1", "yolov8n", {});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, RejectsEmptyInputName) {
  NamedInput bad = vision_input();
  bad.name = "";
  auto r = InferRequest::create("req-1", "yolov8n", {bad});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, RejectsDuplicateInputNames) {
  auto a = vision_input();
  auto b = vision_input();  // same name "image"
  auto r = InferRequest::create("req-1", "yolov8n", {a, b});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, RejectsMissingInputBuffer) {
  NamedInput bad = vision_input();
  bad.buffer = BufferRef{};
  auto r = InferRequest::create("req-1", "yolov8n", {bad});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, RejectsMalformedMetadata) {
  RequestMetadata md;
  md.correlation_id = "";
  auto r = InferRequest::create("req-1", "yolov8n", {vision_input()}, md);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, NoDeadlineMeansNeverExpired) {
  auto r = InferRequest::create("req-1", "yolov8n", {vision_input()});
  ASSERT_TRUE(r.has_value());
  EXPECT_FALSE(r.value().is_expired());
  EXPECT_FALSE(r.value().time_until_deadline().has_value());
}

TEST(InferRequest, FutureDeadlineReportsTimeRemaining) {
  auto deadline = InferRequest::Clock::now() + milliseconds(500);
  auto r = InferRequest::create("req-1", "yolov8n", {vision_input()}, {}, deadline);
  ASSERT_TRUE(r.has_value());
  EXPECT_FALSE(r.value().is_expired());
  auto remaining = r.value().time_until_deadline();
  ASSERT_TRUE(remaining.has_value());
  EXPECT_GT(remaining->count(), 0);
  EXPECT_LE(remaining->count(), 500);
}

TEST(InferRequest, RejectsExpiredDeadline) {
  auto deadline = InferRequest::Clock::now() - milliseconds(10);
  auto r = InferRequest::create("req-1", "yolov8n", {vision_input()}, {}, deadline);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Timeout);
}

TEST(InferRequest, RelativeDeadlineFactoryRejectsNonPositive) {
  auto r = InferRequest::create_with_relative_deadline("req-1", "yolov8n", {vision_input()}, {},
                                                       milliseconds(-1));
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);

  auto zero = InferRequest::create_with_relative_deadline("req-1", "yolov8n", {vision_input()}, {},
                                                          milliseconds(0));
  ASSERT_FALSE(zero.has_value());
  EXPECT_EQ(zero.error().code, Error::Code::ConfigInvalid);
}

TEST(InferRequest, RelativeDeadlineFactoryProducesMonotonicDeadline) {
  auto r = InferRequest::create_with_relative_deadline("req-1", "yolov8n", {vision_input()}, {},
                                                       milliseconds(200));
  ASSERT_TRUE(r.has_value());
  ASSERT_TRUE(r.value().deadline().has_value());
  // Deadline is 200 ms in the future at construction; tolerate scheduling
  // jitter by bounding rather than asserting equality.
  auto remaining = r.value().time_until_deadline();
  ASSERT_TRUE(remaining.has_value());
  EXPECT_LE(remaining->count(), 200);
  EXPECT_GE(remaining->count(), 0);
}

TEST(InferRequest, RelativeDeadlineFactoryAllowsAbsentDeadline) {
  auto r = InferRequest::create_with_relative_deadline("req-1", "yolov8n", {vision_input()}, {},
                                                       std::nullopt);
  ASSERT_TRUE(r.has_value());
  EXPECT_FALSE(r.value().deadline().has_value());
}

TEST(InferRequest, EqualityComparesAllFields) {
  auto a = InferRequest::create("req-1", "yolov8n", {vision_input()});
  auto b = InferRequest::create("req-1", "yolov8n", {vision_input()});
  auto c = InferRequest::create("req-2", "yolov8n", {vision_input()});
  ASSERT_TRUE(a.has_value() && b.has_value() && c.has_value());
  EXPECT_EQ(a.value(), b.value());
  EXPECT_NE(a.value(), c.value());
}

TEST(InferRequest, ConstructionDoesNotRequireBufferPoolOrAdapter) {
  // The acceptance criterion: tests build InferRequest fixtures without
  // any runtime adapter or buffer-pool dependency. This test compiles
  // and runs the fixture using only the value-object headers; the
  // success of the surrounding test fixture suite already proves it,
  // but assert it explicitly here for documentation.
  auto r = InferRequest::create("req-1", "yolov8n", {vision_input()});
  EXPECT_TRUE(r.has_value());
}

}  // namespace
