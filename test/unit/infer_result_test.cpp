// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F04-T01 / T02 / T03 unit coverage for tensorplate::InferResult.

#include "tensorplate/core/infer_result.hpp"

#include <gtest/gtest.h>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

#include <chrono>
#include <utility>
#include <vector>

namespace {

using std::chrono::nanoseconds;
using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::InferenceTiming;
using tensorplate::InferResult;
using tensorplate::Layout;
using tensorplate::NamedOutput;
using tensorplate::TensorView;

NamedOutput vla_chunk_output() {
  // chunk_size = 16, action_dim = 7, dtype f32 -> 16 * 7 * 4 = 448 bytes
  auto buf = BufferRef::create(101, 16 * 7 * 4, BufferOwnership::Owned);
  auto view = TensorView::create(DType::Float32, {16, 7});
  return NamedOutput{"action_chunk", buf.value(), view.value(),
                     std::optional<std::string>{"action_chunk"}};
}

NamedOutput detection_output() {
  auto buf = BufferRef::create(102, 100 * 6 * 4, BufferOwnership::Owned);
  auto view = TensorView::create(DType::Float32, {100, 6});
  return NamedOutput{"detections", buf.value(), view.value(), std::nullopt};
}

TEST(InferResult, SuccessWithChunkOutputPreservesShape) {
  auto r = InferResult::create_success("req-1", {vla_chunk_output()});
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_success());
  EXPECT_FALSE(r.value().is_failure());
  ASSERT_EQ(r.value().outputs().size(), 1u);
  EXPECT_EQ(r.value().outputs()[0].name, "action_chunk");
  EXPECT_EQ(r.value().outputs()[0].tensor.shape().size(), 2u);
  EXPECT_EQ(r.value().outputs()[0].tensor.shape()[0], 16);
  EXPECT_EQ(r.value().outputs()[0].tensor.shape()[1], 7);
  EXPECT_EQ(r.value().outputs()[0].semantic_tag.value_or(""), "action_chunk");
}

TEST(InferResult, MultipleNamedOutputsPreserveOrder) {
  auto r = InferResult::create_success("req-1", {vla_chunk_output(), detection_output()});
  ASSERT_TRUE(r.has_value());
  ASSERT_EQ(r.value().outputs().size(), 2u);
  EXPECT_EQ(r.value().outputs()[0].name, "action_chunk");
  EXPECT_EQ(r.value().outputs()[1].name, "detections");
}

TEST(InferResult, SuccessRejectsEmptyOutputs) {
  auto r = InferResult::create_success("req-1", {});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferResult, SuccessRejectsEmptyOutputName) {
  auto bad = vla_chunk_output();
  bad.name = "";
  auto r = InferResult::create_success("req-1", {bad});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferResult, SuccessRejectsDuplicateOutputNames) {
  auto r = InferResult::create_success("req-1", {vla_chunk_output(), vla_chunk_output()});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(InferResult, FailurePreservesTypedError) {
  auto err = Error::make(Error::Code::Timeout, "deadline exceeded");
  auto r = InferResult::create_failure("req-2", err);
  EXPECT_TRUE(r.is_failure());
  EXPECT_FALSE(r.is_success());
  EXPECT_EQ(r.error(), err);
  EXPECT_EQ(r.error().code, Error::Code::Timeout);
}

TEST(InferResult, FailureAllowsEmptyRequestId) {
  // Ingress-time errors fail before request_id is parsed.
  auto err = Error::make(Error::Code::ConfigInvalid, "malformed payload");
  auto r = InferResult::create_failure("", err);
  EXPECT_TRUE(r.is_failure());
  EXPECT_EQ(r.request_id(), "");
}

TEST(InferResult, AccessorsReturnSafeDefaultsOnWrongState) {
  auto err = Error::make(Error::Code::Timeout, "x");
  auto failure = InferResult::create_failure("req", err);
  // outputs() on failure is empty, never throws.
  EXPECT_TRUE(failure.outputs().empty());

  auto success = InferResult::create_success("req", {vla_chunk_output()});
  ASSERT_TRUE(success.has_value());
  // error() on success returns a placeholder Internal error, never throws.
  EXPECT_EQ(success.value().error().code, Error::Code::Internal);
  EXPECT_TRUE(success.value().error().message.empty());
}

TEST(InferResult, TimingFieldsAreOptionalAndPreserved) {
  InferenceTiming t;
  t.queue_latency = nanoseconds(2'000'000);
  t.execution_latency = nanoseconds(8'000'000);
  // total_latency intentionally absent

  auto err = Error::make(Error::Code::Timeout, "x");
  auto failure = InferResult::create_failure("req", err, t);
  EXPECT_EQ(failure.timing(), t);
  EXPECT_TRUE(failure.timing().queue_latency.has_value());
  EXPECT_FALSE(failure.timing().total_latency.has_value());
}

TEST(InferResult, EqualityComparesAllFields) {
  auto a = InferResult::create_success("req-1", {vla_chunk_output()});
  auto b = InferResult::create_success("req-1", {vla_chunk_output()});
  auto c = InferResult::create_success("req-2", {vla_chunk_output()});
  ASSERT_TRUE(a.has_value() && b.has_value() && c.has_value());
  EXPECT_EQ(a.value(), b.value());
  EXPECT_NE(a.value(), c.value());

  auto err = Error::make(Error::Code::Timeout, "x");
  auto d = InferResult::create_failure("req-1", err);
  auto e = InferResult::create_failure("req-1", err);
  EXPECT_EQ(d, e);
  EXPECT_NE(a.value(), d);  // success != failure even with the same id.
}

TEST(InferResult, ErrorStatusIsCompatibleWithErrorTaxonomy) {
  // Verify every Error::Code can be used in a failure result; this is
  // the F04 acceptance criterion that result error status is "compatible
  // with tp::Error".
  for (auto code : {Error::Code::ConfigInvalid, Error::Code::LoadFailed, Error::Code::NotReady,
                    Error::Code::ShapeMismatch, Error::Code::Unsupported, Error::Code::OOMError,
                    Error::Code::Timeout, Error::Code::InferenceFailed, Error::Code::Internal}) {
    auto r = InferResult::create_failure("req", Error::make(code, "x"));
    EXPECT_TRUE(r.is_failure());
    EXPECT_EQ(r.error().code, code);
  }
}

}  // namespace
