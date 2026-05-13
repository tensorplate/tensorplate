// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01-T01: Public backend capability value object.
//
// `BackendCapability` is the vendor-neutral capability record published
// by every registered execution-backend adapter. The bundle validator,
// status reporter, and adapter conformance harness consume it; no
// caller above the registry has to branch on backend-type strings.
//
// The record has no vendor SDK includes and no raw handles. Generation,
// streaming, and KV-cache capability flags default to `false` for v0.1.0
// adapters and only flip to `true` when an adapter genuinely implements
// the corresponding lifecycle. Fixed-shape and op-coverage information
// is intentionally generic enough to represent TensorRT, LibTorch,
// Python/PyTorch, and a future Vitis AI adapter without revising this
// header.

#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_set>
#include <vector>

#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Coarse classification of how strictly an adapter constrains input
/// tensor shapes. Adapters published this value alongside the precision
/// list so bundle validation can fail fast before session creation.
enum class ShapeSupport : std::uint8_t {
  /// Adapter accepts any rank/shape compatible with the model artifact.
  Dynamic = 0,
  /// Adapter requires inputs to match the binding shapes recorded at
  /// build/load time. TensorRT engines built without optimization
  /// profiles fall here.
  Fixed = 1,
  /// Adapter accepts a bounded range of shapes per dimension.
  RangeBounded = 2,
};

/// Stable snake_case wire name for a ShapeSupport value. Used by the
/// capability JSON Schema.
[[nodiscard]] std::string_view to_string(ShapeSupport support) noexcept;
[[nodiscard]] std::optional<ShapeSupport> shape_support_from_string(std::string_view name) noexcept;

/// Normalized backend capability record. Constructible without a hardware
/// or backend SDK and copyable. Adapters publish one instance per
/// registry entry.
class BackendCapability {
 public:
  /// Validating factory.
  ///
  /// Returns Error::Code::ConfigInvalid if any of:
  ///   - `backend_name` is empty,
  ///   - `supported_precision` is empty,
  ///   - `profile_id` is set but empty,
  ///   - `op_coverage_score_pct` is set and outside [0, 100],
  ///   - `memory_estimate_bytes` is greater than `memory_limit_bytes`
  ///     when both are set.
  static Result<BackendCapability> create(
      std::string backend_name, std::vector<PrecisionHint> supported_precision,
      ShapeSupport shape_support = ShapeSupport::Dynamic,
      std::optional<std::string> profile_id = std::nullopt, bool supports_async = false,
      bool supports_generation = false, bool supports_streaming = false,
      bool supports_kv_cache = false,
      std::optional<std::uint8_t> op_coverage_score_pct = std::nullopt,
      std::optional<std::uint64_t> memory_estimate_bytes = std::nullopt,
      std::optional<std::uint64_t> memory_limit_bytes = std::nullopt,
      std::vector<std::string> target_compatibility_notes = {});

  [[nodiscard]] const std::string& backend_name() const noexcept { return backend_name_; }
  [[nodiscard]] const std::optional<std::string>& profile_id() const noexcept {
    return profile_id_;
  }
  [[nodiscard]] const std::vector<PrecisionHint>& supported_precision() const noexcept {
    return supported_precision_;
  }
  [[nodiscard]] ShapeSupport shape_support() const noexcept { return shape_support_; }
  [[nodiscard]] bool supports_async() const noexcept { return supports_async_; }
  [[nodiscard]] bool supports_generation() const noexcept { return supports_generation_; }
  [[nodiscard]] bool supports_streaming() const noexcept { return supports_streaming_; }
  [[nodiscard]] bool supports_kv_cache() const noexcept { return supports_kv_cache_; }
  [[nodiscard]] const std::optional<std::uint8_t>& op_coverage_score_pct() const noexcept {
    return op_coverage_score_pct_;
  }
  [[nodiscard]] const std::optional<std::uint64_t>& memory_estimate_bytes() const noexcept {
    return memory_estimate_bytes_;
  }
  [[nodiscard]] const std::optional<std::uint64_t>& memory_limit_bytes() const noexcept {
    return memory_limit_bytes_;
  }
  [[nodiscard]] const std::vector<std::string>& target_compatibility_notes() const noexcept {
    return target_compatibility_notes_;
  }

  /// True iff `hint` appears in `supported_precision()`. `PrecisionHint::Auto`
  /// is always treated as supported because it tells the adapter to pick
  /// its own default.
  [[nodiscard]] bool accepts_precision(PrecisionHint hint) const noexcept;

  friend bool operator==(const BackendCapability& lhs, const BackendCapability& rhs) noexcept;
  friend bool operator!=(const BackendCapability& lhs, const BackendCapability& rhs) noexcept {
    return !(lhs == rhs);
  }

 private:
  BackendCapability() = default;

  std::string backend_name_;
  std::optional<std::string> profile_id_;
  std::vector<PrecisionHint> supported_precision_;
  ShapeSupport shape_support_ = ShapeSupport::Dynamic;
  bool supports_async_ = false;
  bool supports_generation_ = false;
  bool supports_streaming_ = false;
  bool supports_kv_cache_ = false;
  std::optional<std::uint8_t> op_coverage_score_pct_;
  std::optional<std::uint64_t> memory_estimate_bytes_;
  std::optional<std::uint64_t> memory_limit_bytes_;
  std::vector<std::string> target_compatibility_notes_;
};

}  // namespace tensorplate
