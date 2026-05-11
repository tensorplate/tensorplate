// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F05: Public opaque buffer-handle value object.
//
// BufferRef is the cross-layer payload-ownership token used by input
// adapters, the scheduler, execution sessions, and backend adapters. It
// carries identity (id) and a coarse ownership state, but it does not
// expose raw memory pointers or hardware-resource destructors. The
// actual allocator and release machinery land in V01-E03 (buffer plane
// baseline); this header defines the public contract.
//
// Ownership semantics (enforced by the buffer pool in V01-E03):
//
//   Owned     The holder of an Owned handle is responsible for releasing
//             the buffer back to the pool exactly once via the buffer-
//             pool API. Releasing through any other Owned-state copy of
//             the same id is a programming error and the pool returns
//             Error::Code::Internal (idempotent / double-release
//             prevention).
//
//   Borrowed  The holder must not release; lifetime is managed by an
//             upstream Owned holder. Reading is permitted while the
//             upstream holder keeps the buffer alive.
//
//   Released  Tombstoned. The handle records the previous identity so
//             logs and metrics can attribute work, but the handle is
//             not valid for I/O. Released handles are idempotent: you
//             can compare them, copy them, and call mark_released()
//             repeatedly without effect.
//
// Copy/move semantics:
//
//   BufferRef is a trivially small value object (id + size + ownership
//   tag). Default copy and move construction/assignment are enabled and
//   produce bit-equivalent handles. The runtime relies on this so that
//   request and result value objects (`InferRequest`, `InferResult`) can
//   be built, validated, and routed without bespoke move-only plumbing.
//   The Owned-vs-Borrowed *responsibility* is enforced at the buffer-
//   pool layer in V01-E03, not by the type itself.
//
//   Holders that need the std::unique_ptr-style "move-out invalidates
//   source" guarantee should call `mark_released()` on the source
//   immediately after the move. Tests for this idiom live alongside the
//   buffer-pool release tests in V01-E03.

#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string_view>

#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Coarse ownership state recorded by a BufferRef. See header comment.
enum class BufferOwnership : std::uint8_t {
  /// Holder is responsible for releasing this buffer back to the pool.
  Owned = 0,
  /// Holder may read; release is forbidden. Lifetime is upstream.
  Borrowed = 1,
  /// Tombstone; handle records prior identity but is not valid for I/O.
  Released = 2,
};

/// Stable wire name for a BufferOwnership value. Snake_case, matches
/// `protocol/schemas/buffer_ref.json`.
[[nodiscard]] std::string_view to_string(BufferOwnership ownership) noexcept;
[[nodiscard]] std::optional<BufferOwnership> buffer_ownership_from_string(
    std::string_view name) noexcept;

/// Opaque buffer handle. See file header for ownership and lifetime rules.
///
/// Default construction yields a `Released` handle with id == 0 and
/// size == 0. This is the canonical "no buffer" sentinel and is used as
/// the placeholder when an adapter declines to publish output payload
/// (e.g., backend reports `Unsupported`).
class BufferRef {
 public:
  /// Sentinel id reserved for "no buffer" / default-constructed handles.
  static constexpr std::uint64_t kNullId = 0;

  /// Default-constructed handle: id = kNullId, size = 0, Released.
  BufferRef() noexcept = default;

  /// Build an explicit handle.
  ///
  /// Returns:
  ///   - Error::Code::ConfigInvalid if `id == kNullId` while requesting
  ///     `Owned` or `Borrowed` (kNullId is reserved for the released
  ///     sentinel).
  ///   - Error::Code::ConfigInvalid if `size_bytes == 0` while requesting
  ///     `Owned` or `Borrowed`. Size-zero buffers are not representable
  ///     in v0.1.0; downstream callers must skip the I/O entirely.
  static Result<BufferRef> create(std::uint64_t id, std::size_t size_bytes,
                                  BufferOwnership ownership) noexcept;

  // Default copy/move semantics; see file header for the documented contract.
  BufferRef(const BufferRef&) noexcept = default;
  BufferRef(BufferRef&&) noexcept = default;
  BufferRef& operator=(const BufferRef&) noexcept = default;
  BufferRef& operator=(BufferRef&&) noexcept = default;
  ~BufferRef() = default;

  /// Buffer identity within the buffer pool. Stable for the lifetime of
  /// a Owned/Borrowed handle; preserved (for logging) on Released.
  [[nodiscard]] std::uint64_t id() const noexcept { return id_; }

  /// Allocated size in bytes. Stable across copy/move. Zero on the
  /// default-constructed (Released, kNullId) handle.
  [[nodiscard]] std::size_t size_bytes() const noexcept { return size_bytes_; }

  /// Current ownership state.
  [[nodiscard]] BufferOwnership ownership() const noexcept { return ownership_; }

  /// True if this handle may be used for I/O (not Released).
  [[nodiscard]] bool is_valid() const noexcept { return ownership_ != BufferOwnership::Released; }

  /// True if this is the canonical "no buffer" sentinel.
  [[nodiscard]] bool is_null() const noexcept {
    return id_ == kNullId && ownership_ == BufferOwnership::Released;
  }

  /// Tombstone this handle. Idempotent. After this call,
  /// `is_valid()` returns false but `id()` and `size_bytes()` are
  /// preserved for logging and metric attribution.
  void mark_released() noexcept { ownership_ = BufferOwnership::Released; }

  friend bool operator==(const BufferRef& lhs, const BufferRef& rhs) noexcept {
    return lhs.id_ == rhs.id_ && lhs.size_bytes_ == rhs.size_bytes_ &&
           lhs.ownership_ == rhs.ownership_;
  }
  friend bool operator!=(const BufferRef& lhs, const BufferRef& rhs) noexcept {
    return !(lhs == rhs);
  }

 private:
  BufferRef(std::uint64_t id, std::size_t size_bytes, BufferOwnership ownership) noexcept
      : id_(id), size_bytes_(size_bytes), ownership_(ownership) {}

  std::uint64_t id_ = kNullId;
  std::size_t size_bytes_ = 0;
  BufferOwnership ownership_ = BufferOwnership::Released;
};

}  // namespace tensorplate
