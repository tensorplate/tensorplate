// SPDX-License-Identifier: Apache-2.0
//
// Regression coverage: the runtime socket write helpers must suppress
// SIGPIPE locally and surface a typed error when the peer has closed,
// without relying on the embedding binary to ignore SIGPIPE.
//
// Both tests reset SIGPIPE to its default (terminating) disposition, so
// an escaped SIGPIPE kills this process rather than failing quietly --
// the tests deliberately do not install a process-wide ignore, exercising
// the library as a third-party embedder would.

#include <arpa/inet.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <csignal>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <span>
#include <string>
#include <thread>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/http/http_server.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

namespace tensorplate {
namespace {

// RAII guard that forces SIGPIPE to its default disposition for the
// duration of a test and restores the previous handler on scope exit.
// With the bug present, an un-suppressed write would deliver SIGPIPE
// here and terminate the test process.
class SigpipeDefaultGuard {
 public:
  SigpipeDefaultGuard() {
    struct sigaction dfl {};
    dfl.sa_handler = SIG_DFL;
    sigemptyset(&dfl.sa_mask);
    dfl.sa_flags = 0;
    ::sigaction(SIGPIPE, &dfl, &prev_);
  }
  ~SigpipeDefaultGuard() { ::sigaction(SIGPIPE, &prev_, nullptr); }

  SigpipeDefaultGuard(const SigpipeDefaultGuard&) = delete;
  SigpipeDefaultGuard& operator=(const SigpipeDefaultGuard&) = delete;

 private:
  struct sigaction prev_ {};
};

std::string unix_socket_path() {
  const auto tmp = std::filesystem::temp_directory_path();
  return (tmp / ("tp_sigpipe_" + std::to_string(::getpid()) + ".sock")).string();
}

ipc::UnixSocket::TimePoint deadline_in(std::chrono::milliseconds d) {
  return ipc::UnixSocket::Clock::now() + d;
}

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

// Writing to a Unix-domain socket whose read end has closed must return
// a typed LoadFailed (EPIPE) error, not raise SIGPIPE.
TEST(SocketSigpipe, UnixSocketWriteToClosedPeerReturnsTypedError) {
  SigpipeDefaultGuard guard;

  const std::string path = unix_socket_path();
  std::filesystem::remove(path);

  auto listener = ipc::UnixSocket::create_stream().value();
  ASSERT_TRUE(listener.bind_and_listen(path).has_value());

  auto client = ipc::UnixSocket::create_stream().value();
  ASSERT_TRUE(client.connect(path, deadline_in(std::chrono::seconds(5))).has_value());
  auto server = listener.accept(deadline_in(std::chrono::seconds(5))).value();

  // Drop the read end. Subsequent client writes must drive the
  // connection to EPIPE.
  server.close();

  // Push well past any plausible socket send buffer so a send() is
  // guaranteed to observe the closed peer. Each call carries its own
  // deadline so the loop cannot hang.
  const std::vector<std::byte> payload(64 * 1024, std::byte{0xAB});
  Result<void> last{};  // success-state; loop runs at least once.
  for (int i = 0; i < 256 && last.has_value(); ++i) {
    last = client.write_all(std::span<const std::byte>(payload.data(), payload.size()),
                            deadline_in(std::chrono::seconds(2)));
  }

  ASSERT_FALSE(last.has_value()) << "write to a closed peer should fail";
  EXPECT_EQ(last.error().code, Error::Code::LoadFailed);

  std::filesystem::remove(path);
}

// The HTTP server must survive a client that disconnects mid-response.
// A large body forces write_all() to loop and observe the closed peer;
// without local SIGPIPE suppression the server thread would take the
// signal and kill the process. After the aborted request the server
// must still answer a fresh one.
TEST(SocketSigpipe, HttpServerSurvivesClientDisconnectMidResponse) {
  SigpipeDefaultGuard guard;

  // 8 MiB exceeds any default socket send buffer, so the server cannot
  // flush the whole body before it notices the peer is gone.
  const std::string big_body(8ULL * 1024ULL * 1024ULL, 'x');

  http::HttpServer server;
  server.add_route("GET", "/big",
                   [&](const http::Request&) { return http::Response::plain(200, big_body); });
  server.add_route("GET", "/ping",
                   [](const http::Request&) { return http::Response::plain(200, "pong"); });

  http::HttpServerConfig cfg;
  cfg.bind_host = "127.0.0.1";
  cfg.bind_port = 0;
  cfg.worker_thread_count = 4;
  ASSERT_TRUE(server.start(cfg).has_value());
  const std::uint16_t port = server.bound_port();
  ASSERT_NE(port, 0);

  // Issue the large request, then close immediately without reading the
  // response. The orderly close (FIN) plus an oversized body drives the
  // server's send() into EPIPE territory.
  {
    const int fd = connect_loopback(port);
    const std::string req = "GET /big HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    ASSERT_EQ(::send(fd, req.data(), req.size(), 0), static_cast<ssize_t>(req.size()));
    // Give the server a moment to begin writing the body before we drop
    // the connection, so the closed peer is observed mid-write.
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    ::close(fd);
  }

  // Liveness: a fresh request must still be served, proving the worker
  // pool was not torn down by a stray signal.
  {
    const int fd = connect_loopback(port);
    const std::string req = "GET /ping HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    ASSERT_EQ(::send(fd, req.data(), req.size(), 0), static_cast<ssize_t>(req.size()));
    const std::string resp = read_to_eof(fd);
    ::close(fd);
    EXPECT_NE(resp.find("200"), std::string::npos) << resp;
    EXPECT_NE(resp.find("pong"), std::string::npos) << resp;
  }

  server.stop();
}

}  // namespace
}  // namespace tensorplate
