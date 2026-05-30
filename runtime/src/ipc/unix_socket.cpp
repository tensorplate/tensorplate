// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F04-T02: Unix-domain socket helper implementation.

#include "tensorplate/ipc/unix_socket.hpp"

#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstddef>
#include <cstring>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>

#include "tensorplate/core/error.hpp"

#include "net/socket_signal.hpp"

namespace tensorplate::ipc {

namespace {

constexpr std::size_t kSunPathMax = sizeof(sockaddr_un::sun_path);

// `fcntl` is variadic by signature; the wrapper centralizes the
// NOLINT suppression so callers stay readable.
int set_fcntl(int fd, int cmd, int arg) noexcept {
  return ::fcntl(fd, cmd, arg);  // NOLINT(cppcoreguidelines-pro-type-vararg)
}

// Treat sockaddr_un::sun_path as a writable char buffer with a known
// extent. This centralizes the
// `cppcoreguidelines-pro-bounds-array-to-pointer-decay` suppression so
// callers stay readable.
char* sun_path_ptr(sockaddr_un* addr) noexcept {
  return static_cast<char*>(
      addr->sun_path);  // NOLINT(cppcoreguidelines-pro-bounds-array-to-pointer-decay)
}

int millis_until(UnixSocket::TimePoint deadline) noexcept {
  const auto now = UnixSocket::Clock::now();
  if (deadline <= now) {
    return 0;
  }
  using ms = std::chrono::milliseconds;
  const auto remaining = std::chrono::duration_cast<ms>(deadline - now);
  // Cap at ~int max so poll() doesn't overflow.
  if (remaining.count() > 1'000'000'000) {
    return 1'000'000'000;
  }
  return static_cast<int>(remaining.count());
}

Error make_errno_error(Error::Code code, const std::string& what) {
  return Error::make(code, what + ": " + std::strerror(errno));
}

Result<void> wait_for(int fd, short events, UnixSocket::TimePoint deadline) {
  struct pollfd pfd {};
  pfd.fd = fd;
  pfd.events = events;
  pfd.revents = 0;
  while (true) {
    const int ms = millis_until(deadline);
    const int r = ::poll(&pfd, 1, ms);
    if (r > 0) {
      // For write waiters, POLLHUP/POLLERR mean the peer is gone and the
      // fd will never accept more data, so surface a typed error instead
      // of letting write_all() spin on EAGAIN. Read waiters are left
      // untouched: POLLHUP can accompany buffered data that recv() still
      // drains before reporting EOF.
      if ((events & POLLOUT) != 0 && (pfd.revents & (POLLHUP | POLLERR | POLLNVAL)) != 0) {
        return unexpected(Error::Code::LoadFailed, "unix-socket peer closed while writing");
      }
      return Result<void>{};
    }
    if (r == 0) {
      return unexpected(Error::Code::Timeout, "deadline elapsed waiting for socket");
    }
    if (errno == EINTR) {
      continue;
    }
    return unexpected(make_errno_error(Error::Code::LoadFailed, "poll failed"));
  }
}

Result<void> set_nonblock(int fd) {
  const int flags = set_fcntl(fd, F_GETFL, 0);
  if (flags < 0) {
    return unexpected(make_errno_error(Error::Code::LoadFailed, "fcntl(F_GETFL)"));
  }
  if (set_fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0) {
    return unexpected(make_errno_error(Error::Code::LoadFailed, "fcntl(F_SETFL O_NONBLOCK)"));
  }
  return Result<void>{};
}

Result<void> fill_sockaddr(sockaddr_un* addr, std::string_view path) {
  if (path.size() + 1 > kSunPathMax) {
    return unexpected(Error::Code::ConfigInvalid, "unix-socket path exceeds sun_path limit (" +
                                                      std::to_string(kSunPathMax) + " bytes)");
  }
  std::memset(addr, 0, sizeof(*addr));
  addr->sun_family = AF_UNIX;
  char* sun = sun_path_ptr(addr);
  std::memcpy(sun, path.data(), path.size());
  sun[path.size()] = '\0';
  return Result<void>{};
}

}  // namespace

UnixSocket::~UnixSocket() {
  close();
}

UnixSocket::UnixSocket(UnixSocket&& other) noexcept : fd_(other.fd_) {
  other.fd_ = -1;
}

UnixSocket& UnixSocket::operator=(UnixSocket&& other) noexcept {
  if (this != &other) {
    close();
    fd_ = other.fd_;
    other.fd_ = -1;
  }
  return *this;
}

void UnixSocket::close() noexcept {
  if (fd_ >= 0) {
    ::close(fd_);
    fd_ = -1;
  }
}

Result<UnixSocket> UnixSocket::create_stream() {
  const int fd = ::socket(AF_UNIX, SOCK_STREAM, 0);
  if (fd < 0) {
    return unexpected(make_errno_error(Error::Code::LoadFailed, "socket(AF_UNIX, SOCK_STREAM)"));
  }
  UnixSocket sock(fd);
  // Write to a peer-closed socket should return EPIPE, not raise SIGPIPE.
  net::suppress_sigpipe(fd);
  if (auto r = set_nonblock(fd); !r.has_value()) {
    return unexpected(r.error());
  }
  return sock;
}

// `connect` / `bind_and_listen` / `accept` / `read_exact` / `write_all`
// modify the kernel-side state of the file descriptor owned by `fd_`.
// clang-tidy's `readability-make-member-function-const` cannot see that
// through the opaque syscall and would otherwise flag every operation
// that does not write to the C++ member. The methods stay non-const
// because the externally observable behavior of the socket changes; the
// suppressions are localized to the affected declarations.

Result<void> UnixSocket::connect(  // NOLINT(readability-make-member-function-const)
    std::string_view path, TimePoint deadline) {
  if (fd_ < 0) {
    return unexpected(Error::Code::Internal, "UnixSocket::connect on closed socket");
  }
  sockaddr_un addr{};
  if (auto r = fill_sockaddr(&addr, path); !r.has_value()) {
    return unexpected(r.error());
  }

  while (true) {
    const int r = ::connect(fd_, reinterpret_cast<sockaddr*>(&addr), sizeof(addr));
    if (r == 0) {
      return Result<void>{};
    }
    if (errno == EINTR) {
      continue;
    }
    if (errno == EINPROGRESS || errno == EALREADY) {
      auto w = wait_for(fd_, POLLOUT, deadline);
      if (!w.has_value()) {
        return unexpected(w.error());
      }
      int err = 0;
      socklen_t len = sizeof(err);
      if (::getsockopt(fd_, SOL_SOCKET, SO_ERROR, &err, &len) < 0) {
        return unexpected(make_errno_error(Error::Code::LoadFailed, "getsockopt(SO_ERROR)"));
      }
      if (err == 0) {
        return Result<void>{};
      }
      errno = err;
      return unexpected(make_errno_error(Error::Code::LoadFailed, "unix-socket connect failed"));
    }
    if (errno == ENOENT) {
      return unexpected(Error::Code::LoadFailed,
                        "unix-socket connect failed: no such file: " + std::string(path));
    }
    return unexpected(make_errno_error(Error::Code::LoadFailed, "unix-socket connect failed"));
  }
}

Result<void> UnixSocket::bind_and_listen(  // NOLINT(readability-make-member-function-const)
    std::string_view path, int backlog) {
  if (fd_ < 0) {
    return unexpected(Error::Code::Internal, "UnixSocket::bind_and_listen on closed socket");
  }
  sockaddr_un addr{};
  if (auto r = fill_sockaddr(&addr, path); !r.has_value()) {
    return unexpected(r.error());
  }

  ::unlink(sun_path_ptr(&addr));
  if (::bind(fd_, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
    return unexpected(make_errno_error(Error::Code::LoadFailed, "unix-socket bind failed"));
  }
  if (::listen(fd_, backlog) < 0) {
    return unexpected(make_errno_error(Error::Code::LoadFailed, "unix-socket listen failed"));
  }
  return Result<void>{};
}

Result<UnixSocket> UnixSocket::accept(  // NOLINT(readability-make-member-function-const)
    TimePoint deadline) {
  if (fd_ < 0) {
    return unexpected(Error::Code::Internal, "UnixSocket::accept on closed socket");
  }
  while (true) {
    const int client = ::accept(fd_, nullptr, nullptr);
    if (client >= 0) {
      UnixSocket s(client);
      // Accepted fds do not inherit SO_NOSIGPIPE from the listener.
      net::suppress_sigpipe(client);
      if (auto r = set_nonblock(client); !r.has_value()) {
        return unexpected(r.error());
      }
      return s;
    }
    if (errno == EINTR) {
      continue;
    }
    if (errno == EAGAIN || errno == EWOULDBLOCK) {
      auto w = wait_for(fd_, POLLIN, deadline);
      if (!w.has_value()) {
        return unexpected(w.error());
      }
      continue;
    }
    return unexpected(make_errno_error(Error::Code::LoadFailed, "unix-socket accept failed"));
  }
}

Result<void> UnixSocket::read_exact(  // NOLINT(readability-make-member-function-const)
    std::span<std::byte> out, TimePoint deadline) {
  if (fd_ < 0) {
    return unexpected(Error::Code::Internal, "UnixSocket::read_exact on closed socket");
  }
  std::size_t offset = 0;
  while (offset < out.size()) {
    const auto n = ::recv(fd_, out.data() + offset, out.size() - offset, 0);
    if (n > 0) {
      offset += static_cast<std::size_t>(n);
      continue;
    }
    if (n == 0) {
      return unexpected(Error::Code::LoadFailed, "unix-socket peer closed mid-frame");
    }
    if (errno == EINTR) {
      continue;
    }
    if (errno == EAGAIN || errno == EWOULDBLOCK) {
      auto w = wait_for(fd_, POLLIN, deadline);
      if (!w.has_value()) {
        return unexpected(w.error());
      }
      continue;
    }
    return unexpected(make_errno_error(Error::Code::LoadFailed, "unix-socket recv failed"));
  }
  return Result<void>{};
}

Result<void> UnixSocket::write_all(  // NOLINT(readability-make-member-function-const)
    std::span<const std::byte> bytes, TimePoint deadline) {
  if (fd_ < 0) {
    return unexpected(Error::Code::Internal, "UnixSocket::write_all on closed socket");
  }
  std::size_t offset = 0;
  while (offset < bytes.size()) {
    const auto n = ::send(fd_, bytes.data() + offset, bytes.size() - offset, net::kSendNoSignal);
    if (n > 0) {
      offset += static_cast<std::size_t>(n);
      continue;
    }
    if (errno == EINTR) {
      continue;
    }
    if (errno == EAGAIN || errno == EWOULDBLOCK) {
      auto w = wait_for(fd_, POLLOUT, deadline);
      if (!w.has_value()) {
        return unexpected(w.error());
      }
      continue;
    }
    if (errno == EPIPE) {
      return unexpected(Error::Code::LoadFailed, "unix-socket peer closed (EPIPE)");
    }
    return unexpected(make_errno_error(Error::Code::LoadFailed, "unix-socket send failed"));
  }
  return Result<void>{};
}

}  // namespace tensorplate::ipc
