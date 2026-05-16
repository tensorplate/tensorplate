// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F02: Loopback HTTP/1.1 server.
//
// This server is intentionally small: a single accept loop on a
// loopback TCP socket dispatches connections onto a fixed worker
// pool. The pool reads and parses one request per connection, calls
// the route handler, writes the response, and closes. Keep-alive is
// not supported in v0.1.0; serving worker traffic is small-volume
// local control-plane traffic and per-request connections keep the
// implementation auditable.
//
// Framework-selection rationale (documented in
// `docs/architecture/serving-http.md`):
//
//   - Loopback binding: trivially enforced because the listener
//     refuses to bind any address that is not in the loopback set.
//   - Request limits: enforced inline by the parser; oversized
//     requests are rejected with 413 before the buffer plane
//     allocates anything.
//   - Graceful shutdown: a dedicated `shutdown()` method closes
//     the listener and signals the worker pool to drain.
//   - Testability: the server can bind ephemeral ports and returns
//     the assigned port through `bound_port()`. Tests use that to
//     avoid race-prone fixed-port assignments.
//   - Dependency weight: nothing beyond the POSIX sockets layer
//     and `nlohmann::json` (already required for the python sidecar).
//
// The server is not a general-purpose HTTP framework. It does not
// support websockets, HTTP/2, TLS, multipart streaming, server-sent
// events, chunked transfer encoding, range responses, or compressed
// transports. Adding any of those should be a deliberate v0.2+
// decision, not a drive-by.

#pragma once

#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <string>
#include <string_view>

#include "tensorplate/core/result.hpp"
#include "tensorplate/http/http_message.hpp"

namespace tensorplate::http {

/// HTTP server configuration. Mirrors the relevant fields from
/// `tensorplate::HttpLimits` plus a bind address and port.
struct HttpServerConfig {
  std::string bind_host = "127.0.0.1";
  std::uint16_t bind_port = 0;
  std::size_t max_body_bytes = 16ULL * 1024ULL * 1024ULL;
  std::size_t max_header_bytes = 32ULL * 1024ULL;
  std::chrono::milliseconds request_timeout{8000};
  std::size_t worker_thread_count = 16;
  bool allow_non_loopback = false;
};

/// Route handler signature. Implementations must be safe to call
/// concurrently and must not throw; the server treats an uncaught
/// exception as a 500.
using RouteHandler = std::function<Response(const Request&)>;

/// HTTP server. Owns the listening socket, the worker pool, and the
/// route table. Owning code (the serving worker composition root)
/// wires routes through `add_route` before calling `start`.
class HttpServer {
 public:
  HttpServer();
  ~HttpServer();

  HttpServer(const HttpServer&) = delete;
  HttpServer& operator=(const HttpServer&) = delete;
  HttpServer(HttpServer&&) = delete;
  HttpServer& operator=(HttpServer&&) = delete;

  /// Register a route handler. The lookup matches `method` + exact
  /// `path`. Path patterns with parameters (e.g.
  /// `/policy/result/<id>`) are registered by `add_prefix_route`,
  /// which performs prefix matching and exposes the trailing path
  /// segment via `Request::path` minus the prefix in the
  /// `correlation_id` field (see `runtime/src/http/http_server.cpp`).
  void add_route(std::string method, std::string path, RouteHandler handler);

  /// Register a prefix-matched route handler. Useful for LeRobot
  /// async-policy result/cancel routes where the trailing segment is
  /// the request id. The handler receives the full request; route
  /// helpers parse the trailing segment.
  void add_prefix_route(std::string method, std::string prefix, RouteHandler handler);

  /// Start listening. Returns:
  ///   - ConfigInvalid: malformed host.
  ///   - Unsupported: non-loopback host without opt-in.
  ///   - LoadFailed: socket / bind failure.
  [[nodiscard]] Result<void> start(const HttpServerConfig& config);

  /// Stop accepting and join workers. Idempotent. After stop()
  /// returns, no further request handlers will run.
  void stop();

  /// True if start() has succeeded and stop() has not yet been
  /// called.
  [[nodiscard]] bool is_running() const noexcept;

  /// Port the listener bound to. 0 until `start()` succeeds.
  [[nodiscard]] std::uint16_t bound_port() const noexcept;

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

}  // namespace tensorplate::http
