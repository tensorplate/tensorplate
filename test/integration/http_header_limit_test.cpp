// SPDX-License-Identifier: Apache-2.0
//
// Regression coverage for issue #23: HTTP header-size enforcement used
// to check for the CRLFCRLF terminator before checking whether the
// accumulated header buffer exceeded max_header_bytes. A single read
// chunk that both crossed the limit and carried the terminator was
// therefore accepted instead of being rejected with 431.
//
// The fix enforces max_header_bytes on the header block (bytes up to and
// including the terminator) at the point the terminator is found, so an
// oversized header is rejected even when the terminator arrives in the
// same read that overflows the limit. The body that may share that read
// is deliberately *not* counted against the header limit -- a second
// test pins that down so a future "just reorder the checks" change
// cannot silently start rejecting small-header / large-body requests.

#include <arpa/inet.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>

#include "tensorplate/http/http_server.hpp"

namespace tensorplate {
namespace {

int connect_loopback(std::uint16_t port) {
  const int fd = ::socket(AF_INET, SOCK_STREAM, 0);
  EXPECT_GE(fd, 0);
  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_port = htons(port);
  EXPECT_EQ(::inet_pton(AF_INET, "127.0.0.1", &addr.sin_addr), 1);
  EXPECT_EQ(::connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0)
      << std::strerror(errno);
  return fd;
}

// Bound the time any single recv() blocks so a regression (no response
// arriving) fails the test instead of hanging the process.
void set_recv_timeout(int fd, std::chrono::milliseconds d) {
  timeval tv{};
  tv.tv_sec = static_cast<time_t>(d.count() / 1000);
  tv.tv_usec = static_cast<suseconds_t>((d.count() % 1000) * 1000);
  ::setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
}

// Read an HTTP response to EOF. The server always replies with
// `connection: close`, so the read loop ends at the peer FIN (or when
// the receive timeout trips, returning whatever has arrived).
std::string read_to_eof(int fd) {
  std::string out;
  std::array<char, 4096> buf{};
  while (true) {
    const ssize_t n = ::recv(fd, buf.data(), buf.size(), 0);
    if (n <= 0) {
      break;
    }
    out.append(buf.data(), static_cast<std::size_t>(n));
  }
  return out;
}

bool send_all(int fd, const std::string& data) {
  std::size_t sent = 0;
  while (sent < data.size()) {
    const ssize_t n = ::send(fd, data.data() + sent, data.size() - sent, 0);
    if (n <= 0) {
      return false;
    }
    sent += static_cast<std::size_t>(n);
  }
  return true;
}

// An oversized header block whose CRLFCRLF terminator arrives in the
// same read that crosses max_header_bytes must be rejected with 431.
// Before the fix the terminator check short-circuited the size check and
// the request was accepted and dispatched.
TEST(HttpHeaderLimit, OversizedHeaderWithTerminatorInSameReadRejected) {
  http::HttpServer server;
  bool dispatched = false;
  server.add_route("GET", "/ping", [&dispatched](const http::Request&) {
    dispatched = true;
    return http::Response::plain(200, "pong");
  });

  http::HttpServerConfig cfg;
  cfg.bind_host = "127.0.0.1";
  cfg.bind_port = 0;
  cfg.worker_thread_count = 2;
  cfg.max_header_bytes = 256;
  ASSERT_TRUE(server.start(cfg).has_value());

  const std::uint16_t port = server.bound_port();
  ASSERT_NE(port, 0);

  const int fd = connect_loopback(port);
  ASSERT_GE(fd, 0);
  set_recv_timeout(fd, std::chrono::milliseconds{2000});

  // ~1 KiB header value pushes the header block well past 256 bytes; the
  // whole request (terminator included) is one send() and, being far
  // under the 16 KiB read chunk, arrives in a single recv() -- exactly
  // the "crosses the limit in the chunk that carries the terminator"
  // case the issue describes.
  const std::string big_value(1024, 'x');
  const std::string req =
      "GET /ping HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Big: " + big_value + "\r\n\r\n";
  ASSERT_GT(req.size(), cfg.max_header_bytes);
  ASSERT_TRUE(send_all(fd, req));

  const std::string resp = read_to_eof(fd);
  ::close(fd);

  EXPECT_NE(resp.find("431"), std::string::npos) << resp;
  EXPECT_FALSE(dispatched) << "oversized header reached route dispatch";

  server.stop();
}

// A request whose header section fits within max_header_bytes but whose
// body pushes the total bytes in a single read past that limit must
// still be accepted: the limit applies to the header block, not to body
// bytes that happen to share the read. This guards against a naive fix
// that simply reorders the size check ahead of the terminator check.
TEST(HttpHeaderLimit, SmallHeaderLargeBodyInSameReadAccepted) {
  http::HttpServer server;
  server.add_route("POST", "/echo",
                   [](const http::Request& req) { return http::Response::plain(200, req.body); });

  http::HttpServerConfig cfg;
  cfg.bind_host = "127.0.0.1";
  cfg.bind_port = 0;
  cfg.worker_thread_count = 2;
  cfg.max_header_bytes = 256;
  cfg.max_body_bytes = 8192;
  ASSERT_TRUE(server.start(cfg).has_value());

  const std::uint16_t port = server.bound_port();
  ASSERT_NE(port, 0);

  const int fd = connect_loopback(port);
  ASSERT_GE(fd, 0);
  set_recv_timeout(fd, std::chrono::milliseconds{2000});

  const std::string body(1024, 'y');  // body alone exceeds max_header_bytes
  const std::string req =
      "POST /echo HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: " + std::to_string(body.size()) +
      "\r\n\r\n" + body;
  // Header block is small; total request bytes exceed max_header_bytes.
  ASSERT_LT(req.size() - body.size(), cfg.max_header_bytes);
  ASSERT_GT(req.size(), cfg.max_header_bytes);
  ASSERT_TRUE(send_all(fd, req));

  const std::string resp = read_to_eof(fd);
  ::close(fd);

  EXPECT_NE(resp.find("200"), std::string::npos) << resp;
  EXPECT_EQ(resp.find("431"), std::string::npos) << resp;
  EXPECT_NE(resp.find(body), std::string::npos) << "echoed body missing from response";

  server.stop();
}

}  // namespace
}  // namespace tensorplate
