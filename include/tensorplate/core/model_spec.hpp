// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F02: Public model identity and backend-selection value object.
//
// ModelSpec is consumed by deployment, loading, and execution-session setup.
// It carries no vendor SDK types and is constructible without hardware
// access. Mirrors `protocol/schemas/model_spec.json` and
// `protocol/rust/src/model_spec.rs`.

#pragma once

#include "tensorplate/core/result.hpp"

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

namespace tensorplate {

/// Model class taxonomy. v0.1.0 validates only Vision and Vla, but the
/// schema covers the full v0.1.0+ taxonomy so bundle parsing is forward
/// compatible without a schema bump.
enum class ModelClass : std::uint8_t {
  Vision = 0,
  Speech = 1,
  Language = 2,
  Vla = 3,
  Embedding = 4,
  Custom = 5,
};

/// Numeric precision profile hint. The selected backend honors the hint or
/// returns Error::Code::Unsupported; v0.1.0 backends do not silently
/// downgrade.
enum class PrecisionHint : std::uint8_t {
  /// Backend chooses a default profile.
  Auto = 0,
  Fp32 = 1,
  Fp16 = 2,
  BFloat16 = 3,
  Int8 = 4,
  Int4 = 5,
};

/// Stable wire name for a ModelClass. Snake_case, matches the JSON Schema.
[[nodiscard]] std::string_view to_string(ModelClass cls) noexcept;
[[nodiscard]] std::optional<ModelClass> model_class_from_string(std::string_view name) noexcept;

/// Stable wire name for a PrecisionHint. Snake_case, matches the JSON Schema.
[[nodiscard]] std::string_view to_string(PrecisionHint hint) noexcept;
[[nodiscard]] std::optional<PrecisionHint> precision_hint_from_string(
    std::string_view name) noexcept;

/// Model identity, class, and backend-selection value object.
///
/// Immutable after construction. Use `ModelSpec::create()` for validation;
/// the public constructor is private and the type cannot be default
/// constructed.
///
/// Backend hints are free-form strings (e.g., "tensorrt", "libtorch",
/// "python_pytorch") rather than an enum so future adapters can plug in
/// without revising the public header. The runtime resolves the string to
/// a backend factory at deploy time and rejects unknown hints with
/// Error::Code::Unsupported (V01-E05).
class ModelSpec {
 public:
  /// Validating constructor.
  ///
  /// Returns Error::Code::ConfigInvalid if any of:
  ///   - `model_id` is empty
  ///   - `artifact_path` is empty
  ///   - `backend_hint` is empty
  ///   - `profile_id` is set but empty
  static Result<ModelSpec> create(std::string model_id, ModelClass model_class,
                                  std::string artifact_path, std::string backend_hint,
                                  PrecisionHint precision_hint = PrecisionHint::Auto,
                                  std::optional<std::string> profile_id = std::nullopt);

  [[nodiscard]] const std::string& model_id() const noexcept { return model_id_; }
  [[nodiscard]] ModelClass model_class() const noexcept { return model_class_; }
  [[nodiscard]] const std::string& artifact_path() const noexcept { return artifact_path_; }
  [[nodiscard]] const std::string& backend_hint() const noexcept { return backend_hint_; }
  [[nodiscard]] PrecisionHint precision_hint() const noexcept { return precision_hint_; }
  [[nodiscard]] const std::optional<std::string>& profile_id() const noexcept {
    return profile_id_;
  }

  friend bool operator==(const ModelSpec& lhs, const ModelSpec& rhs) noexcept {
    return lhs.model_id_ == rhs.model_id_ && lhs.model_class_ == rhs.model_class_ &&
           lhs.artifact_path_ == rhs.artifact_path_ && lhs.backend_hint_ == rhs.backend_hint_ &&
           lhs.precision_hint_ == rhs.precision_hint_ && lhs.profile_id_ == rhs.profile_id_;
  }
  friend bool operator!=(const ModelSpec& lhs, const ModelSpec& rhs) noexcept {
    return !(lhs == rhs);
  }

 private:
  ModelSpec() = default;

  std::string model_id_;
  ModelClass model_class_ = ModelClass::Custom;
  std::string artifact_path_;
  std::string backend_hint_;
  PrecisionHint precision_hint_ = PrecisionHint::Auto;
  std::optional<std::string> profile_id_;
};

}  // namespace tensorplate
