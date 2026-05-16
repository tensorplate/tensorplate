// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F02: Loopback HTTP/1.1 server implementation.
//
// One acceptor thread plus a worker pool: each worker reads one
// request, runs the route handler, writes the response, and closes
// the connection. Keep-alive is not implemented in v0.1.0; serving
// traffic is small-volume local control-plane traffic and per-
// request connections keep the code auditable.

#include "tensorplate/http/http_server.hpp"

#include <algorithm>
#include <array>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <condition_variable>
#include <csignal>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#if defined(_WIN32)
#error "TensorPlate v0.1.0 serving worker is POSIX-only (Jetson/Linux + macOS)."
#endif

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

namespace tensorplate::http {

namespace {

constexpr std::size_t kReadChunk = 16384;

bool is_loopback_literal(std::string_view host) {
  return host == "127.0.0.1" || host == "::1" || host == "localhost";
}

int set_nonblock(int fd) {
  // POSIX ::fcntl is a vararg interface; suppress the cppcoreguidelines
  // vararg check here rather than at every call site.
  int flags = ::fcntl(fd, F_GETFL, 0);  // NOLINT(cppcoreguidelines-pro-type-vararg)
  if (flags < 0) {
    return -1;
  }
  return ::fcntl(fd, F_SETFL, flags | O_NONBLOCK);  // NOLINT(cppcoreguidelines-pro-type-vararg)
}

// Read until either CRLFCRLF (header terminator) is seen, the
// per-request timeout expires, or the limit is exceeded. Returns
// the number of bytes read into `buffer`; -1 on error.
ssize_t read_until_headers(int fd, std::string& buffer, std::size_t max_header_bytes,
                           std::chrono::steady_clock::time_point deadline) {
  while (true) {
    if (auto p = buffer.find("\r\n\r\n"); p != std::string::npos) {
      return static_cast<ssize_t>(buffer.size());
    }
    if (buffer.size() > max_header_bytes) {
      return -2;  // header too large
    }
    auto now = std::chrono::steady_clock::now();
    if (now >= deadline) {
      return -3;  // timeout
    }
    auto remaining_ms = std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now);
    pollfd p{fd, POLLIN, 0};
    int pr = ::poll(&p, 1, static_cast<int>(remaining_ms.count()));
    if (pr < 0) {
      if (errno == EINTR) {
        continue;
      }
      return -1;
    }
    if (pr == 0) {
      return -3;  // timeout
    }
    std::array<char, kReadChunk> chunk{};
    ssize_t n = ::recv(fd, chunk.data(), chunk.size(), 0);
    if (n < 0) {
      if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) {
        continue;
      }
      return -1;
    }
    if (n == 0) {
      return -4;  // EOF before headers
    }
    buffer.append(chunk.data(), static_cast<std::size_t>(n));
  }
}

ssize_t read_remaining_body(int fd, std::string& buffer, std::size_t need_total,
                            std::chrono::steady_clock::time_point deadline) {
  while (buffer.size() < need_total) {
    auto now = std::chrono::steady_clock::now();
    if (now >= deadline) {
      return -3;
    }
    auto remaining_ms = std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now);
    pollfd p{fd, POLLIN, 0};
    int pr = ::poll(&p, 1, static_cast<int>(remaining_ms.count()));
    if (pr < 0) {
      if (errno == EINTR) {
        continue;
      }
      return -1;
    }
    if (pr == 0) {
      return -3;
    }
    std::array<char, kReadChunk> chunk{};
    ssize_t to_read =
        static_cast<ssize_t>(std::min<std::size_t>(chunk.size(), need_total - buffer.size()));
    ssize_t n = ::recv(fd, chunk.data(), static_cast<std::size_t>(to_read), 0);
    if (n < 0) {
      if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) {
        continue;
      }
      return -1;
    }
    if (n == 0) {
      return -4;
    }
    buffer.append(chunk.data(), static_cast<std::size_t>(n));
  }
  return static_cast<ssize_t>(buffer.size());
}

bool write_all(int fd, const std::string& data, std::chrono::steady_clock::time_point deadline) {
  std::size_t sent = 0;
  while (sent < data.size()) {
    auto now = std::chrono::steady_clock::now();
    if (now >= deadline) {
      return false;
    }
    auto remaining_ms = std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now);
    pollfd p{fd, POLLOUT, 0};
    int pr = ::poll(&p, 1, static_cast<int>(remaining_ms.count()));
    if (pr < 0) {
      if (errno == EINTR) {
        continue;
      }
      return false;
    }
    if (pr == 0) {
      return false;
    }
    ssize_t n = ::send(fd, data.data() + sent, data.size() - sent, 0);
    if (n < 0) {
      if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) {
        continue;
      }
      return false;
    }
    if (n == 0) {
      return false;
    }
    sent += static_cast<std::size_t>(n);
  }
  return true;
}

Request parse_request_head(std::string_view raw, std::size_t header_end_pos) {
  Request req;
  std::string_view head = raw.substr(0, header_end_pos);
  // Find start line.
  auto sl_end = head.find("\r\n");
  if (sl_end == std::string_view::npos) {
    return req;
  }
  std::string_view start_line = head.substr(0, sl_end);
  auto sp1 = start_line.find(' ');
  if (sp1 == std::string_view::npos) {
    return req;
  }
  auto sp2 = start_line.find(' ', sp1 + 1);
  if (sp2 == std::string_view::npos) {
    return req;
  }
  req.method = std::string(start_line.substr(0, sp1));
  std::string raw_target(start_line.substr(sp1 + 1, sp2 - sp1 - 1));
  // Split path and query.
  if (auto q = raw_target.find('?'); q != std::string::npos) {
    req.path = raw_target.substr(0, q);
    req.query = raw_target.substr(q + 1);
  } else {
    req.path = std::move(raw_target);
  }
  // Headers.
  std::size_t pos = sl_end + 2;
  while (pos < head.size()) {
    auto eol = head.find("\r\n", pos);
    if (eol == std::string_view::npos || eol == pos) {
      break;
    }
    std::string_view line = head.substr(pos, eol - pos);
    auto colon = line.find(':');
    if (colon != std::string_view::npos) {
      std::string name = lower_ascii(std::string(line.substr(0, colon)));
      std::string_view value = line.substr(colon + 1);
      // Trim leading spaces.
      while (!value.empty() && (value.front() == ' ' || value.front() == '\t')) {
        value.remove_prefix(1);
      }
      while (!value.empty() && (value.back() == ' ' || value.back() == '\t')) {
        value.remove_suffix(1);
      }
      req.headers.push_back(Header{std::move(name), std::string(value)});
    }
    pos = eol + 2;
  }
  return req;
}

std::string format_response(const Response& res) {
  std::string out;
  out.reserve(256 + res.body.size());
  out.append("HTTP/1.1 ");
  out.append(std::to_string(res.status));
  out.append(" ");
  out.append(http_reason(res.status));
  out.append("\r\n");
  bool has_content_length = false;
  bool has_content_type = false;
  bool has_connection = false;
  for (const auto& h : res.headers) {
    out.append(h.name);
    out.append(": ");
    out.append(h.value);
    out.append("\r\n");
    if (h.name == "content-length") {
      has_content_length = true;
    } else if (h.name == "content-type") {
      has_content_type = true;
    } else if (h.name == "connection") {
      has_connection = true;
    }
  }
  if (!has_content_type) {
    out.append("content-type: text/plain; charset=utf-8\r\n");
  }
  if (!has_content_length) {
    out.append("content-length: ");
    out.append(std::to_string(res.body.size()));
    out.append("\r\n");
  }
  if (!has_connection) {
    out.append("connection: close\r\n");
  }
  out.append("\r\n");
  out.append(res.body);
  return out;
}

}  // namespace

struct HttpServer::Impl {
  HttpServerConfig config;
  int listen_fd = -1;
  std::uint16_t bound_port = 0;
  std::atomic<bool> running{false};
  std::atomic<bool> stopping{false};
  std::thread accept_thread;

  // Worker pool: bounded queue of accepted fds.
  std::vector<std::thread> workers;
  std::mutex queue_mutex;
  std::condition_variable queue_cv;
  std::deque<int> connection_queue;
  static constexpr std::size_t kMaxQueueDepth = 256;

  struct ExactRoute {
    std::string method;
    std::string path;
    RouteHandler handler;
  };
  struct PrefixRoute {
    std::string method;
    std::string prefix;
    RouteHandler handler;
  };
  std::vector<ExactRoute> exact_routes;
  std::vector<PrefixRoute> prefix_routes;
  std::mutex routes_mutex;

  Response dispatch(const Request& req) {
    std::lock_guard<std::mutex> g(routes_mutex);
    for (const auto& r : exact_routes) {
      if (r.method == req.method && r.path == req.path) {
        try {
          return r.handler(req);
        } catch (const std::exception& e) {
          return Response::plain(500, std::string{"internal error: "} + e.what());
        } catch (...) {
          return Response::plain(500, "internal error");
        }
      }
    }
    for (const auto& r : prefix_routes) {
      if (r.method == req.method && req.path.compare(0, r.prefix.size(), r.prefix) == 0 &&
          req.path.size() > r.prefix.size()) {
        try {
          return r.handler(req);
        } catch (const std::exception& e) {
          return Response::plain(500, std::string{"internal error: "} + e.what());
        } catch (...) {
          return Response::plain(500, "internal error");
        }
      }
    }
    // 405 if path matches but method doesn't; otherwise 404.
    bool path_known = false;
    for (const auto& r : exact_routes) {
      if (r.path == req.path) {
        path_known = true;
        break;
      }
    }
    if (path_known) {
      return Response::plain(405, "method not allowed");
    }
    return Response::plain(404, "not found");
  }

  // NOLINTNEXTLINE(readability-function-cognitive-complexity)
  void handle_connection(int fd) {
    auto deadline = std::chrono::steady_clock::now() + config.request_timeout;
    std::string buffer;
    set_nonblock(fd);

    ssize_t hr = read_until_headers(fd, buffer, config.max_header_bytes, deadline);
    if (hr == -2) {
      auto resp = format_response(Response::plain(431, "request header fields too large"));
      (void)write_all(fd, resp, deadline);
      ::close(fd);
      return;
    }
    if (hr == -3 || hr == -4) {
      auto resp = format_response(Response::plain(408, "request timeout"));
      (void)write_all(fd, resp, deadline);
      ::close(fd);
      return;
    }
    if (hr < 0) {
      ::close(fd);
      return;
    }

    auto header_end = buffer.find("\r\n\r\n");
    if (header_end == std::string::npos) {
      ::close(fd);
      return;
    }
    Request req = parse_request_head(buffer, header_end);
    if (req.method.empty() || req.path.empty()) {
      auto resp = format_response(Response::plain(400, "bad request"));
      (void)write_all(fd, resp, deadline);
      ::close(fd);
      return;
    }
    if (auto cid = req.header("x-correlation-id"); cid.has_value()) {
      req.correlation_id = std::string{*cid};
    }
    // Peer string.
    {
      sockaddr_storage ss{};
      socklen_t slen = sizeof(ss);
      if (::getpeername(fd, reinterpret_cast<sockaddr*>(&ss), &slen) == 0) {
        std::array<char, INET6_ADDRSTRLEN> host{};
        if (ss.ss_family == AF_INET) {
          const auto* sa = reinterpret_cast<const sockaddr_in*>(&ss);
          ::inet_ntop(AF_INET, &sa->sin_addr, host.data(), host.size());
          req.peer = std::string{host.data()} + ":" + std::to_string(ntohs(sa->sin_port));
        } else if (ss.ss_family == AF_INET6) {
          const auto* sa = reinterpret_cast<const sockaddr_in6*>(&ss);
          ::inet_ntop(AF_INET6, &sa->sin6_addr, host.data(), host.size());
          req.peer = std::string{"["} + host.data() + "]:" + std::to_string(ntohs(sa->sin6_port));
        }
      }
    }
    // Content-length and body.
    std::size_t header_total = header_end + 4;
    std::size_t body_in_buf = buffer.size() - header_total;
    std::size_t content_length = 0;
    if (auto cl = req.header("content-length"); cl.has_value()) {
      try {
        content_length = static_cast<std::size_t>(std::stoull(std::string(*cl)));
      } catch (...) {
        auto resp = format_response(Response::plain(400, "bad content-length"));
        (void)write_all(fd, resp, deadline);
        ::close(fd);
        return;
      }
    }
    if (content_length > config.max_body_bytes) {
      auto resp = format_response(Response::plain(413, "payload too large"));
      (void)write_all(fd, resp, deadline);
      ::close(fd);
      return;
    }
    if (content_length > 0) {
      std::string body(buffer, header_total, body_in_buf);
      std::size_t need_total = body.size() + (content_length - body.size());
      if (body.size() < content_length) {
        ssize_t br = read_remaining_body(fd, body, content_length, deadline);
        if (br == -3 || br == -4) {
          auto resp = format_response(Response::plain(408, "request timeout"));
          (void)write_all(fd, resp, deadline);
          ::close(fd);
          return;
        }
        if (br < 0) {
          ::close(fd);
          return;
        }
      }
      if (body.size() > config.max_body_bytes) {
        auto resp = format_response(Response::plain(413, "payload too large"));
        (void)write_all(fd, resp, deadline);
        ::close(fd);
        return;
      }
      req.body = std::move(body);
      (void)need_total;
    }
    // Dispatch.
    Response response = dispatch(req);
    auto wire = format_response(response);
    (void)write_all(fd, wire, deadline);
    // Linger then close.
    ::shutdown(fd, SHUT_WR);
    ::close(fd);
  }

  void worker_loop() {
    while (true) {
      int fd = -1;
      {
        std::unique_lock<std::mutex> g(queue_mutex);
        queue_cv.wait(g, [this] { return !connection_queue.empty() || stopping.load(); });
        if (connection_queue.empty()) {
          if (stopping.load()) {
            return;
          }
          continue;
        }
        fd = connection_queue.front();
        connection_queue.pop_front();
      }
      if (fd < 0) {
        continue;
      }
      handle_connection(fd);
    }
  }

  void accept_loop() {
    while (!stopping.load()) {
      pollfd p{listen_fd, POLLIN, 0};
      int pr = ::poll(&p, 1, 200);
      if (pr < 0) {
        if (errno == EINTR) {
          continue;
        }
        break;
      }
      if (pr == 0) {
        continue;
      }
      sockaddr_storage ss{};
      socklen_t slen = sizeof(ss);
      int fd = ::accept(listen_fd, reinterpret_cast<sockaddr*>(&ss), &slen);
      if (fd < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) {
          continue;
        }
        break;
      }
      int one = 1;
      ::setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
      {
        std::unique_lock<std::mutex> g(queue_mutex);
        if (connection_queue.size() >= kMaxQueueDepth) {
          ::close(fd);
          continue;
        }
        connection_queue.push_back(fd);
      }
      queue_cv.notify_one();
    }
  }
};

HttpServer::HttpServer() : impl_(std::make_unique<Impl>()) {}

HttpServer::~HttpServer() {
  stop();
}

void HttpServer::add_route(std::string method, std::string path, RouteHandler handler) {
  std::lock_guard<std::mutex> g(impl_->routes_mutex);
  impl_->exact_routes.push_back(
      Impl::ExactRoute{std::move(method), std::move(path), std::move(handler)});
}

void HttpServer::add_prefix_route(std::string method, std::string prefix, RouteHandler handler) {
  std::lock_guard<std::mutex> g(impl_->routes_mutex);
  impl_->prefix_routes.push_back(
      Impl::PrefixRoute{std::move(method), std::move(prefix), std::move(handler)});
}

Result<void> HttpServer::start(const HttpServerConfig& config) {
  if (impl_->running.load()) {
    return Result<void>{};
  }
  if (config.bind_host.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "http server: bind_host is empty");
  }
  if (!is_loopback_literal(config.bind_host) && !config.allow_non_loopback) {
    return unexpected(
        Error::Code::Unsupported,
        std::string{"http server: refuses to bind non-loopback host: "} + config.bind_host);
  }
  impl_->config = config;

  int fd = ::socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) {
    return unexpected(Error::Code::LoadFailed,
                      std::string{"http server: socket() failed: "} + std::strerror(errno));
  }
  int one = 1;
  ::setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
#ifdef SO_REUSEPORT
  ::setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one, sizeof(one));
#endif

  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_port = htons(config.bind_port);
  std::string host_for_inet = config.bind_host == "localhost" ? "127.0.0.1" : config.bind_host;
  if (::inet_pton(AF_INET, host_for_inet.c_str(), &addr.sin_addr) != 1) {
    ::close(fd);
    return unexpected(Error::Code::ConfigInvalid,
                      std::string{"http server: inet_pton failed for "} + host_for_inet);
  }
  if (::bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
    int saved = errno;
    ::close(fd);
    return unexpected(Error::Code::LoadFailed,
                      std::string{"http server: bind() failed: "} + std::strerror(saved));
  }
  if (::listen(fd, 64) < 0) {
    int saved = errno;
    ::close(fd);
    return unexpected(Error::Code::LoadFailed,
                      std::string{"http server: listen() failed: "} + std::strerror(saved));
  }
  // Read the assigned port back.
  sockaddr_in bound{};
  socklen_t blen = sizeof(bound);
  if (::getsockname(fd, reinterpret_cast<sockaddr*>(&bound), &blen) == 0) {
    impl_->bound_port = ntohs(bound.sin_port);
  } else {
    impl_->bound_port = config.bind_port;
  }
  impl_->listen_fd = fd;
  impl_->running.store(true);
  impl_->stopping.store(false);

  // Spawn workers.
  impl_->workers.clear();
  for (std::size_t i = 0; i < std::max<std::size_t>(1, config.worker_thread_count); ++i) {
    impl_->workers.emplace_back([this] { impl_->worker_loop(); });
  }
  impl_->accept_thread = std::thread([this] { impl_->accept_loop(); });
  return Result<void>{};
}

void HttpServer::stop() {
  if (!impl_->running.exchange(false)) {
    return;
  }
  impl_->stopping.store(true);
  // Close listen socket to unblock accept_loop.
  if (impl_->listen_fd >= 0) {
    ::shutdown(impl_->listen_fd, SHUT_RDWR);
    ::close(impl_->listen_fd);
    impl_->listen_fd = -1;
  }
  if (impl_->accept_thread.joinable()) {
    impl_->accept_thread.join();
  }
  // Wake workers, drain remaining connections without dispatch.
  {
    std::lock_guard<std::mutex> g(impl_->queue_mutex);
    for (int fd : impl_->connection_queue) {
      if (fd >= 0) {
        ::close(fd);
      }
    }
    impl_->connection_queue.clear();
  }
  impl_->queue_cv.notify_all();
  for (auto& w : impl_->workers) {
    if (w.joinable()) {
      w.join();
    }
  }
  impl_->workers.clear();
  impl_->bound_port = 0;
}

bool HttpServer::is_running() const noexcept {
  return impl_->running.load();
}

std::uint16_t HttpServer::bound_port() const noexcept {
  return impl_->bound_port;
}

}  // namespace tensorplate::http
