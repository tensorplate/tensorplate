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

std::string config_with_model(const std::string& extra_model_fields) {
  return R"({"schema_version": "0.1", "deployment": {"use_mock_session": false,
    "endpoint": "stt", "backend": "python_pytorch", "model": {"model_id": "stt",
    "model_class": "speech", "artifact_path": "/bundle/entry.json",
    "backend_hint": "python_pytorch")" +
         extra_model_fields + "}}}";
}

TEST(ServingConfig, RunnerProfileReachesTheModelSpecAndSurvivesARoundtrip) {
  auto r = ServingConfig::parse_json(config_with_model(R"(, "runner_profile": "faster_whisper")"));
  ASSERT_TRUE(r) << r.error().message;
  ASSERT_TRUE(r.value().deployment.model.has_value());
  EXPECT_EQ(r.value().deployment.model->runner_profile(),
            std::optional<std::string>{"faster_whisper"});

  auto again = ServingConfig::parse_json(r.value().to_json());
  ASSERT_TRUE(again) << again.error().message;
  EXPECT_EQ(again.value().deployment.model, r.value().deployment.model);
}

TEST(ServingConfig, ModelWithoutRunnerProfileHasNone) {
  auto r = ServingConfig::parse_json(config_with_model(""));
  ASSERT_TRUE(r) << r.error().message;
  EXPECT_FALSE(r.value().deployment.model->runner_profile().has_value());
  EXPECT_EQ(r.value().to_json().find("runner_profile"), std::string::npos);
}

TEST(ServingConfig, RunnerProfileThatIsNotANonEmptyStringIsRefused) {
  for (const char* value : {"null", "7", "[\"kokoro\"]", "\"\""}) {
    SCOPED_TRACE(value);
    auto r =
        ServingConfig::parse_json(config_with_model(std::string{", \"runner_profile\": "} + value));
    ASSERT_FALSE(r);
    EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
  }
}

TEST(ServingConfig, DeploymentGenerationIsOptionalAndRoundTrips) {
  const auto without = ServingConfig::parse_json(R"({"schema_version":"0.1"})");
  ASSERT_TRUE(without.has_value());
  EXPECT_FALSE(without->deployment.generation.has_value());
  EXPECT_EQ(without->to_json().find("generation"), std::string::npos);

  const auto with = ServingConfig::parse_json(
      R"({"schema_version":"0.1","deployment":{"endpoint":"speech-en","generation":7}})");
  ASSERT_TRUE(with.has_value()) << with.error().message;
  EXPECT_EQ(with->deployment.generation, std::optional<std::uint64_t>{7});
  const auto again = ServingConfig::parse_json(with->to_json());
  ASSERT_TRUE(again.has_value());
  EXPECT_EQ(again->deployment.generation, std::optional<std::uint64_t>{7});

  const auto largest = ServingConfig::parse_json(
      R"({"schema_version":"0.1","deployment":{"generation":9007199254740991}})");
  ASSERT_TRUE(largest.has_value());
  EXPECT_EQ(largest->deployment.generation, std::optional<std::uint64_t>{9007199254740991ULL});
}

TEST(ServingConfig, DeploymentGenerationMustBeAPositiveSafeInteger) {
  for (const char* generation :
       {"0", "-1", "1.5", "\"7\"", "null", "true", "9007199254740992", "[7]"}) {
    SCOPED_TRACE(generation);
    const auto parsed = ServingConfig::parse_json(
        std::string{R"({"schema_version":"0.1","deployment":{"generation":)"} + generation + "}}");
    ASSERT_FALSE(parsed.has_value());
    EXPECT_EQ(parsed.error().code, Error::Code::ConfigInvalid);
  }
  ServingConfig config;
  config.deployment.generation = 0;
  EXPECT_EQ(config.validate().error().code, Error::Code::ConfigInvalid);
  config.deployment.generation = (std::uint64_t{1} << 53U);
  EXPECT_EQ(config.validate().error().code, Error::Code::ConfigInvalid);
  config.deployment.generation = 1;
  EXPECT_TRUE(config.validate().has_value());
}

TEST(ServingConfig, MemberEndpointMustBeADeploymentId) {
  ServingConfig config;
  config.deployment.generation = 7;
  for (const char* endpoint : {"default", "speech-en", "whisper.large_v3-turbo", "A1"}) {
    config.deployment.endpoint = endpoint;
    EXPECT_TRUE(config.validate().has_value()) << endpoint;
  }
  for (const std::string& endpoint :
       {std::string{"bad name"}, std::string{"a/b"}, std::string{"."}, std::string{".."},
        std::string{"caf\xC3\xA9"}, std::string(129, 'a')}) {
    config.deployment.endpoint = endpoint;
    const auto validated = config.validate();
    ASSERT_FALSE(validated.has_value()) << endpoint;
    EXPECT_EQ(validated.error().code, Error::Code::ConfigInvalid);
  }
  // Without a generation the endpoint stays free-form.
  config.deployment.generation.reset();
  config.deployment.endpoint = "bad name";
  EXPECT_TRUE(config.validate().has_value());
  config.deployment.endpoint = std::string(128, 'a');
  config.deployment.generation = 7;
  EXPECT_TRUE(config.validate().has_value());
}

TEST(ServingConfig, HealthAndMetricsModeNames) {
  EXPECT_EQ(to_string(HealthMode::LocalJson), "local_json");
  EXPECT_EQ(to_string(MetricsMode::PrometheusText), "prometheus_text");
  EXPECT_EQ(health_mode_from_string("disabled"), std::optional{HealthMode::Disabled});
  EXPECT_EQ(metrics_mode_from_string("json"), std::optional{MetricsMode::Json});
}

}  // namespace
