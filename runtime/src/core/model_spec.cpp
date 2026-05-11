// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F02-T01: ModelSpec validation, enum string mappings, and the
// inverse parsers used by JSON binding code.

#include "tensorplate/core/model_spec.hpp"

#include <array>
#include <string>
#include <string_view>
#include <utility>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

namespace {

constexpr std::array<std::pair<ModelClass, std::string_view>, 6> kClassNames = {{
    {ModelClass::Vision, "vision"},
    {ModelClass::Speech, "speech"},
    {ModelClass::Language, "language"},
    {ModelClass::Vla, "vla"},
    {ModelClass::Embedding, "embedding"},
    {ModelClass::Custom, "custom"},
}};

constexpr std::array<std::pair<PrecisionHint, std::string_view>, 6> kPrecisionNames = {{
    {PrecisionHint::Auto, "auto"},
    {PrecisionHint::Fp32, "fp32"},
    {PrecisionHint::Fp16, "fp16"},
    {PrecisionHint::BFloat16, "bfloat16"},
    {PrecisionHint::Int8, "int8"},
    {PrecisionHint::Int4, "int4"},
}};

}  // namespace

std::string_view to_string(ModelClass cls) noexcept {
  for (const auto& [c, name] : kClassNames) {
    if (c == cls) {
      return name;
    }
  }
  return "custom";
}

std::optional<ModelClass> model_class_from_string(std::string_view name) noexcept {
  for (const auto& [c, candidate] : kClassNames) {
    if (candidate == name) {
      return c;
    }
  }
  return std::nullopt;
}

std::string_view to_string(PrecisionHint hint) noexcept {
  for (const auto& [p, name] : kPrecisionNames) {
    if (p == hint) {
      return name;
    }
  }
  return "auto";
}

std::optional<PrecisionHint> precision_hint_from_string(std::string_view name) noexcept {
  for (const auto& [p, candidate] : kPrecisionNames) {
    if (candidate == name) {
      return p;
    }
  }
  return std::nullopt;
}

Result<ModelSpec> ModelSpec::create(std::string model_id, ModelClass model_class,
                                    std::string artifact_path, std::string backend_hint,
                                    PrecisionHint precision_hint,
                                    std::optional<std::string> profile_id) {
  if (model_id.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "ModelSpec.model_id must be non-empty");
  }
  if (artifact_path.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "ModelSpec.artifact_path must be non-empty");
  }
  if (backend_hint.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "ModelSpec.backend_hint must be non-empty");
  }
  if (profile_id.has_value() && profile_id->empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "ModelSpec.profile_id, if present, must be non-empty");
  }

  ModelSpec spec;
  spec.model_id_ = std::move(model_id);
  spec.model_class_ = model_class;
  spec.artifact_path_ = std::move(artifact_path);
  spec.backend_hint_ = std::move(backend_hint);
  spec.precision_hint_ = precision_hint;
  spec.profile_id_ = std::move(profile_id);
  return spec;
}

}  // namespace tensorplate
