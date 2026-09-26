// SPDX-License-Identifier: Apache-2.0
//
// Strict UTF-8 well-formedness check shared by the job value objects.

#pragma once

#include <cstddef>
#include <cstdint>
#include <string_view>

namespace tensorplate::internal {

/// True iff `text` is well-formed UTF-8 (Unicode Table 3-7): no overlong
/// form, no surrogate (U+D800..U+DFFF), nothing above U+10FFFF and no
/// truncated sequence. The empty string is well-formed.
[[nodiscard]] constexpr bool is_well_formed_utf8(std::string_view text) noexcept {
  const auto at = [text](std::size_t i) { return static_cast<std::uint8_t>(text[i]); };
  const auto continues = [text, at](std::size_t i, std::uint8_t lo, std::uint8_t hi) {
    return i < text.size() && at(i) >= lo && at(i) <= hi;
  };
  std::size_t i = 0;
  while (i < text.size()) {
    const std::uint8_t lead = at(i);
    std::size_t length = 0;
    std::uint8_t lo = 0x80;
    std::uint8_t hi = 0xBF;
    if (lead <= 0x7F) {
      length = 1;
    } else if (lead >= 0xC2 && lead <= 0xDF) {
      length = 2;
    } else if (lead >= 0xE0 && lead <= 0xEF) {
      length = 3;
      lo = lead == 0xE0 ? 0xA0 : 0x80;  // overlong
      hi = lead == 0xED ? 0x9F : 0xBF;  // surrogates
    } else if (lead >= 0xF0 && lead <= 0xF4) {
      length = 4;
      lo = lead == 0xF0 ? 0x90 : 0x80;  // overlong
      hi = lead == 0xF4 ? 0x8F : 0xBF;  // above U+10FFFF
    } else {
      return false;
    }
    for (std::size_t k = 1; k < length; ++k) {
      if (!continues(i + k, k == 1 ? lo : std::uint8_t{0x80}, k == 1 ? hi : std::uint8_t{0xBF})) {
        return false;
      }
    }
    i += length;
  }
  return true;
}

}  // namespace tensorplate::internal
