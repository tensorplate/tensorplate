// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F01-T01: Serving worker config schema unit tests.

#include <gtest/gtest.h>

#include <cstdint>
#include <cstdlib>
#include <fstream>
#include <nlohmann/json.hpp>
#include <string>

#include "tensorplate/ipc/worker_control.hpp"
#include "tensorplate/serving/config.hpp"

namespace {

using namespace tensorplate;

constexpr const char* kDigest =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

TEST(ServingConfig, DefaultsValidate) {
  ServingConfig cfg;
  EXPECT_TRUE(cfg.validate());
  EXPECT_EQ(cfg.bind.host, "127.0.0.1");
  EXPECT_EQ(cfg.scheduler.policy, "fifo");
  EXPECT_EQ(cfg.metrics_mode, MetricsMode::PrometheusText);
}

nlohmann::json pcm_16k() {
  return {{"encoding", "pcm_s16le"}, {"sample_rate_hz", 16000}, {"channels", 1}};
}

nlohmann::json schema(const char* path) {
  std::ifstream input{std::string{TP_SOURCE_DIR} + path};
  return nlohmann::json::parse(input);
}

StreamingAudioFormat format_of(const nlohmann::json& format) {
  return {format.at("encoding").get<std::string>(),
          format.at("sample_rate_hz").get<std::uint32_t>(),
          format.at("channels").get<std::uint32_t>()};
}

/// A config whose streaming object holds only `formats`, as JSON text.
std::string with_formats(const std::string& formats) {
  return std::string{R"({"schema_version":"0.1","streaming":{"input_audio_formats":)"} + formats +
         "}}";
}

TEST(ServingConfig, StreamingDisabledByDefault) {
  for (const char* text :
       {R"({"schema_version":"0.1"})", R"({"schema_version":"0.1","streaming":{}})",
        R"({"schema_version":"0.1","streaming":{"enabled":false}})"}) {
    SCOPED_TRACE(text);
    const auto parsed = ServingConfig::parse_json(text);
    ASSERT_TRUE(parsed.has_value());
    EXPECT_FALSE(parsed->streaming.enabled);
    EXPECT_EQ(parsed->to_json().find("streaming"), std::string::npos);
  }
  EXPECT_FALSE(ServingConfig{}.streaming.enabled);
}

TEST(ServingConfig, StreamingSchemaDeclaresOptionalFields) {
  std::ifstream input{std::string{TP_SOURCE_DIR} + "/config/schemas/serving_worker.json"};
  ASSERT_TRUE(input.is_open());
  const auto schema = nlohmann::json::parse(input);
  const auto& streaming = schema.at("properties").at("streaming");
  EXPECT_EQ(streaming.at("type"), "object");
  EXPECT_EQ(streaming.at("additionalProperties"), false);
  const auto& enabled = streaming.at("properties").at("enabled");
  EXPECT_EQ(enabled.at("type"), "boolean");
  EXPECT_EQ(enabled.at("default"), ServingConfig{}.streaming.enabled);
  const auto& port = streaming.at("properties").at("port");
  EXPECT_EQ(port.at("type"), "integer");
  EXPECT_EQ(port.at("minimum"), 0);
  EXPECT_EQ(port.at("maximum"), 65535);
  EXPECT_EQ(port.at("default"), ServingConfig{}.streaming.port);
  const auto& max_sessions = streaming.at("properties").at("max_sessions");
  EXPECT_EQ(max_sessions.at("type"), "integer");
  EXPECT_EQ(max_sessions.at("minimum"), 1);
  EXPECT_EQ(max_sessions.at("maximum"), ipc::kWorkerControlMaxSessions);
  EXPECT_EQ(max_sessions.at("default"), ServingConfig{}.streaming.max_sessions);
  const auto& formats = streaming.at("properties").at("input_audio_formats");
  EXPECT_EQ(formats.at("type"), "array");
  EXPECT_EQ(formats.at("uniqueItems"), true);
  EXPECT_EQ(formats.at("contains").at("const"), pcm_16k());
  EXPECT_EQ(formats.at("default"), nlohmann::json::array({pcm_16k()}));
  EXPECT_EQ(streaming.at("properties").size(), 4U);
  EXPECT_FALSE(streaming.contains("required"));
  for (const auto& required : schema.at("required")) {
    EXPECT_NE(required, "streaming");
  }
}

TEST(ServingConfig, SchemaDeclaresTheDescriptorDigestBesideTheGeneration) {
  std::ifstream input{std::string{TP_SOURCE_DIR} + "/config/schemas/serving_worker.json"};
  ASSERT_TRUE(input.is_open());
  const auto deployment = nlohmann::json::parse(input).at("properties").at("deployment");
  const auto& digest = deployment.at("properties").at("descriptor_digest");
  EXPECT_EQ(digest.at("type"), "string");
  EXPECT_EQ(digest.at("pattern"), "^sha256:[0-9a-f]{64}$");
  EXPECT_EQ(deployment.at("dependencies").at("descriptor_digest"),
            nlohmann::json::array({"generation"}));
  EXPECT_FALSE(deployment.contains("required"));
}

TEST(ServingConfig, StreamingRequiresCompiledFeature) {
  ServingConfig config;
  config.streaming.enabled = true;
  const auto validated = config.validate();
  const auto parsed = ServingConfig::parse_json(
      R"({"schema_version":"0.1","streaming":{"enabled":true,"port":50051}})");
#if TP_ENABLE_STREAMING_GRPC
  ASSERT_TRUE(validated.has_value());
  ASSERT_TRUE(parsed.has_value());
  EXPECT_TRUE(parsed->streaming.enabled);
  EXPECT_EQ(parsed->streaming.port, 50051);
  const auto again = ServingConfig::parse_json(parsed->to_json());
  ASSERT_TRUE(again.has_value());
  EXPECT_TRUE(again->streaming.enabled);
  EXPECT_EQ(again->streaming.port, 50051);
#else
  ASSERT_FALSE(validated.has_value());
  EXPECT_EQ(validated.error().code, Error::Code::Unsupported);
  ASSERT_FALSE(parsed.has_value());
  EXPECT_EQ(parsed.error().code, Error::Code::Unsupported);
  EXPECT_NE(parsed.error().message.find("TP_ENABLE_STREAMING_GRPC"), std::string::npos);
#endif
}

TEST(ServingConfig, StreamingRejectsMalformedConfiguration) {
  for (const char* value :
       {"null", "true", "[]", "7", R"("on")", R"({"enabled":null})", R"({"enabled":1})",
        R"({"enabled":"true"})", R"({"enabled":[]})", R"({"enable":true})",
        R"({"enabled":false,"extra":1})", R"({"port":-1})", R"({"port":65536})", R"({"port":"1"})",
        R"({"port":1.5})", R"({"port":null})", R"({"port":true})"}) {
    SCOPED_TRACE(value);
    const auto parsed = ServingConfig::parse_json(
        std::string{R"({"schema_version":"0.1","streaming":)"} + value + "}");
    ASSERT_FALSE(parsed.has_value());
    EXPECT_EQ(parsed.error().code, Error::Code::ConfigInvalid);
    EXPECT_NE(parsed.error().message.find("streaming accepts only"), std::string::npos);
  }
}

TEST(ServingConfig, StreamingRejectsAMalformedSessionCount) {
  for (const char* value : {"0", "2049", "-1", R"("2")", "1.5", "null", "true"}) {
    SCOPED_TRACE(value);
    const auto parsed = ServingConfig::parse_json(
        std::string{R"({"schema_version":"0.1","streaming":{"max_sessions":)"} + value + "}}");
    ASSERT_FALSE(parsed.has_value());
    EXPECT_EQ(parsed.error().code, Error::Code::ConfigInvalid);
    EXPECT_NE(parsed.error().message.find("streaming accepts only"), std::string::npos);
  }
}

TEST(ServingConfig, StreamingPortAloneRoundTripsInEitherBuild) {
  const auto parsed =
      ServingConfig::parse_json(R"({"schema_version":"0.1","streaming":{"port":65535}})");
  ASSERT_TRUE(parsed.has_value());
  EXPECT_FALSE(parsed->streaming.enabled);
  EXPECT_EQ(parsed->streaming.port, 65535);
  EXPECT_EQ(parsed->to_json().find("input_audio_formats"), std::string::npos);
  const auto again = ServingConfig::parse_json(parsed->to_json());
  ASSERT_TRUE(again.has_value());
  EXPECT_EQ(again->streaming.port, 65535);
  EXPECT_FALSE(again->streaming.enabled);
}

TEST(ServingConfig, StreamingServesOneSessionByDefault) {
  EXPECT_EQ(ServingConfig{}.streaming.max_sessions, 1U);
  const auto parsed = ServingConfig::parse_json(R"({"schema_version":"0.1","streaming":{}})");
  ASSERT_TRUE(parsed.has_value());
  EXPECT_EQ(parsed->streaming.max_sessions, 1U);
}

// The schema's bound is the control channel's; this worker refuses more than
// 64 in either build, with streaming on or off.
TEST(ServingConfig, StreamingMaxSessionsIsAtMostSixtyFourInThisWorker) {
  const auto with = [](std::uint32_t count) {
    return std::string{R"({"schema_version":"0.1","streaming":{"max_sessions":)"} +
           std::to_string(count) + "}}";
  };
  const auto most = ServingConfig::parse_json(with(64));
  ASSERT_TRUE(most.has_value()) << most.error().message;
  EXPECT_EQ(most->streaming.max_sessions, 64U);
  const auto again = ServingConfig::parse_json(most->to_json());
  ASSERT_TRUE(again.has_value());
  EXPECT_EQ(again->streaming.max_sessions, 64U);
  EXPECT_FALSE(again->streaming.enabled);

  for (const std::uint32_t count : {65U, ipc::kWorkerControlMaxSessions}) {
    SCOPED_TRACE(count);
    const auto parsed = ServingConfig::parse_json(with(count));
    ASSERT_FALSE(parsed.has_value());
    EXPECT_EQ(parsed.error().code, Error::Code::ConfigInvalid);
    EXPECT_NE(parsed.error().message.find("streaming.max_sessions must be from 1 to 64"),
              std::string::npos)
        << parsed.error().message;
  }

  for (const std::uint32_t count : {0U, 65U}) {
    SCOPED_TRACE(count);
    ServingConfig config;
    config.streaming.max_sessions = count;
    const auto refused = config.validate();
    ASSERT_FALSE(refused.has_value());
    EXPECT_EQ(refused.error().code, Error::Code::ConfigInvalid);
  }
  ServingConfig two;
  two.streaming.max_sessions = 2;
  EXPECT_TRUE(two.validate().has_value());
}

TEST(ServingConfig, StreamingRefusesAMemberWithoutADigestABadDeploymentIdAndTheHttpPort) {
  ServingConfig with_generation;
  with_generation.streaming.enabled = true;
  with_generation.deployment.endpoint = "dep-a";
  with_generation.deployment.generation = 3;
  const auto refused = with_generation.validate();
  ASSERT_FALSE(refused.has_value());
#if TP_ENABLE_STREAMING_GRPC
  EXPECT_EQ(refused.error().code, Error::Code::ConfigInvalid);
  EXPECT_NE(refused.error().message.find("deployment.descriptor_digest"), std::string::npos);
  with_generation.deployment.descriptor_digest = kDigest;
  EXPECT_TRUE(with_generation.validate().has_value());
#else
  EXPECT_EQ(refused.error().code, Error::Code::Unsupported);
#endif
  with_generation.streaming.enabled = false;
  with_generation.deployment.descriptor_digest.reset();
  EXPECT_TRUE(with_generation.validate().has_value());

  ServingConfig malformed;
  malformed.streaming.enabled = true;
  malformed.deployment.endpoint = "dep a";
  const auto invalid = malformed.validate();
  ASSERT_FALSE(invalid.has_value());
#if TP_ENABLE_STREAMING_GRPC
  EXPECT_EQ(invalid.error().code, Error::Code::ConfigInvalid);
  EXPECT_NE(invalid.error().message.find("deployment.endpoint"), std::string::npos);
#else
  EXPECT_EQ(invalid.error().code, Error::Code::Unsupported);
#endif
  malformed.streaming.enabled = false;
  EXPECT_TRUE(malformed.validate().has_value());

  ServingConfig shared_port;
  shared_port.streaming.enabled = true;
  shared_port.bind.port = 18080;
  shared_port.streaming.port = 18080;
  const auto clash = shared_port.validate();
  ASSERT_FALSE(clash.has_value());
#if TP_ENABLE_STREAMING_GRPC
  EXPECT_EQ(clash.error().code, Error::Code::ConfigInvalid);
  shared_port.streaming.port = 18081;
  EXPECT_TRUE(shared_port.validate().has_value());
#endif
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

TEST(ServingConfig, DeploymentDescriptorDigestIsOptionalAndRoundTrips) {
  const auto without = ServingConfig::parse_json(
      R"({"schema_version":"0.1","deployment":{"endpoint":"speech-en","generation":7}})");
  ASSERT_TRUE(without.has_value()) << without.error().message;
  EXPECT_FALSE(without->deployment.descriptor_digest.has_value());
  EXPECT_EQ(without->to_json().find("descriptor_digest"), std::string::npos);

  const auto with = ServingConfig::parse_json(
      std::string{R"({"schema_version":"0.1","deployment":{"endpoint":"speech-en","generation":7,)"
                  R"("descriptor_digest":")"} +
      kDigest + R"("}})");
  ASSERT_TRUE(with.has_value()) << with.error().message;
  EXPECT_EQ(with->deployment.descriptor_digest, std::optional<std::string>{kDigest});
  const auto again = ServingConfig::parse_json(with->to_json());
  ASSERT_TRUE(again.has_value());
  EXPECT_EQ(again->deployment.descriptor_digest, std::optional<std::string>{kDigest});
}

TEST(ServingConfig, DeploymentDescriptorDigestIsASha256DigestBesideAGeneration) {
  const std::string hex(64, 'a');
  for (const std::string& digest :
       {std::string{"null"}, std::string{"7"}, std::string{"[]"},
        "\"sha256:" + hex.substr(1) + "\"", "\"sha256:" + hex + "0\"",
        "\"sha256:" + std::string(64, 'A') + "\"", "\"sha256:" + hex.substr(1) + "g\"",
        "\"sha512:" + hex + "\"", "\"sha256-" + hex + "\"", "\"" + hex + "\"",
        "\"sha256:" + hex.substr(1) + "/\"", "\"sha256:" + hex.substr(1) + ":\"",
        "\"sha256:" + hex.substr(1) + "`\"", std::string{R"("")"}}) {
    SCOPED_TRACE(digest);
    const auto parsed = ServingConfig::parse_json(
        std::string{
            R"({"schema_version":"0.1","deployment":{"generation":7,"descriptor_digest":)"} +
        digest + "}}");
    ASSERT_FALSE(parsed.has_value());
    EXPECT_EQ(parsed.error().code, Error::Code::ConfigInvalid);
    EXPECT_NE(parsed.error().message.find("deployment.descriptor_digest"), std::string::npos);
  }
  const auto without_generation = ServingConfig::parse_json(
      std::string{R"({"schema_version":"0.1","deployment":{"descriptor_digest":")"} + kDigest +
      R"("}})");
  ASSERT_FALSE(without_generation.has_value());
  EXPECT_EQ(without_generation.error().code, Error::Code::ConfigInvalid);
  EXPECT_NE(without_generation.error().message.find("requires deployment.generation"),
            std::string::npos);

  ServingConfig config;
  config.deployment.descriptor_digest = kDigest;
  EXPECT_EQ(config.validate().error().code, Error::Code::ConfigInvalid);
  config.deployment.generation = 7;
  EXPECT_TRUE(config.validate().has_value());
  config.deployment.descriptor_digest = "sha256:" + std::string(54, '0') + "0123456789";
  EXPECT_TRUE(config.validate().has_value());
  config.deployment.descriptor_digest = "sha256:" + std::string(58, 'f') + "abcdef";
  EXPECT_TRUE(config.validate().has_value());
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

// One spelling everywhere: the formats the worker's schema lists are the ones
// a bundle's speech block may declare, the default is 16 kHz PCM, and
// validate() takes each of them.
TEST(ServingConfig, InputAudioFormatsAreTheOnesABundleMayDeclare) {
  const auto worker = schema("/config/schemas/serving_worker.json");
  const auto& listed = worker.at("properties")
                           .at("streaming")
                           .at("properties")
                           .at("input_audio_formats")
                           .at("items")
                           .at("enum");
  const auto manifest = schema("/protocol/schemas/bundle_manifest.json");
  EXPECT_EQ(listed, manifest.at("definitions").at("stt_input_audio_format").at("enum"));
  ASSERT_EQ(listed.size(), 2U);
  EXPECT_EQ(ServingConfig{}.streaming.input_audio_formats,
            std::vector<StreamingAudioFormat>{format_of(pcm_16k())});
  ServingConfig config;
  config.streaming.input_audio_formats.clear();
  for (const auto& format : listed) {
    config.streaming.input_audio_formats.push_back(format_of(format));
  }
  const auto validated = config.validate();
  EXPECT_TRUE(validated.has_value()) << validated.error().message;
}

TEST(ServingConfig, InputAudioFormatsRoundTripInEitherBuild) {
  const std::string mulaw = R"({"encoding":"mulaw","sample_rate_hz":8000,"channels":1})";
  const auto parsed =
      ServingConfig::parse_json(with_formats("[" + pcm_16k().dump() + "," + mulaw + "]"));
  ASSERT_TRUE(parsed.has_value()) << parsed.error().message;
  const std::vector<StreamingAudioFormat> both{{"pcm_s16le", 16000, 1}, {"mulaw", 8000, 1}};
  EXPECT_EQ(parsed->streaming.input_audio_formats, both);
  EXPECT_FALSE(parsed->streaming.enabled);
  const auto again = ServingConfig::parse_json(parsed->to_json());
  ASSERT_TRUE(again.has_value()) << again.error().message;
  EXPECT_EQ(again->streaming.input_audio_formats, both);

  const auto alone = ServingConfig::parse_json(with_formats("[" + pcm_16k().dump() + "]"));
  ASSERT_TRUE(alone.has_value()) << alone.error().message;
  EXPECT_EQ(alone->streaming.input_audio_formats, ServingConfig{}.streaming.input_audio_formats);
  EXPECT_EQ(alone->to_json().find("input_audio_formats"), std::string::npos);
}

TEST(ServingConfig, InputAudioFormatsRefuseWhatTheDispatchCannotTake) {
  const std::string pcm = pcm_16k().dump();
  const std::string mulaw = R"({"encoding":"mulaw","sample_rate_hz":8000,"channels":1})";
  const struct {
    std::string formats;
    const char* refusal;
  } cases[] = {
      {"[" + pcm + R"(,{"encoding":"mulaw","sample_rate_hz":16000,"channels":1}])", "takes only"},
      {"[" + pcm + R"(,{"encoding":"pcm_s16le","sample_rate_hz":8000,"channels":1}])",
       "takes only"},
      {"[" + pcm + R"(,{"encoding":"mulaw","sample_rate_hz":8000,"channels":2}])", "takes only"},
      {"[" + pcm + R"(,{"encoding":"pcm_s16le","sample_rate_hz":16000,"channels":2}])",
       "takes only"},
      {"[" + pcm + R"(,{"encoding":"MULAW","sample_rate_hz":8000,"channels":1}])", "takes only"},
      {"[" + pcm + R"(,{"encoding":"alaw","sample_rate_hz":8000,"channels":1}])", "takes only"},
      {"[]", "must hold mono pcm_s16le"},
      {"[" + mulaw + "]", "must hold mono pcm_s16le"},
      {"[" + pcm + "," + pcm + "]", "lists a format twice"},
      {"[" + pcm + "," + mulaw + "," + mulaw + "]", "lists a format twice"},
      {"[" + pcm + "," + mulaw + "," + pcm + "]", "lists a format twice"},
  };
  for (const auto& [formats, refusal] : cases) {
    SCOPED_TRACE(formats);
    const auto parsed = ServingConfig::parse_json(with_formats(formats));
    ASSERT_FALSE(parsed.has_value());
    EXPECT_EQ(parsed.error().code, Error::Code::ConfigInvalid);
    EXPECT_NE(parsed.error().message.find(refusal), std::string::npos) << parsed.error().message;
  }
}

TEST(ServingConfig, InputAudioFormatsRefuseAMalformedList) {
  for (const char* formats :
       {"null", "{}", R"("pcm_s16le")", "[null]", R"(["pcm_s16le"])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":16000}])",
        R"([{"encoding":"pcm_s16le","channels":1,"rate":16000}])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":16000,"channel":1}])",
        R"([{"codec":"pcm_s16le","sample_rate_hz":16000,"channels":1}])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":16000,"channels":1,"extra":0}])",
        R"([{"encoding":1,"sample_rate_hz":16000,"channels":1}])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":"16000","channels":1}])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":16000.0,"channels":1}])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":-16000,"channels":1}])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":4294983296,"channels":1}])",
        R"([{"encoding":"pcm_s16le","sample_rate_hz":16000,"channels":4294967297}])"}) {
    SCOPED_TRACE(formats);
    const auto parsed = ServingConfig::parse_json(with_formats(formats));
    ASSERT_FALSE(parsed.has_value());
    EXPECT_EQ(parsed.error().code, Error::Code::ConfigInvalid);
    EXPECT_NE(parsed.error().message.find("input_audio_formats must be an array"),
              std::string::npos)
        << parsed.error().message;
  }
}

}  // namespace
