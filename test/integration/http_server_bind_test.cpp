// SPDX-License-Identifier: Apache-2.0
//
// Regression coverage for issue #22: the serving config validator
// accepts the IPv6 loopback literal "::1", but HttpServer::start() used
// to create an AF_INET socket and parse the host with
// inet_pton(AF_INET, ...). A config that passed validation then failed
// at listener startup. The server must bind whichever loopback family
// the literal names, so validation and runtime behavior agree.

#include <arpa/inet.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>

#include "tensorplate/core/error.hpp"
#include "tensorplate/http/http_server.hpp"

namespace tensorplate {
namespace {

// Read an HTTP response to EOF. The server always replies with
// `connection: close`, so the read loop terminates at the peer FIN.
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

// Connect to an IPv4 loopback listener. Returns -1 on failure.
int connect_v4(std::uint16_t port) {
  const int fd = ::socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) {
    return -1;
  }
  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_port = htons(port);
  if (::inet_pton(AF_INET, "127.0.0.1", &addr.sin_addr) != 1 ||
      ::connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) != 0) {
    ::close(fd);
    return -1;
  }
  return fd;
}

// Connect to an IPv6 loopback listener. Returns -1 on failure.
int connect_v6(std::uint16_t port) {
  const int fd = ::socket(AF_INET6, SOCK_STREAM, 0);
  if (fd < 0) {
    return -1;
  }
  sockaddr_in6 addr{};
  addr.sin6_family = AF_INET6;
  addr.sin6_port = htons(port);
  if (::inet_pton(AF_INET6, "::1", &addr.sin6_addr) != 1 ||
      ::connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) != 0) {
    ::close(fd);
    return -1;
  }
  return fd;
}

std::string get(int fd, const std::string& path, const std::string& host) {
  const std::string req = "GET " + path + " HTTP/1.1\r\nHost: " + host + "\r\n\r\n";
  EXPECT_EQ(::send(fd, req.data(), req.size(), 0), static_cast<ssize_t>(req.size()));
  return read_to_eof(fd);
}

// The IPv6 loopback literal must bind an AF_INET6 listener and serve
// requests over it. Before the fix, start() returned ConfigInvalid
// because inet_pton(AF_INET, "::1", ...) cannot parse an IPv6 address.
TEST(HttpServerBind, IPv6LoopbackBindsAndServes) {
  http::HttpServer server;
  server.add_route("GET", "/ping",
                   [](const http::Request&) { return http::Response::plain(200, "pong"); });

  http::HttpServerConfig cfg;
  cfg.bind_host = "::1";
  cfg.bind_port = 0;
  cfg.worker_thread_count = 2;

  auto started = server.start(cfg);
  if (!started) {
    // The fix guarantees we never fail on address *parsing*. A bind
    // failure (e.g. a host with IPv6 loopback disabled) is an
    // environment limitation, not the regression under test.
    ASSERT_NE(started.error().code, Error::Code::ConfigInvalid)
        << "::1 must parse as an IPv6 address, not fail validation: "
        << started.error().message;
    GTEST_SKIP() << "IPv6 loopback unavailable on this host: " << started.error().message;
  }

  const std::uint16_t port = server.bound_port();
  ASSERT_NE(port, 0);

  const int fd = connect_v6(port);
  ASSERT_GE(fd, 0) << "could not connect to ::1:" << port << ": " << std::strerror(errno);
  const std::string resp = get(fd, "/ping", "[::1]");
  ::close(fd);
  EXPECT_NE(resp.find("200"), std::string::npos) << resp;
  EXPECT_NE(resp.find("pong"), std::string::npos) << resp;

  server.stop();
}

// The IPv4 loopback path must keep working unchanged after the fix.
TEST(HttpServerBind, IPv4LoopbackStillBindsAndServes) {
  http::HttpServer server;
  server.add_route("GET", "/ping",
                   [](const http::Request&) { return http::Response::plain(200, "pong"); });

  http::HttpServerConfig cfg;
  cfg.bind_host = "127.0.0.1";
  cfg.bind_port = 0;
  cfg.worker_thread_count = 2;
  ASSERT_TRUE(server.start(cfg).has_value());

  const std::uint16_t port = server.bound_port();
  ASSERT_NE(port, 0);

  const int fd = connect_v4(port);
  ASSERT_GE(fd, 0) << "could not connect to 127.0.0.1:" << port << ": " << std::strerror(errno);
  const std::string resp = get(fd, "/ping", "127.0.0.1");
  ::close(fd);
  EXPECT_NE(resp.find("200"), std::string::npos) << resp;
  EXPECT_NE(resp.find("pong"), std::string::npos) << resp;

  server.stop();
}

// A host string that is neither an IPv4 nor an IPv6 literal must surface
// a typed ConfigInvalid error rather than crashing or binding garbage.
TEST(HttpServerBind, UnparseableHostReturnsConfigInvalid) {
  http::HttpServer server;
  http::HttpServerConfig cfg;
  cfg.bind_host = "not-an-address";
  cfg.bind_port = 0;
  // allow_non_loopback so we get past the loopback gate to the parser.
  cfg.allow_non_loopback = true;

  auto started = server.start(cfg);
  ASSERT_FALSE(started.has_value());
  EXPECT_EQ(started.error().code, Error::Code::ConfigInvalid);
}

}  // namespace
}  // namespace tensorplate
