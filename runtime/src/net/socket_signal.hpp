// SPDX-License-Identifier: Apache-2.0
//
// Issue #19: library-local SIGPIPE suppression for socket writes.
//
// `tp_runtime` is a library and must not rely on the embedding binary
// installing a process-wide `SIGPIPE` ignore. When a peer closes the
// connection before or during a write, a plain `send()` raises
// `SIGPIPE`, whose default disposition terminates the process before the
// write helper can observe `EPIPE` and return a typed error.
//
// POSIX exposes two non-overlapping mechanisms; we apply whichever the
// platform provides, and applying both is harmless:
//
//   - Linux / modern POSIX: pass `MSG_NOSIGNAL` to each `send()` so the
//     signal is suppressed for that one call (`SO_NOSIGPIPE` is absent).
//   - macOS / BSD: older SDKs do not define `MSG_NOSIGNAL`; instead the
//     per-socket `SO_NOSIGPIPE` option suppresses `SIGPIPE` for every
//     write on that fd. It is set once, right after the fd is created or
//     accepted (`MSG_NOSIGNAL` is then 0, a no-op flag).
//
// Keeping the platform selection here lets the HTTP server and the
// Unix-socket helper share one tested policy without `#ifdef` noise at
// the call sites.

#pragma once

#include <sys/socket.h>

namespace tensorplate::net {

/// `send()` flag that suppresses `SIGPIPE` per call where the platform
/// supports it (Linux). 0 elsewhere (macOS/BSD rely on `SO_NOSIGPIPE`,
/// applied by `suppress_sigpipe` below), where it is an inert flag.
#if defined(MSG_NOSIGNAL)
inline constexpr int kSendNoSignal = MSG_NOSIGNAL;
#else
inline constexpr int kSendNoSignal = 0;
#endif

/// Suppress `SIGPIPE` for all writes on `fd` on platforms that expose a
/// per-socket option (macOS/BSD `SO_NOSIGPIPE`). No-op where writes use
/// the per-call `MSG_NOSIGNAL` flag instead (Linux).
///
/// Best-effort: a `setsockopt` failure is intentionally ignored. The
/// per-call flag still applies on platforms that have it, and on the
/// platforms that need `SO_NOSIGPIPE` the call only fails for fds that
/// are already invalid (in which case the subsequent `send` fails
/// cleanly with its own typed error). Callers therefore need not react.
inline void suppress_sigpipe(int fd) noexcept {
#if defined(SO_NOSIGPIPE)
  const int one = 1;
  ::setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one));
#else
  (void)fd;
#endif
}

}  // namespace tensorplate::net
