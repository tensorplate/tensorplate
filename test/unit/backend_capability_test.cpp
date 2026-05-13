// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01-T01: Unit tests for `BackendCapability`.

#include "tensorplate/backend/capability.hpp"

#include <gtest/gtest.h>

#include <string>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/model_spec.hpp"

namespace tensorplate {
namespace {

TEST(BackendCapability, MinimalConstructionSucceeds) {
  auto cap = BackendCapability::create("tensorrt", {PrecisionHint::Fp16, PrecisionHint::Int8});
  ASSERT_TRUE(cap.has_value()) << cap.error().message;
  EXPECT_EQ(cap.value().backend_name(), "tensorrt");
  EXPECT_EQ(cap.value().supported_precision().size(), 2u);
  EXPECT_EQ(cap.value().shape_support(), ShapeSupport::Dynamic);
  EXPECT_FALSE(cap.value().supports_async());
  EXPECT_FALSE(cap.value().supports_generation());
  EXPECT_FALSE(cap.value().supports_streaming());
  EXPECT_FALSE(cap.value().supports_kv_cache());
  EXPECT_FALSE(cap.value().profile_id().has_value());
}

TEST(BackendCapability, EmptyBackendNameRejected) {
  auto cap = BackendCapability::create("", {PrecisionHint::Fp32});
  ASSERT_FALSE(cap.has_value());
  EXPECT_EQ(cap.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendCapability, EmptyPrecisionListRejected) {
  auto cap = BackendCapability::create("tensorrt", {});
  ASSERT_FALSE(cap.has_value());
  EXPECT_EQ(cap.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendCapability, EmptyProfileIdRejected) {
  auto cap = BackendCapability::create("tensorrt", {PrecisionHint::Fp16}, ShapeSupport::Fixed,
                                       std::optional<std::string>{""});
  ASSERT_FALSE(cap.has_value());
  EXPECT_EQ(cap.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendCapability, OutOfRangeOpCoverageRejected) {
  auto cap = BackendCapability::create(
      "tensorrt", {PrecisionHint::Fp16}, ShapeSupport::Dynamic, std::nullopt,
      /*supports_async=*/false, false, false, false,
      /*op_coverage_score_pct=*/std::optional<std::uint8_t>{200});
  ASSERT_FALSE(cap.has_value());
  EXPECT_EQ(cap.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendCapability, EstimateGreaterThanLimitRejected) {
  auto cap = BackendCapability::create(
      "tensorrt", {PrecisionHint::Fp16}, ShapeSupport::Dynamic, std::nullopt, false, false, false,
      false, std::nullopt,
      /*memory_estimate_bytes=*/std::optional<std::uint64_t>{200u},
      /*memory_limit_bytes=*/std::optional<std::uint64_t>{100u});
  ASSERT_FALSE(cap.has_value());
  EXPECT_EQ(cap.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendCapability, AcceptsPrecisionRespectsAutoAndList) {
  auto cap =
      BackendCapability::create("tensorrt", {PrecisionHint::Fp16, PrecisionHint::Int8}).value();
  EXPECT_TRUE(cap.accepts_precision(PrecisionHint::Auto));
  EXPECT_TRUE(cap.accepts_precision(PrecisionHint::Fp16));
  EXPECT_TRUE(cap.accepts_precision(PrecisionHint::Int8));
  EXPECT_FALSE(cap.accepts_precision(PrecisionHint::Fp32));
  EXPECT_FALSE(cap.accepts_precision(PrecisionHint::Int4));
}

TEST(BackendCapability, ShapeSupportRoundTrips) {
  EXPECT_EQ(to_string(ShapeSupport::Dynamic), "dynamic");
  EXPECT_EQ(to_string(ShapeSupport::Fixed), "fixed");
  EXPECT_EQ(to_string(ShapeSupport::RangeBounded), "range_bounded");
  EXPECT_EQ(shape_support_from_string("dynamic").value(), ShapeSupport::Dynamic);
  EXPECT_EQ(shape_support_from_string("fixed").value(), ShapeSupport::Fixed);
  EXPECT_EQ(shape_support_from_string("range_bounded").value(), ShapeSupport::RangeBounded);
  EXPECT_FALSE(shape_support_from_string("garbage").has_value());
}

TEST(BackendCapability, EqualityCoversEveryField) {
  auto a = BackendCapability::create("tensorrt", {PrecisionHint::Fp16}).value();
  auto b = BackendCapability::create("tensorrt", {PrecisionHint::Fp16}).value();
  EXPECT_EQ(a, b);

  auto c = BackendCapability::create("tensorrt", {PrecisionHint::Fp32}).value();
  EXPECT_NE(a, c);
}

}  // namespace
}  // namespace tensorplate
