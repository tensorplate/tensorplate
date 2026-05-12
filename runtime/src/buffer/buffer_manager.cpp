// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F01 / F02 / F06: CPU buffer manager implementation.
//
// The implementation is intentionally simple: each allocation is a
// distinct heap block tracked by a monotonic id. A single mutex guards
// the lookup table and accounting counters. v0.1.0 does not need a
// slab/arena allocator; correctness, deterministic release, and clear
// metrics matter more than allocation throughput at this stage.

#include "tensorplate/buffer/buffer_manager.hpp"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <mutex>
#include <new>
#include <span>
#include <string>
#include <string_view>
#include <unordered_map>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

namespace {

constexpr std::string_view kPressureNormal = "normal";
constexpr std::string_view kPressureWarning = "warning";
constexpr std::string_view kPressureCritical = "critical";

bool is_power_of_two(std::size_t value) noexcept {
  return value != 0 && (value & (value - 1)) == 0;
}

std::size_t round_up_to_alignment(std::size_t value, std::size_t alignment) noexcept {
  if (alignment <= 1) {
    return value;
  }
  return (value + alignment - 1) & ~(alignment - 1);
}

}  // namespace

std::string_view to_string(MemoryPressure pressure) noexcept {
  switch (pressure) {
    case MemoryPressure::Normal:
      return kPressureNormal;
    case MemoryPressure::Warning:
      return kPressureWarning;
    case MemoryPressure::Critical:
      return kPressureCritical;
  }
  return kPressureNormal;
}

struct BufferManager::Impl {
  BufferManagerConfig config;
  mutable std::mutex mu;

  // Monotonic id generator. id 0 is reserved for the released sentinel
  // (`BufferRef::kNullId`); the first allocation is id 1.
  std::uint64_t next_id = 1;

  struct Entry {
    // Storage block, aligned to the request. Allocated with operator new
    // aligned variant; freed via the matching delete.
    std::byte* storage = nullptr;
    std::size_t size_bytes = 0;
    std::size_t alignment = 0;
    bool released = false;
  };

  // Active table. We keep released entries in the table briefly after
  // release so a subsequent release through a copied handle can be
  // diagnosed as double-release rather than "unknown id" — but only
  // until destructor / explicit reset. The simple v0.1.0 policy is to
  // *retain* released entries until destruction so the diagnostics stay
  // useful in test fixtures. This costs O(1) per id (no payload), which
  // is acceptable for the v0.1.0 traffic envelope.
  std::unordered_map<std::uint64_t, Entry> entries;

  // Accounting. All updates happen under `mu`.
  std::size_t in_use_bytes = 0;
  std::size_t active_count = 0;
  std::size_t high_water_bytes = 0;
  std::uint64_t allocation_failures = 0;
  std::uint64_t release_failures = 0;
  MemoryPressure last_pressure = MemoryPressure::Normal;

  // Pressure-subscriber registry.
  std::uint64_t next_subscription_id = 1;
  std::vector<std::pair<std::uint64_t, PressureSubscriber>> pressure_subscribers;

  MemoryPressure compute_pressure_locked() const noexcept {
    if (config.capacity_bytes == 0) {
      return MemoryPressure::Normal;
    }
    const double fraction = static_cast<double>(in_use_bytes) /
                            static_cast<double>(config.capacity_bytes);
    if (fraction >= config.critical_threshold) {
      return MemoryPressure::Critical;
    }
    if (fraction >= config.warning_threshold) {
      return MemoryPressure::Warning;
    }
    return MemoryPressure::Normal;
  }

  // Emit a pressure event to all subscribers if the level changed.
  // Subscribers run synchronously under `mu`; the contract documents that
  // they must not block or re-enter the manager.
  void maybe_emit_pressure_event_locked() {
    const MemoryPressure now = compute_pressure_locked();
    if (now == last_pressure) {
      return;
    }
    BufferPressureEvent event{};
    event.pool_name = config.pool_name;
    event.previous = last_pressure;
    event.current = now;
    event.capacity_bytes = config.capacity_bytes;
    event.in_use_bytes = in_use_bytes;
    event.active_count = active_count;
    event.high_water_bytes = high_water_bytes;
    event.allocation_failures = allocation_failures;
    last_pressure = now;
    for (const auto& [_, sub] : pressure_subscribers) {
      if (sub) {
        sub(event);
      }
    }
  }

  static void free_storage(std::byte* p, std::size_t alignment) noexcept {
    if (p == nullptr) {
      return;
    }
    // Match the aligned new used in `allocate_storage`.
    ::operator delete(p, std::align_val_t{alignment});
  }

  static std::byte* allocate_storage(std::size_t size, std::size_t alignment) {
    // Size is rounded up to alignment so the underlying allocator can
    // serve the request reliably across platforms.
    const std::size_t allocated = round_up_to_alignment(size, alignment);
    return static_cast<std::byte*>(::operator new(allocated, std::align_val_t{alignment}));
  }
};

namespace {

Result<void> validate_config(const BufferManagerConfig& cfg) {
  if (cfg.capacity_bytes == 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "BufferManagerConfig.capacity_bytes must be > 0");
  }
  if (cfg.max_buffer_bytes != 0 && cfg.max_buffer_bytes > cfg.capacity_bytes) {
    return unexpected(Error::Code::ConfigInvalid,
                      "BufferManagerConfig.max_buffer_bytes must be <= capacity_bytes");
  }
  if (!is_power_of_two(cfg.default_alignment) ||
      cfg.default_alignment > BufferManager::kMaxAlignment) {
    return unexpected(Error::Code::ConfigInvalid,
                      "BufferManagerConfig.default_alignment must be a power of two in "
                      "[1, kMaxAlignment]");
  }
  if (!(cfg.warning_threshold > 0.0 && cfg.warning_threshold <= 1.0)) {
    return unexpected(Error::Code::ConfigInvalid,
                      "BufferManagerConfig.warning_threshold must be in (0.0, 1.0]");
  }
  if (!(cfg.critical_threshold > 0.0 && cfg.critical_threshold <= 1.0)) {
    return unexpected(Error::Code::ConfigInvalid,
                      "BufferManagerConfig.critical_threshold must be in (0.0, 1.0]");
  }
  if (cfg.critical_threshold < cfg.warning_threshold) {
    return unexpected(Error::Code::ConfigInvalid,
                      "BufferManagerConfig.critical_threshold must be >= warning_threshold");
  }
  return Result<void>{};
}

}  // namespace

BufferManager::BufferManager(std::unique_ptr<Impl> impl) noexcept : impl_(std::move(impl)) {}

BufferManager::~BufferManager() {
  if (!impl_) {
    return;
  }
  // Reclaim any storage still held in the table. We do not invoke
  // subscribers from the destructor: the manager is going away and
  // there is no useful pressure transition to report.
  std::lock_guard<std::mutex> lock(impl_->mu);
  for (auto& [_, entry] : impl_->entries) {
    if (!entry.released && entry.storage != nullptr) {
      Impl::free_storage(entry.storage, entry.alignment);
      entry.storage = nullptr;
      entry.released = true;
    }
  }
  impl_->entries.clear();
}

Result<std::unique_ptr<BufferManager>> BufferManager::create(BufferManagerConfig config) {
  if (config.pool_name.empty()) {
    config.pool_name = "default";
  }
  auto ok = validate_config(config);
  if (!ok) {
    return unexpected(std::move(ok).error());
  }
  auto impl = std::make_unique<Impl>();
  impl->config = std::move(config);
  // Wrap with the private constructor; std::make_unique cannot reach it.
  return std::unique_ptr<BufferManager>(new BufferManager(std::move(impl)));
}

Result<BufferRef> BufferManager::allocate(std::size_t size_bytes, std::size_t alignment) {
  if (size_bytes == 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "BufferManager.allocate: size_bytes must be > 0");
  }
  std::size_t effective_alignment = alignment;
  {
    std::lock_guard<std::mutex> lock(impl_->mu);
    if (effective_alignment == 0) {
      effective_alignment = impl_->config.default_alignment;
    }
    if (!is_power_of_two(effective_alignment) || effective_alignment > kMaxAlignment) {
      return unexpected(Error::Code::ConfigInvalid,
                        "BufferManager.allocate: alignment must be a power of two in "
                        "[1, kMaxAlignment]");
    }
    if (impl_->config.max_buffer_bytes != 0 && size_bytes > impl_->config.max_buffer_bytes) {
      // Per-buffer cap is a contract limit, surfaced as Unsupported so the
      // caller can distinguish "you asked for too much" from "the system
      // is full" (OOMError).
      return unexpected(Error::Code::Unsupported,
                        "BufferManager.allocate: requested size exceeds "
                        "BufferManagerConfig.max_buffer_bytes");
    }
    if (impl_->in_use_bytes + size_bytes < impl_->in_use_bytes ||  // overflow guard
        impl_->in_use_bytes + size_bytes > impl_->config.capacity_bytes) {
      ++impl_->allocation_failures;
      impl_->maybe_emit_pressure_event_locked();
      return unexpected(Error::Code::OOMError,
                        "BufferManager.allocate: capacity_bytes would be exceeded");
    }

    std::byte* storage = nullptr;
    try {
      storage = Impl::allocate_storage(size_bytes, effective_alignment);
    } catch (const std::bad_alloc&) {
      ++impl_->allocation_failures;
      impl_->maybe_emit_pressure_event_locked();
      return unexpected(Error::Code::OOMError,
                        "BufferManager.allocate: underlying allocator failed");
    }

    const std::uint64_t id = impl_->next_id++;
    Impl::Entry entry{};
    entry.storage = storage;
    entry.size_bytes = size_bytes;
    entry.alignment = effective_alignment;
    entry.released = false;
    impl_->entries.emplace(id, entry);

    impl_->in_use_bytes += size_bytes;
    ++impl_->active_count;
    if (impl_->in_use_bytes > impl_->high_water_bytes) {
      impl_->high_water_bytes = impl_->in_use_bytes;
    }
    impl_->maybe_emit_pressure_event_locked();

    auto ref = BufferRef::create(id, size_bytes, BufferOwnership::Owned);
    if (!ref) {
      // Should not happen given the validation above; treat as a manager
      // invariant violation.
      Impl::free_storage(storage, effective_alignment);
      impl_->entries.erase(id);
      impl_->in_use_bytes -= size_bytes;
      --impl_->active_count;
      ++impl_->allocation_failures;
      return unexpected(std::move(ref).error());
    }
    return ref;
  }
}

Result<void> BufferManager::release(BufferRef handle) {
  if (handle.is_null()) {
    return unexpected(Error::Code::Internal,
                      "BufferManager.release: refusing to release the null sentinel handle");
  }
  std::lock_guard<std::mutex> lock(impl_->mu);
  auto it = impl_->entries.find(handle.id());
  if (it == impl_->entries.end()) {
    ++impl_->release_failures;
    return unexpected(Error::Code::Internal,
                      "BufferManager.release: handle id " + std::to_string(handle.id()) +
                          " is not known to this manager");
  }
  auto& entry = it->second;
  if (entry.released) {
    ++impl_->release_failures;
    return unexpected(Error::Code::Internal,
                      "BufferManager.release: double release of handle id " +
                          std::to_string(handle.id()));
  }
  if (entry.size_bytes != handle.size_bytes()) {
    ++impl_->release_failures;
    return unexpected(Error::Code::Internal,
                      "BufferManager.release: handle size_bytes disagrees with manager record");
  }
  Impl::free_storage(entry.storage, entry.alignment);
  entry.storage = nullptr;
  entry.released = true;

  impl_->in_use_bytes -= entry.size_bytes;
  --impl_->active_count;
  impl_->maybe_emit_pressure_event_locked();
  return Result<void>{};
}

Result<bool> BufferManager::release_if_owned(BufferRef handle) {
  if (handle.is_null() || handle.ownership() == BufferOwnership::Released) {
    return false;
  }
  auto r = release(std::move(handle));
  if (!r) {
    return unexpected(std::move(r).error());
  }
  return true;
}

Result<std::span<std::byte>> BufferManager::data(const BufferRef& handle) {
  std::lock_guard<std::mutex> lock(impl_->mu);
  auto it = impl_->entries.find(handle.id());
  if (it == impl_->entries.end() || it->second.released) {
    return unexpected(Error::Code::Internal,
                      "BufferManager.data: handle is unknown or already released");
  }
  return std::span<std::byte>(it->second.storage, it->second.size_bytes);
}

Result<std::span<const std::byte>> BufferManager::data(const BufferRef& handle) const {
  std::lock_guard<std::mutex> lock(impl_->mu);
  auto it = impl_->entries.find(handle.id());
  if (it == impl_->entries.end() || it->second.released) {
    return unexpected(Error::Code::Internal,
                      "BufferManager.data: handle is unknown or already released");
  }
  return std::span<const std::byte>(it->second.storage, it->second.size_bytes);
}

Result<std::span<const std::byte>> BufferManager::view(const BufferRef& handle,
                                                       const TensorView& view) const {
  std::lock_guard<std::mutex> lock(impl_->mu);
  auto it = impl_->entries.find(handle.id());
  if (it == impl_->entries.end() || it->second.released) {
    return unexpected(Error::Code::Internal,
                      "BufferManager.view: handle is unknown or already released");
  }
  const auto& entry = it->second;
  // Overflow-safe bounds check: byte_offset + byte_size <= size_bytes.
  if (view.byte_offset() > entry.size_bytes ||
      view.byte_size() > entry.size_bytes - view.byte_offset()) {
    return unexpected(Error::Code::ShapeMismatch,
                      "BufferManager.view: tensor window [offset, offset+size) exceeds buffer "
                      "size_bytes");
  }
  return std::span<const std::byte>(entry.storage + view.byte_offset(), view.byte_size());
}

BufferAccounting BufferManager::accounting() const {
  std::lock_guard<std::mutex> lock(impl_->mu);
  BufferAccounting snap{};
  snap.pool_name = impl_->config.pool_name;
  snap.capacity_bytes = impl_->config.capacity_bytes;
  snap.in_use_bytes = impl_->in_use_bytes;
  snap.active_count = impl_->active_count;
  snap.high_water_bytes = impl_->high_water_bytes;
  snap.allocation_failures = impl_->allocation_failures;
  snap.release_failures = impl_->release_failures;
  snap.pressure = impl_->compute_pressure_locked();
  return snap;
}

std::uint64_t BufferManager::subscribe_pressure(PressureSubscriber subscriber) {
  if (!subscriber) {
    return 0;
  }
  std::lock_guard<std::mutex> lock(impl_->mu);
  const std::uint64_t id = impl_->next_subscription_id++;
  impl_->pressure_subscribers.emplace_back(id, std::move(subscriber));
  return id;
}

void BufferManager::unsubscribe_pressure(std::uint64_t subscription_id) {
  if (subscription_id == 0) {
    return;
  }
  std::lock_guard<std::mutex> lock(impl_->mu);
  auto& subs = impl_->pressure_subscribers;
  subs.erase(std::remove_if(subs.begin(), subs.end(),
                            [&](const auto& kv) { return kv.first == subscription_id; }),
             subs.end());
}

std::string_view BufferManager::pool_name() const noexcept {
  return impl_->config.pool_name;
}

const BufferManagerConfig& BufferManager::config() const noexcept {
  return impl_->config;
}

}  // namespace tensorplate
