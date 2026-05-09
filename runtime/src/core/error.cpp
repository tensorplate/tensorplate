// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01-T01: Implementation of Error::Code <-> string mappings and
// Error::format(). These are the only routines that need a translation unit;
// Error itself is a header-only value object.

#include "tensorplate/core/error.hpp"

#include <array>
#include <cstddef>
#include <string>
#include <string_view>
#include <utility>

namespace tensorplate {

namespace {

// Stable wire-format names. Order matches Error::Code enumeration; updates
// here must keep `protocol/schemas/error.json` and
// `protocol/rust/src/error.rs` in lockstep.
constexpr std::array<std::pair<Error::Code, std::string_view>, 9> kCodeNames = {{
    {Error::Code::ConfigInvalid, "config_invalid"},
    {Error::Code::LoadFailed, "load_failed"},
    {Error::Code::NotReady, "not_ready"},
    {Error::Code::ShapeMismatch, "shape_mismatch"},
    {Error::Code::Unsupported, "unsupported"},
    {Error::Code::OOMError, "oom_error"},
    {Error::Code::Timeout, "timeout"},
    {Error::Code::InferenceFailed, "inference_failed"},
    {Error::Code::Internal, "internal"},
}};

}  // namespace

std::string_view to_string(Error::Code code) noexcept {
  for (const auto& [c, name] : kCodeNames) {
    if (c == code) {
      return name;
    }
  }
  return "internal";
}

std::optional<Error::Code> error_code_from_string(std::string_view name) noexcept {
  for (const auto& [c, candidate] : kCodeNames) {
    if (candidate == name) {
      return c;
    }
  }
  return std::nullopt;
}

std::string format(const Error& err) {
  std::string out;
  out.reserve(err.message.size() + 32);
  out += '[';
  out += to_string(err.code);
  out += "] ";
  out += err.message;
  if (err.context.has_value()) {
    out += " (";
    out += *err.context;
    out += ')';
  }
  return out;
}

}  // namespace tensorplate
