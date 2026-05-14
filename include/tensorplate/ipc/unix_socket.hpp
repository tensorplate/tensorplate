// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F04-T02: Unix-domain socket helpers.
//
// Minimal RAII wrappers around POSIX socket primitives used by the
// Python/PyTorch sidecar adapter (V01-E05-F05). The helpers do not
// know anything about sidecar message semantics; they handle bounded
// reads and writes with monotonic-deadline support so the adapter can
// implement timeouts (V01-E05-F05-T03).
//
// Threading: each `UnixSocket` is owned by one thread. The adapter
// serializes all I/O on it via the V01-E04 session NVI's per-session
// concurrency model.

#pragma once

#include <chrono>
#include <cstddef>
#include <span>
#include <string>
#include <string_view>

#include "tensorplate/core/result.hpp"

namespace tensorplate::ipc {

/// RAII Unix-domain stream socket. Move-only. Closes the underlying
/// fd in the destructor; closing an already-closed socket is a no-op.
class UnixSocket {
 public:
  using Clock = std::chrono::steady_clock;
  using TimePoint = Clock::time_point;

  UnixSocket() noexcept = default;
  ~UnixSocket();

  UnixSocket(const UnixSocket&) = delete;
  UnixSocket& operator=(const UnixSocket&) = delete;
  UnixSocket(UnixSocket&& other) noexcept;
  UnixSocket& operator=(UnixSocket&& other) noexcept;

  /// Create an unbound stream socket. Returns a `LoadFailed` error if
  /// the OS rejects the request (EMFILE, ENFILE, ...).
  static Result<UnixSocket> create_stream();

  /// Connect to a `SOCK_STREAM` Unix-domain socket bound at `path`.
  /// Returns:
  ///   - `LoadFailed` if `path` does not exist or refuses the connect,
  ///   - `Timeout` if the deadline elapses before connect succeeds.
  ///
  /// `path` is constrained by `sockaddr_un::sun_path` (typically 108
  /// bytes on Linux). Paths longer than that surface as ConfigInvalid.
  Result<void> connect(std::string_view path, TimePoint deadline);

  /// Bind to `path` and listen for one client. Returns ConfigInvalid
  /// when the path is too long.
  Result<void> bind_and_listen(std::string_view path, int backlog = 1);

  /// Accept one client connection. Blocks until the deadline elapses.
  Result<UnixSocket> accept(TimePoint deadline);

  /// Read exactly `out.size()` bytes from the socket. Honors the
  /// deadline (`Timeout`) and surfaces `LoadFailed` if the peer
  /// closes early.
  Result<void> read_exact(std::span<std::byte> out, TimePoint deadline);

  /// Write all of `bytes` to the socket. Honors the deadline.
  Result<void> write_all(std::span<const std::byte> bytes, TimePoint deadline);

  /// Close the underlying fd. Idempotent.
  void close() noexcept;

  /// Underlying fd accessor, primarily for tests; -1 if closed.
  [[nodiscard]] int fd() const noexcept { return fd_; }

  /// True iff the socket holds an open fd.
  [[nodiscard]] bool is_open() const noexcept { return fd_ >= 0; }

 private:
  explicit UnixSocket(int fd) noexcept : fd_(fd) {}

  int fd_ = -1;
};

}  // namespace tensorplate::ipc
