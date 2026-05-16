// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F06: Local serving metrics.
//
// Metrics are accumulated in process by `ServingMetrics`, a thread-
// safe counter/histogram bag with bounded labels: `endpoint`,
// `model_class`, `model_name`, and `backend`. Labels are bounded at
// the source so a misbehaving client cannot blow up cardinality.
//
// Two export modes are supported (selected by `MetricsMode` in
// `ServingConfig`):
//
//   - PrometheusText: the default. Emits the Prometheus 0.0.4
//     text-format body suitable for `/metrics` scrape.
//   - Json: a stable JSON body keyed by metric name. Used by agent
//     side scrapers that prefer machine-friendly bodies.
//
// Latency histograms use a small fixed set of bucket boundaries
// matched to robotics-class latency budgets (1 ms ... 5 s). The
// boundaries are part of the wire contract; downstream observability
// (V01-E12) re-uses them.

#pragma once

#include <array>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <mutex>
#include <string>
#include <vector>

#include "tensorplate/core/error.hpp"

namespace tensorplate {

/// Latency bucket boundaries (milliseconds). The trailing +Inf bucket
/// is implicit.
inline constexpr std::array<double, 11> kLatencyBucketsMs{0.5,  1.0,   2.0,   5.0,    10.0,  25.0,
                                                          50.0, 100.0, 250.0, 1000.0, 5000.0};

/// Snapshot of a single latency histogram.
struct LatencyHistogramSnapshot {
  std::array<std::uint64_t, kLatencyBucketsMs.size() + 1> bucket_counts{};
  std::uint64_t total_count = 0;
  double sum_ms = 0.0;
};

/// Thread-safe latency histogram with the v0.1.0 bucket layout.
class LatencyHistogram {
 public:
  void observe_ms(double ms);
  [[nodiscard]] LatencyHistogramSnapshot snapshot() const;
  void reset() noexcept;

 private:
  mutable std::mutex mutex_;
  std::array<std::uint64_t, kLatencyBucketsMs.size() + 1> bucket_counts_{};
  std::uint64_t total_count_ = 0;
  double sum_ms_ = 0.0;
};

/// Stable per-process bounded labels echoed onto every metric. The
/// labels are populated at composition-root time and never derived
/// from request headers / payloads.
struct MetricsLabels {
  std::string endpoint;
  std::string model_class;
  std::string model_name;
  std::string backend;
};

/// Snapshot of the full serving-metric bag. Cheap to take; intended
/// to be serialized in one go by the export path.
struct ServingMetricsSnapshot {
  MetricsLabels labels;

  // Request counters (sync /infer + async policy combined unless
  // noted; per-route counters are derivable from the routes table).
  std::uint64_t requests_total = 0;
  std::uint64_t requests_succeeded = 0;
  std::uint64_t requests_failed = 0;
  std::uint64_t requests_rejected_malformed = 0;
  std::uint64_t requests_rejected_oversize = 0;
  std::uint64_t requests_rejected_overload = 0;
  std::uint64_t requests_rejected_deadline = 0;
  std::uint64_t requests_rejected_unsupported = 0;
  std::uint64_t requests_cancelled = 0;
  std::uint64_t requests_expired = 0;

  // Async-route counters.
  std::uint64_t async_accepted = 0;
  std::uint64_t async_completed = 0;
  std::uint64_t async_cancelled = 0;
  std::uint64_t async_stale = 0;
  std::uint64_t async_evicted = 0;

  // Shutdown / drain counters.
  std::uint64_t shutdown_started = 0;
  std::uint64_t shutdown_completed = 0;
  std::uint64_t requests_rejected_stopping = 0;

  // Buffer plane.
  std::uint64_t buffer_in_use_bytes = 0;
  std::uint64_t buffer_active_count = 0;
  std::uint64_t buffer_high_water_bytes = 0;

  // Scheduler snapshot mirrored for the serving exporter.
  std::uint64_t scheduler_queue_depth = 0;
  std::uint64_t scheduler_in_flight = 0;
  std::uint64_t scheduler_admitted_total = 0;
  std::uint64_t scheduler_completed_success = 0;
  std::uint64_t scheduler_completed_failure = 0;

  // Latency histograms.
  LatencyHistogramSnapshot ingress_latency;
  LatencyHistogramSnapshot queue_wait;
  LatencyHistogramSnapshot execution_latency;
  LatencyHistogramSnapshot total_latency;
};

/// Counter/histogram bag for the serving worker.
class ServingMetrics {
 public:
  ServingMetrics() = default;
  ~ServingMetrics() = default;
  ServingMetrics(const ServingMetrics&) = delete;
  ServingMetrics& operator=(const ServingMetrics&) = delete;
  ServingMetrics(ServingMetrics&&) = delete;
  ServingMetrics& operator=(ServingMetrics&&) = delete;

  void set_labels(MetricsLabels labels);

  void increment_requests_total() noexcept { requests_total_.fetch_add(1); }
  void increment_requests_succeeded() noexcept { requests_succeeded_.fetch_add(1); }
  void increment_requests_failed() noexcept { requests_failed_.fetch_add(1); }
  void increment_rejected_malformed() noexcept { requests_rejected_malformed_.fetch_add(1); }
  void increment_rejected_oversize() noexcept { requests_rejected_oversize_.fetch_add(1); }
  void increment_rejected_overload() noexcept { requests_rejected_overload_.fetch_add(1); }
  void increment_rejected_deadline() noexcept { requests_rejected_deadline_.fetch_add(1); }
  void increment_rejected_unsupported() noexcept { requests_rejected_unsupported_.fetch_add(1); }
  void increment_cancelled() noexcept { requests_cancelled_.fetch_add(1); }
  void increment_expired() noexcept { requests_expired_.fetch_add(1); }
  void increment_async_accepted() noexcept { async_accepted_.fetch_add(1); }
  void increment_async_completed() noexcept { async_completed_.fetch_add(1); }
  void increment_async_cancelled() noexcept { async_cancelled_.fetch_add(1); }
  void increment_async_stale() noexcept { async_stale_.fetch_add(1); }
  void increment_async_evicted() noexcept { async_evicted_.fetch_add(1); }
  void increment_shutdown_started() noexcept { shutdown_started_.fetch_add(1); }
  void increment_shutdown_completed() noexcept { shutdown_completed_.fetch_add(1); }
  void increment_rejected_stopping() noexcept { requests_rejected_stopping_.fetch_add(1); }

  /// Increment the rejection counter that matches `code`. Convenience
  /// wrapper used by the request router so the typed-error -> metric
  /// mapping lives in one place.
  void record_rejection(Error::Code code) noexcept;

  void observe_ingress_ms(double ms) { ingress_.observe_ms(ms); }
  void observe_queue_wait_ms(double ms) { queue_wait_.observe_ms(ms); }
  void observe_execution_ms(double ms) { execution_.observe_ms(ms); }
  void observe_total_ms(double ms) { total_.observe_ms(ms); }

  /// Capture buffer-plane accounting from `BufferManager::accounting`.
  void record_buffer_accounting(std::size_t in_use_bytes, std::size_t active_count,
                                std::size_t high_water_bytes) noexcept;

  /// Capture scheduler-side counters from `SchedulerMetrics`.
  void record_scheduler_accounting(std::size_t queue_depth, std::size_t in_flight,
                                   std::uint64_t admitted_total, std::uint64_t completed_success,
                                   std::uint64_t completed_failure) noexcept;

  [[nodiscard]] ServingMetricsSnapshot snapshot() const;

 private:
  mutable std::mutex labels_mutex_;
  MetricsLabels labels_;

  std::atomic<std::uint64_t> requests_total_{0};
  std::atomic<std::uint64_t> requests_succeeded_{0};
  std::atomic<std::uint64_t> requests_failed_{0};
  std::atomic<std::uint64_t> requests_rejected_malformed_{0};
  std::atomic<std::uint64_t> requests_rejected_oversize_{0};
  std::atomic<std::uint64_t> requests_rejected_overload_{0};
  std::atomic<std::uint64_t> requests_rejected_deadline_{0};
  std::atomic<std::uint64_t> requests_rejected_unsupported_{0};
  std::atomic<std::uint64_t> requests_cancelled_{0};
  std::atomic<std::uint64_t> requests_expired_{0};
  std::atomic<std::uint64_t> async_accepted_{0};
  std::atomic<std::uint64_t> async_completed_{0};
  std::atomic<std::uint64_t> async_cancelled_{0};
  std::atomic<std::uint64_t> async_stale_{0};
  std::atomic<std::uint64_t> async_evicted_{0};
  std::atomic<std::uint64_t> shutdown_started_{0};
  std::atomic<std::uint64_t> shutdown_completed_{0};
  std::atomic<std::uint64_t> requests_rejected_stopping_{0};
  std::atomic<std::uint64_t> buffer_in_use_bytes_{0};
  std::atomic<std::uint64_t> buffer_active_count_{0};
  std::atomic<std::uint64_t> buffer_high_water_bytes_{0};
  std::atomic<std::uint64_t> scheduler_queue_depth_{0};
  std::atomic<std::uint64_t> scheduler_in_flight_{0};
  std::atomic<std::uint64_t> scheduler_admitted_total_{0};
  std::atomic<std::uint64_t> scheduler_completed_success_{0};
  std::atomic<std::uint64_t> scheduler_completed_failure_{0};

  LatencyHistogram ingress_;
  LatencyHistogram queue_wait_;
  LatencyHistogram execution_;
  LatencyHistogram total_;
};

/// Render a metrics snapshot in Prometheus 0.0.4 text exposition
/// format. The output is stable across patch versions of v0.1.0.
[[nodiscard]] std::string render_prometheus_text(const ServingMetricsSnapshot& snap);

/// Render a metrics snapshot as JSON.
[[nodiscard]] std::string render_metrics_json(const ServingMetricsSnapshot& snap);

}  // namespace tensorplate
