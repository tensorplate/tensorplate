// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F01-T01: Serving worker config schema unit tests.

#include <gtest/gtest.h>

#include <cstdlib>

#include "tensorplate/serving/config.hpp"

namespace {

using namespace tensorplate;

TEST(ServingConfig, DefaultsValidate) {
  ServingConfig cfg;
  EXPECT_TRUE(cfg.validate());
  EXPECT_EQ(cfg.bind.host, "127.0.0.1");
  EXPECT_EQ(cfg.scheduler.policy, "fifo");
  EXPECT_EQ(cfg.metrics_mode, MetricsMode::PrometheusText);
}

TEST(ServingConfig, AcceptsIPv6LoopbackLiteral) {
  // Issue #22: "::1" is a documented loopback literal. The validator and
  // the HTTP listener must agree that it is bindable; this pins the
  // config-layer half of that contract.
  ServingConfig cfg;
  cfg.bind.host = "::1";
  EXPECT_TRUE(cfg.validate());
}

TEST(ServingConfig, RejectsNonLoopbackByDefault) {
  ServingConfig cfg;
  cfg.bind.host = "0.0.0.0";
  auto r = cfg.validate();
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

TEST(ServingConfig, RejectsEmptyEndpoint) {
  ServingConfig cfg;
  cfg.deployment.endpoint.clear();
  auto r = cfg.validate();
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingConfig, RejectsZeroBodyLimit) {
  ServingConfig cfg;
  cfg.http.max_body_bytes = 0;
  auto r = cfg.validate();
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingConfig, RejectsMissingModelForRealBackend) {
  ServingConfig cfg;
  cfg.deployment.use_mock_session = false;
  cfg.deployment.backend = "tensorrt";
  // No model set.
  auto r = cfg.validate();
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingConfig, ParseJsonValidates) {
  const std::string json = R"({
    "schema_version": "0.1",
    "bind": {"host": "127.0.0.1", "port": 0},
    "http": {"max_body_bytes": 1024, "max_header_bytes": 256, "request_timeout_ms": 1000},
    "deployment": {"use_mock_session": true, "endpoint": "default", "backend": "mock"}
  })";
  auto r = ServingConfig::parse_json(json);
  ASSERT_TRUE(r);
  auto cfg = std::move(r).value();
  EXPECT_EQ(cfg.http.max_body_bytes, 1024U);
  EXPECT_EQ(cfg.deployment.endpoint, "default");
}

TEST(ServingConfig, ParseJsonRejectsUnknownSchemaVersion) {
  const std::string json = R"({"schema_version": "9.9"})";
  auto r = ServingConfig::parse_json(json);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

TEST(ServingConfig, ParseJsonRejectsMalformed) {
  auto r = ServingConfig::parse_json("not json");
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingConfig, ToJsonRoundtrip) {
  ServingConfig cfg;
  cfg.bind.port = 5555;
  cfg.async_policy.max_pending = 7;
  const auto text = cfg.to_json();
  auto re = ServingConfig::parse_json(text);
  ASSERT_TRUE(re);
  EXPECT_EQ(re.value().bind.port, 5555);
  EXPECT_EQ(re.value().async_policy.max_pending, 7U);
}

TEST(ServingConfig, HealthAndMetricsModeNames) {
  EXPECT_EQ(to_string(HealthMode::LocalJson), "local_json");
  EXPECT_EQ(to_string(MetricsMode::PrometheusText), "prometheus_text");
  EXPECT_EQ(health_mode_from_string("disabled"), std::optional{HealthMode::Disabled});
  EXPECT_EQ(metrics_mode_from_string("json"), std::optional{MetricsMode::Json});
}

}  // namespace
