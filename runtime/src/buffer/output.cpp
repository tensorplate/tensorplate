// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F05: Implementation of session output buffer helpers.

#include "tensorplate/buffer/output.hpp"

#include <cstddef>
#include <limits>
#include <string>
#include <unordered_set>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

namespace {

// Compute the minimum buffer size that holds the tensor window. Returns
// std::nullopt on overflow so the caller can surface a typed error.
std::optional<std::size_t> required_buffer_size(const TensorView& view) noexcept {
  const std::size_t offset = view.byte_offset();
  const std::size_t size = view.byte_size();
  if (offset > std::numeric_limits<std::size_t>::max() - size) {
    return std::nullopt;
  }
  return offset + size;
}

void rollback(BufferManager& manager, std::vector<BufferRef>& allocated) noexcept {
  for (auto& h : allocated) {
    (void)manager.release_if_owned(h);
  }
  allocated.clear();
}

}  // namespace

Result<BufferRef> allocate_output_buffer(BufferManager& manager, const TensorView& tensor,
                                         std::size_t buffer_size_bytes,
                                         const OutputAllocationContext& /*ctx*/) {
  auto required = required_buffer_size(tensor);
  if (!required.has_value()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "allocate_output_buffer: tensor byte_offset + byte_size overflows "
                      "std::size_t");
  }
  std::size_t alloc_size = buffer_size_bytes != 0 ? buffer_size_bytes : *required;
  if (alloc_size < *required) {
    return unexpected(Error::Code::ConfigInvalid,
                      "allocate_output_buffer: buffer_size_bytes is smaller than the tensor "
                      "window described by byte_offset + byte_size");
  }
  // Allocation context is currently used only for log attribution at
  // failure boundaries; the manager records pool-level metrics and
  // V01-E12 wires per-request labels above this layer.
  return manager.allocate(alloc_size);
}

Result<NamedOutput> build_named_output(BufferManager& manager, const OutputDescriptor& descriptor,
                                       const OutputAllocationContext& ctx) {
  if (descriptor.name.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "build_named_output: descriptor has empty name");
  }
  auto handle =
      allocate_output_buffer(manager, descriptor.tensor, descriptor.buffer_size_bytes, ctx);
  if (!handle) {
    return unexpected(std::move(handle).error());
  }
  // The size sanity check above is sufficient, but keeping the explicit
  // bounds-vs-buffer check here means future view-side changes still
  // produce a single typed error before the value object is assembled.
  auto required = required_buffer_size(descriptor.tensor);
  if (!required.has_value() || *required > handle.value().size_bytes()) {
    (void)manager.release_if_owned(handle.value());
    return unexpected(Error::Code::ShapeMismatch,
                      "build_named_output: tensor window does not fit inside allocated "
                      "buffer for output `" +
                          descriptor.name + "`");
  }
  NamedOutput out{descriptor.name, handle.value(), descriptor.tensor, descriptor.semantic_tag};
  return out;
}

Result<std::vector<NamedOutput>> build_named_outputs(
    BufferManager& manager, const std::vector<OutputDescriptor>& descriptors,
    const OutputAllocationContext& ctx) {
  if (descriptors.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "build_named_outputs: descriptor list is empty");
  }
  std::vector<NamedOutput> result;
  result.reserve(descriptors.size());
  std::vector<BufferRef> allocated;
  allocated.reserve(descriptors.size());
  std::unordered_set<std::string> seen_names;
  seen_names.reserve(descriptors.size());

  for (const auto& d : descriptors) {
    if (d.name.empty()) {
      rollback(manager, allocated);
      return unexpected(Error::Code::ConfigInvalid,
                        "build_named_outputs: descriptor has empty name");
    }
    if (!seen_names.insert(d.name).second) {
      rollback(manager, allocated);
      return unexpected(Error::Code::ConfigInvalid,
                        "build_named_outputs: duplicate output name `" + d.name + "`");
    }
    auto out = build_named_output(manager, d, ctx);
    if (!out) {
      rollback(manager, allocated);
      return unexpected(std::move(out).error());
    }
    allocated.push_back(out.value().buffer);
    result.push_back(std::move(out).value());
  }
  return result;
}

}  // namespace tensorplate
