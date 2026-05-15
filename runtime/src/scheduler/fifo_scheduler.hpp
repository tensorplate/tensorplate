// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F02..F06: v0.1.0 default scheduler (FIFO + deadline-aware
// admission + pressure-aware admission + deterministic buffer cleanup).
//
// FifoScheduler is the only InferScheduler implementation in v0.1.0.
// Its public surface is the InferScheduler interface; this header is
// runtime-private so executor code never depends on the concrete type.
//
// Behavior summary:
//
//   admit()
//     - rejects empty / malformed envelopes with ConfigInvalid,
//     - rejects new admission while shutdown() has been called
//       (NotReady),
//     - rejects when queue_capacity is reached (OOMError, overload),
//     - rejects when the most recent pressure signal severity is at or
//       above `pressure_reject_threshold` (OOMError, pressure),
//     - rejects when the request is already past deadline (Timeout) or
//       when the estimated completion exceeds deadline + margin (Timeout),
//     - otherwise enqueues in FIFO order.
//
//   next()
//     - sweeps expired queued requests first (calls the same release
//       path as the explicit expire_due()),
//     - returns std::nullopt if the queue is empty or in_flight is at
//       capacity,
//     - otherwise pops the head, records it as in-flight, and emits
//       Dispatched with the wait time populated.
//
//   on_completion()
//     - removes the request from in-flight; duplicate completion or
//       completion of an unknown request returns Error::Code::Internal
//       and does not change accounting.
//
//   cancel()
//     - queued: removes from the queue and releases input BufferRefs
//       via release_request_buffers() when a buffer manager is wired.
//     - in-flight: clears in-flight accounting, records the
//       cancellation intent in `cancelled_in_flight_ids_` so a racing
//       on_completion is a typed no-op, and emits Cancelled. The
//       executor still owns the SchedulerRequest at this point and
//       remains responsible for buffer release on the in-flight side.
//     - unknown id: returns NotReady.
//
//   expire_due()
//     - sweeps the queue once, releasing each expired request's input
//       buffers and emitting Expired events.
//
//   on_pressure()
//     - records the most recent severity per source and emits
//       MemoryPressure / ThermalPressure events.
//
//   shutdown()
//     - cancels every queued and in-flight request with reason
//       Shutdown, releases queued buffers, and flips the scheduler to
//       a NotReady-on-admit state.

#pragma once

#include <deque>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_set>

#include "tensorplate/scheduler/scheduler.hpp"

namespace tensorplate {

class FifoScheduler final : public InferScheduler {
 public:
  /// Validating factory. The factory closure in factory.cpp forwards
  /// every typed error unchanged.
  ///
  /// Returns:
  ///   - ConfigInvalid : SchedulerConfig fields fail validation. The
  ///                     same validation runs in `validate_common_config`
  ///                     so the registry surfaces config errors before
  ///                     calling this factory; the duplicate check is
  ///                     defensive.
  static Result<std::unique_ptr<InferScheduler>> create(const SchedulerConfig& config,
                                                        SchedulerRuntimeHooks hooks);

  ~FifoScheduler() override;

  // -- InferScheduler ------------------------------------------------------

  Result<void> admit(SchedulerRequest request) override;
  std::optional<SchedulerRequest> next() override;
  Result<void> on_completion(std::string_view request_id, CompletionStatus status,
                             std::optional<Error::Code> error_code) override;
  Result<void> cancel(std::string_view request_id, CancellationReason reason) override;
  std::size_t expire_due() override;
  void on_pressure(const PressureSignal& signal) override;
  std::size_t shutdown() override;
  SchedulerMetrics metrics() const override;
  std::string_view policy_name() const noexcept override { return "fifo"; }

 private:
  FifoScheduler(SchedulerConfig config, SchedulerRuntimeHooks hooks);

  // Internal helpers; all callers must hold mutex_ unless noted.
  void emit_event_locked(const SchedulerEvent& event);
  bool is_expired_locked(const SchedulerRequest& req) const;
  SchedulerClock::TimePoint now() const noexcept {
    return clock_ != nullptr ? clock_->now() : std::chrono::steady_clock::now();
  }
  void release_request_buffers_safely(SchedulerRequest& req) noexcept;
  void record_wait_locked(SchedulerClock::Duration wait);

  // Sweep the queue removing every expired request. Returns the count
  // of removed requests. Callers must hold mutex_.
  std::size_t expire_due_locked();

  // Returns true if pressure rejection is currently active.
  bool pressure_rejects_locked() const;

  // Active rejection severity is the max of last_memory_severity_ and
  // last_thermal_severity_. Used by admission to apply the configured
  // pressure_reject_threshold uniformly.
  PressureSeverity active_pressure_severity_locked() const;

  SchedulerConfig config_;
  SchedulerEventSink* event_sink_ = nullptr;
  BufferManager* buffer_manager_ = nullptr;
  const SchedulerClock* clock_ = nullptr;

  mutable std::mutex mutex_;
  std::deque<SchedulerRequest> queue_;
  // Tracks request ids that have been dispatched and not yet completed.
  // We don't keep the SchedulerRequest itself in-flight: it has been
  // moved out to the executor on next(). The id set lets us detect
  // duplicate completion, racing cancellation/completion, and lets
  // cancel() target in-flight requests.
  std::unordered_set<std::string> in_flight_ids_;
  // Ids cancelled while in-flight. The next on_completion for one of
  // these is a typed no-op so adapters that race cancellation with
  // completion observe deterministic behavior.
  std::unordered_set<std::string> cancelled_in_flight_ids_;

  bool shutdown_called_ = false;

  // Metrics counters and aggregates. The snapshot in metrics() is
  // taken under mutex_ and is a value copy of this state.
  SchedulerMetrics metrics_;
};

static_assert(InferSchedulerConcept<FifoScheduler>);

}  // namespace tensorplate
