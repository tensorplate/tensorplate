// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/config.hpp"

#include <algorithm>
#include <array>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <nlohmann/json.hpp>
#include <stdexcept>
#include <string>
#include <string_view>
#include <utility>

namespace tensorplate {

namespace {

constexpr std::array<std::pair<HealthMode, std::string_view>, 2> kHealthModeNames{{
    {HealthMode::LocalJson, "local_json"},
    {HealthMode::Disabled, "disabled"},
}};

constexpr std::array<std::pair<MetricsMode, std::string_view>, 3> kMetricsModeNames{{
    {MetricsMode::PrometheusText, "prometheus_text"},
    {MetricsMode::Json, "json"},
    {MetricsMode::Disabled, "disabled"},
}};

bool host_is_loopback(std::string_view host) {
  // Accept the canonical loopback literals plus "localhost". v0.1.0
  // does not resolve hostnames at the config layer; clients that need
  // anything else must pre-resolve.
  return host == "127.0.0.1" || host == "::1" || host == "localhost";
}

bool env_allows_non_loopback() {
  // Test-only escape hatch. Required for the V01-E07-F08 harness only
  // when a developer explicitly opts in.
  const char* v = std::getenv("TP_E2E_ALLOW_NON_LOOPBACK");
  return v != nullptr && std::string_view(v) == "1";
}

}  // namespace

std::string_view to_string(HealthMode mode) noexcept {
  for (const auto& [k, v] : kHealthModeNames) {
    if (k == mode) {
      return v;
    }
  }
  return "local_json";
}

std::optional<HealthMode> health_mode_from_string(std::string_view name) noexcept {
  for (const auto& [k, v] : kHealthModeNames) {
    if (v == name) {
      return k;
    }
  }
  return std::nullopt;
}

std::string_view to_string(MetricsMode mode) noexcept {
  for (const auto& [k, v] : kMetricsModeNames) {
    if (k == mode) {
      return v;
    }
  }
  return "prometheus_text";
}

std::optional<MetricsMode> metrics_mode_from_string(std::string_view name) noexcept {
  for (const auto& [k, v] : kMetricsModeNames) {
    if (v == name) {
      return k;
    }
  }
  return std::nullopt;
}

Result<void> ServingConfig::validate() const {
  if (schema_version != "0.1") {
    return unexpected(
        Error::Code::Unsupported,
        std::string{"serving config schema_version not supported: "} + schema_version);
  }
  if (bind.host.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "serving config: bind.host is empty");
  }
  if (!host_is_loopback(bind.host)) {
    if (!bind.allow_non_loopback || !env_allows_non_loopback()) {
      return unexpected(
          Error::Code::Unsupported,
          std::string{"serving config: non-loopback bind not permitted: "} + bind.host);
    }
  }
  if (http.max_body_bytes == 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: http.max_body_bytes must be > 0");
  }
  if (http.max_header_bytes == 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: http.max_header_bytes must be > 0");
  }
  if (http.request_timeout.count() <= 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: http.request_timeout must be > 0 ms");
  }
  if (http.accept_thread_pool_size == 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: http.accept_thread_pool_size must be > 0");
  }
  if (shutdown.drain_deadline.count() < 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: shutdown.drain_deadline must be >= 0");
  }
  if (async_policy.max_completed == 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: async_policy.max_completed must be > 0");
  }
  if (async_policy.max_pending == 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: async_policy.max_pending must be > 0");
  }
  if (async_policy.completed_ttl.count() < 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: async_policy.completed_ttl must be >= 0");
  }
  if (deployment.endpoint.empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: deployment.endpoint must be non-empty");
  }
  if (deployment.backend.empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: deployment.backend must be non-empty");
  }
  if (!deployment.use_mock_session && !deployment.model.has_value()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "serving config: deployment.model required when use_mock_session=false");
  }
  return Result<void>{};
}

namespace {

using json = nlohmann::json;

Result<ModelClass> parse_model_class(std::string_view name) {
  if (auto v = model_class_from_string(name); v.has_value()) {
    return *v;
  }
  return unexpected(Error::Code::Unsupported,
                    std::string{"serving config: unknown model_class "} + std::string(name));
}

Result<PrecisionHint> parse_precision_hint(std::string_view name) {
  if (auto v = precision_hint_from_string(name); v.has_value()) {
    return *v;
  }
  return unexpected(Error::Code::Unsupported,
                    std::string{"serving config: unknown precision_hint "} + std::string(name));
}

template <typename T>
T value_or_default(const json& obj, std::string_view key, T fallback) {
  if (obj.contains(key) && !obj[key].is_null()) {
    return obj[key].get<T>();
  }
  return fallback;
}

}  // namespace

// NOLINTNEXTLINE(readability-function-cognitive-complexity)
Result<ServingConfig> ServingConfig::parse_json(std::string_view text) {
  json root;
  try {
    root = json::parse(text);
  } catch (const json::parse_error& e) {
    return unexpected(Error::Code::ConfigInvalid,
                      std::string{"serving config: JSON parse error: "} + e.what());
  }
  if (!root.is_object()) {
    return unexpected(Error::Code::ConfigInvalid, "serving config: JSON root must be an object");
  }
  ServingConfig cfg;
  try {
    cfg.schema_version = value_or_default<std::string>(root, "schema_version", "0.1");

    if (root.contains("bind") && root["bind"].is_object()) {
      const auto& b = root["bind"];
      cfg.bind.host = value_or_default<std::string>(b, "host", cfg.bind.host);
      cfg.bind.port = static_cast<std::uint16_t>(value_or_default<int>(b, "port", cfg.bind.port));
      cfg.bind.allow_non_loopback =
          value_or_default<bool>(b, "allow_non_loopback", cfg.bind.allow_non_loopback);
    }
    if (root.contains("http") && root["http"].is_object()) {
      const auto& h = root["http"];
      cfg.http.max_body_bytes = static_cast<std::size_t>(
          value_or_default<std::uint64_t>(h, "max_body_bytes", cfg.http.max_body_bytes));
      cfg.http.max_header_bytes = static_cast<std::size_t>(
          value_or_default<std::uint64_t>(h, "max_header_bytes", cfg.http.max_header_bytes));
      cfg.http.request_timeout = std::chrono::milliseconds{value_or_default<std::int64_t>(
          h, "request_timeout_ms", cfg.http.request_timeout.count())};
      cfg.http.accept_thread_pool_size = static_cast<std::size_t>(value_or_default<std::uint64_t>(
          h, "accept_thread_pool_size", cfg.http.accept_thread_pool_size));
    }
    if (root.contains("scheduler") && root["scheduler"].is_object()) {
      const auto& s = root["scheduler"];
      cfg.scheduler.policy = value_or_default<std::string>(s, "policy", cfg.scheduler.policy);
      cfg.scheduler.queue_capacity = static_cast<std::size_t>(
          value_or_default<std::uint64_t>(s, "queue_capacity", cfg.scheduler.queue_capacity));
      cfg.scheduler.in_flight_capacity = static_cast<std::size_t>(value_or_default<std::uint64_t>(
          s, "in_flight_capacity", cfg.scheduler.in_flight_capacity));
      cfg.scheduler.deadline_margin = std::chrono::milliseconds{value_or_default<std::int64_t>(
          s, "deadline_margin_ms", cfg.scheduler.deadline_margin.count())};
      cfg.scheduler.default_service_estimate =
          std::chrono::milliseconds{value_or_default<std::int64_t>(
              s, "default_service_estimate_ms", cfg.scheduler.default_service_estimate.count())};
    }
    if (root.contains("buffer") && root["buffer"].is_object()) {
      const auto& b = root["buffer"];
      cfg.buffer.pool_name = value_or_default<std::string>(b, "pool_name", cfg.buffer.pool_name);
      cfg.buffer.capacity_bytes = static_cast<std::size_t>(
          value_or_default<std::uint64_t>(b, "capacity_bytes", cfg.buffer.capacity_bytes));
      cfg.buffer.max_buffer_bytes = static_cast<std::size_t>(
          value_or_default<std::uint64_t>(b, "max_buffer_bytes", cfg.buffer.max_buffer_bytes));
    }
    if (root.contains("async_policy") && root["async_policy"].is_object()) {
      const auto& a = root["async_policy"];
      cfg.async_policy.max_completed = static_cast<std::size_t>(
          value_or_default<std::uint64_t>(a, "max_completed", cfg.async_policy.max_completed));
      cfg.async_policy.max_pending = static_cast<std::size_t>(
          value_or_default<std::uint64_t>(a, "max_pending", cfg.async_policy.max_pending));
      cfg.async_policy.completed_ttl = std::chrono::milliseconds{value_or_default<std::int64_t>(
          a, "completed_ttl_ms", cfg.async_policy.completed_ttl.count())};
    }
    if (root.contains("shutdown") && root["shutdown"].is_object()) {
      const auto& s = root["shutdown"];
      cfg.shutdown.drain_deadline = std::chrono::milliseconds{value_or_default<std::int64_t>(
          s, "drain_deadline_ms", cfg.shutdown.drain_deadline.count())};
      cfg.shutdown.cancel_queued_immediately = value_or_default<bool>(
          s, "cancel_queued_immediately", cfg.shutdown.cancel_queued_immediately);
    }
    if (root.contains("health_mode") && root["health_mode"].is_string()) {
      auto mode = health_mode_from_string(root["health_mode"].get<std::string>());
      if (!mode.has_value()) {
        return unexpected(Error::Code::Unsupported,
                          std::string{"serving config: unknown health_mode "} +
                              root["health_mode"].get<std::string>());
      }
      cfg.health_mode = *mode;
    }
    if (root.contains("metrics_mode") && root["metrics_mode"].is_string()) {
      auto mode = metrics_mode_from_string(root["metrics_mode"].get<std::string>());
      if (!mode.has_value()) {
        return unexpected(Error::Code::Unsupported,
                          std::string{"serving config: unknown metrics_mode "} +
                              root["metrics_mode"].get<std::string>());
      }
      cfg.metrics_mode = *mode;
    }
    cfg.enable_stderr_logs =
        value_or_default<bool>(root, "enable_stderr_logs", cfg.enable_stderr_logs);

    if (root.contains("deployment") && root["deployment"].is_object()) {
      const auto& d = root["deployment"];
      cfg.deployment.use_mock_session =
          value_or_default<bool>(d, "use_mock_session", cfg.deployment.use_mock_session);
      cfg.deployment.backend = value_or_default<std::string>(d, "backend", cfg.deployment.backend);
      cfg.deployment.endpoint =
          value_or_default<std::string>(d, "endpoint", cfg.deployment.endpoint);
      if (d.contains("model") && d["model"].is_object()) {
        const auto& m = d["model"];
        if (!m.contains("model_id") || !m.contains("model_class") || !m.contains("artifact_path") ||
            !m.contains("backend_hint")) {
          return unexpected(Error::Code::ConfigInvalid,
                            "serving config: deployment.model missing required fields");
        }
        auto mc_r = parse_model_class(m["model_class"].get<std::string>());
        if (!mc_r) {
          return unexpected(mc_r.error());
        }
        PrecisionHint precision = PrecisionHint::Auto;
        if (m.contains("precision_hint") && m["precision_hint"].is_string()) {
          auto p_r = parse_precision_hint(m["precision_hint"].get<std::string>());
          if (!p_r) {
            return unexpected(p_r.error());
          }
          precision = p_r.value();
        }
        std::optional<std::string> profile;
        if (m.contains("profile_id") && m["profile_id"].is_string()) {
          profile = m["profile_id"].get<std::string>();
        }
        auto spec_r = ModelSpec::create(m["model_id"].get<std::string>(), mc_r.value(),
                                        m["artifact_path"].get<std::string>(),
                                        m["backend_hint"].get<std::string>(), precision, profile);
        if (!spec_r) {
          return unexpected(spec_r.error());
        }
        cfg.deployment.model = std::move(spec_r).value();
      }
    }
  } catch (const json::exception& e) {
    return unexpected(Error::Code::ConfigInvalid,
                      std::string{"serving config: JSON decode error: "} + e.what());
  }
  if (auto v = cfg.validate(); !v) {
    return unexpected(v.error());
  }
  return cfg;
}

std::string ServingConfig::to_json() const {
  json root;
  root["schema_version"] = schema_version;
  root["bind"] = {
      {"host", bind.host}, {"port", bind.port}, {"allow_non_loopback", bind.allow_non_loopback}};
  root["http"] = {{"max_body_bytes", http.max_body_bytes},
                  {"max_header_bytes", http.max_header_bytes},
                  {"request_timeout_ms", static_cast<std::int64_t>(http.request_timeout.count())},
                  {"accept_thread_pool_size", http.accept_thread_pool_size}};
  root["scheduler"] = {
      {"policy", scheduler.policy},
      {"queue_capacity", scheduler.queue_capacity},
      {"in_flight_capacity", scheduler.in_flight_capacity},
      {"deadline_margin_ms", static_cast<std::int64_t>(scheduler.deadline_margin.count())},
      {"default_service_estimate_ms",
       static_cast<std::int64_t>(scheduler.default_service_estimate.count())},
  };
  root["buffer"] = {{"pool_name", buffer.pool_name},
                    {"capacity_bytes", buffer.capacity_bytes},
                    {"max_buffer_bytes", buffer.max_buffer_bytes}};
  root["async_policy"] = {
      {"max_completed", async_policy.max_completed},
      {"max_pending", async_policy.max_pending},
      {"completed_ttl_ms", static_cast<std::int64_t>(async_policy.completed_ttl.count())},
  };
  root["shutdown"] = {
      {"drain_deadline_ms", static_cast<std::int64_t>(shutdown.drain_deadline.count())},
      {"cancel_queued_immediately", shutdown.cancel_queued_immediately},
  };
  root["health_mode"] = std::string{to_string(health_mode)};
  root["metrics_mode"] = std::string{to_string(metrics_mode)};
  root["enable_stderr_logs"] = enable_stderr_logs;
  json dep;
  dep["use_mock_session"] = deployment.use_mock_session;
  dep["backend"] = deployment.backend;
  dep["endpoint"] = deployment.endpoint;
  if (deployment.model.has_value()) {
    const auto& m = *deployment.model;
    dep["model"] = {
        {"model_id", m.model_id()},
        {"model_class", std::string{to_string(m.model_class())}},
        {"artifact_path", m.artifact_path()},
        {"backend_hint", m.backend_hint()},
        {"precision_hint", std::string{to_string(m.precision_hint())}},
    };
    if (m.profile_id().has_value()) {
      dep["model"]["profile_id"] = *m.profile_id();
    }
  }
  root["deployment"] = std::move(dep);
  return root.dump();
}

}  // namespace tensorplate
