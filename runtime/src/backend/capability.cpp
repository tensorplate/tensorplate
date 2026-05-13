// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01-T01: BackendCapability implementation.

#include "tensorplate/backend/capability.hpp"

#include <algorithm>
#include <string>
#include <utility>

#include "tensorplate/core/error.hpp"

namespace tensorplate {

std::string_view to_string(ShapeSupport support) noexcept {
  switch (support) {
    case ShapeSupport::Dynamic:
      return "dynamic";
    case ShapeSupport::Fixed:
      return "fixed";
    case ShapeSupport::RangeBounded:
      return "range_bounded";
  }
  return "dynamic";
}

std::optional<ShapeSupport> shape_support_from_string(std::string_view name) noexcept {
  if (name == "dynamic") {
    return ShapeSupport::Dynamic;
  }
  if (name == "fixed") {
    return ShapeSupport::Fixed;
  }
  if (name == "range_bounded") {
    return ShapeSupport::RangeBounded;
  }
  return std::nullopt;
}

Result<BackendCapability> BackendCapability::create(
    std::string backend_name, std::vector<PrecisionHint> supported_precision,
    ShapeSupport shape_support, std::optional<std::string> profile_id, bool supports_async,
    bool supports_generation, bool supports_streaming, bool supports_kv_cache,
    std::optional<std::uint8_t> op_coverage_score_pct,
    std::optional<std::uint64_t> memory_estimate_bytes,
    std::optional<std::uint64_t> memory_limit_bytes,
    std::vector<std::string> target_compatibility_notes) {
  if (backend_name.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "backend_name must not be empty");
  }
  if (supported_precision.empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "supported_precision must list at least one PrecisionHint");
  }
  if (profile_id.has_value() && profile_id->empty()) {
    return unexpected(Error::Code::ConfigInvalid, "profile_id must not be empty when set");
  }
  if (op_coverage_score_pct.has_value() && *op_coverage_score_pct > 100U) {
    return unexpected(Error::Code::ConfigInvalid,
                      "op_coverage_score_pct must be in the range [0, 100]");
  }
  if (memory_estimate_bytes.has_value() && memory_limit_bytes.has_value() &&
      *memory_estimate_bytes > *memory_limit_bytes) {
    return unexpected(Error::Code::ConfigInvalid,
                      "memory_estimate_bytes must not exceed memory_limit_bytes");
  }

  BackendCapability cap;
  cap.backend_name_ = std::move(backend_name);
  cap.profile_id_ = std::move(profile_id);
  cap.supported_precision_ = std::move(supported_precision);
  cap.shape_support_ = shape_support;
  cap.supports_async_ = supports_async;
  cap.supports_generation_ = supports_generation;
  cap.supports_streaming_ = supports_streaming;
  cap.supports_kv_cache_ = supports_kv_cache;
  cap.op_coverage_score_pct_ = op_coverage_score_pct;
  cap.memory_estimate_bytes_ = memory_estimate_bytes;
  cap.memory_limit_bytes_ = memory_limit_bytes;
  cap.target_compatibility_notes_ = std::move(target_compatibility_notes);
  return cap;
}

bool BackendCapability::accepts_precision(PrecisionHint hint) const noexcept {
  if (hint == PrecisionHint::Auto) {
    return true;
  }
  return std::any_of(supported_precision_.begin(), supported_precision_.end(),
                     [hint](PrecisionHint h) noexcept { return h == hint; });
}

bool operator==(const BackendCapability& lhs, const BackendCapability& rhs) noexcept {
  return lhs.backend_name_ == rhs.backend_name_ && lhs.profile_id_ == rhs.profile_id_ &&
         lhs.supported_precision_ == rhs.supported_precision_ &&
         lhs.shape_support_ == rhs.shape_support_ && lhs.supports_async_ == rhs.supports_async_ &&
         lhs.supports_generation_ == rhs.supports_generation_ &&
         lhs.supports_streaming_ == rhs.supports_streaming_ &&
         lhs.supports_kv_cache_ == rhs.supports_kv_cache_ &&
         lhs.op_coverage_score_pct_ == rhs.op_coverage_score_pct_ &&
         lhs.memory_estimate_bytes_ == rhs.memory_estimate_bytes_ &&
         lhs.memory_limit_bytes_ == rhs.memory_limit_bytes_ &&
         lhs.target_compatibility_notes_ == rhs.target_compatibility_notes_;
}

}  // namespace tensorplate
