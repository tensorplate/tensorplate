// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F06-T01 / T02: TensorView validation, dtype/layout string
// mappings, and byte-width tables shared with the JSON Schema and the
// Rust mirror.

#include "tensorplate/buffer/tensor_view.hpp"

#include <array>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <string_view>
#include <utility>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

namespace {

constexpr std::array<std::pair<DType, std::string_view>, 9> kDtypeNames = {{
    {DType::Float32, "float32"},
    {DType::Float16, "float16"},
    {DType::BFloat16, "bfloat16"},
    {DType::Int64, "int64"},
    {DType::Int32, "int32"},
    {DType::Int16, "int16"},
    {DType::Int8, "int8"},
    {DType::UInt8, "uint8"},
    {DType::Bool, "bool"},
}};

constexpr std::array<std::pair<Layout, std::string_view>, 2> kLayoutNames = {{
    {Layout::RowMajor, "row_major"},
    {Layout::ColMajor, "col_major"},
}};

constexpr std::array<std::pair<DType, std::size_t>, 9> kDtypeByteWidths = {{
    {DType::Float32, 4},
    {DType::Float16, 2},
    {DType::BFloat16, 2},
    {DType::Int64, 8},
    {DType::Int32, 4},
    {DType::Int16, 2},
    {DType::Int8, 1},
    {DType::UInt8, 1},
    {DType::Bool, 1},
}};

}  // namespace

std::size_t dtype_byte_width(DType dtype) noexcept {
  for (const auto& [d, width] : kDtypeByteWidths) {
    if (d == dtype) {
      return width;
    }
  }
  return 0;
}

std::string_view to_string(DType dtype) noexcept {
  for (const auto& [d, name] : kDtypeNames) {
    if (d == dtype) {
      return name;
    }
  }
  return "float32";
}

std::string_view to_string(Layout layout) noexcept {
  for (const auto& [l, name] : kLayoutNames) {
    if (l == layout) {
      return name;
    }
  }
  return "row_major";
}

std::optional<DType> dtype_from_string(std::string_view name) noexcept {
  for (const auto& [d, candidate] : kDtypeNames) {
    if (candidate == name) {
      return d;
    }
  }
  return std::nullopt;
}

std::optional<Layout> layout_from_string(std::string_view name) noexcept {
  for (const auto& [l, candidate] : kLayoutNames) {
    if (candidate == name) {
      return l;
    }
  }
  return std::nullopt;
}

std::int64_t TensorView::num_elements() const noexcept {
  std::int64_t n = 1;
  for (auto d : shape_) {
    n *= d;
  }
  return n;
}

namespace {

// Computes product(shape) * dtype_byte_width(dtype) with overflow-aware
// arithmetic. Returns std::nullopt on overflow so the caller can surface a
// typed Error::Code::ShapeMismatch.
std::optional<std::size_t> compute_byte_size(const std::vector<std::int64_t>& shape,
                                             DType dtype) noexcept {
  const std::size_t element_width = dtype_byte_width(dtype);
  if (element_width == 0) {
    return std::nullopt;
  }
  std::size_t total = element_width;
  for (auto d : shape) {
    if (d <= 0) {
      return std::nullopt;
    }
    const auto ud = static_cast<std::size_t>(d);
    if (ud != 0 && total > std::numeric_limits<std::size_t>::max() / ud) {
      return std::nullopt;
    }
    total *= ud;
  }
  return total;
}

}  // namespace

Result<TensorView> TensorView::create(DType dtype, std::vector<std::int64_t> shape, Layout layout,
                                      std::size_t byte_offset, std::size_t byte_size) {
  // Enum range checks. Anything outside the declared values comes from
  // upstream JSON decoding of an unknown string; surface the typed error.
  if (dtype_byte_width(dtype) == 0) {
    return unexpected(Error::Code::ConfigInvalid, "TensorView.dtype is not a recognized value");
  }
  if (layout != Layout::RowMajor && layout != Layout::ColMajor) {
    return unexpected(Error::Code::ConfigInvalid, "TensorView.layout is not a recognized value");
  }
  if (shape.empty()) {
    return unexpected(Error::Code::ShapeMismatch, "TensorView.shape must be rank >= 1");
  }
  for (auto d : shape) {
    if (d <= 0) {
      return unexpected(Error::Code::ShapeMismatch,
                        "TensorView.shape entries must be >= 1; zero-volume tensors are not "
                        "representable in v0.1.0");
    }
  }
  auto computed = compute_byte_size(shape, dtype);
  if (!computed.has_value()) {
    return unexpected(Error::Code::ShapeMismatch,
                      "TensorView byte-size computation overflowed std::size_t");
  }
  if (byte_size == 0) {
    byte_size = *computed;
  } else if (byte_size < *computed) {
    return unexpected(Error::Code::ShapeMismatch,
                      "TensorView.byte_size is smaller than product(shape) * "
                      "dtype_byte_width(dtype)");
  }

  TensorView v;
  v.dtype_ = dtype;
  v.layout_ = layout;
  v.shape_ = std::move(shape);
  v.byte_offset_ = byte_offset;
  v.byte_size_ = byte_size;
  return v;
}

}  // namespace tensorplate
