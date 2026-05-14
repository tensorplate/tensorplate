// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F02..F06: FifoScheduler implementation.

#include "scheduler/fifo_scheduler.hpp"

#include <algorithm>
#include <chrono>
#include <exception>
#include <string>
#include <utility>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"

namespace tensorplate {

namespace {

constexpr std::chrono::nanoseconds kZeroNs{0};

SchedulerClock::Duration as_nanos(std::chrono::milliseconds ms) {
  return std::chrono::duration_cast<SchedulerClock::Duration>(ms);
}

}  // namespace

Result<std::unique_ptr<InferScheduler>> FifoScheduler::create(const SchedulerConfig& config,
                                                              SchedulerRuntimeHooks hooks) {
  // Defensive validation mirroring the common factory validator.
  if (config.policy != "fifo") {
    return unexpected(Error::Code::ConfigInvalid,
                      std::string{"FifoScheduler requires policy=fifo, got "} + config.policy);
  }
  if (config.queue_capacity == 0) {
    return unexpected(Error::Code::ConfigInvalid, "FifoScheduler queue_capacity must be > 0");
  }
  if (config.in_flight_capacity == 0) {
    return unexpected(Error::Code::ConfigInvalid, "FifoScheduler in_flight_capacity must be > 0");
  }
  if (config.deadline_margin.count() < 0) {
    return unexpected(Error::Code::ConfigInvalid, "FifoScheduler deadline_margin must be >= 0");
  }
  if (config.default_service_estimate.count() < 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "FifoScheduler default_service_estimate must be >= 0");
  }
  return std::unique_ptr<InferScheduler>(new FifoScheduler(config, hooks));
}

FifoScheduler::FifoScheduler(SchedulerConfig config, SchedulerRuntimeHooks hooks)
    : config_(std::move(config)),
      event_sink_(hooks.event_sink),
      buffer_manager_(hooks.buffer_manager),
      clock_(hooks.clock) {
  metrics_.policy = config_.policy;
}

FifoScheduler::~FifoScheduler() {
  // If the caller forgot to call shutdown() we still release queued
  // buffers so we do not leak BufferRefs at process teardown.
  std::lock_guard<std::mutex> guard(mutex_);
  if (!queue_.empty()) {
    for (auto& req : queue_) {
      release_request_buffers_safely(req);
    }
    queue_.clear();
  }
}

void FifoScheduler::emit_event_locked(SchedulerEvent event) {
  if (event_sink_ == nullptr) {
    return;
  }
  // Defensive: do not let a misbehaving sink corrupt scheduler state.
  // This mirrors the V01-E04 SessionEvent emission pattern.
  try {
    event_sink_->on_event(event);
  } catch (...) {  // NOLINT(bugprone-empty-catch)
    // Fire-and-forget: a throwing sink is the sink author's bug.
  }
}

bool FifoScheduler::is_expired_locked(const SchedulerRequest& req) const {
  const auto& deadline = req.request().deadline();
  if (!deadline.has_value()) {
    return false;
  }
  const auto now_tp = now();
  // Past-deadline requests are always expired regardless of margin
  // because no service estimate can rescue them. (Margin only widens
  // admission; it cannot retroactively un-expire a request.)
  return now_tp >= *deadline;
}

void FifoScheduler::release_request_buffers_safely(SchedulerRequest& req) noexcept {
  if (buffer_manager_ == nullptr) {
    return;
  }
  try {
    (void)release_request_buffers(*buffer_manager_, req.request());
  } catch (...) {  // NOLINT(bugprone-empty-catch)
    // release_request_buffers is documented noexcept, but defending
    // here keeps cleanup paths bullet-proof if a future change relaxes
    // that.
  }
}

void FifoScheduler::record_wait_locked(SchedulerClock::Duration wait) {
  if (wait.count() < 0) {
    wait = kZeroNs;
  }
  metrics_.wait_time_sum += wait;
  ++metrics_.wait_time_samples;
  if (wait > metrics_.wait_time_max) {
    metrics_.wait_time_max = wait;
  }
}

bool FifoScheduler::pressure_rejects_locked() const {
  return active_pressure_severity_locked() >= config_.pressure_reject_threshold &&
         config_.pressure_reject_threshold != PressureSeverity::Normal;
}

PressureSeverity FifoScheduler::active_pressure_severity_locked() const {
  const auto m = metrics_.last_memory_severity;
  const auto t = metrics_.last_thermal_severity;
  return m >= t ? m : t;
}

Result<void> FifoScheduler::admit(SchedulerRequest request) {
  std::lock_guard<std::mutex> guard(mutex_);

  if (shutdown_called_) {
    auto& req = request;
    release_request_buffers_safely(req);
    SchedulerEvent event;
    event.kind = SchedulerEventKind::AdmissionRejected;
    event.request_id = req.request_id();
    event.endpoint = req.endpoint();
    event.backend_name = req.backend_name();
    event.policy = config_.policy;
    event.error_code = Error::Code::NotReady;
    event.timestamp = now();
    emit_event_locked(std::move(event));
    return unexpected(Error::Code::NotReady, "scheduler is shut down");
  }

  // Envelope validation. InferRequest::create already validates, but a
  // future caller may build a SchedulerRequest by other means; the
  // defensive check is cheap.
  if (request.request_id().empty() || request.endpoint().empty()) {
    auto& req = request;
    release_request_buffers_safely(req);
    SchedulerEvent event;
    event.kind = SchedulerEventKind::AdmissionRejected;
    event.request_id = req.request_id();
    event.endpoint = req.endpoint();
    event.backend_name = req.backend_name();
    event.policy = config_.policy;
    event.error_code = Error::Code::ConfigInvalid;
    event.timestamp = now();
    emit_event_locked(std::move(event));
    return unexpected(Error::Code::ConfigInvalid,
                      "scheduler request_id and endpoint must be non-empty");
  }

  // Pressure-aware rejection (F06). The most recent severity per
  // source is consulted; v0.1.0 baseline never silently degrades.
  if (pressure_rejects_locked()) {
    ++metrics_.admission_rejected_pressure;
    SchedulerEvent event;
    event.kind = SchedulerEventKind::AdmissionRejected;
    event.request_id = request.request_id();
    event.endpoint = request.endpoint();
    event.backend_name = request.backend_name();
    event.policy = config_.policy;
    event.error_code = Error::Code::OOMError;
    event.pressure_severity = active_pressure_severity_locked();
    event.timestamp = now();
    release_request_buffers_safely(request);
    emit_event_locked(std::move(event));
    return unexpected(Error::Code::OOMError, "scheduler rejecting admission due to pressure");
  }

  // Capacity check (F02).
  if (queue_.size() >= config_.queue_capacity) {
    ++metrics_.admission_rejected_overload;
    SchedulerEvent event;
    event.kind = SchedulerEventKind::AdmissionRejected;
    event.request_id = request.request_id();
    event.endpoint = request.endpoint();
    event.backend_name = request.backend_name();
    event.policy = config_.policy;
    event.error_code = Error::Code::OOMError;
    event.timestamp = now();
    release_request_buffers_safely(request);
    emit_event_locked(std::move(event));
    return unexpected(Error::Code::OOMError, "scheduler queue is at capacity");
  }

  // Deadline feasibility (F03). Three sub-checks:
  //   1. Request already past its deadline.
  //   2. Estimated completion exceeds deadline + margin.
  //   3. Otherwise admitted.
  const auto& deadline_opt = request.request().deadline();
  if (deadline_opt.has_value()) {
    const auto now_tp = now();
    const auto deadline = *deadline_opt;
    if (now_tp >= deadline) {
      ++metrics_.admission_rejected_deadline;
      SchedulerEvent event;
      event.kind = SchedulerEventKind::AdmissionRejected;
      event.request_id = request.request_id();
      event.endpoint = request.endpoint();
      event.backend_name = request.backend_name();
      event.policy = config_.policy;
      event.error_code = Error::Code::Timeout;
      event.timestamp = now_tp;
      release_request_buffers_safely(request);
      emit_event_locked(std::move(event));
      return unexpected(Error::Code::Timeout, "scheduler rejecting admission: deadline passed");
    }

    // Estimated completion = now + queued wait estimate + in-flight
    // estimate + per-request service estimate. Wait/in-flight
    // estimates fall back to default_service_estimate when no
    // per-request estimate is available, multiplied by current queue
    // depth and in_flight count respectively. This is conservative
    // for v0.1.0; finer-grained policies live in v0.2+.
    const SchedulerClock::Duration default_est = as_nanos(config_.default_service_estimate);
    const SchedulerClock::Duration service_est =
        request.estimate().estimated_service_time.value_or(default_est);
    const SchedulerClock::Duration queued_wait_est = default_est * queue_.size();
    const SchedulerClock::Duration in_flight_est = default_est * in_flight_ids_.size();
    const auto estimated_completion = now_tp + queued_wait_est + in_flight_est + service_est;
    const auto allowed_completion = deadline + as_nanos(config_.deadline_margin);
    if (estimated_completion > allowed_completion) {
      ++metrics_.admission_rejected_deadline;
      SchedulerEvent event;
      event.kind = SchedulerEventKind::AdmissionRejected;
      event.request_id = request.request_id();
      event.endpoint = request.endpoint();
      event.backend_name = request.backend_name();
      event.policy = config_.policy;
      event.error_code = Error::Code::Timeout;
      event.timestamp = now_tp;
      release_request_buffers_safely(request);
      emit_event_locked(std::move(event));
      return unexpected(Error::Code::Timeout,
                        "scheduler rejecting admission: deadline + margin exceeded by estimate");
    }
  }

  // Admit: enqueue, update accounting, and emit Admitted.
  const auto now_tp = now();
  // Re-stamp enqueue_time so wait_time is measured against the same
  // clock the scheduler uses for everything else. Callers that
  // pre-populate enqueue_time may carry monotonic timestamps from
  // upstream, but the scheduler's wait accounting must be measured
  // from admission, not from upstream.
  SchedulerRequest accepted{InferRequest{request.request()},
                            request.backend_name(),
                            request.model_id(),
                            request.estimate(),
                            now_tp,
                            request.priority()};
  queue_.push_back(std::move(accepted));
  ++metrics_.admitted_total;
  metrics_.queue_depth = queue_.size();
  if (queue_.size() > metrics_.queue_depth_high_water) {
    metrics_.queue_depth_high_water = queue_.size();
  }

  SchedulerEvent event;
  event.kind = SchedulerEventKind::Admitted;
  event.request_id = request.request_id();
  event.endpoint = request.endpoint();
  event.backend_name = request.backend_name();
  event.policy = config_.policy;
  event.timestamp = now_tp;
  emit_event_locked(std::move(event));
  return {};
}

std::optional<SchedulerRequest> FifoScheduler::next() {
  std::lock_guard<std::mutex> guard(mutex_);

  // Sweep expired entries first so we never dispatch stale work.
  (void)expire_due_locked();

  if (queue_.empty()) {
    return std::nullopt;
  }
  if (in_flight_ids_.size() >= config_.in_flight_capacity) {
    return std::nullopt;
  }

  SchedulerRequest head = std::move(queue_.front());
  queue_.pop_front();
  metrics_.queue_depth = queue_.size();

  const auto now_tp = now();
  const auto wait = std::chrono::duration_cast<SchedulerClock::Duration>(now_tp - head.enqueue_time());
  record_wait_locked(wait);

  in_flight_ids_.insert(head.request_id());
  if (in_flight_ids_.size() > metrics_.in_flight_high_water) {
    metrics_.in_flight_high_water = in_flight_ids_.size();
  }
  metrics_.in_flight = in_flight_ids_.size();

  SchedulerEvent event;
  event.kind = SchedulerEventKind::Dispatched;
  event.request_id = head.request_id();
  event.endpoint = head.endpoint();
  event.backend_name = head.backend_name();
  event.policy = config_.policy;
  event.wait_time = wait;
  event.timestamp = now_tp;
  emit_event_locked(std::move(event));
  return std::optional<SchedulerRequest>{std::move(head)};
}

Result<void> FifoScheduler::on_completion(std::string_view request_id, CompletionStatus status,
                                          std::optional<Error::Code> error_code) {
  std::lock_guard<std::mutex> guard(mutex_);
  const std::string id{request_id};
  // If this request was cancelled while in-flight, the cancellation
  // already cleared in_flight_ids_. The completion is a typed no-op
  // for racing adapters.
  if (auto cancel_it = cancelled_in_flight_ids_.find(id);
      cancel_it != cancelled_in_flight_ids_.end()) {
    cancelled_in_flight_ids_.erase(cancel_it);
    return unexpected(Error::Code::Internal,
                      "scheduler::on_completion: request was cancelled in-flight");
  }
  auto it = in_flight_ids_.find(id);
  if (it == in_flight_ids_.end()) {
    return unexpected(Error::Code::Internal,
                      "scheduler::on_completion: unknown or duplicate request id");
  }
  in_flight_ids_.erase(it);
  metrics_.in_flight = in_flight_ids_.size();
  if (status == CompletionStatus::Success) {
    ++metrics_.completed_success;
  } else {
    ++metrics_.completed_failure;
  }

  SchedulerEvent event;
  event.kind = SchedulerEventKind::Completed;
  event.request_id = id;
  event.policy = config_.policy;
  event.completion_status = status;
  event.error_code = error_code;
  event.timestamp = now();
  emit_event_locked(std::move(event));
  return {};
}

Result<void> FifoScheduler::cancel(std::string_view request_id, CancellationReason reason) {
  std::lock_guard<std::mutex> guard(mutex_);
  const std::string id{request_id};

  // Queued path: find and remove from the queue, release buffers.
  for (auto it = queue_.begin(); it != queue_.end(); ++it) {
    if (it->request_id() == id) {
      const auto wait = std::chrono::duration_cast<SchedulerClock::Duration>(
          now() - it->enqueue_time());
      record_wait_locked(wait);

      SchedulerEvent event;
      event.kind = SchedulerEventKind::Cancelled;
      event.request_id = id;
      event.endpoint = it->endpoint();
      event.backend_name = it->backend_name();
      event.policy = config_.policy;
      event.cancellation_reason = reason;
      event.wait_time = wait;
      event.timestamp = now();

      release_request_buffers_safely(*it);
      queue_.erase(it);
      metrics_.queue_depth = queue_.size();
      ++metrics_.cancelled_queued;
      emit_event_locked(std::move(event));
      return {};
    }
  }

  // In-flight path: clear accounting and tombstone the id so a racing
  // on_completion is a typed no-op. The executor still holds the
  // SchedulerRequest, so buffer release is the executor's
  // responsibility on the in-flight side.
  if (auto in_it = in_flight_ids_.find(id); in_it != in_flight_ids_.end()) {
    in_flight_ids_.erase(in_it);
    cancelled_in_flight_ids_.insert(id);
    metrics_.in_flight = in_flight_ids_.size();
    ++metrics_.cancelled_in_flight;

    SchedulerEvent event;
    event.kind = SchedulerEventKind::Cancelled;
    event.request_id = id;
    event.policy = config_.policy;
    event.cancellation_reason = reason;
    event.timestamp = now();
    emit_event_locked(std::move(event));
    return {};
  }

  // Already cancelled or never known.
  if (auto cancel_it = cancelled_in_flight_ids_.find(id);
      cancel_it != cancelled_in_flight_ids_.end()) {
    return unexpected(Error::Code::NotReady,
                      "scheduler::cancel: request already cancelled in-flight");
  }
  return unexpected(Error::Code::NotReady,
                    "scheduler::cancel: unknown, completed, or never-admitted request id");
}

std::size_t FifoScheduler::expire_due_locked() {
  std::size_t removed = 0;
  for (auto it = queue_.begin(); it != queue_.end();) {
    if (is_expired_locked(*it)) {
      const auto wait = std::chrono::duration_cast<SchedulerClock::Duration>(
          now() - it->enqueue_time());
      record_wait_locked(wait);

      SchedulerEvent event;
      event.kind = SchedulerEventKind::Expired;
      event.request_id = it->request_id();
      event.endpoint = it->endpoint();
      event.backend_name = it->backend_name();
      event.policy = config_.policy;
      event.error_code = Error::Code::Timeout;
      event.wait_time = wait;
      event.timestamp = now();

      release_request_buffers_safely(*it);
      it = queue_.erase(it);
      ++metrics_.expired_total;
      ++removed;
      emit_event_locked(std::move(event));
    } else {
      ++it;
    }
  }
  metrics_.queue_depth = queue_.size();
  return removed;
}

std::size_t FifoScheduler::expire_due() {
  std::lock_guard<std::mutex> guard(mutex_);
  return expire_due_locked();
}

void FifoScheduler::on_pressure(const PressureSignal& signal) {
  std::lock_guard<std::mutex> guard(mutex_);
  if (signal.source == PressureSource::Memory) {
    metrics_.last_memory_severity = signal.severity;
    ++metrics_.pressure_events_memory;
  } else {
    metrics_.last_thermal_severity = signal.severity;
    ++metrics_.pressure_events_thermal;
  }

  SchedulerEvent event;
  event.kind = signal.source == PressureSource::Memory ? SchedulerEventKind::MemoryPressure
                                                       : SchedulerEventKind::ThermalPressure;
  event.policy = config_.policy;
  event.pressure_source = signal.source;
  event.pressure_severity = signal.severity;
  // Timestamp prefers the signal's own monotonic stamp when populated.
  event.timestamp = signal.timestamp.time_since_epoch().count() == 0 ? now() : signal.timestamp;
  emit_event_locked(std::move(event));
}

std::size_t FifoScheduler::shutdown() {
  std::lock_guard<std::mutex> guard(mutex_);
  shutdown_called_ = true;
  std::size_t cancelled = 0;

  // Cancel everything queued.
  while (!queue_.empty()) {
    auto& head = queue_.front();
    const auto wait = std::chrono::duration_cast<SchedulerClock::Duration>(
        now() - head.enqueue_time());
    record_wait_locked(wait);

    SchedulerEvent event;
    event.kind = SchedulerEventKind::Cancelled;
    event.request_id = head.request_id();
    event.endpoint = head.endpoint();
    event.backend_name = head.backend_name();
    event.policy = config_.policy;
    event.cancellation_reason = CancellationReason::Shutdown;
    event.wait_time = wait;
    event.timestamp = now();

    release_request_buffers_safely(head);
    queue_.pop_front();
    ++metrics_.cancelled_queued;
    ++cancelled;
    emit_event_locked(std::move(event));
  }
  metrics_.queue_depth = 0;

  // Cancel in-flight ids by tombstoning them.
  for (auto& id : in_flight_ids_) {
    cancelled_in_flight_ids_.insert(id);
    ++metrics_.cancelled_in_flight;
    ++cancelled;
    SchedulerEvent event;
    event.kind = SchedulerEventKind::Cancelled;
    event.request_id = id;
    event.policy = config_.policy;
    event.cancellation_reason = CancellationReason::Shutdown;
    event.timestamp = now();
    emit_event_locked(std::move(event));
  }
  in_flight_ids_.clear();
  metrics_.in_flight = 0;
  return cancelled;
}

SchedulerMetrics FifoScheduler::metrics() const {
  std::lock_guard<std::mutex> guard(mutex_);
  return metrics_;
}

}  // namespace tensorplate
