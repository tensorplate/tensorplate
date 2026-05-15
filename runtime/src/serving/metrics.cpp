// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/metrics.hpp"

#include <algorithm>
#include <chrono>
#include <iomanip>
#include <mutex>
#include <sstream>
#include <string>
#include <utility>

#include <nlohmann/json.hpp>

namespace tensorplate {

void LatencyHistogram::observe_ms(double ms) {
  std::lock_guard<std::mutex> g(mutex_);
  std::size_t idx = bucket_counts_.size() - 1;
  for (std::size_t i = 0; i < kLatencyBucketsMs.size(); ++i) {
    if (ms <= kLatencyBucketsMs[i]) {
      idx = i;
      break;
    }
  }
  bucket_counts_[idx] += 1;
  total_count_ += 1;
  sum_ms_ += ms;
}

LatencyHistogramSnapshot LatencyHistogram::snapshot() const {
  std::lock_guard<std::mutex> g(mutex_);
  LatencyHistogramSnapshot out;
  out.bucket_counts = bucket_counts_;
  out.total_count = total_count_;
  out.sum_ms = sum_ms_;
  return out;
}

void LatencyHistogram::reset() noexcept {
  std::lock_guard<std::mutex> g(mutex_);
  bucket_counts_.fill(0);
  total_count_ = 0;
  sum_ms_ = 0.0;
}

void ServingMetrics::set_labels(MetricsLabels labels) {
  std::lock_guard<std::mutex> g(labels_mutex_);
  labels_ = std::move(labels);
}

void ServingMetrics::record_rejection(Error::Code code) noexcept {
  switch (code) {
    case Error::Code::ConfigInvalid:
    case Error::Code::ShapeMismatch:
      increment_rejected_malformed();
      break;
    case Error::Code::Unsupported:
      increment_rejected_unsupported();
      break;
    case Error::Code::OOMError:
      increment_rejected_overload();
      break;
    case Error::Code::Timeout:
      increment_rejected_deadline();
      break;
    case Error::Code::NotReady:
      increment_rejected_stopping();
      break;
    default:
      increment_requests_failed();
      break;
  }
}

void ServingMetrics::record_buffer_accounting(std::size_t in_use_bytes,
                                              std::size_t active_count,
                                              std::size_t high_water_bytes) noexcept {
  buffer_in_use_bytes_.store(in_use_bytes);
  buffer_active_count_.store(active_count);
  buffer_high_water_bytes_.store(high_water_bytes);
}

void ServingMetrics::record_scheduler_accounting(std::size_t queue_depth, std::size_t in_flight,
                                                 std::uint64_t admitted_total,
                                                 std::uint64_t completed_success,
                                                 std::uint64_t completed_failure) noexcept {
  scheduler_queue_depth_.store(queue_depth);
  scheduler_in_flight_.store(in_flight);
  scheduler_admitted_total_.store(admitted_total);
  scheduler_completed_success_.store(completed_success);
  scheduler_completed_failure_.store(completed_failure);
}

ServingMetricsSnapshot ServingMetrics::snapshot() const {
  ServingMetricsSnapshot out;
  {
    std::lock_guard<std::mutex> g(labels_mutex_);
    out.labels = labels_;
  }
  out.requests_total = requests_total_.load();
  out.requests_succeeded = requests_succeeded_.load();
  out.requests_failed = requests_failed_.load();
  out.requests_rejected_malformed = requests_rejected_malformed_.load();
  out.requests_rejected_oversize = requests_rejected_oversize_.load();
  out.requests_rejected_overload = requests_rejected_overload_.load();
  out.requests_rejected_deadline = requests_rejected_deadline_.load();
  out.requests_rejected_unsupported = requests_rejected_unsupported_.load();
  out.requests_cancelled = requests_cancelled_.load();
  out.requests_expired = requests_expired_.load();
  out.async_accepted = async_accepted_.load();
  out.async_completed = async_completed_.load();
  out.async_cancelled = async_cancelled_.load();
  out.async_stale = async_stale_.load();
  out.async_evicted = async_evicted_.load();
  out.shutdown_started = shutdown_started_.load();
  out.shutdown_completed = shutdown_completed_.load();
  out.requests_rejected_stopping = requests_rejected_stopping_.load();
  out.buffer_in_use_bytes = buffer_in_use_bytes_.load();
  out.buffer_active_count = buffer_active_count_.load();
  out.buffer_high_water_bytes = buffer_high_water_bytes_.load();
  out.scheduler_queue_depth = scheduler_queue_depth_.load();
  out.scheduler_in_flight = scheduler_in_flight_.load();
  out.scheduler_admitted_total = scheduler_admitted_total_.load();
  out.scheduler_completed_success = scheduler_completed_success_.load();
  out.scheduler_completed_failure = scheduler_completed_failure_.load();
  out.ingress_latency = ingress_.snapshot();
  out.queue_wait = queue_wait_.snapshot();
  out.execution_latency = execution_.snapshot();
  out.total_latency = total_.snapshot();
  return out;
}

namespace {

std::string render_label_str(const MetricsLabels& labels) {
  std::ostringstream oss;
  oss << "endpoint=\"" << labels.endpoint << "\","
      << "model_class=\"" << labels.model_class << "\","
      << "model_name=\"" << labels.model_name << "\","
      << "backend=\"" << labels.backend << "\"";
  return oss.str();
}

void render_counter(std::ostringstream& oss, std::string_view name, std::string_view help,
                    std::uint64_t value, const std::string& label_str) {
  oss << "# HELP " << name << " " << help << "\n";
  oss << "# TYPE " << name << " counter\n";
  oss << name << "{" << label_str << "} " << value << "\n";
}

void render_gauge(std::ostringstream& oss, std::string_view name, std::string_view help,
                  std::uint64_t value, const std::string& label_str) {
  oss << "# HELP " << name << " " << help << "\n";
  oss << "# TYPE " << name << " gauge\n";
  oss << name << "{" << label_str << "} " << value << "\n";
}

void render_histogram(std::ostringstream& oss, std::string_view name, std::string_view help,
                      const LatencyHistogramSnapshot& h, const std::string& label_str) {
  oss << "# HELP " << name << " " << help << "\n";
  oss << "# TYPE " << name << " histogram\n";
  std::uint64_t cumulative = 0;
  for (std::size_t i = 0; i < kLatencyBucketsMs.size(); ++i) {
    cumulative += h.bucket_counts[i];
    oss << name << "_bucket{" << label_str << ",le=\"" << kLatencyBucketsMs[i] << "\"} "
        << cumulative << "\n";
  }
  cumulative += h.bucket_counts.back();
  oss << name << "_bucket{" << label_str << ",le=\"+Inf\"} " << cumulative << "\n";
  oss << name << "_sum{" << label_str << "} " << h.sum_ms << "\n";
  oss << name << "_count{" << label_str << "} " << h.total_count << "\n";
}

}  // namespace

std::string render_prometheus_text(const ServingMetricsSnapshot& snap) {
  std::ostringstream oss;
  oss << std::fixed << std::setprecision(3);
  const auto labels = render_label_str(snap.labels);

  render_counter(oss, "tensorplate_serving_requests_total", "Requests received by the serving worker.",
                 snap.requests_total, labels);
  render_counter(oss, "tensorplate_serving_requests_succeeded", "Requests that completed successfully.",
                 snap.requests_succeeded, labels);
  render_counter(oss, "tensorplate_serving_requests_failed", "Requests that returned a typed failure.",
                 snap.requests_failed, labels);
  render_counter(oss, "tensorplate_serving_rejected_malformed",
                 "Requests rejected before buffer allocation for malformed payload.",
                 snap.requests_rejected_malformed, labels);
  render_counter(oss, "tensorplate_serving_rejected_oversize",
                 "Requests rejected before buffer allocation for exceeding the size cap.",
                 snap.requests_rejected_oversize, labels);
  render_counter(oss, "tensorplate_serving_rejected_overload",
                 "Requests rejected by scheduler admission due to queue/in-flight overload.",
                 snap.requests_rejected_overload, labels);
  render_counter(oss, "tensorplate_serving_rejected_deadline",
                 "Requests rejected by scheduler admission for deadline infeasibility.",
                 snap.requests_rejected_deadline, labels);
  render_counter(oss, "tensorplate_serving_rejected_unsupported",
                 "Requests rejected for unsupported capability.",
                 snap.requests_rejected_unsupported, labels);
  render_counter(oss, "tensorplate_serving_cancellations_total",
                 "Cancelled requests reaching the scheduler cancellation path.",
                 snap.requests_cancelled, labels);
  render_counter(oss, "tensorplate_serving_expirations_total",
                 "Requests expired by the scheduler.", snap.requests_expired, labels);

  render_counter(oss, "tensorplate_serving_async_accepted", "Async-policy requests accepted.",
                 snap.async_accepted, labels);
  render_counter(oss, "tensorplate_serving_async_completed", "Async-policy requests completed.",
                 snap.async_completed, labels);
  render_counter(oss, "tensorplate_serving_async_cancelled", "Async-policy requests cancelled.",
                 snap.async_cancelled, labels);
  render_counter(oss, "tensorplate_serving_async_stale", "Async-policy requests marked stale.",
                 snap.async_stale, labels);
  render_counter(oss, "tensorplate_serving_async_evicted",
                 "Async-policy entries evicted by retention bounds.", snap.async_evicted, labels);

  render_counter(oss, "tensorplate_serving_shutdowns_started", "Shutdown started events.",
                 snap.shutdown_started, labels);
  render_counter(oss, "tensorplate_serving_shutdowns_completed", "Shutdown completed events.",
                 snap.shutdown_completed, labels);
  render_counter(oss, "tensorplate_serving_rejected_stopping",
                 "Requests rejected because the worker is stopping.",
                 snap.requests_rejected_stopping, labels);

  render_gauge(oss, "tensorplate_serving_buffer_in_use_bytes",
               "Buffer plane in-use bytes at snapshot time.", snap.buffer_in_use_bytes, labels);
  render_gauge(oss, "tensorplate_serving_buffer_active_count",
               "Buffer plane active-buffer count.", snap.buffer_active_count, labels);
  render_gauge(oss, "tensorplate_serving_buffer_high_water_bytes",
               "Buffer plane high-water bytes.", snap.buffer_high_water_bytes, labels);
  render_gauge(oss, "tensorplate_serving_scheduler_queue_depth", "Scheduler queue depth.",
               snap.scheduler_queue_depth, labels);
  render_gauge(oss, "tensorplate_serving_scheduler_in_flight", "Scheduler in-flight count.",
               snap.scheduler_in_flight, labels);
  render_counter(oss, "tensorplate_serving_scheduler_admitted_total",
                 "Scheduler admitted requests since process start.",
                 snap.scheduler_admitted_total, labels);
  render_counter(oss, "tensorplate_serving_scheduler_completed_success",
                 "Scheduler completions with success status.", snap.scheduler_completed_success,
                 labels);
  render_counter(oss, "tensorplate_serving_scheduler_completed_failure",
                 "Scheduler completions with failure status.", snap.scheduler_completed_failure,
                 labels);

  render_histogram(oss, "tensorplate_serving_ingress_latency_ms",
                   "HTTP ingress (parse + decode) latency, ms.", snap.ingress_latency, labels);
  render_histogram(oss, "tensorplate_serving_queue_wait_ms",
                   "Time spent waiting in the scheduler queue, ms.", snap.queue_wait, labels);
  render_histogram(oss, "tensorplate_serving_execution_latency_ms",
                   "Time spent inside the adapter infer call, ms.", snap.execution_latency,
                   labels);
  render_histogram(oss, "tensorplate_serving_total_latency_ms",
                   "End-to-end serving latency, ms.", snap.total_latency, labels);
  return oss.str();
}

std::string render_metrics_json(const ServingMetricsSnapshot& snap) {
  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["labels"] = {
      {"endpoint", snap.labels.endpoint},
      {"model_class", snap.labels.model_class},
      {"model_name", snap.labels.model_name},
      {"backend", snap.labels.backend},
  };
  j["counters"] = {
      {"requests_total", snap.requests_total},
      {"requests_succeeded", snap.requests_succeeded},
      {"requests_failed", snap.requests_failed},
      {"requests_rejected_malformed", snap.requests_rejected_malformed},
      {"requests_rejected_oversize", snap.requests_rejected_oversize},
      {"requests_rejected_overload", snap.requests_rejected_overload},
      {"requests_rejected_deadline", snap.requests_rejected_deadline},
      {"requests_rejected_unsupported", snap.requests_rejected_unsupported},
      {"requests_cancelled", snap.requests_cancelled},
      {"requests_expired", snap.requests_expired},
      {"async_accepted", snap.async_accepted},
      {"async_completed", snap.async_completed},
      {"async_cancelled", snap.async_cancelled},
      {"async_stale", snap.async_stale},
      {"async_evicted", snap.async_evicted},
      {"shutdown_started", snap.shutdown_started},
      {"shutdown_completed", snap.shutdown_completed},
      {"requests_rejected_stopping", snap.requests_rejected_stopping},
      {"scheduler_admitted_total", snap.scheduler_admitted_total},
      {"scheduler_completed_success", snap.scheduler_completed_success},
      {"scheduler_completed_failure", snap.scheduler_completed_failure},
  };
  j["gauges"] = {
      {"buffer_in_use_bytes", snap.buffer_in_use_bytes},
      {"buffer_active_count", snap.buffer_active_count},
      {"buffer_high_water_bytes", snap.buffer_high_water_bytes},
      {"scheduler_queue_depth", snap.scheduler_queue_depth},
      {"scheduler_in_flight", snap.scheduler_in_flight},
  };
  auto histogram_to_json = [](const LatencyHistogramSnapshot& h) {
    nlohmann::json out;
    out["sum_ms"] = h.sum_ms;
    out["count"] = h.total_count;
    nlohmann::json buckets = nlohmann::json::array();
    for (std::size_t i = 0; i < kLatencyBucketsMs.size(); ++i) {
      buckets.push_back({{"le_ms", kLatencyBucketsMs[i]}, {"count", h.bucket_counts[i]}});
    }
    buckets.push_back({{"le_ms", "+Inf"}, {"count", h.bucket_counts.back()}});
    out["buckets"] = std::move(buckets);
    return out;
  };
  j["latency_ms"] = {
      {"ingress", histogram_to_json(snap.ingress_latency)},
      {"queue_wait", histogram_to_json(snap.queue_wait)},
      {"execution", histogram_to_json(snap.execution_latency)},
      {"total", histogram_to_json(snap.total_latency)},
  };
  return j.dump();
}

}  // namespace tensorplate
