// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F08: Tiny loopback HTTP/1.1 client used by serving worker
// integration tests. Single request per connection; matches the
// `runtime/src/http/http_server.cpp` server's keep-alive behavior.

#pragma once

#include <cerrno>
#include <chrono>
#include <cstring>
#include <stdexcept>
#include <string>
#include <string_view>
#include <vector>

#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

namespace tensorplate::testing {

struct HttpClientResponse {
  int status = 0;
  std::string status_line;
  std::vector<std::pair<std::string, std::string>> headers;
  std::string body;

  [[nodiscard]] std::string header(std::string_view name) const {
    for (const auto& [n, v] : headers) {
      if (n == name) {
        return v;
      }
    }
    return {};
  }
};

class HttpClient {
 public:
  HttpClient(std::string host, std::uint16_t port) : host_(std::move(host)), port_(port) {}

  HttpClientResponse send(std::string_view method, std::string_view path, std::string_view body,
                          const std::vector<std::pair<std::string, std::string>>& headers = {}) {
    int fd = ::socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
      throw std::runtime_error(std::string{"socket: "} + std::strerror(errno));
    }
    sockaddr_in addr{};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port_);
    if (::inet_pton(AF_INET, host_.c_str(), &addr.sin_addr) != 1) {
      ::close(fd);
      throw std::runtime_error("inet_pton failed");
    }
    if (::connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
      ::close(fd);
      throw std::runtime_error(std::string{"connect: "} + std::strerror(errno));
    }
    std::string req;
    req.reserve(256 + body.size());
    req.append(method);
    req.append(" ");
    req.append(path);
    req.append(" HTTP/1.1\r\n");
    req.append("host: ");
    req.append(host_);
    req.append("\r\n");
    bool has_ct = false;
    for (const auto& [k, v] : headers) {
      req.append(k);
      req.append(": ");
      req.append(v);
      req.append("\r\n");
      if (k == "content-type") {
        has_ct = true;
      }
    }
    if (!body.empty()) {
      if (!has_ct) {
        req.append("content-type: application/json\r\n");
      }
      req.append("content-length: ");
      req.append(std::to_string(body.size()));
      req.append("\r\n");
    } else {
      req.append("content-length: 0\r\n");
    }
    req.append("connection: close\r\n\r\n");
    req.append(body);
    if (::send(fd, req.data(), req.size(), 0) < 0) {
      ::close(fd);
      throw std::runtime_error(std::string{"send: "} + std::strerror(errno));
    }
    // Read entire response until EOF.
    std::string raw;
    char buf[4096];
    while (true) {
      ssize_t n = ::recv(fd, buf, sizeof(buf), 0);
      if (n <= 0) {
        break;
      }
      raw.append(buf, static_cast<std::size_t>(n));
    }
    ::close(fd);
    return parse(raw);
  }

  HttpClientResponse get(std::string_view path,
                         const std::vector<std::pair<std::string, std::string>>& headers = {}) {
    return send("GET", path, {}, headers);
  }

  HttpClientResponse post(std::string_view path, std::string_view body,
                          const std::vector<std::pair<std::string, std::string>>& headers = {}) {
    return send("POST", path, body, headers);
  }

 private:
  static HttpClientResponse parse(const std::string& raw) {
    HttpClientResponse out;
    auto headers_end = raw.find("\r\n\r\n");
    if (headers_end == std::string::npos) {
      throw std::runtime_error("http client: no header terminator");
    }
    std::string head = raw.substr(0, headers_end);
    auto sl_end = head.find("\r\n");
    if (sl_end == std::string::npos) {
      throw std::runtime_error("http client: no status line");
    }
    out.status_line = head.substr(0, sl_end);
    // HTTP/1.1 <code> <reason>
    auto sp1 = out.status_line.find(' ');
    auto sp2 = sp1 != std::string::npos ? out.status_line.find(' ', sp1 + 1) : std::string::npos;
    if (sp1 != std::string::npos) {
      try {
        out.status = std::stoi(out.status_line.substr(sp1 + 1, sp2 - sp1 - 1));
      } catch (...) {
        out.status = 0;
      }
    }
    std::size_t pos = sl_end + 2;
    while (pos < head.size()) {
      auto eol = head.find("\r\n", pos);
      if (eol == std::string::npos || eol == pos) {
        break;
      }
      std::string line = head.substr(pos, eol - pos);
      auto colon = line.find(':');
      if (colon != std::string::npos) {
        std::string name = line.substr(0, colon);
        for (auto& c : name) {
          if (c >= 'A' && c <= 'Z') {
            c = static_cast<char>(c + ('a' - 'A'));
          }
        }
        std::string value = line.substr(colon + 1);
        while (!value.empty() && (value.front() == ' ' || value.front() == '\t')) {
          value.erase(value.begin());
        }
        while (!value.empty() && (value.back() == ' ' || value.back() == '\t')) {
          value.pop_back();
        }
        out.headers.emplace_back(std::move(name), std::move(value));
      }
      pos = eol + 2;
    }
    out.body = raw.substr(headers_end + 4);
    return out;
  }

  std::string host_;
  std::uint16_t port_;
};

}  // namespace tensorplate::testing
