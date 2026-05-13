// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F04: Implementation of ingress copy helpers.

#include "tensorplate/buffer/ingress.hpp"

#include <cstring>
#include <limits>
#include <span>
#include <string>
#include <unordered_set>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

Result<BufferRef> copy_payload_into_buffer(BufferManager& manager,
                                           std::span<const std::byte> payload) {
  if (payload.empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "copy_payload_into_buffer: payload must be non-empty");
  }
  auto handle = manager.allocate(payload.size());
  if (!handle) {
    return unexpected(std::move(handle).error());
  }
  auto dst = manager.data(handle.value());
  if (!dst) {
    // Roll back the allocation so accounting stays consistent.
    (void)manager.release(handle.value());
    return unexpected(std::move(dst).error());
  }
  std::memcpy(dst.value().data(), payload.data(), payload.size());
  return handle.value();
}

namespace {

// Release every BufferRef in `cleanup` back to the manager. Used on the
// failure path of build_named_inputs so partial allocations do not leak.
void rollback(BufferManager& manager, std::vector<BufferRef>& cleanup) noexcept {
  for (auto& h : cleanup) {
    (void)manager.release_if_owned(h);
  }
  cleanup.clear();
}

}  // namespace

Result<std::vector<NamedInput>> build_named_inputs(BufferManager& manager,
                                                   const std::vector<IngressInput>& inputs,
                                                   const IngressLimits& limits) {
  if (inputs.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "build_named_inputs: inputs list is empty");
  }
  if (limits.max_inputs != 0 && inputs.size() > limits.max_inputs) {
    return unexpected(Error::Code::Unsupported,
                      "build_named_inputs: input count exceeds IngressLimits.max_inputs");
  }

  // Total-size precheck. The same check happens implicitly at allocate
  // time, but doing it up front gives a clearer error and avoids
  // allocating part of a doomed request.
  std::size_t total = 0;
  for (const auto& in : inputs) {
    if (in.payload.empty()) {
      return unexpected(Error::Code::ConfigInvalid,
                        "build_named_inputs: input `" + in.name + "` has empty payload");
    }
    if (total > std::numeric_limits<std::size_t>::max() - in.payload.size()) {
      return unexpected(Error::Code::Unsupported,
                        "build_named_inputs: total payload size overflows std::size_t");
    }
    total += in.payload.size();
  }
  if (limits.max_total_bytes != 0 && total > limits.max_total_bytes) {
    return unexpected(Error::Code::Unsupported,
                      "build_named_inputs: total payload size exceeds "
                      "IngressLimits.max_total_bytes");
  }

  std::vector<NamedInput> result;
  result.reserve(inputs.size());

  std::vector<BufferRef> allocated;
  allocated.reserve(inputs.size());

  std::unordered_set<std::string> seen_names;
  seen_names.reserve(inputs.size());

  for (const auto& in : inputs) {
    if (in.name.empty()) {
      rollback(manager, allocated);
      return unexpected(Error::Code::ConfigInvalid, "build_named_inputs: input has empty name");
    }
    if (!seen_names.insert(in.name).second) {
      rollback(manager, allocated);
      return unexpected(Error::Code::ConfigInvalid,
                        "build_named_inputs: duplicate input name `" + in.name + "`");
    }

    auto handle = copy_payload_into_buffer(manager, in.payload);
    if (!handle) {
      rollback(manager, allocated);
      return unexpected(std::move(handle).error());
    }
    allocated.push_back(handle.value());

    // Validate the declared window against the allocated size. The
    // manager already enforces this for `view()` access, but the
    // request-build path wants the typed error before scheduling.
    if (in.tensor.byte_offset() > handle.value().size_bytes() ||
        in.tensor.byte_size() > handle.value().size_bytes() - in.tensor.byte_offset()) {
      rollback(manager, allocated);
      return unexpected(Error::Code::ShapeMismatch,
                        "build_named_inputs: declared tensor window does not fit inside the "
                        "allocated buffer for input `" +
                            in.name + "`");
    }

    result.push_back(NamedInput{in.name, handle.value(), in.tensor});
  }

  return result;
}

}  // namespace tensorplate
