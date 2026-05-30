// SPDX-License-Identifier: Apache-2.0
//
// Library-local SIGPIPE suppression for socket writes, so a write to a
// peer-closed connection returns EPIPE instead of terminating the
// process. POSIX exposes two mechanisms; both are applied and each is
// inert where the other is active:
//   - MSG_NOSIGNAL on each send() (Linux).
//   - SO_NOSIGPIPE on each fd (macOS/BSD, which may lack MSG_NOSIGNAL).

#pragma once

#include <sys/socket.h>

namespace tensorplate::net {

/// `send()` flag that suppresses `SIGPIPE` per call (Linux); 0 where the
/// platform relies on `SO_NOSIGPIPE` instead.
#if defined(MSG_NOSIGNAL)
inline constexpr int kSendNoSignal = MSG_NOSIGNAL;
#else
inline constexpr int kSendNoSignal = 0;
#endif

/// Set `SO_NOSIGPIPE` on `fd` where available (macOS/BSD); no-op on
/// platforms that suppress per call via `MSG_NOSIGNAL`. Best-effort: a
/// failure is ignored, since the subsequent `send` still fails with its
/// own typed error.
inline void suppress_sigpipe(int fd) noexcept {
#if defined(SO_NOSIGPIPE)
  const int one = 1;
  ::setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof(one));
#else
  (void)fd;
#endif
}

}  // namespace tensorplate::net
