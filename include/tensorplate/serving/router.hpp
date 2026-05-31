// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F03 / F04: Request router for the loopback HTTP endpoint.
//
// The router converts decoded HTTP request envelopes into
// `InferRequest` value objects, dispatches them through the serving
// pipeline, and serializes the resulting `InferResult` or typed
// `Error` back into HTTP responses. It also implements the LeRobot-
// compatible async route family (`/policy/infer`, `/policy/result`,
// `/policy/cancel`) by talking to the same pipeline.
//
// The router intentionally does **not** know about HTTP transport
// internals beyond `Request` / `Response`; it can be exercised
// without a TCP listener.

#pragma once

#include <atomic>
#include <chrono>
#include <cstddef>
#include <memory>
#include <optional>
#include <string>
#include <string_view>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/http/http_message.hpp"

namespace tensorplate {

class BufferManager;
class HealthState;
class ServingMetrics;
class InferScheduler;

namespace serving {

class AsyncPolicyStore;
class ServingPipeline;

/// Construction-time dependencies. Owned externally.
struct RequestRouterDeps {
  ServingPipeline* pipeline = nullptr;
  AsyncPolicyStore* async_store = nullptr;
  BufferManager* buffer_manager = nullptr;
  ServingMetrics* metrics = nullptr;
  HealthState* health = nullptr;
  /// Scheduler reference used to fan out stale-sequence cancellation
  /// and explicit /policy/cancel calls without going through the
  /// pipeline. Owned externally by the composition root.
  InferScheduler* scheduler = nullptr;
  /// Maximum body bytes the HTTP server allowed through (used for
  /// rejection telemetry only; the HTTP server enforces the cap).
  std::size_t max_body_bytes = 16ULL * 1024ULL * 1024ULL;
  /// Stable endpoint label echoed into responses, metric labels,
  /// and structured logs.
  std::string endpoint;
  /// True when the resolved execution backend supports native async
  /// inference/cancellation semantics. When false, the policy route
  /// family returns 501 before retaining request buffers.
  bool async_policy_supported = true;
};

class RequestRouter {
 public:
  explicit RequestRouter(RequestRouterDeps deps);
  ~RequestRouter();

  RequestRouter(const RequestRouter&) = delete;
  RequestRouter& operator=(const RequestRouter&) = delete;
  RequestRouter(RequestRouter&&) = delete;
  RequestRouter& operator=(RequestRouter&&) = delete;

  /// POST /infer
  [[nodiscard]] http::Response handle_infer(const http::Request& req);

  /// POST /policy/infer
  [[nodiscard]] http::Response handle_policy_infer(const http::Request& req);

  /// GET /policy/result/<request_id>
  [[nodiscard]] http::Response handle_policy_result(const http::Request& req,
                                                    std::string_view request_id);

  /// POST /policy/cancel/<request_id>
  [[nodiscard]] http::Response handle_policy_cancel(const http::Request& req,
                                                    std::string_view request_id);

  /// Mark router as stopping. Subsequent admission returns 503
  /// without touching the pipeline.
  void set_stopping(bool stopping) noexcept;
  [[nodiscard]] bool is_stopping() const noexcept;

  /// Endpoint label echoed back to clients.
  [[nodiscard]] std::string_view endpoint() const noexcept;

 private:
  RequestRouterDeps deps_;
  std::atomic<bool> stopping_{false};

  [[nodiscard]] http::Response make_error_response(
      int http_status, std::string_view request_id, std::string_view correlation_id,
      Error::Code code, std::string_view message,
      std::optional<std::string_view> detail = {}) const;
};

}  // namespace serving
}  // namespace tensorplate
