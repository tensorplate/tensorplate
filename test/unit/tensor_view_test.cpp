// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F06-T01 / T02 / T03 unit coverage for tensorplate::TensorView.

#include "tensorplate/buffer/tensor_view.hpp"

#include <gtest/gtest.h>

#include <cstdint>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace {

using tensorplate::DType;
using tensorplate::dtype_byte_width;
using tensorplate::dtype_from_string;
using tensorplate::Error;
using tensorplate::Layout;
using tensorplate::layout_from_string;
using tensorplate::TensorView;
using tensorplate::to_string;

TEST(TensorView, DTypeNamesRoundTripViaSnakeCase) {
  for (auto d : {DType::Float32, DType::Float16, DType::BFloat16, DType::Int64, DType::Int32,
                 DType::Int16, DType::Int8, DType::UInt8, DType::Bool}) {
    auto parsed = dtype_from_string(to_string(d));
    ASSERT_TRUE(parsed.has_value());
    EXPECT_EQ(*parsed, d);
  }
}

TEST(TensorView, DTypeByteWidthsAreLocked) {
  EXPECT_EQ(dtype_byte_width(DType::Float32), 4u);
  EXPECT_EQ(dtype_byte_width(DType::Float16), 2u);
  EXPECT_EQ(dtype_byte_width(DType::BFloat16), 2u);
  EXPECT_EQ(dtype_byte_width(DType::Int64), 8u);
  EXPECT_EQ(dtype_byte_width(DType::Int32), 4u);
  EXPECT_EQ(dtype_byte_width(DType::Int16), 2u);
  EXPECT_EQ(dtype_byte_width(DType::Int8), 1u);
  EXPECT_EQ(dtype_byte_width(DType::UInt8), 1u);
  EXPECT_EQ(dtype_byte_width(DType::Bool), 1u);
}

TEST(TensorView, LayoutNamesRoundTripViaSnakeCase) {
  EXPECT_EQ(to_string(Layout::RowMajor), "row_major");
  EXPECT_EQ(to_string(Layout::ColMajor), "col_major");
  EXPECT_EQ(layout_from_string("row_major").value(), Layout::RowMajor);
  EXPECT_EQ(layout_from_string("col_major").value(), Layout::ColMajor);
  EXPECT_FALSE(layout_from_string("RowMajor").has_value());
}

TEST(TensorView, ValidConstructionPreservesAllFields) {
  auto r = TensorView::create(DType::Float16, {1, 3, 224, 224}, Layout::RowMajor);
  ASSERT_TRUE(r.has_value());
  const auto& v = r.value();
  EXPECT_EQ(v.dtype(), DType::Float16);
  EXPECT_EQ(v.layout(), Layout::RowMajor);
  EXPECT_EQ(v.rank(), 4u);
  EXPECT_EQ(v.num_elements(), 1 * 3 * 224 * 224);
  EXPECT_EQ(v.byte_size(), static_cast<std::size_t>(1 * 3 * 224 * 224 * 2));
  EXPECT_EQ(v.byte_offset(), 0u);
}

TEST(TensorView, ByteSizeAutomaticWhenZero) {
  auto r = TensorView::create(DType::Float32, {10});
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().byte_size(), 40u);
}

TEST(TensorView, ExplicitByteSizeAllowsPadding) {
  auto r = TensorView::create(DType::Float32, {10}, Layout::RowMajor, 0, 64);
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().byte_size(), 64u);
}

TEST(TensorView, ExplicitByteSizeMustBeAtLeastComputed) {
  auto r = TensorView::create(DType::Float32, {10}, Layout::RowMajor, 0, 16);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
}

TEST(TensorView, EmptyShapeIsRejected) {
  auto r = TensorView::create(DType::Float32, {});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
}

TEST(TensorView, NonPositiveDimIsRejected) {
  auto r1 = TensorView::create(DType::Float32, {1, 0, 3});
  ASSERT_FALSE(r1.has_value());
  EXPECT_EQ(r1.error().code, Error::Code::ShapeMismatch);

  auto r2 = TensorView::create(DType::Float32, {1, -1});
  ASSERT_FALSE(r2.has_value());
  EXPECT_EQ(r2.error().code, Error::Code::ShapeMismatch);
}

TEST(TensorView, ChunkShapedOutputIsExpressible) {
  // SmolVLA-style action chunk: [chunk_size, action_dim] with float32.
  auto r = TensorView::create(DType::Float32, {16, 7});
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().rank(), 2u);
  EXPECT_EQ(r.value().num_elements(), 16 * 7);
  EXPECT_EQ(r.value().byte_size(), static_cast<std::size_t>(16 * 7 * 4));
}

TEST(TensorView, ByteOffsetIsPreservedForSubBufferLayouts) {
  auto r = TensorView::create(DType::UInt8, {640, 480, 3}, Layout::RowMajor, 4096);
  ASSERT_TRUE(r.has_value());
  EXPECT_EQ(r.value().byte_offset(), 4096u);
}

TEST(TensorView, EqualityComparesAllFields) {
  auto a = TensorView::create(DType::Float16, {1, 3});
  auto b = TensorView::create(DType::Float16, {1, 3});
  auto c = TensorView::create(DType::Float32, {1, 3});
  auto d = TensorView::create(DType::Float16, {3, 1});
  auto e = TensorView::create(DType::Float16, {1, 3}, Layout::ColMajor);
  ASSERT_TRUE(a.has_value() && b.has_value() && c.has_value() && d.has_value() && e.has_value());

  EXPECT_EQ(a.value(), b.value());
  EXPECT_NE(a.value(), c.value());
  EXPECT_NE(a.value(), d.value());
  EXPECT_NE(a.value(), e.value());
}

}  // namespace
