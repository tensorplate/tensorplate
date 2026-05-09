// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F03: Public normalized inference-request value object.
//
// One request carries a vector of named input tensors plus metadata, so a
// single-input vision request is the n = 1 case and SmolVLA-class
// multi-input requests (image_front, image_wrist, state, instruction)
// share the same type. No vendor SDK headers are pulled in.
//
// Deadline handling:
//   - In-process: deadline is std::chrono::steady_clock::time_point
//     (monotonic) so InferRequest::is_expired() does not depend on wall-
//     clock changes.
//   - On the wire: the JSON Schema records a relative deadline in
//     milliseconds (deadline_ms). HTTP/IPC adapters in V01-E07 convert
//     between the two by sampling the receiver's steady clock.

#pragma once

#include <chrono>
#include <cstdint>
#include <optional>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// One named input binding: a stable name, the buffer holding the payload,
/// and the tensor metadata describing its shape/dtype/layout.
struct NamedInput {
  std::string name;
  BufferRef buffer;
  TensorView tensor;

  friend bool operator==(const NamedInput& lhs, const NamedInput& rhs) noexcept {
    return lhs.name == rhs.name && lhs.buffer == rhs.buffer && lhs.tensor == rhs.tensor;
  }
  friend bool operator!=(const NamedInput& lhs, const NamedInput& rhs) noexcept {
    return !(lhs == rhs);
  }
};

/// Request-scoped metadata preserved across the scheduler and session
/// boundary. The explicit fields support LeRobot PolicyServer-compatible
/// async inference (correlation, action-chunk identity/sequence, and
/// stale-request cancellation); `extra` carries caller-defined free-form
/// strings without leaking deployment-specific behavior into the runtime.
struct RequestMetadata {
  /// Correlation id propagated from request ingress through result. Used
  /// by metrics and structured logs (V01-E12).
  std::optional<std::string> correlation_id;

  /// LeRobot-style asynchronous-action-chunk identity. Distinct from
  /// request_id: a single chunk identity may be served by multiple
  /// inference requests if the policy server retries.
  std::optional<std::string> action_chunk_id;

  /// Sequence number of an asynchronous action chunk within an episode.
  std::optional<std::int64_t> action_chunk_sequence;

  /// LeRobot stale-request marker: if set, requests with an
  /// `action_chunk_sequence` <= this value are considered stale and
  /// must be cancelled or expired.
  std::optional<std::int64_t> stale_after_sequence;

  /// Free-form caller metadata. v0.1.0 transport: only string values
  /// to keep the JSON schema simple.
  std::unordered_map<std::string, std::string> extra;

  friend bool operator==(const RequestMetadata& lhs, const RequestMetadata& rhs) noexcept {
    return lhs.correlation_id == rhs.correlation_id && lhs.action_chunk_id == rhs.action_chunk_id &&
           lhs.action_chunk_sequence == rhs.action_chunk_sequence &&
           lhs.stale_after_sequence == rhs.stale_after_sequence && lhs.extra == rhs.extra;
  }
  friend bool operator!=(const RequestMetadata& lhs, const RequestMetadata& rhs) noexcept {
    return !(lhs == rhs);
  }
};

/// Normalized inference-request value object. Constructible without
/// runtime-adapter dependencies and copyable so tests can build fixtures
/// without a buffer-pool plumbed in.
class InferRequest {
 public:
  using Clock = std::chrono::steady_clock;
  using TimePoint = Clock::time_point;
  using Duration = std::chrono::milliseconds;

  /// Validating factory.
  ///
  /// Returns Error::Code::ConfigInvalid if any of:
  ///   - `request_id` is empty
  ///   - `endpoint` is empty
  ///   - `inputs` is empty
  ///   - any input has an empty name
  ///   - duplicate input names are present
  static Result<InferRequest> create(std::string request_id, std::string endpoint,
                                     std::vector<NamedInput> inputs, RequestMetadata metadata = {},
                                     std::optional<TimePoint> deadline = std::nullopt);

  /// Convenience factory that converts a relative deadline (milliseconds)
  /// into a monotonic absolute deadline at the moment of construction.
  /// Used by HTTP / IPC adapters that receive a wire-format relative
  /// deadline in JSON.
  static Result<InferRequest> create_with_relative_deadline(
      std::string request_id, std::string endpoint, std::vector<NamedInput> inputs,
      RequestMetadata metadata, std::optional<Duration> relative_deadline);

  [[nodiscard]] const std::string& request_id() const noexcept { return request_id_; }
  [[nodiscard]] const std::string& endpoint() const noexcept { return endpoint_; }
  [[nodiscard]] const std::vector<NamedInput>& inputs() const noexcept { return inputs_; }
  [[nodiscard]] const RequestMetadata& metadata() const noexcept { return metadata_; }
  [[nodiscard]] const std::optional<TimePoint>& deadline() const noexcept { return deadline_; }

  /// True if a deadline is set and has already passed against the
  /// monotonic steady clock.
  [[nodiscard]] bool is_expired() const noexcept;

  /// Time remaining until the deadline. std::nullopt if no deadline is
  /// set; negative values are clamped to zero so admission policies can
  /// treat "expired" and "0 ms remaining" identically.
  [[nodiscard]] std::optional<Duration> time_until_deadline() const noexcept;

  friend bool operator==(const InferRequest& lhs, const InferRequest& rhs) noexcept {
    return lhs.request_id_ == rhs.request_id_ && lhs.endpoint_ == rhs.endpoint_ &&
           lhs.inputs_ == rhs.inputs_ && lhs.metadata_ == rhs.metadata_ &&
           lhs.deadline_ == rhs.deadline_;
  }
  friend bool operator!=(const InferRequest& lhs, const InferRequest& rhs) noexcept {
    return !(lhs == rhs);
  }

 private:
  InferRequest() = default;

  std::string request_id_;
  std::string endpoint_;
  std::vector<NamedInput> inputs_;
  RequestMetadata metadata_;
  std::optional<TimePoint> deadline_;
};

}  // namespace tensorplate
