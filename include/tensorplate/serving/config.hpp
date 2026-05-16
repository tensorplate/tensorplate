// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F01: Public serving-worker runtime configuration.
//
// ServingConfig is consumed by `tensorplate-serving`'s composition root
// (V01-E07-F01-T02) and by the loopback HTTP server (V01-E07-F02). It
// carries the bind address, request limits, scheduler policy, buffer
// limits, the active deployment / session reference, metrics mode,
// health mode, and shutdown policy.
//
// Defaults are conservative: loopback binding, 16 MiB request cap, 8 s
// request timeout, 32 KiB header cap, FIFO scheduler with deadline-
// aware admission, and a 5 s drain on shutdown. Production callers
// should still validate against `protocol/schemas/
// serving_worker_config.json`.
//
// The config does **not** carry hardware-specific knobs (no CUDA
// device id, no TensorRT engine paths). Those belong to the bundle
// (V01-E13) and are resolved by the agent (V01-E08) before this
// process is started.

#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace tensorplate {

/// Active-deployment selector for the serving worker. v0.1.0 supports
/// two modes:
///
///   - "mock" sessions, used by host CI and by the V01-E07-F08 test
///     harness. A `mock_outputs` descriptor selects how the mock
///     adapter responds.
///   - real-backend sessions, where `model` and `backend` are honored
///     and the registered adapter for `backend` is instantiated.
struct ActiveDeploymentConfig {
  /// Mock mode discriminator. When true, the composition root selects
  /// the in-process `MockServingSession` instead of a registered
  /// backend. The mock is intentionally lightweight; it produces a
  /// fixed-shape successful result so the request/router/scheduler
  /// path can be exercised end-to-end without TensorRT, LibTorch, or
  /// the Python sidecar.
  bool use_mock_session = true;

  /// Optional model spec. Required when `use_mock_session = false`.
  /// The composition root forwards this to `ExecutionSession::load`.
  std::optional<ModelSpec> model;

  /// Backend label used when `use_mock_session = false`. Must match a
  /// registered backend in `BackendRegistry`.
  std::string backend = "mock";

  /// Endpoint name surfaced by `/health` and used by /infer routing.
  /// Defaults to "default".
  std::string endpoint = "default";
};

/// Health response mode. v0.1.0 supports a single "local_json"
/// representation; agent / observability service consumers parse the
/// same JSON shape as the HTTP `/health` body.
enum class HealthMode : std::uint8_t {
  /// Default. Local JSON response served from `/health`.
  LocalJson = 0,
  /// Disabled. `/health` returns 404. Test-only.
  Disabled = 1,
};

[[nodiscard]] std::string_view to_string(HealthMode mode) noexcept;
[[nodiscard]] std::optional<HealthMode> health_mode_from_string(std::string_view name) noexcept;

/// Metrics-export mode. v0.1.0 ships a Prometheus text-format
/// exporter on `/metrics`. The structured-JSON mode is reserved for
/// agent-side consumers that prefer machine-friendly bodies and is
/// not the default.
enum class MetricsMode : std::uint8_t {
  /// Prometheus text format on `/metrics` (default).
  PrometheusText = 0,
  /// Structured JSON on `/metrics`. Useful for agent-side scraping.
  Json = 1,
  /// Disabled. `/metrics` returns 404. Test-only.
  Disabled = 2,
};

[[nodiscard]] std::string_view to_string(MetricsMode mode) noexcept;
[[nodiscard]] std::optional<MetricsMode> metrics_mode_from_string(std::string_view name) noexcept;

/// HTTP request-handling limits enforced before buffer allocation.
struct HttpLimits {
  /// Maximum total request-body size in bytes. Larger requests are
  /// rejected with 413 Payload Too Large before any buffer is
  /// allocated. Defaults to 16 MiB.
  std::size_t max_body_bytes = 16ULL * 1024ULL * 1024ULL;

  /// Maximum size of the start line + headers in bytes. Defaults to
  /// 32 KiB.
  std::size_t max_header_bytes = 32ULL * 1024ULL;

  /// Per-request timeout enforced by the HTTP server. Includes the
  /// time spent reading the request and producing the response.
  /// Defaults to 8 seconds.
  std::chrono::milliseconds request_timeout{8000};

  /// Maximum number of HTTP worker threads. The server uses one
  /// thread per in-flight connection; concurrent serving is bounded
  /// by this number plus the scheduler's `in_flight_capacity`.
  /// Defaults to 16.
  std::size_t accept_thread_pool_size = 16;
};

/// Shutdown policy. The serving worker stops admission, drains for
/// up to `drain_deadline`, then cancels remaining work.
struct ShutdownPolicy {
  /// Maximum time to wait for in-flight requests to drain after
  /// admission stops. After this, queued and in-flight work are
  /// cancelled. Defaults to 5 seconds.
  std::chrono::milliseconds drain_deadline{5000};

  /// When true, queued work is cancelled at shutdown start rather
  /// than allowed to dispatch. Defaults to true so a SIGTERM does
  /// not stretch out the drain window with newly-dispatched work.
  bool cancel_queued_immediately = true;
};

/// Loopback bind policy. v0.1.0 always binds loopback; setting
/// `allow_non_loopback` to true is an opt-in for test fixtures only
/// and is rejected by the config validator in production builds (it
/// reaches the validator as `Unsupported` unless TP_E2E_ALLOW_NON_LOOPBACK
/// is set in the environment).
struct BindConfig {
  /// Bind host. Must be a loopback literal: "127.0.0.1", "::1", or
  /// "localhost". Defaults to "127.0.0.1".
  std::string host = "127.0.0.1";

  /// TCP port to bind. Use 0 to request an ephemeral port; the
  /// composition root surfaces the assigned port through
  /// `ServingWorker::bound_port()` so tests can connect without
  /// racing.
  std::uint16_t port = 0;

  /// Permit non-loopback host strings (e.g., "0.0.0.0"). v0.1.0
  /// disallows this; the field is retained so the config schema can
  /// surface a typed `Unsupported` error rather than silently
  /// accepting a public bind.
  bool allow_non_loopback = false;
};

/// Async policy-store config. Bounded retention for accepted-but-
/// undelivered LeRobot async results.
struct AsyncPolicyConfig {
  /// Maximum number of completed async results retained for client
  /// pickup. Older results are evicted; their buffers are released.
  /// Defaults to 64.
  std::size_t max_completed = 64;

  /// Maximum time a completed result is retained before eviction.
  /// Defaults to 60 seconds.
  std::chrono::milliseconds completed_ttl{60'000};

  /// Maximum number of concurrently accepted async requests. Acts as
  /// an upper bound separate from `SchedulerConfig::queue_capacity`
  /// to keep async clients from starving sync `/infer`. Defaults to
  /// 256.
  std::size_t max_pending = 256;
};

/// Full serving worker runtime configuration. Validated by
/// `ServingConfig::validate()`; the composition root calls validate
/// before constructing any runtime components.
struct ServingConfig {
  /// Schema version. Reserved for forward compatibility; the parser
  /// rejects unknown values with `Error::Code::Unsupported`.
  std::string schema_version = "0.1";

  BindConfig bind;
  HttpLimits http;
  SchedulerConfig scheduler;
  BufferManagerConfig buffer;
  ActiveDeploymentConfig deployment;
  AsyncPolicyConfig async_policy;
  ShutdownPolicy shutdown;
  HealthMode health_mode = HealthMode::LocalJson;
  MetricsMode metrics_mode = MetricsMode::PrometheusText;

  /// Optional structured-log writer enable flag. When true, the
  /// composition root installs a stderr-bound structured log sink.
  /// Tests disable this so gtest output stays clean.
  bool enable_stderr_logs = true;

  /// Validate the config. Returns:
  ///   - ConfigInvalid: malformed host, zero limits, drain deadline
  ///     less than zero, missing model spec when use_mock_session
  ///     is false, etc.
  ///   - Unsupported: non-loopback bind without opt-in, unknown
  ///     schema_version.
  [[nodiscard]] Result<void> validate() const;

  /// Convenience: parse a JSON document into a ServingConfig.
  ///
  /// The string must conform to
  /// `protocol/schemas/serving_worker_config.json`. Validation runs
  /// after parsing so missing required fields surface the same
  /// `ConfigInvalid` errors as a programmatic configuration.
  [[nodiscard]] static Result<ServingConfig> parse_json(std::string_view text);

  /// Serialize the (already-validated) config to JSON. Used by the
  /// composition root to echo the active configuration into the
  /// structured logs and into `/health`.
  [[nodiscard]] std::string to_json() const;
};

}  // namespace tensorplate
