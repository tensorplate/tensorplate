// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F06: Health and metrics unit tests.

#include <gtest/gtest.h>

#include <nlohmann/json.hpp>
#include <string>

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
