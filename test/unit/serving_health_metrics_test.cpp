// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F06: Health and metrics unit tests.

#include <gtest/gtest.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/scheduler/scheduler.hpp"
#include "tensorplate/serving/health.hpp"
#include "tensorplate/serving/metrics.hpp"

namespace {

using namespace tensorplate;

TEST(HealthState, StateTransitions) {
  HealthState h;
  h.set_identity("default", "mock", std::optional<std::string>{"m1"});
  h.set_state(ServingState::Starting);
  EXPECT_EQ(h.state(), ServingState::Starting);
  h.set_state(ServingState::Ready);
  EXPECT_EQ(h.state(), ServingState::Ready);
  h.record_error(Error{Error::Code::OOMError, "overload", std::nullopt});
  const auto snap = h.snapshot();
  ASSERT_TRUE(snap.last_error_code.has_value());
  EXPECT_EQ(*snap.last_error_code, Error::Code::OOMError);
  EXPECT_EQ(snap.last_error_message, "overload");
}

TEST(HealthState, HttpStatusMapping) {
  EXPECT_EQ(health_http_status(ServingState::Ready), 200);
  EXPECT_EQ(health_http_status(ServingState::Degraded), 200);
  EXPECT_EQ(health_http_status(ServingState::Failed), 503);
  EXPECT_EQ(health_http_status(ServingState::Stopping), 503);
}

TEST(HealthState, JsonSerialization) {
  HealthState h;
  h.set_identity("default", "mock", std::optional<std::string>{"m1"});
  h.set_state(ServingState::Ready);
  const auto body = serialize_health_json(h.snapshot());
  auto j = nlohmann::json::parse(body);
  EXPECT_EQ(j["schema_version"], "0.1");
  EXPECT_EQ(j["state"], "ready");
  EXPECT_EQ(j["endpoint"], "default");
  EXPECT_EQ(j["backend"], "mock");
  EXPECT_EQ(j["active_model_id"], "m1");
}

TEST(ServingMetrics, RecordsRejectionsByCode) {
  ServingMetrics m;
  MetricsLabels labels{"e", "vision", "model", "mock"};
  m.set_labels(labels);
  m.record_rejection(Error::Code::OOMError);
  m.record_rejection(Error::Code::Timeout);
  m.record_rejection(Error::Code::Unsupported);
  m.record_rejection(Error::Code::ShapeMismatch);
  const auto s = m.snapshot();
  EXPECT_EQ(s.requests_rejected_overload, 1U);
  EXPECT_EQ(s.requests_rejected_deadline, 1U);
  EXPECT_EQ(s.requests_rejected_unsupported, 1U);
  EXPECT_EQ(s.requests_rejected_malformed, 1U);
}

TEST(ServingMetrics, RecordsAppendedCodesInTheirCounters) {
  ServingMetrics m;
  m.record_rejection(Error::Code::ResourceExhausted);
  auto s = m.snapshot();
  EXPECT_EQ(s.requests_rejected_overload, 1U);
  EXPECT_EQ(s.requests_failed, 0U);

  m.record_rejection(Error::Code::Unavailable);
  s = m.snapshot();
  EXPECT_EQ(s.requests_failed, 1U);
  EXPECT_EQ(s.requests_rejected_stopping, 0U);

  // A cancelled request is a typed failure here; requests_cancelled is fed
  // by the scheduler's Cancelled event, so this must not touch it.
  m.record_rejection(Error::Code::Cancelled);
  s = m.snapshot();
  EXPECT_EQ(s.requests_failed, 2U);
  EXPECT_EQ(s.requests_cancelled, 0U);
}

TEST(ServingMetrics, LatencyHistogramAndPrometheusRender) {
  ServingMetrics m;
  MetricsLabels labels{"e", "vision", "model", "mock"};
  m.set_labels(labels);
  m.observe_total_ms(0.5);
  m.observe_total_ms(5.0);
  m.observe_total_ms(100.0);
  const auto s = m.snapshot();
  EXPECT_EQ(s.total_latency.total_count, 3U);
  const auto txt = render_prometheus_text(s);
  EXPECT_NE(txt.find("tensorplate_serving_total_latency_ms_bucket"), std::string::npos);
  EXPECT_NE(txt.find("endpoint=\"e\""), std::string::npos);
  EXPECT_NE(txt.find("backend=\"mock\""), std::string::npos);
}

SchedulerMetrics distinct_scheduler_snapshot() {
  SchedulerMetrics m;
  m.queue_depth = 4;
  m.in_flight = 1;
  m.in_flight_logical = 1;
  m.in_flight_physical = 3;
  m.in_flight_physical_cancelled = 2;
  m.admitted_total = 9;
  m.completed_success = 5;
  m.completed_failure = 6;
  return m;
}

TEST(ServingMetrics, RecordsTheSchedulerSnapshot) {
  ServingMetrics m;
  m.record_scheduler_accounting(distinct_scheduler_snapshot());
  const auto s = m.snapshot();
  EXPECT_EQ(s.scheduler_queue_depth, 4U);
  EXPECT_EQ(s.scheduler_in_flight, 1U);
  EXPECT_EQ(s.scheduler_in_flight_physical, std::optional<std::uint64_t>{3});
  EXPECT_EQ(s.scheduler_in_flight_physical_cancelled, std::optional<std::uint64_t>{2});
  EXPECT_EQ(s.scheduler_admitted_total, 9U);
  EXPECT_EQ(s.scheduler_completed_success, 5U);
  EXPECT_EQ(s.scheduler_completed_failure, 6U);
}

TEST(ServingMetrics, FiveValueCaptureOmitsThePhysicalGauges) {
  ServingMetrics m;
  EXPECT_FALSE(m.snapshot().scheduler_in_flight_physical.has_value());

  m.record_scheduler_accounting(distinct_scheduler_snapshot());
  m.record_scheduler_accounting(/*queue_depth=*/0, /*in_flight=*/1, /*admitted_total=*/1,
                                /*completed_success=*/0, /*completed_failure=*/0);
  const auto snap = m.snapshot();
  EXPECT_EQ(snap.scheduler_in_flight, 1U);
  EXPECT_FALSE(snap.scheduler_in_flight_physical.has_value());
  EXPECT_FALSE(snap.scheduler_in_flight_physical_cancelled.has_value());

  const auto gauges = nlohmann::json::parse(render_metrics_json(snap)).at("gauges");
  EXPECT_EQ(gauges.at("scheduler_in_flight"), 1);
  EXPECT_EQ(gauges.at("scheduler_in_flight_logical"), 1);
  EXPECT_FALSE(gauges.contains("scheduler_in_flight_physical"));
  EXPECT_FALSE(gauges.contains("scheduler_in_flight_physical_cancelled"));
  const auto txt = render_prometheus_text(snap);
  EXPECT_NE(txt.find("tensorplate_serving_scheduler_in_flight_logical{"), std::string::npos);
  EXPECT_EQ(txt.find("scheduler_in_flight_physical"), std::string::npos);

  m.record_scheduler_accounting(distinct_scheduler_snapshot());
  EXPECT_EQ(m.snapshot().scheduler_in_flight_physical, std::optional<std::uint64_t>{3});
}

TEST(ServingMetrics, PrometheusRendersEveryInFlightGauge) {
  ServingMetrics m;
  m.set_labels(MetricsLabels{"e", "vision", "model", "mock"});
  m.record_scheduler_accounting(distinct_scheduler_snapshot());
  const auto txt = render_prometheus_text(m.snapshot());
  const std::string labels =
      "{endpoint=\"e\",model_class=\"vision\",model_name=\"model\",backend=\"mock\"}";
  for (const auto& [name, value] : std::vector<std::pair<std::string, std::string>>{
           {"tensorplate_serving_scheduler_in_flight", "1"},
           {"tensorplate_serving_scheduler_in_flight_logical", "1"},
           {"tensorplate_serving_scheduler_in_flight_physical", "3"},
           {"tensorplate_serving_scheduler_in_flight_physical_cancelled", "2"}}) {
    EXPECT_NE(txt.find("\n" + name + labels + " " + value + "\n"), std::string::npos) << name;
    EXPECT_NE(txt.find("# TYPE " + name + " gauge\n"), std::string::npos) << name;
  }
  // The existing gauge keeps its help text.
  EXPECT_NE(txt.find("# HELP tensorplate_serving_scheduler_in_flight Scheduler in-flight count.\n"),
            std::string::npos);
}

TEST(ServingMetrics, OneSnapshotNeverMixesTwoSchedulerCaptures) {
  // The worker records a capture from its scheduler event sink and from
  // /metrics at the same time. A snapshot must take all scheduler fields from
  // one capture, or it can show fewer physical than logical plus cancelled.
  ServingMetrics metrics;
  SchedulerMetrics busy;
  busy.in_flight = 1;
  busy.in_flight_logical = 1;
  busy.in_flight_physical = 1;
  SchedulerMetrics held;
  held.in_flight_physical = 1;
  held.in_flight_physical_cancelled = 1;
  const SchedulerMetrics idle;
  std::atomic<bool> stop{false};
  std::thread writer([&] {
    while (!stop.load()) {
      metrics.record_scheduler_accounting(busy);
      metrics.record_scheduler_accounting(held);
      metrics.record_scheduler_accounting(idle);
    }
  });
  std::size_t mixed = 0;
  for (int i = 0; i < 200000; ++i) {
    const auto snap = metrics.snapshot();
    const std::uint64_t physical = snap.scheduler_in_flight_physical.value_or(0);
    const std::uint64_t cancelled = snap.scheduler_in_flight_physical_cancelled.value_or(0);
    if (physical < snap.scheduler_in_flight + cancelled) {
      ++mixed;
    }
  }
  stop.store(true);
  writer.join();
  EXPECT_EQ(mixed, 0U);
}

TEST(ServingMetrics, JsonRenderHasStableShape) {
  ServingMetrics m;
  MetricsLabels labels{"e", "vision", "model", "mock"};
  m.set_labels(labels);
  m.increment_requests_total();
  const auto j = nlohmann::json::parse(render_metrics_json(m.snapshot()));
  EXPECT_TRUE(j.contains("counters"));
  EXPECT_TRUE(j.contains("gauges"));
  EXPECT_TRUE(j.contains("latency_ms"));
  EXPECT_TRUE(j["latency_ms"].contains("total"));
}

}  // namespace
