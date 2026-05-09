// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F01: Public error value object and error-code taxonomy.
//
// Error is a plain value object. It carries a stable `Code`, a human-readable
// message, and an optional context string. Hardware resources are never held.
// The same code values appear in `protocol/schemas/error.json` (snake_case
// strings) and in the Rust mirror at `protocol/rust/src/error.rs`.

#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

namespace tensorplate {

/// Typed error value used by Result<T> and by hardware-boundary operations.
struct Error {
  /// Stable error codes shared by C++, Rust, and protocol schemas.
  ///
  /// Numeric values are part of the C++ ABI but **not** part of the wire
  /// protocol; the wire encoding uses the snake_case names from
  /// `to_string(Code)` and matches `protocol/schemas/error.json`.
  enum class Code : std::uint32_t {
    /// Schema, config, or manifest validation failure.
    ConfigInvalid = 0,
    /// Model artifact could not be loaded.
    LoadFailed = 1,
    /// Session is not in a state that permits this operation.
    NotReady = 2,
    /// Tensor shape does not match the model contract.
    ShapeMismatch = 3,
    /// Operation, capability, or schema version is not supported.
    Unsupported = 4,
    /// Out-of-memory during allocation or execution.
    OOMError = 5,
    /// Operation exceeded its deadline.
    Timeout = 6,
    /// Inference execution failed for backend-specific reasons.
    InferenceFailed = 7,
    /// Unexpected internal error; usually a bug.
    Internal = 8,
  };

  Code code = Code::Internal;
  std::string message;
  std::optional<std::string> context;

  /// Equality compares all three fields.
  friend bool operator==(const Error& lhs, const Error& rhs) noexcept {
    return lhs.code == rhs.code && lhs.message == rhs.message && lhs.context == rhs.context;
  }
  friend bool operator!=(const Error& lhs, const Error& rhs) noexcept { return !(lhs == rhs); }

  /// Convenience factory matching the protocol "code/message/context" shape.
  static Error make(Code c, std::string msg) {
    return Error{c, std::move(msg), std::nullopt};
  }
  static Error make(Code c, std::string msg, std::string ctx) {
    return Error{c, std::move(msg), std::optional<std::string>{std::move(ctx)}};
  }
};

/// Serialized snake_case name for an Error::Code. Stable across releases.
/// The inverse mapping for protocol decoders lives in
/// `protocol/rust/src/error.rs` and any future C++ JSON binding.
[[nodiscard]] std::string_view to_string(Error::Code code) noexcept;

/// Parse a snake_case error-code name back into an Error::Code.
/// Returns std::nullopt for unknown names so callers can map to
/// Error::Code::Unsupported for unknown protocol versions.
[[nodiscard]] std::optional<Error::Code> error_code_from_string(std::string_view name) noexcept;

/// Human-readable single-line representation of an Error. Intended for logs;
/// not a stable wire format.
[[nodiscard]] std::string format(const Error& err);

}  // namespace tensorplate

namespace tp = tensorplate;  // NOLINT(misc-unused-alias-decls): public alias.
