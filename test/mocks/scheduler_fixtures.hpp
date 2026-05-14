// SPDX-License-Identifier: Apache-2.0
//
// V01-E06: Shared scheduler-test fixtures. Helpers to build small
// InferRequests and SchedulerRequests without rebuilding the
// boilerplate (NamedInput + BufferRef + TensorView) in every test.
//
// These helpers do **not** allocate through BufferManager by default;
// tests that need real buffer-plane release behavior should call
// `make_request_with_buffer(*manager, ...)`. The default helpers build
// a single Borrowed BufferRef so envelope validation passes without
// pulling a manager into every test.

#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/scheduler/scheduler.hpp"
#include "tensorplate/scheduler/scheduler_request.hpp"

namespace tensorplate::testing {

/// Build a minimal NamedInput with a Borrowed BufferRef. Useful when
/// the test does not care about buffer-pool release semantics.
inline NamedInput make_named_input(std::string name, std::uint64_t buffer_id = 1,
                                   std::size_t byte_size = 64) {
  auto buf = BufferRef::create(buffer_id, byte_size, BufferOwnership::Borrowed).value();
  auto view = TensorView::create(DType::Float32,
                                 {1, static_cast<std::int64_t>(byte_size / 4)}, Layout::RowMajor,
                                 0, byte_size)
                  .value();
  return NamedInput{std::move(name), buf, view};
}

/// Build a minimal InferRequest with one input. Optionally sets a
/// monotonic deadline relative to a base clock.
inline InferRequest make_infer_request(
    std::string request_id, std::string endpoint = "test/endpoint",
    std::optional<InferRequest::TimePoint> deadline = std::nullopt, RequestMetadata metadata = {}) {
  std::vector<NamedInput> inputs;
  inputs.push_back(make_named_input("input"));
  auto req = InferRequest::create(std::move(request_id), std::move(endpoint), std::move(inputs),
                                  std::move(metadata), deadline);
  return std::move(req).value();
}

/// Build a SchedulerRequest from a base InferRequest. The clock's
/// `now()` is used as enqueue_time; tests that want a specific
/// enqueue_time can construct SchedulerRequest directly.
inline SchedulerRequest make_scheduler_request(
    InferRequest request, const SchedulerClock& clock, std::string backend_name = "mock",
    std::string model_id = "model", ServiceEstimate estimate = {}) {
  return SchedulerRequest{std::move(request), std::move(backend_name), std::move(model_id),
                          estimate, clock.now()};
}

/// Build a scheduler request envelope that allocates owned input
/// buffers from `manager`. Caller is responsible for ensuring the
/// scheduler releases them through cancel/expire/shutdown paths (or
/// the test releases them after dispatch).
inline SchedulerRequest make_request_with_owned_buffer(
    BufferManager& manager, const SchedulerClock& clock, std::string request_id,
    std::size_t byte_size = 64,
    std::optional<InferRequest::TimePoint> deadline = std::nullopt) {
  auto buf_r = manager.allocate(byte_size);
  if (!buf_r) {
    throw std::runtime_error("test fixture: manager.allocate failed: " + buf_r.error().message);
  }
  auto buf = std::move(buf_r).value();
  auto view_r = TensorView::create(DType::Float32,
                                   {1, static_cast<std::int64_t>(byte_size / 4)},
                                   Layout::RowMajor, 0, byte_size);
  if (!view_r) {
    throw std::runtime_error("test fixture: TensorView::create failed: " +
                             view_r.error().message);
  }
  auto view = std::move(view_r).value();
  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{"input", buf, view});
  auto req_r = InferRequest::create(std::move(request_id), "test/endpoint", std::move(inputs),
                                    {}, deadline);
  if (!req_r) {
    throw std::runtime_error("test fixture: InferRequest::create failed: " +
                             req_r.error().message);
  }
  return SchedulerRequest{std::move(req_r).value(), "mock", "model", {}, clock.now()};
}

/// Recording event sink used across scheduler tests.
class RecordingSchedulerEventSink final : public SchedulerEventSink {
 public:
  void on_event(const SchedulerEvent& event) override {
    std::lock_guard<std::mutex> guard(mu_);
    events_.push_back(event);
  }
  [[nodiscard]] std::vector<SchedulerEvent> events() const {
    std::lock_guard<std::mutex> guard(mu_);
    return events_;
  }
  [[nodiscard]] std::size_t size() const {
    std::lock_guard<std::mutex> guard(mu_);
    return events_.size();
  }
  /// Returns how many events of `kind` have been recorded.
  [[nodiscard]] std::size_t count(SchedulerEventKind kind) const {
    std::lock_guard<std::mutex> guard(mu_);
    std::size_t n = 0;
    for (const auto& e : events_) {
      if (e.kind == kind) {
        ++n;
      }
    }
    return n;
  }
  void clear() {
    std::lock_guard<std::mutex> guard(mu_);
    events_.clear();
  }

 private:
  mutable std::mutex mu_;
  std::vector<SchedulerEvent> events_;
};

}  // namespace tensorplate::testing
