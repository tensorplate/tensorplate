// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F02: HTTP/1.1 request and response value types.
//
// The serving worker uses a minimal loopback HTTP/1.1 server (see
// `runtime/src/http/http_server.cpp`) that produces these value
// objects from incoming connections and consumes them when writing
// responses. The types are vendor-neutral and deliberately small;
// they are not a general-purpose HTTP framework.
//
// The route contract for v0.1.0 is documented in
// `docs/architecture/serving-http.md`.

#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace tensorplate::http {

/// Header field. Names are stored lowercase to match HTTP semantics
/// without forcing the parser/router to re-normalize on every
/// lookup.
struct Header {
  std::string name;
  std::string value;
};

/// Request value object produced by the parser. The body is held by
/// value to keep the type simple; the server enforces a per-request
/// body byte cap (see `HttpLimits`) before parsing reaches the
/// router.
struct Request {
  std::string method;
  std::string path;
  std::string query;
  std::vector<Header> headers;
  std::string body;
  /// Client peer string ("ip:port"). Loopback only in v0.1.0.
  std::string peer;
  /// Correlation id extracted from headers (e.g., `x-correlation-id`)
  /// or generated at ingress if absent. Always populated by the
  /// router before user code sees the request.
  std::string correlation_id;

  /// Convenience: lookup a header by lowercase name. Returns
  /// std::nullopt if not present.
  [[nodiscard]] std::optional<std::string_view> header(std::string_view name) const noexcept;
};

/// Response value object produced by route handlers and written
/// back by the HTTP server. The body is held by value; chunked /
/// streaming responses are not supported in v0.1.0.
struct Response {
  int status = 200;
  std::vector<Header> headers;
  std::string body;

  /// Convenience: set a header (replaces existing same-name).
  void set_header(std::string_view name, std::string value);

  /// Convenience: build a JSON 200 response.
  static Response ok_json(std::string body) noexcept;
  /// Convenience: build a JSON response with a custom status.
  static Response json(int status, std::string body) noexcept;
  /// Convenience: build a plain-text response.
  static Response plain(int status, std::string body) noexcept;
};

/// Reason-phrase for an HTTP status code. Stable across releases.
[[nodiscard]] std::string_view http_reason(int status) noexcept;

/// Lowercase a header name in place. Used by the parser/router.
[[nodiscard]] std::string lower_ascii(std::string_view in);

}  // namespace tensorplate::http
