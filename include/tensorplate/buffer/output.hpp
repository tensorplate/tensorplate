// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F05: Session output buffer helpers.
//
// Execution sessions (V01-E04) and backend adapters (V01-E05) hand
// payload bytes back to the runtime through the same buffer manager
// that holds inputs. The runtime-public output story is therefore:
//
//   - One owned BufferRef per output, allocated by the buffer manager.
//   - One TensorView per output, describing the byte window inside that
//     buffer. The view's offset+size must fit inside the buffer.
//   - A NamedOutput value object pairs the two with a stable name and
//     an optional semantic tag (e.g. "action_chunk", "logits").
//
// Adapters never expose their own internal memory types through this
// path. CUDA, TensorRT, LibTorch, Python sidecar, and (in the future)
// Vitis AI / DPU buffers stay strictly inside their owning adapter; the
// adapter copies into manager-owned storage before publishing.
//
// The helpers in this header are intentionally narrow: allocate one
// output, assemble a NamedOutput with bounds validation, and assemble
// a vector of NamedOutputs with rollback on failure. The execution-
// session NVI wrapper (V01-E04) calls these from the post-infer step.

#pragma once

#include <cstddef>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Optional diagnostic context attached to an output allocation. These
/// fields appear in log lines emitted by the manager when allocation
/// fails, and are echoed back on the NamedOutput for downstream
/// observability work (V01-E12).
struct OutputAllocationContext {
  /// Request id this output belongs to. v0.1.0 does not bake the request
  /// id into the BufferRef; it is logged on failure paths only.
  std::optional<std::string> request_id;

  /// Stable output name used by the bundle / model contract.
  std::optional<std::string> output_name;
};

/// Description of one output the session wants to allocate.
struct OutputDescriptor {
  std::string name;
  TensorView tensor;
  std::optional<std::string> semantic_tag;

  /// Optional override of the buffer's allocation size in bytes. Defaults
  /// to `tensor.byte_offset() + tensor.byte_size()` so the view fits
  /// exactly. Increase this only when the adapter writes more bytes than
  /// the view exposes (e.g., over-allocated alignment padding).
  std::size_t buffer_size_bytes = 0;
};

/// Allocate a single owned output buffer sized to fit `tensor`. The
/// returned BufferRef is Owned by the caller; pair it with `tensor` in
/// a NamedOutput, or release it on the error path.
///
/// Errors:
///   - ConfigInvalid: byte_offset + byte_size overflows std::size_t, or
///     `buffer_size_bytes < byte_offset + byte_size`.
///   - Unsupported / OOMError: passed through from BufferManager.
[[nodiscard]] Result<BufferRef> allocate_output_buffer(BufferManager& manager,
                                                       const TensorView& tensor,
                                                       std::size_t buffer_size_bytes = 0,
                                                       const OutputAllocationContext& ctx = {});

/// Assemble one NamedOutput by allocating an owned buffer and validating
/// the view bounds against it.
[[nodiscard]] Result<NamedOutput> build_named_output(BufferManager& manager,
                                                     const OutputDescriptor& descriptor,
                                                     const OutputAllocationContext& ctx = {});

/// Assemble multiple NamedOutputs. If any descriptor fails, every
/// buffer this call allocated is released before the error is returned.
/// The buffer manager's active count is unchanged on failure.
///
/// Errors include ConfigInvalid (empty descriptor list, empty name,
/// duplicate name), ShapeMismatch (view does not fit), and the standard
/// allocator errors from BufferManager.
[[nodiscard]] Result<std::vector<NamedOutput>> build_named_outputs(
    BufferManager& manager, const std::vector<OutputDescriptor>& descriptors,
    const OutputAllocationContext& ctx = {});

}  // namespace tensorplate
