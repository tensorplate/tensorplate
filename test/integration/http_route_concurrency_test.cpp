// SPDX-License-Identifier: Apache-2.0
//
// Regression coverage for issue #21: HttpServer::Impl::dispatch() must
// release the route-lookup mutex before invoking a handler. If the lock
// were held across handler execution, a long-running route (e.g.
// /infer) would serialize every other route -- /health, /metrics, and
// the async-policy result/cancel routes -- behind it, turning a route
// table mutex into a global request-execution lock.
//
// The test blocks one handler inside the server and asserts that an
// unrelated route still completes while it is stalled. A receive timeout
// on the client socket converts a regression into a failed assertion
// rather than a hung test process.

#include <arpa/inet.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <mutex>
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

// Bound the time any single recv() will block. Without this, a
// regression (the response never arriving) would hang the test instead
// of failing it.
void set_recv_timeout(int fd, std::chrono::milliseconds d) {
  timeval tv{};
  tv.tv_sec = static_cast<time_t>(d.count() / 1000);
  tv.tv_usec = static_cast<suseconds_t>((d.count() % 1000) * 1000);
  ::setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
}

// Read an HTTP response to EOF. The server always replies with
// `connection: close`, so the read loop terminates at the peer FIN (or
// when the receive timeout trips, returning whatever has arrived).
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

// A slow handler that is blocked inside the server must not prevent an
// unrelated route from being served. With the bug present, the second
// request blocks acquiring routes_mutex (held across the slow handler),
// so its response never arrives and the recv() below times out.
TEST(HttpRouteConcurrency, SlowHandlerDoesNotBlockUnrelatedRoutes) {
  std::mutex m;
  std::condition_variable cv;
  bool slow_entered = false;  // the slow handler has begun executing
  bool release = false;       // the test permits the slow handler to finish

  http::HttpServer server;
  // Stand-in for a long-running /infer: it parks inside the handler
  // until the test releases it, holding a worker thread the whole time.
  server.add_route("POST", "/infer", [&](const http::Request&) {
    {
      std::lock_guard<std::mutex> g(m);
      slow_entered = true;
    }
    cv.notify_all();
    std::unique_lock<std::mutex> g(m);
    cv.wait(g, [&] { return release; });
    return http::Response::plain(200, "infer-done");
  });
  server.add_route("GET", "/health",
                   [](const http::Request&) { return http::Response::plain(200, "ok"); });

  http::HttpServerConfig cfg;
  cfg.bind_host = "127.0.0.1";
  cfg.bind_port = 0;
  cfg.worker_thread_count = 4;  // >= 2 so a second request can be served concurrently
  ASSERT_TRUE(server.start(cfg).has_value());
  const std::uint16_t port = server.bound_port();
  ASSERT_NE(port, 0);

  // 1. Fire the slow /infer request and leave it pending.
  const int slow_fd = connect_loopback(port);
  const std::string infer_req =
      "POST /infer HTTP/1.1\r\nHost: 127.0.0.1\r\ncontent-length: 0\r\n\r\n";
  ASSERT_EQ(::send(slow_fd, infer_req.data(), infer_req.size(), 0),
            static_cast<ssize_t>(infer_req.size()));

  // 2. Wait until the slow handler is actually executing. From this
  //    point on, the buggy server holds routes_mutex.
  {
    std::unique_lock<std::mutex> g(m);
    ASSERT_TRUE(cv.wait_for(g, std::chrono::seconds(5), [&] { return slow_entered; }))
        << "slow handler never started";
  }

  // 3. While /infer is parked, /health must still complete promptly.
  {
    const int fast_fd = connect_loopback(port);
    set_recv_timeout(fast_fd, std::chrono::seconds(5));
    const std::string health_req = "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    ASSERT_EQ(::send(fast_fd, health_req.data(), health_req.size(), 0),
              static_cast<ssize_t>(health_req.size()));
    const std::string resp = read_to_eof(fast_fd);
    ::close(fast_fd);
    EXPECT_NE(resp.find("200"), std::string::npos)
        << "/health did not complete while /infer was running: [" << resp << "]";
    EXPECT_NE(resp.find("ok"), std::string::npos) << resp;
  }

  // 4. Release the slow handler; it must still finish and answer correctly.
  {
    std::lock_guard<std::mutex> g(m);
    release = true;
  }
  cv.notify_all();

  set_recv_timeout(slow_fd, std::chrono::seconds(5));
  const std::string infer_resp = read_to_eof(slow_fd);
  ::close(slow_fd);
  EXPECT_NE(infer_resp.find("200"), std::string::npos) << infer_resp;
  EXPECT_NE(infer_resp.find("infer-done"), std::string::npos) << infer_resp;

  server.stop();
}

}  // namespace
}  // namespace tensorplate
