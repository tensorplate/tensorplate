// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F01-T01: Scheduler-facing request envelope.
//
// SchedulerRequest wraps a normalized InferRequest with the policy
// inputs the scheduler needs to make admission, dispatch, and expiry
// decisions:
//
//   - estimated service time (per-backend / per-profile or capability
//     -derived default),
//   - estimated peak memory (used by capacity / pressure admission),
//   - the backend identity that will execute the request (so events
//     can be labeled without re-resolving the model),
//   - an optional priority field preserved through the v0.1.0 FIFO
//     scheduler so a future priority policy does not need an interface
//     change.
//
// The envelope is move-only with a copy escape hatch (clone) because
// the underlying InferRequest can carry owned BufferRef handles; the
// scheduler treats the envelope as the single point of truth between
// admission and dispatch.

#pragma once

#include <chrono>
#include <cstdint>
#include <optional>
#include <string>
#include <utility>

#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/scheduler/clock.hpp"

namespace tensorplate {

/// Policy-input estimate attached to a SchedulerRequest at admission.
/// All fields are optional so callers that have no telemetry data can
/// rely on configured defaults; deadline-feasibility uses the
/// configured `default_service_estimate` from SchedulerConfig when
/// `estimated_service_time` is not set.
struct ServiceEstimate {
  /// Expected wall-clock duration of one infer call against the chosen
  /// backend. Monotonic duration semantics.
  std::optional<SchedulerClock::Duration> estimated_service_time;

  /// Expected peak memory footprint in bytes. v0.1.0 admission policy
  /// only consults this when memory pressure is active; the field is
  /// preserved for future capacity-aware policy.
  std::optional<std::uint64_t> estimated_peak_memory_bytes;
};

/// Scheduler-facing request envelope. Carries the normalized
/// InferRequest plus admission/dispatch policy inputs. Move-only;
/// callers must explicitly clone() when they need to retain a copy
/// (e.g. for cancellation lookup tables).
class SchedulerRequest {
 public:
  /// Optional priority field. Preserved across queues for future
  /// priority policies (v0.1.0 FIFO ignores priority). Higher numbers
  /// indicate higher priority; default 0.
  using Priority = std::int32_t;

  SchedulerRequest(InferRequest request, std::string backend_name, std::string model_id,
                   ServiceEstimate estimate, SchedulerClock::TimePoint enqueue_time,
                   Priority priority = 0) noexcept
      : request_(std::move(request)),
        backend_name_(std::move(backend_name)),
        model_id_(std::move(model_id)),
        estimate_(estimate),
        enqueue_time_(enqueue_time),
        priority_(priority) {}

  SchedulerRequest(const SchedulerRequest&) = delete;
  SchedulerRequest& operator=(const SchedulerRequest&) = delete;
  SchedulerRequest(SchedulerRequest&&) noexcept = default;
  SchedulerRequest& operator=(SchedulerRequest&&) noexcept = default;
  ~SchedulerRequest() = default;

  [[nodiscard]] const InferRequest& request() const noexcept { return request_; }
  [[nodiscard]] InferRequest& mutable_request() noexcept { return request_; }
  [[nodiscard]] const std::string& backend_name() const noexcept { return backend_name_; }
  [[nodiscard]] const std::string& model_id() const noexcept { return model_id_; }
  [[nodiscard]] const ServiceEstimate& estimate() const noexcept { return estimate_; }
  [[nodiscard]] SchedulerClock::TimePoint enqueue_time() const noexcept { return enqueue_time_; }
  [[nodiscard]] Priority priority() const noexcept { return priority_; }

  /// Convenience accessor for the underlying request id.
  [[nodiscard]] const std::string& request_id() const noexcept { return request_.request_id(); }

  /// Convenience accessor for the underlying endpoint.
  [[nodiscard]] const std::string& endpoint() const noexcept { return request_.endpoint(); }

 private:
  InferRequest request_;
  std::string backend_name_;
  std::string model_id_;
  ServiceEstimate estimate_;
  SchedulerClock::TimePoint enqueue_time_;
  Priority priority_ = 0;
};

}  // namespace tensorplate
