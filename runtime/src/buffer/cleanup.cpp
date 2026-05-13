// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F03: Cancellation, timeout, and error-path release helpers.

#include "tensorplate/buffer/cleanup.hpp"

#include <cstdint>
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
void record_error(CleanupReport& report, Error&& error) noexcept {
  ++report.release_errors;
  try {
    report.errors.push_back(std::move(error));
  } catch (...) {
    ++report.dropped_errors;
  }
}

void record_cleanup_exception(CleanupReport& report) noexcept {
  ++report.release_errors;
  try {
    report.errors.push_back(
        Error::make(Error::Code::Internal,
                    "buffer cleanup helper caught an exception while releasing a buffer"));
  } catch (...) {
    ++report.dropped_errors;
  }
}

void release_unique(BufferManager& manager, BufferRef handle, CleanupReport& report) noexcept {
  if (handle.is_null() || handle.ownership() == BufferOwnership::Released) {
    return;
  }
  try {
    auto r = manager.release_if_owned(handle);
    if (!r) {
      record_error(report, std::move(r).error());
      return;
    }
    if (r.value()) {
      ++report.buffers_released;
    }
  } catch (...) {
    record_cleanup_exception(report);
  }
}

bool seen_input_id(const std::vector<NamedInput>& inputs, std::size_t index) noexcept {
  const auto id = inputs[index].buffer.id();
  for (std::size_t i = 0; i < index; ++i) {
    if (inputs[i].buffer.id() == id) {
      return true;
    }
  }
  return false;
}

bool seen_output_id(const std::vector<NamedOutput>& outputs, std::size_t index) noexcept {
  const auto id = outputs[index].buffer.id();
  for (std::size_t i = 0; i < index; ++i) {
    if (outputs[i].buffer.id() == id) {
      return true;
    }
  }
  return false;
}

}  // namespace

CleanupReport release_request_buffers(BufferManager& manager,
                                      const InferRequest& request) noexcept {
  CleanupReport report;
  const auto& inputs = request.inputs();
  for (std::size_t i = 0; i < inputs.size(); ++i) {
    const auto& input = inputs[i];
    const auto id = input.buffer.id();
    if (id == BufferRef::kNullId) {
      continue;
    }
    if (seen_input_id(inputs, i)) {
      continue;
    }
    release_unique(manager, input.buffer, report);
  }
  return report;
}

CleanupReport release_partial_outputs(BufferManager& manager,
                                      const std::vector<NamedOutput>& outputs) noexcept {
  CleanupReport report;
  for (std::size_t i = 0; i < outputs.size(); ++i) {
    const auto& out = outputs[i];
    const auto id = out.buffer.id();
    if (id == BufferRef::kNullId) {
      continue;
    }
    if (seen_output_id(outputs, i)) {
      continue;
    }
    release_unique(manager, out.buffer, report);
  }
  return report;
}

RequestBufferGuard::RequestBufferGuard(BufferManager& manager, const InferRequest& request) noexcept
    : manager_(&manager), request_(&request) {}

RequestBufferGuard::~RequestBufferGuard() noexcept {
  if (dismissed_ || manager_ == nullptr || request_ == nullptr) {
    return;
  }
  report_ = release_request_buffers(*manager_, *request_);
}

void RequestBufferGuard::dismiss() noexcept {
  dismissed_ = true;
}

}  // namespace tensorplate
