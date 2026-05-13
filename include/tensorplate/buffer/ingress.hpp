// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F04: Single-copy ingress helpers.
//
// These helpers turn caller-owned byte spans into buffer-plane-owned
// storage so that no caller memory crosses into the runtime. They are
// the primitives that V01-E07's HTTP endpoint and the request router
// will consume; this header does not implement HTTP or any wire
// protocol.
//
// Design rules:
//
//   1. One copy. The helpers allocate via `BufferManager`, copy bytes
//      in, and return an Owned `BufferRef`. No in-place borrowing of
//      caller memory.
//   2. Validation before allocation. Empty spans, oversized payloads,
//      and impossible declared windows are rejected *before* the
//      manager is asked to allocate. This keeps allocation-failure
//      counters meaningful as a pressure signal.
//   3. Deterministic cleanup. If a multi-input ingress build fails
//      partway through, any already-allocated payload buffers are
//      released before the error is returned.
//   4. Backend-neutral. The helpers do not assume HTTP, Unix socket
//      IPC, or any specific transport. They take spans + descriptors.

#pragma once

#include <cstddef>
#include <cstdint>
#include <span>
#include <string>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Ingress-time per-input descriptor. Names + dtype/shape/layout come
/// from the wire payload; the bytes are the raw payload region.
struct IngressInput {
  /// Stable input name (matches model bundle naming).
  std::string name;

  /// Raw payload bytes. The helper copies them into manager storage and
  /// never retains a pointer to this span.
  std::span<const std::byte> payload;

  /// Tensor metadata declared by the caller. The byte window described
  /// by `(byte_offset, byte_size)` must fit inside the allocated buffer,
  /// where the allocated buffer is sized to `max(payload.size(), declared
  /// tensor footprint)`.
  TensorView tensor;
};

/// Optional limits for an ingress pass. v0.1.0 defaults are conservative;
/// the serving worker can override per-endpoint.
struct IngressLimits {
  /// Maximum total bytes across all inputs in one request. 0 disables.
  /// Defaults to 64 MiB.
  std::size_t max_total_bytes = 64ULL * 1024ULL * 1024ULL;

  /// Maximum number of named inputs in one request. 0 disables.
  /// Defaults to 16 — enough for SmolVLA-class multi-camera + state.
  std::size_t max_inputs = 16;
};

/// Allocate buffer-plane storage and copy `payload` into it.
///
/// Errors:
///   - ConfigInvalid: payload is empty.
///   - Unsupported: payload exceeds per-buffer or per-request limits.
///   - OOMError: buffer-plane capacity rejection.
///
/// On success the returned BufferRef is Owned by the caller; the caller
/// must either install it in an InferRequest or release it.
[[nodiscard]] Result<BufferRef> copy_payload_into_buffer(BufferManager& manager,
                                                         std::span<const std::byte> payload);

/// Build a NamedInput vector from an ingress descriptor list. Each
/// payload is copied into a fresh Owned buffer; the matching TensorView
/// byte window is validated against the allocated buffer size.
///
/// On failure, every buffer this call allocated is released before the
/// error is returned so the buffer manager's active count is unchanged.
///
/// Errors:
///   - ConfigInvalid: empty `inputs` list, empty name, empty payload,
///     or duplicate input name.
///   - Unsupported: total payload size exceeds `limits.max_total_bytes`,
///     or input count exceeds `limits.max_inputs`.
///   - ShapeMismatch: declared tensor window does not fit inside the
///     allocated buffer.
///   - OOMError: buffer-plane capacity rejection.
[[nodiscard]] Result<std::vector<NamedInput>> build_named_inputs(
    BufferManager& manager, const std::vector<IngressInput>& inputs,
    const IngressLimits& limits = {});

}  // namespace tensorplate
