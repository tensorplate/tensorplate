// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F03: Cancellation, timeout, and error-path release helpers.

#include "tensorplate/buffer/cleanup.hpp"

#include <cstdint>
#include <unordered_set>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"

namespace tensorplate {

namespace {

// Deduplicate by id so malformed request fixtures with the same buffer
// referenced twice do not cause a double-release in this helper. The
// underlying manager would correctly diagnose the second call, but the
// helper's contract is to release each unique buffer at most once.
void release_unique(BufferManager& manager, BufferRef handle, CleanupReport& report) {
  if (handle.is_null() || handle.ownership() == BufferOwnership::Released) {
    return;
  }
  auto r = manager.release_if_owned(handle);
  if (!r) {
    report.errors.push_back(std::move(r).error());
    return;
  }
  if (r.value()) {
    ++report.buffers_released;
  }
}

}  // namespace

CleanupReport release_request_buffers(BufferManager& manager, const InferRequest& request) noexcept {
  CleanupReport report;
  std::unordered_set<std::uint64_t> seen;
  seen.reserve(request.inputs().size());
  for (const auto& input : request.inputs()) {
    const auto id = input.buffer.id();
    if (id == BufferRef::kNullId) {
      continue;
    }
    if (!seen.insert(id).second) {
      continue;
    }
    release_unique(manager, input.buffer, report);
  }
  return report;
}

CleanupReport release_partial_outputs(BufferManager& manager,
                                      const std::vector<NamedOutput>& outputs) noexcept {
  CleanupReport report;
  std::unordered_set<std::uint64_t> seen;
  seen.reserve(outputs.size());
  for (const auto& out : outputs) {
    const auto id = out.buffer.id();
    if (id == BufferRef::kNullId) {
      continue;
    }
    if (!seen.insert(id).second) {
      continue;
    }
    release_unique(manager, out.buffer, report);
  }
  return report;
}

RequestBufferGuard::RequestBufferGuard(BufferManager& manager, const InferRequest& request) noexcept
    : manager_(&manager), request_(&request) {}

RequestBufferGuard::~RequestBufferGuard() {
  if (dismissed_ || manager_ == nullptr || request_ == nullptr) {
    return;
  }
  report_ = release_request_buffers(*manager_, *request_);
}

void RequestBufferGuard::dismiss() noexcept { dismissed_ = true; }

}  // namespace tensorplate
