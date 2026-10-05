// SPDX-License-Identifier: Apache-2.0
//
// The agent's side of a serving worker's control channel, for tests: a
// socket pair whose worker end a child process inherits as fd 0, the way the
// agent passes it, and newline-delimited frames on the other end.

#pragma once

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <csignal>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

namespace tensorplate::testing {

#ifdef MSG_NOSIGNAL
inline constexpr int kSendWithoutSigpipe = MSG_NOSIGNAL;
#else
inline constexpr int kSendWithoutSigpipe = 0;
#endif

/// Owns one descriptor.
class OwnedFd {
 public:
  OwnedFd() = default;
  explicit OwnedFd(int fd) noexcept : fd_(fd) {}
  OwnedFd(OwnedFd&& other) noexcept : fd_(std::exchange(other.fd_, -1)) {}
  OwnedFd& operator=(OwnedFd&& other) noexcept {
    if (this != &other) {
      reset();
      fd_ = std::exchange(other.fd_, -1);
    }
    return *this;
  }
  OwnedFd(const OwnedFd&) = delete;
  OwnedFd& operator=(const OwnedFd&) = delete;
  ~OwnedFd() { reset(); }

  [[nodiscard]] int get() const noexcept { return fd_; }
  [[nodiscard]] int release() noexcept { return std::exchange(fd_, -1); }
  void reset() noexcept {
    if (fd_ >= 0) {
      ::close(fd_);
      fd_ = -1;
    }
  }

 private:
  int fd_ = -1;
};

/// A connected stream pair. Both ends are close-on-exec, as the agent
/// creates them; a child gets one only by having it duplicated onto fd 0.
struct ControlSocketPair {
  OwnedFd agent;
  OwnedFd worker;

  static ControlSocketPair create() {
    int fds[2] = {-1, -1};
    if (::socketpair(AF_UNIX, SOCK_STREAM, 0, fds) != 0) {
      throw std::runtime_error("socketpair failed");
    }
    for (const int fd : fds) {
      ::fcntl(fd, F_SETFD, FD_CLOEXEC);
#ifdef SO_NOSIGPIPE
      const int on = 1;
      ::setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, sizeof(on));
#endif
    }
    return {OwnedFd{fds[0]}, OwnedFd{fds[1]}};
  }
};

/// Puts `fd` on this process's fd 0 and restores what was there when it
/// goes out of scope.
class StdinReplaced {
 public:
  explicit StdinReplaced(int fd) : saved_(::dup(STDIN_FILENO)) {
    if (saved_ < 0 || ::dup2(fd, STDIN_FILENO) != STDIN_FILENO) {
      throw std::runtime_error("cannot replace fd 0");
    }
  }
  StdinReplaced(const StdinReplaced&) = delete;
  StdinReplaced& operator=(const StdinReplaced&) = delete;
  ~StdinReplaced() {
    ::dup2(saved_, STDIN_FILENO);
    ::close(saved_);
  }

 private:
  int saved_;
};

/// Inode of what a descriptor refers to: for a socket, the number
/// /proc/<pid>/fd shows as "socket:[<inode>]".
inline std::uint64_t inode_of(int fd) {
  struct stat info {};
  if (::fstat(fd, &info) != 0) {
    throw std::runtime_error("fstat failed");
  }
  return static_cast<std::uint64_t>(info.st_ino);
}

/// A loopback TCP port nothing listens on right now.
inline std::uint16_t unused_loopback_port() {
  const OwnedFd probe{::socket(AF_INET, SOCK_STREAM, 0)};
  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t size = sizeof(addr);
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast)
  auto* generic = reinterpret_cast<sockaddr*>(&addr);
  if (probe.get() < 0 || ::bind(probe.get(), generic, size) != 0 ||
      ::getsockname(probe.get(), generic, &size) != 0) {
    throw std::runtime_error("cannot pick a loopback port");
  }
  return ntohs(addr.sin_port);
}

/// True if something accepts TCP connections on the loopback `port`.
inline bool loopback_port_listening(std::uint16_t port) {
  const OwnedFd client{::socket(AF_INET, SOCK_STREAM, 0)};
  sockaddr_in addr{};
  addr.sin_family = AF_INET;
  addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  addr.sin_port = htons(port);
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast)
  return client.get() >= 0 &&
         ::connect(client.get(), reinterpret_cast<const sockaddr*>(&addr), sizeof(addr)) == 0;
}

/// A child process whose fd 0 is `stdin_fd` and whose stderr goes to a file.
class ChildProcess {
 public:
  /// `environment` holds NAME=value entries added to the child's own.
  static ChildProcess spawn(const std::vector<std::string>& arguments, int stdin_fd,
                            const std::filesystem::path& stderr_file,
                            const std::vector<std::string>& environment = {}) {
    std::vector<char*> argv;
    for (const auto& argument : arguments) {
      // NOLINTNEXTLINE(cppcoreguidelines-pro-type-const-cast)
      argv.push_back(const_cast<char*>(argument.c_str()));
    }
    argv.push_back(nullptr);
    const std::string stderr_path = stderr_file.string();
    const pid_t pid = ::fork();
    if (pid < 0) {
      throw std::runtime_error("fork failed");
    }
    if (pid == 0) {
      const int log = ::open(stderr_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0600);
      if (::dup2(stdin_fd, STDIN_FILENO) < 0 || log < 0 || ::dup2(log, STDERR_FILENO) < 0) {
        ::_exit(126);
      }
      for (const auto& entry : environment) {
        const auto equals = entry.find('=');
        ::setenv(entry.substr(0, equals).c_str(), entry.substr(equals + 1).c_str(), 1);
      }
      ::execv(argv[0], argv.data());
      ::_exit(127);
    }
    return ChildProcess{pid, stderr_file};
  }

  ChildProcess(ChildProcess&& other) noexcept
      : pid_(std::exchange(other.pid_, -1)), stderr_file_(std::move(other.stderr_file_)) {}
  ChildProcess& operator=(ChildProcess&&) = delete;
  ChildProcess(const ChildProcess&) = delete;
  ChildProcess& operator=(const ChildProcess&) = delete;
  ~ChildProcess() { kill_and_reap(); }

  [[nodiscard]] pid_t pid() const noexcept { return pid_; }

  /// The exit status if the child exits within `timeout`: its exit code, or
  /// 128 plus the signal that ended it.
  [[nodiscard]] std::optional<int> wait_exit(std::chrono::milliseconds timeout) {
    const auto give_up = std::chrono::steady_clock::now() + timeout;
    while (pid_ > 0) {
      int status = 0;
      if (::waitpid(pid_, &status, WNOHANG) == pid_) {
        pid_ = -1;
        return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
      }
      if (std::chrono::steady_clock::now() >= give_up) {
        return std::nullopt;
      }
      std::this_thread::sleep_for(std::chrono::milliseconds{5});
    }
    return std::nullopt;
  }

  void kill_and_reap() noexcept {
    if (pid_ > 0) {
      ::kill(pid_, SIGKILL);
      ::waitpid(pid_, nullptr, 0);
      pid_ = -1;
    }
  }

  [[nodiscard]] std::string stderr_text() const {
    std::ifstream in(stderr_file_);
    return {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
  }

 private:
  ChildProcess(pid_t pid, std::filesystem::path stderr_file)
      : pid_(pid), stderr_file_(std::move(stderr_file)) {}

  pid_t pid_ = -1;
  std::filesystem::path stderr_file_;
};

/// Writes all of `frame`; false once the peer is gone.
inline bool send_frame(int fd, std::string_view frame) {
  while (!frame.empty()) {
    const auto sent = ::send(fd, frame.data(), frame.size(), kSendWithoutSigpipe);
    if (sent <= 0) {
      return false;
    }
    frame.remove_prefix(static_cast<std::size_t>(sent));
  }
  return true;
}

/// How reading one frame ended.
enum class FrameRead { Frame, Closed, TimedOut };

/// Reads up to and including the next newline. `frame` holds what arrived.
inline FrameRead read_frame(int fd, std::chrono::milliseconds timeout, std::string& frame) {
  frame.clear();
  const auto give_up = std::chrono::steady_clock::now() + timeout;
  for (;;) {
    const auto left = std::chrono::duration_cast<std::chrono::milliseconds>(
        give_up - std::chrono::steady_clock::now());
    pollfd waiting{fd, POLLIN, 0};
    if (left.count() <= 0 || ::poll(&waiting, 1, static_cast<int>(left.count())) <= 0) {
      return FrameRead::TimedOut;
    }
    char byte = 0;
    const auto got = ::recv(fd, &byte, 1, 0);
    if (got == 0 || (got < 0 && errno != EINTR)) {
      return FrameRead::Closed;
    }
    if (got == 1) {
      frame.push_back(byte);
      if (byte == '\n') {
        return FrameRead::Frame;
      }
    }
  }
}
}  // namespace tensorplate::testing
