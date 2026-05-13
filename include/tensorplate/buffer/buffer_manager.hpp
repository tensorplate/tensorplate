// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F01 / F02 / F06: Public buffer-manager API for the v0.1.0 CPU
// buffer plane.
//
// BufferManager owns the heap storage backing every active BufferRef and
// mediates all data access. The runtime-facing rules are:
//
//   1. BufferRef remains an opaque value object (V01-E02 contract). It
//      never carries raw pointers. Storage is reached only by calling
//      BufferManager::data() or BufferManager::view() with a valid handle.
//
//   2. Allocation returns an Owned BufferRef whose id is unique across the
//      lifetime of the BufferManager (monotonic, collision-resistant).
//      Ids are not recycled while any tombstoned/released history is
//      relevant; v0.1.0 simply never reuses ids.
//
//   3. Release is deterministic and idempotent at the manager boundary.
//      Double release returns Error::Code::Internal; release of a stale
//      or unknown handle returns Error::Code::Internal. Storage is freed
//      exactly once.
//
//   4. Accounting (capacity, in-use bytes, active count, high-water mark,
//      allocation/release failure counters) is reported via a snapshot
//      type. The snapshot is the only safe way to inspect the manager's
//      state from outside; it never exposes internal storage.
//
//   5. Memory-pressure state is derived from the same accounting and
//      exposed alongside the snapshot. The baseline policy is intentionally
//      conservative: warning at >= 75 %, critical at >= 90 %, configurable
//      via BufferManagerConfig.
//
// This header is consumed by:
//   - input adapters (V01-E03-F04 copy-fallback path, V01-E07 HTTP router)
//   - execution sessions / adapters for output allocation (V01-E03-F05)
//   - scheduler tests and cleanup helpers (V01-E03-F03, V01-E06)
//
// No vendor SDK type appears here; nothing in this header pulls in CUDA,
// TensorRT, LibTorch, or any other hardware SDK.

#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Coarse memory-pressure state derived from buffer-manager accounting.
/// The exact thresholds are configurable; defaults live in
/// `BufferManagerConfig`.
enum class MemoryPressure : std::uint8_t {
  /// In-use bytes below the warning threshold.
  Normal = 0,
  /// In-use bytes between the warning and critical thresholds.
  Warning = 1,
  /// In-use bytes at or above the critical threshold.
  Critical = 2,
};

/// Stable snake_case name for a MemoryPressure value. Matches the
/// JSON schema in `protocol/schemas/buffer_pressure_event.json`.
[[nodiscard]] std::string_view to_string(MemoryPressure pressure) noexcept;

/// Configuration for a BufferManager instance. All fields are validated
/// on `BufferManager::create`; invalid values surface as
/// Error::Code::ConfigInvalid.
struct BufferManagerConfig {
  /// Stable, low-cardinality label used for metrics and pressure events.
  /// Snake_case is recommended. Empty defaults to "default".
  std::string pool_name = "default";

  /// Maximum total in-use bytes the manager may hand out at one time.
  /// Allocation requests that would push in-use above this value are
  /// rejected with Error::Code::OOMError. The cap is a soft limit:
  /// the manager does not pre-reserve memory from the OS.
  std::size_t capacity_bytes = 256ULL * 1024ULL * 1024ULL;  // 256 MiB default

  /// Maximum bytes for a single allocation. Defaults to 64 MiB, which
  /// covers a 4K RGB image plus headroom; raise it for VLA-class
  /// payloads. Set to 0 to disable the per-buffer cap.
  std::size_t max_buffer_bytes = 64ULL * 1024ULL * 1024ULL;

  /// Default alignment in bytes for buffers when the caller does not
  /// request one explicitly. Must be a power of two between 1 and
  /// kMaxAlignment. Defaults to 64 to match common SIMD/cache-line use.
  std::size_t default_alignment = 64;

  /// Warning pressure threshold expressed as a fraction of capacity_bytes.
  /// Defaults to 0.75 (75 %). Must be in (0.0, 1.0] and <= critical.
  double warning_threshold = 0.75;

  /// Critical pressure threshold expressed as a fraction of capacity_bytes.
  /// Defaults to 0.90 (90 %). Must be in (0.0, 1.0] and >= warning.
  double critical_threshold = 0.90;
};

/// Snapshot of BufferManager accounting at one point in time. Cheap to
/// take (single mutex acquisition) and safe to share. The snapshot never
/// carries raw pointers or storage; it is the only public way to inspect
/// manager state.
struct BufferAccounting {
  /// Stable pool label echoed back from `BufferManagerConfig::pool_name`.
  std::string pool_name;

  /// Configured cap in bytes (`BufferManagerConfig::capacity_bytes`).
  std::size_t capacity_bytes = 0;

  /// Sum of `size_bytes` across every currently active buffer.
  std::size_t in_use_bytes = 0;

  /// Number of currently active (Owned) buffers.
  std::size_t active_count = 0;

  /// Largest value `in_use_bytes` has reached since manager construction.
  std::size_t high_water_bytes = 0;

  /// Monotonically increasing count of allocation failures (capacity or
  /// per-buffer cap rejection, allocator-throwing bad_alloc). Excludes
  /// validation rejections (zero-byte, alignment errors) since those are
  /// caller bugs, not pressure signal.
  std::uint64_t allocation_failures = 0;

  /// Monotonically increasing count of release failures (double-release,
  /// stale/unknown handle).
  std::uint64_t release_failures = 0;

  /// Derived pressure level using the configured thresholds.
  MemoryPressure pressure = MemoryPressure::Normal;
};

/// Pressure-transition event. Emitted by the manager when `pressure`
/// changes value. Plain data; the manager records a bounded in-memory copy
/// that callers can drain outside allocation/release hot paths. Stable wire
/// names live in `protocol/schemas/buffer_pressure_event.json`.
struct BufferPressureEvent {
  std::string pool_name;
  MemoryPressure previous = MemoryPressure::Normal;
  MemoryPressure current = MemoryPressure::Normal;
  std::size_t capacity_bytes = 0;
  std::size_t in_use_bytes = 0;
  std::size_t active_count = 0;
  std::size_t high_water_bytes = 0;
  std::uint64_t allocation_failures = 0;
};

/// CPU-backed buffer plane.
///
/// Lifetime:
///   - Manager outlives every BufferRef it issues. Destroying a manager
///     with active buffers reclaims their storage and marks them in the
///     internal table as Released, but downstream value-object copies of
///     those BufferRef handles already in flight cannot be retroactively
///     tombstoned. Tear down deterministically: drain the scheduler,
///     release all outputs, and then drop the manager.
///
/// Thread safety:
///   - All public methods are safe to call concurrently from multiple
///     threads. A single mutex protects allocation, release, lookup, and
///     accounting; this is correct and cheap for v0.1.0. Per-buffer hot
///     paths (read/write through `data()`) take only the lookup mutex.
///
/// No vendor SDK type leaks through this interface.
class BufferManager {
 public:
  /// Maximum supported alignment. Larger alignments require platform-
  /// specific allocators that are out of scope for v0.1.0.
  static constexpr std::size_t kMaxAlignment = 4096;

  /// Maximum number of pressure transitions retained for later draining.
  /// When the ring is full, newer transitions overwrite the oldest entries.
  static constexpr std::size_t kPressureEventBufferCapacity = 32;

  /// Validating factory. Returns Error::Code::ConfigInvalid on bad config.
  static Result<std::unique_ptr<BufferManager>> create(BufferManagerConfig config);

  ~BufferManager();

  BufferManager(const BufferManager&) = delete;
  BufferManager& operator=(const BufferManager&) = delete;
  BufferManager(BufferManager&&) = delete;
  BufferManager& operator=(BufferManager&&) = delete;

  /// Allocate `size_bytes` of owned storage. Returns an Owned BufferRef.
  ///
  /// Validation errors (typed):
  ///   - ConfigInvalid: size_bytes == 0, alignment is not a power of two,
  ///     alignment exceeds `kMaxAlignment`.
  ///   - Unsupported: size_bytes exceeds the configured per-buffer cap.
  ///   - OOMError: allocation would push in-use bytes above capacity, or
  ///     the underlying allocator threw bad_alloc.
  ///
  /// Successful allocation:
  ///   - increments active_count and in_use_bytes,
  ///   - updates high_water_bytes if needed,
  ///   - emits a pressure-transition event if the new in-use crosses a
  ///     threshold,
  ///   - returns a BufferRef with a fresh, monotonically increasing id.
  ///
  /// `alignment == 0` uses the manager's `default_alignment`.
  [[nodiscard]] Result<BufferRef> allocate(std::size_t size_bytes, std::size_t alignment = 0);

  /// Release `handle` back to the manager.
  ///
  /// Errors:
  ///   - Internal: handle is unknown to this manager (never issued, or
  ///     issued by a different manager).
  ///   - Internal: handle is already Released in the manager's table.
  ///
  /// Side effects on success:
  ///   - Storage is freed.
  ///   - active_count and in_use_bytes are decremented.
  ///   - A pressure-transition event is emitted if the new in-use crosses
  ///     a threshold downward.
  ///
  /// Release is safe to call from cleanup paths (cancellation, timeout,
  /// shutdown). Callers should pass the handle by value; the manager
  /// does not mutate the caller's copy. See `release_if_owned` for the
  /// cleanup-helper form that ignores already-Released handles.
  [[nodiscard]] Result<void> release(BufferRef handle);

  /// Release `handle` if it is Owned and known to the manager. Used by
  /// cancellation/timeout cleanup paths that may be called with a mix
  /// of Owned and Released handles.
  ///
  /// Returns true if storage was actually freed. Returns false (without
  /// recording a release failure) if the handle is the canonical "no
  /// buffer" sentinel or already Released. Returns an Error only for
  /// unexpected manager-state mismatches.
  [[nodiscard]] Result<bool> release_if_owned(BufferRef handle);

  /// Borrow read access to the bytes behind `handle`. The returned span is
  /// invalidated when the buffer is released; callers must complete the
  /// read before any concurrent release path runs. Returns Error if the
  /// handle is unknown or already Released.
  ///
  /// View kept narrow on purpose: a single accessor for read/write keeps
  /// the data-access boundary auditable. v0.1.0 has no const-only mode.
  [[nodiscard]] Result<std::span<std::byte>> data(const BufferRef& handle);
  [[nodiscard]] Result<std::span<const std::byte>> data(const BufferRef& handle) const;

  /// Copy a tensor-window described by `view` out of `handle`. Validates
  /// `view.byte_offset() + view.byte_size() <= handle.size_bytes()`.
  /// Returned span is the subwindow only; same invalidation rules as
  /// `data()`.
  [[nodiscard]] Result<std::span<const std::byte>> view(const BufferRef& handle,
                                                        const TensorView& view) const;

  /// Snapshot of current accounting plus derived pressure state.
  [[nodiscard]] BufferAccounting accounting() const;

  /// Drain recorded pressure-transition events. Allocation and release only
  /// append to a bounded in-memory ring; callbacks and I/O are intentionally
  /// kept out of those hot paths. This method copies the retained events and
  /// clears the ring.
  [[nodiscard]] std::vector<BufferPressureEvent> drain_pressure_events();

  /// Returns the configured pool label (low-cardinality metric label).
  [[nodiscard]] std::string_view pool_name() const noexcept;

  /// Returns the validated config used to construct this manager.
  [[nodiscard]] const BufferManagerConfig& config() const noexcept;

 private:
  struct Impl;
  explicit BufferManager(std::unique_ptr<Impl> impl) noexcept;
  std::unique_ptr<Impl> impl_;
};

}  // namespace tensorplate
