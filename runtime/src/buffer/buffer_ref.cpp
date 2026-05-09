// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F05-T01: BufferRef validation and ownership-string mappings.

#include "tensorplate/buffer/buffer_ref.hpp"

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

#include <array>
#include <string_view>
#include <utility>

namespace tensorplate {

namespace {

constexpr std::array<std::pair<BufferOwnership, std::string_view>, 3> kOwnershipNames = {{
    {BufferOwnership::Owned, "owned"},
    {BufferOwnership::Borrowed, "borrowed"},
    {BufferOwnership::Released, "released"},
}};

}  // namespace

std::string_view to_string(BufferOwnership ownership) noexcept {
  for (const auto& [o, name] : kOwnershipNames) {
    if (o == ownership) return name;
  }
  return "released";
}

std::optional<BufferOwnership> buffer_ownership_from_string(std::string_view name) noexcept {
  for (const auto& [o, candidate] : kOwnershipNames) {
    if (candidate == name) return o;
  }
  return std::nullopt;
}

Result<BufferRef> BufferRef::create(std::uint64_t id, std::size_t size_bytes,
                                    BufferOwnership ownership) noexcept {
  // Active handles must carry a non-sentinel id and a non-zero size.
  // The Released alternative is reserved for the default-constructed
  // sentinel and for explicit tombstones via mark_released().
  if (ownership != BufferOwnership::Released) {
    if (id == kNullId) {
      return unexpected(Error::Code::ConfigInvalid,
                        "BufferRef.id == 0 is reserved for the released sentinel");
    }
    if (size_bytes == 0) {
      return unexpected(Error::Code::ConfigInvalid,
                        "BufferRef.size_bytes must be > 0 for Owned/Borrowed handles");
    }
  }
  return BufferRef{id, size_bytes, ownership};
}

}  // namespace tensorplate
