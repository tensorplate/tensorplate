// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F04: Public normalized inference-result value object.
//
// Mirrors the request shape: one result carries request_id, a vector of
// named output tensors, timing breakdown, and either success outputs or
// a typed Error. Chunk-shaped VLA action output is one pattern of
// `outputs` (e.g. `[chunk_size, action_dim]` float32) and does not need
// a VLA-specific result type.
//
// Latency stamping (queue_latency, execution_latency, total_latency) is
// populated by the V01-E04 ExecutionSession NVI wrapper. The fields are
// optional in v0.1.0 so partial timing info during failure paths is
// representable.

#pragma once

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

#include <chrono>
#include <optional>
#include <string>
#include <utility>
#include <variant>
#include <vector>

namespace tensorplate {

/// One named output binding: stable name, payload buffer, tensor metadata,
/// and an optional semantic tag (e.g. `"action_chunk"`, `"logits"`,
/// `"detections"`) used by adapters that publish multi-purpose outputs.
struct NamedOutput {
  std::string name;
  BufferRef buffer;
  TensorView tensor;
  std::optional<std::string> semantic_tag;

  friend bool operator==(const NamedOutput& lhs, const NamedOutput& rhs) noexcept {
    return lhs.name == rhs.name && lhs.buffer == rhs.buffer && lhs.tensor == rhs.tensor &&
           lhs.semantic_tag == rhs.semantic_tag;
  }
  friend bool operator!=(const NamedOutput& lhs, const NamedOutput& rhs) noexcept {
    return !(lhs == rhs);
  }
};

/// Timing breakdown for one inference call. Populated by the
/// ExecutionSession NVI wrapper (V01-E04). All fields are optional so a
/// failed inference can still carry partial timing.
struct InferenceTiming {
  using Duration = std::chrono::nanoseconds;

  /// Time spent in the scheduler queue before dispatch.
  std::optional<Duration> queue_latency;
  /// Time spent inside the backend's infer() call.
  std::optional<Duration> execution_latency;
  /// End-to-end latency from request ingress to result publish.
  std::optional<Duration> total_latency;

  friend bool operator==(const InferenceTiming& lhs, const InferenceTiming& rhs) noexcept {
    return lhs.queue_latency == rhs.queue_latency &&
           lhs.execution_latency == rhs.execution_latency &&
           lhs.total_latency == rhs.total_latency;
  }
  friend bool operator!=(const InferenceTiming& lhs, const InferenceTiming& rhs) noexcept {
    return !(lhs == rhs);
  }
};

/// Normalized inference-result value object. Distinct from
/// tensorplate::Result<T>; this is the request/response payload, not the
/// fallible-operation alias.
class InferResult {
 public:
  using Outputs = std::vector<NamedOutput>;

  /// Build a successful result. Validates output naming the same way
  /// InferRequest validates inputs (non-empty names, no duplicates).
  static Result<InferResult> create_success(std::string request_id, Outputs outputs,
                                            InferenceTiming timing = {});

  /// Build a failed result. Failure construction never fails: a result
  /// with no request_id is allowed for ingress-time errors that occur
  /// before request_id is parsed.
  static InferResult create_failure(std::string request_id, Error error,
                                    InferenceTiming timing = {});

  [[nodiscard]] const std::string& request_id() const noexcept { return request_id_; }
  [[nodiscard]] bool is_success() const noexcept {
    return std::holds_alternative<Outputs>(payload_);
  }
  [[nodiscard]] bool is_failure() const noexcept { return !is_success(); }

  /// Outputs accessor. Returns an empty vector if this is a failure
  /// result; callers must check is_success() first if they need to
  /// disambiguate "no outputs" from "failure".
  [[nodiscard]] const Outputs& outputs() const noexcept;

  /// Error accessor. Returns a placeholder Error{Internal,""} if this
  /// is a success result; callers must check is_failure() first.
  [[nodiscard]] const Error& error() const noexcept;

  [[nodiscard]] const InferenceTiming& timing() const noexcept { return timing_; }

  friend bool operator==(const InferResult& lhs, const InferResult& rhs) noexcept {
    return lhs.request_id_ == rhs.request_id_ && lhs.payload_ == rhs.payload_ &&
           lhs.timing_ == rhs.timing_;
  }
  friend bool operator!=(const InferResult& lhs, const InferResult& rhs) noexcept {
    return !(lhs == rhs);
  }

 private:
  InferResult() = default;

  std::string request_id_;
  std::variant<Outputs, Error> payload_{Outputs{}};
  InferenceTiming timing_;
};

}  // namespace tensorplate
