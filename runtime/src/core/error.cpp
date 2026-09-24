// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01-T01: Implementation of Error::Code <-> string mappings and
// Error::format(). These are the only routines that need a translation unit;
// Error itself is a header-only value object.

#include "tensorplate/core/error.hpp"

#include <cstdint>
#include <string>
#include <string_view>

namespace tensorplate {

namespace {

// Stable wire-format names; `protocol/schemas/error.json` lists them in this
// numeric order and `protocol/rust/src/error.rs` mirrors them. No default:
// -Wswitch makes a code appended to Error::Code without a name here a build
// error. An empty result means the value is not an enumerator.
constexpr std::string_view name_of(Error::Code code) noexcept {
  switch (code) {
    case Error::Code::ConfigInvalid:
      return "config_invalid";
    case Error::Code::LoadFailed:
      return "load_failed";
    case Error::Code::NotReady:
      return "not_ready";
    case Error::Code::ShapeMismatch:
      return "shape_mismatch";
    case Error::Code::Unsupported:
      return "unsupported";
    case Error::Code::OOMError:
      return "oom_error";
    case Error::Code::Timeout:
      return "timeout";
    case Error::Code::InferenceFailed:
      return "inference_failed";
    case Error::Code::Internal:
      return "internal";
    case Error::Code::Cancelled:
      return "cancelled";
    case Error::Code::Unavailable:
      return "unavailable";
    case Error::Code::ResourceExhausted:
      return "resource_exhausted";
  }
  return {};
}

}  // namespace

std::string_view to_string(Error::Code code) noexcept {
  const std::string_view name = name_of(code);
  return name.empty() ? std::string_view{"internal"} : name;
}

std::optional<Error::Code> error_code_from_string(std::string_view name) noexcept {
  if (name.empty()) {
    return std::nullopt;
  }
  // Codes are dense from 0 (values only ever append), so the first value
  // without a name ends the search.
  for (std::uint32_t value = 0;; ++value) {
    const auto code = static_cast<Error::Code>(value);
    const std::string_view candidate = name_of(code);
    if (candidate.empty()) {
      return std::nullopt;
    }
    if (candidate == name) {
      return code;
    }
  }
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
