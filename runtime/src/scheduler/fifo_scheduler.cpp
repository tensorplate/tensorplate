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

void FifoScheduler::emit_event(const SchedulerEvent& event) noexcept {
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

bool FifoScheduler::request_id_active_locked(std::string_view request_id) const {
  for (const auto& req : queue_) {
    if (req.request_id() == request_id) {
      return true;
    }
  }
  if (in_flight_.contains(std::string{request_id})) {
    return true;
  }
  return cancelled_in_flight_ids_.contains(std::string{request_id});
}

bool FifoScheduler::deadline_infeasible_locked(const SchedulerRequest& req,
                                               std::size_t queued_ahead) const {
  const auto& deadline = req.request().deadline();
  if (!deadline.has_value()) {
    return false;
  }
  const auto now_tp = now();
  // Past-deadline requests are always expired regardless of margin because
  // no service estimate can rescue them. Margin only widens admission; it
  // cannot retroactively un-expire a request.
  if (now_tp >= *deadline) {
    return true;
  }

  const SchedulerClock::Duration default_est = as_nanos(config_.default_service_estimate);
  const SchedulerClock::Duration service_est =
      req.estimate().estimated_service_time.value_or(default_est);
  const SchedulerClock::Duration queued_wait_est = default_est * queued_ahead;
  const SchedulerClock::Duration in_flight_est = default_est * in_flight_.size();
  const auto estimated_completion = now_tp + queued_wait_est + in_flight_est + service_est;
  const auto allowed_completion = *deadline + as_nanos(config_.deadline_margin);
  return estimated_completion > allowed_completion;
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
  std::optional<SchedulerEvent> event;
  std::optional<Error> rejection;

  {
    std::lock_guard<std::mutex> guard(mutex_);

    if (shutdown_called_) {
      release_request_buffers_safely(request);
      SchedulerEvent rejected;
      rejected.kind = SchedulerEventKind::AdmissionRejected;
      rejected.request_id = request.request_id();
      rejected.endpoint = request.endpoint();
      rejected.backend_name = request.backend_name();
      rejected.model_id = request.model_id();
      rejected.policy = config_.policy;
      rejected.error_code = Error::Code::NotReady;
      rejected.timestamp = now();
      event = std::move(rejected);
      rejection = Error::make(Error::Code::NotReady, "scheduler is shut down");
    } else if (request.request_id().empty() || request.endpoint().empty() ||
               (request.estimate().estimated_service_time.has_value() &&
                request.estimate().estimated_service_time->count() < 0)) {
      // Envelope validation. InferRequest::create already validates, but a
      // future caller may build a SchedulerRequest by other means; the
      // defensive check is cheap.
      release_request_buffers_safely(request);
      SchedulerEvent rejected;
      rejected.kind = SchedulerEventKind::AdmissionRejected;
      rejected.request_id = request.request_id();
      rejected.endpoint = request.endpoint();
      rejected.backend_name = request.backend_name();
      rejected.model_id = request.model_id();
      rejected.policy = config_.policy;
      rejected.error_code = Error::Code::ConfigInvalid;
      rejected.timestamp = now();
      event = std::move(rejected);
      rejection = Error::make(Error::Code::ConfigInvalid,
                              "scheduler request_id, endpoint, and service estimate must be valid");
    } else if (request_id_active_locked(request.request_id())) {
      SchedulerEvent rejected;
      rejected.kind = SchedulerEventKind::AdmissionRejected;
      rejected.request_id = request.request_id();
      rejected.endpoint = request.endpoint();
      rejected.backend_name = request.backend_name();
      rejected.model_id = request.model_id();
      rejected.policy = config_.policy;
      rejected.error_code = Error::Code::ConfigInvalid;
      rejected.timestamp = now();
      release_request_buffers_safely(request);
      event = std::move(rejected);
      rejection = Error::make(Error::Code::ConfigInvalid, "scheduler request_id is already active");
    } else if (pressure_rejects_locked()) {
      // Pressure-aware rejection (F06). The most recent severity per
      // source is consulted; v0.1.0 baseline never silently degrades.
      ++metrics_.admission_rejected_pressure;
      SchedulerEvent rejected;
      rejected.kind = SchedulerEventKind::AdmissionRejected;
      rejected.request_id = request.request_id();
      rejected.endpoint = request.endpoint();
      rejected.backend_name = request.backend_name();
      rejected.model_id = request.model_id();
      rejected.policy = config_.policy;
      rejected.error_code = Error::Code::OOMError;
      rejected.pressure_severity = active_pressure_severity_locked();
      rejected.timestamp = now();
      release_request_buffers_safely(request);
      event = std::move(rejected);
      rejection =
          Error::make(Error::Code::OOMError, "scheduler rejecting admission due to pressure");
    } else if (queue_.size() >= config_.queue_capacity) {
      // Capacity check (F02).
      ++metrics_.admission_rejected_overload;
      SchedulerEvent rejected;
      rejected.kind = SchedulerEventKind::AdmissionRejected;
      rejected.request_id = request.request_id();
      rejected.endpoint = request.endpoint();
      rejected.backend_name = request.backend_name();
      rejected.model_id = request.model_id();
      rejected.policy = config_.policy;
      rejected.error_code = Error::Code::OOMError;
      rejected.timestamp = now();
      release_request_buffers_safely(request);
      event = std::move(rejected);
      rejection = Error::make(Error::Code::OOMError, "scheduler queue is at capacity");
    } else if (deadline_infeasible_locked(request, queue_.size())) {
      ++metrics_.admission_rejected_deadline;
      SchedulerEvent rejected;
      rejected.kind = SchedulerEventKind::AdmissionRejected;
      rejected.request_id = request.request_id();
      rejected.endpoint = request.endpoint();
      rejected.backend_name = request.backend_name();
      rejected.model_id = request.model_id();
      rejected.policy = config_.policy;
      rejected.error_code = Error::Code::Timeout;
      rejected.timestamp = now();
      release_request_buffers_safely(request);
      event = std::move(rejected);
      rejection =
          Error::make(Error::Code::Timeout,
                      "scheduler rejecting admission: deadline + margin exceeded by estimate");
    } else {
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

      SchedulerEvent admitted;
      admitted.kind = SchedulerEventKind::Admitted;
      admitted.request_id = request.request_id();
      admitted.endpoint = request.endpoint();
      admitted.backend_name = request.backend_name();
      admitted.model_id = request.model_id();
      admitted.policy = config_.policy;
      admitted.timestamp = now_tp;
      event = std::move(admitted);
    }
  }

  if (event.has_value()) {
    emit_event(*event);
  }
  if (rejection.has_value()) {
    return unexpected(std::move(*rejection));
  }
  return {};
}

std::optional<SchedulerRequest> FifoScheduler::next() {
  std::vector<SchedulerEvent> events;
  std::optional<SchedulerRequest> result;

  {
    std::lock_guard<std::mutex> guard(mutex_);

    // Sweep expired/infeasible entries first so we never dispatch stale work.
    (void)expire_due_locked(events);

    if (!queue_.empty() && in_flight_.size() < config_.in_flight_capacity) {
      SchedulerRequest head = std::move(queue_.front());
      queue_.pop_front();
      metrics_.queue_depth = queue_.size();

      const auto now_tp = now();
      const auto wait =
          std::chrono::duration_cast<SchedulerClock::Duration>(now_tp - head.enqueue_time());
      record_wait_locked(wait);

      const std::string id = head.request_id();
      in_flight_.emplace(id, InFlightRecord{head.endpoint(), head.backend_name(), head.model_id()});
      if (in_flight_.size() > metrics_.in_flight_high_water) {
        metrics_.in_flight_high_water = in_flight_.size();
      }
      metrics_.in_flight = in_flight_.size();

      SchedulerEvent dispatched;
      dispatched.kind = SchedulerEventKind::Dispatched;
      dispatched.request_id = head.request_id();
      dispatched.endpoint = head.endpoint();
      dispatched.backend_name = head.backend_name();
      dispatched.model_id = head.model_id();
      dispatched.policy = config_.policy;
      dispatched.wait_time = wait;
      dispatched.timestamp = now_tp;
      events.push_back(std::move(dispatched));
      result.emplace(std::move(head));
    }
  }

  for (const auto& event : events) {
    emit_event(event);
  }
  return result;
}

Result<void> FifoScheduler::on_completion(std::string_view request_id, CompletionStatus status,
                                          std::optional<Error::Code> error_code) {
  std::optional<SchedulerEvent> event;
  std::optional<Error> error;
  const std::string id{request_id};

  {
    std::lock_guard<std::mutex> guard(mutex_);
    // If this request was cancelled while in-flight, the cancellation
    // already cleared in_flight_. The completion is a typed no-op for
    // racing adapters.
    if (auto cancel_it = cancelled_in_flight_ids_.find(id);
        cancel_it != cancelled_in_flight_ids_.end()) {
      cancelled_in_flight_ids_.erase(cancel_it);
      error = Error::make(Error::Code::Internal,
                          "scheduler::on_completion: request was cancelled in-flight");
    } else if (auto it = in_flight_.find(id); it == in_flight_.end()) {
      error = Error::make(Error::Code::Internal,
                          "scheduler::on_completion: unknown or duplicate request id");
    } else {
      InFlightRecord record = std::move(it->second);
      in_flight_.erase(it);
      metrics_.in_flight = in_flight_.size();
      if (status == CompletionStatus::Success) {
        ++metrics_.completed_success;
      } else {
        ++metrics_.completed_failure;
      }

      SchedulerEvent completed;
      completed.kind = SchedulerEventKind::Completed;
      completed.request_id = id;
      completed.endpoint = record.endpoint;
      completed.backend_name = record.backend_name;
      completed.model_id = record.model_id;
      completed.policy = config_.policy;
      completed.completion_status = status;
      completed.error_code = error_code;
      completed.timestamp = now();
      event = std::move(completed);
    }
  }

  if (event.has_value()) {
    emit_event(*event);
  }
  if (error.has_value()) {
    return unexpected(std::move(*error));
  }
  return {};
}

Result<void> FifoScheduler::cancel(std::string_view request_id, CancellationReason reason) {
  std::optional<SchedulerEvent> event;
  std::optional<Error> error;
  const std::string id{request_id};

  {
    std::lock_guard<std::mutex> guard(mutex_);

    // Queued path: find and remove from the queue, release buffers.
    for (auto it = queue_.begin(); it != queue_.end(); ++it) {
      if (it->request_id() == id) {
        const auto wait =
            std::chrono::duration_cast<SchedulerClock::Duration>(now() - it->enqueue_time());
        record_wait_locked(wait);

        SchedulerEvent cancelled;
        cancelled.kind = SchedulerEventKind::Cancelled;
        cancelled.request_id = id;
        cancelled.endpoint = it->endpoint();
        cancelled.backend_name = it->backend_name();
        cancelled.model_id = it->model_id();
        cancelled.policy = config_.policy;
        cancelled.cancellation_reason = reason;
        cancelled.wait_time = wait;
        cancelled.timestamp = now();

        release_request_buffers_safely(*it);
        queue_.erase(it);
        metrics_.queue_depth = queue_.size();
        ++metrics_.cancelled_queued;
        event = std::move(cancelled);
        break;
      }
    }

    // In-flight path: clear accounting and tombstone the id so a racing
    // on_completion is a typed no-op. The executor still holds the
    // SchedulerRequest, so buffer release is the executor's
    // responsibility on the in-flight side.
    if (!event.has_value()) {
      if (auto in_it = in_flight_.find(id); in_it != in_flight_.end()) {
        InFlightRecord record = std::move(in_it->second);
        in_flight_.erase(in_it);
        cancelled_in_flight_ids_.insert(id);
        metrics_.in_flight = in_flight_.size();
        ++metrics_.cancelled_in_flight;

        SchedulerEvent cancelled;
        cancelled.kind = SchedulerEventKind::Cancelled;
        cancelled.request_id = id;
        cancelled.endpoint = record.endpoint;
        cancelled.backend_name = record.backend_name;
        cancelled.model_id = record.model_id;
        cancelled.policy = config_.policy;
        cancelled.cancellation_reason = reason;
        cancelled.timestamp = now();
        event = std::move(cancelled);
      }
    }

    if (!event.has_value()) {
      if (auto cancel_it = cancelled_in_flight_ids_.find(id);
          cancel_it != cancelled_in_flight_ids_.end()) {
        error = Error::make(Error::Code::NotReady,
                            "scheduler::cancel: request already cancelled in-flight");
      } else {
        error = Error::make(Error::Code::NotReady,
                            "scheduler::cancel: unknown, completed, or never-admitted request id");
      }
    }
  }

  if (event.has_value()) {
    emit_event(*event);
  }
  if (error.has_value()) {
    return unexpected(std::move(*error));
  }
  return {};
}

std::size_t FifoScheduler::expire_due_locked(std::vector<SchedulerEvent>& events) {
  std::size_t removed = 0;
  std::size_t queued_ahead = 0;
  for (auto it = queue_.begin(); it != queue_.end();) {
    if (deadline_infeasible_locked(*it, queued_ahead)) {
      const auto wait =
          std::chrono::duration_cast<SchedulerClock::Duration>(now() - it->enqueue_time());
      record_wait_locked(wait);

      SchedulerEvent event;
      event.kind = SchedulerEventKind::Expired;
      event.request_id = it->request_id();
      event.endpoint = it->endpoint();
      event.backend_name = it->backend_name();
      event.model_id = it->model_id();
      event.policy = config_.policy;
      event.error_code = Error::Code::Timeout;
      event.wait_time = wait;
      event.timestamp = now();

      release_request_buffers_safely(*it);
      it = queue_.erase(it);
      ++metrics_.expired_total;
      ++removed;
      events.push_back(std::move(event));
    } else {
      ++queued_ahead;
      ++it;
    }
  }
  metrics_.queue_depth = queue_.size();
  return removed;
}

std::size_t FifoScheduler::expire_due() {
  std::vector<SchedulerEvent> events;
  std::size_t removed = 0;
  {
    std::lock_guard<std::mutex> guard(mutex_);
    removed = expire_due_locked(events);
  }
  for (const auto& event : events) {
    emit_event(event);
  }
  return removed;
}

void FifoScheduler::on_pressure(const PressureSignal& signal) {
  SchedulerEvent event;
  {
    std::lock_guard<std::mutex> guard(mutex_);
    if (signal.source == PressureSource::Memory) {
      metrics_.last_memory_severity = signal.severity;
      ++metrics_.pressure_events_memory;
    } else {
      metrics_.last_thermal_severity = signal.severity;
      ++metrics_.pressure_events_thermal;
    }

    event.kind = signal.source == PressureSource::Memory ? SchedulerEventKind::MemoryPressure
                                                         : SchedulerEventKind::ThermalPressure;
    event.policy = config_.policy;
    event.pressure_source = signal.source;
    event.pressure_severity = signal.severity;
    // Timestamp prefers the signal's own monotonic stamp when populated.
    event.timestamp = signal.timestamp.time_since_epoch().count() == 0 ? now() : signal.timestamp;
  }
  emit_event(event);
}

std::size_t FifoScheduler::shutdown() {
  std::vector<SchedulerEvent> events;
  std::size_t cancelled = 0;

  {
    std::lock_guard<std::mutex> guard(mutex_);
    shutdown_called_ = true;

    // Cancel everything queued.
    while (!queue_.empty()) {
      auto& head = queue_.front();
      const auto wait =
          std::chrono::duration_cast<SchedulerClock::Duration>(now() - head.enqueue_time());
      record_wait_locked(wait);

      SchedulerEvent event;
      event.kind = SchedulerEventKind::Cancelled;
      event.request_id = head.request_id();
      event.endpoint = head.endpoint();
      event.backend_name = head.backend_name();
      event.model_id = head.model_id();
      event.policy = config_.policy;
      event.cancellation_reason = CancellationReason::Shutdown;
      event.wait_time = wait;
      event.timestamp = now();

      release_request_buffers_safely(head);
      queue_.pop_front();
      ++metrics_.cancelled_queued;
      ++cancelled;
      events.push_back(std::move(event));
    }
    metrics_.queue_depth = 0;

    // Cancel in-flight ids by tombstoning them.
    for (const auto& [id, record] : in_flight_) {
      cancelled_in_flight_ids_.insert(id);
      ++metrics_.cancelled_in_flight;
      ++cancelled;
      SchedulerEvent event;
      event.kind = SchedulerEventKind::Cancelled;
      event.request_id = id;
      event.endpoint = record.endpoint;
      event.backend_name = record.backend_name;
      event.model_id = record.model_id;
      event.policy = config_.policy;
      event.cancellation_reason = CancellationReason::Shutdown;
      event.timestamp = now();
      events.push_back(std::move(event));
    }
    in_flight_.clear();
    metrics_.in_flight = 0;
  }

  for (const auto& event : events) {
    emit_event(event);
  }
  return cancelled;
}

SchedulerMetrics FifoScheduler::metrics() const {
  std::lock_guard<std::mutex> guard(mutex_);
  return metrics_;
}

}  // namespace tensorplate
