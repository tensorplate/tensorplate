// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F06: Public tensor-metadata value object.
//
// TensorView describes the shape, dtype, layout, and byte-window of a
// tensor region inside a BufferRef. It does **not** own memory and does
// not depend on any vendor SDK. Backends (TensorRT, LibTorch, the
// Python sidecar, or a future Vitis AI adapter) interpret the metadata
// to materialize their own tensor primitives without leaking SDK types
// into public TensorPlate headers.

#pragma once

#include "tensorplate/core/result.hpp"

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string_view>
#include <vector>

namespace tensorplate {

/// Element data type. Stable wire names live in tensor_view.cpp / the JSON
/// Schema. v0.1.0 covers the dtypes required by vision-on-TensorRT and
/// SmolVLA-on-`python_pytorch`; additional dtypes can be added without a
/// schema bump.
enum class DType : std::uint8_t {
  Float32 = 0,
  Float16 = 1,
  BFloat16 = 2,
  Int64 = 3,
  Int32 = 4,
  Int16 = 5,
  Int8 = 6,
  UInt8 = 7,
  Bool = 8,
};

/// Memory layout. v0.1.0 supports row-major (C-contiguous) and column-major
/// (Fortran-contiguous) only; per-axis stride support is deferred.
enum class Layout : std::uint8_t {
  /// Last index changes fastest. Default. Matches NumPy / PyTorch default.
  RowMajor = 0,
  /// First index changes fastest. Used by some Vitis AI / DPU buffers.
  ColMajor = 1,
};

/// Byte width of a DType element.
[[nodiscard]] std::size_t dtype_byte_width(DType dtype) noexcept;

/// Stable wire name for a DType / Layout. Snake_case, matches the JSON
/// Schema. Inverse parsers return std::nullopt for unknown strings so
/// JSON decoders can map to Error::Code::Unsupported.
[[nodiscard]] std::string_view to_string(DType dtype) noexcept;
[[nodiscard]] std::string_view to_string(Layout layout) noexcept;
[[nodiscard]] std::optional<DType> dtype_from_string(std::string_view name) noexcept;
[[nodiscard]] std::optional<Layout> layout_from_string(std::string_view name) noexcept;

/// Tensor metadata value object. Pairs with a BufferRef to describe an
/// input or output tensor without owning memory.
///
/// Invariants enforced by the validating factory:
///   - shape is non-empty (rank >= 1).
///   - All shape entries are >= 1 (no zero-volume tensors in v0.1.0).
///   - dtype/layout enum values are within range.
///   - byte_size == product(shape) * dtype_byte_width(dtype).
///
/// byte_offset and byte_size describe the placement of the tensor inside
/// its owning BufferRef; offset + size <= buffer.size_bytes() is checked
/// at the consumer (e.g., the scheduler or session NVI) where the buffer
/// size is known. The TensorView itself does not see the buffer.
class TensorView {
 public:
  /// Validating factory.
  ///
  /// Returns Error::Code::ShapeMismatch if shape is empty, contains a
  /// non-positive entry, or the implied byte_size disagrees with the
  /// argument. Returns Error::Code::ConfigInvalid for nonsense
  /// dtype/layout values that escape enum class typing (e.g., from a
  /// JSON decode of an unknown string).
  ///
  /// If `byte_size` is zero, it is computed automatically from
  /// product(shape) * dtype_byte_width(dtype). Pass an explicit
  /// `byte_size` only when describing an over-allocated buffer where
  /// the trailing bytes are reserved padding.
  static Result<TensorView> create(DType dtype, std::vector<std::int64_t> shape,
                                   Layout layout = Layout::RowMajor,
                                   std::size_t byte_offset = 0, std::size_t byte_size = 0);

  [[nodiscard]] DType dtype() const noexcept { return dtype_; }
  [[nodiscard]] const std::vector<std::int64_t>& shape() const noexcept { return shape_; }
  [[nodiscard]] Layout layout() const noexcept { return layout_; }
  [[nodiscard]] std::size_t byte_offset() const noexcept { return byte_offset_; }
  [[nodiscard]] std::size_t byte_size() const noexcept { return byte_size_; }
  [[nodiscard]] std::size_t rank() const noexcept { return shape_.size(); }

  /// Number of elements (product of shape). Always positive after
  /// successful construction.
  [[nodiscard]] std::int64_t num_elements() const noexcept;

  friend bool operator==(const TensorView& lhs, const TensorView& rhs) noexcept {
    return lhs.dtype_ == rhs.dtype_ && lhs.layout_ == rhs.layout_ && lhs.shape_ == rhs.shape_ &&
           lhs.byte_offset_ == rhs.byte_offset_ && lhs.byte_size_ == rhs.byte_size_;
  }
  friend bool operator!=(const TensorView& lhs, const TensorView& rhs) noexcept {
    return !(lhs == rhs);
  }

 private:
  TensorView() = default;

  DType dtype_ = DType::Float32;
  Layout layout_ = Layout::RowMajor;
  std::vector<std::int64_t> shape_;
  std::size_t byte_offset_ = 0;
  std::size_t byte_size_ = 0;
};

}  // namespace tensorplate
