// SPDX-License-Identifier: Apache-2.0
//
// Whole sidecar frames on the adapter's socket, and the adapter's message
// ids. Shared by the session's synchronous exchange and the job channel.

#pragma once

#include <cstddef>
#include <string>

#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

namespace tensorplate::adapters::python_pytorch {

/// The next `message_id` of this process; never empty and never repeated.
[[nodiscard]] std::string next_message_id();

/// Writes one whole frame, or fails at `deadline`.
[[nodiscard]] Result<void> write_frame(ipc::UnixSocket& sock, const ipc::SidecarFrame& frame,
                                       ipc::UnixSocket::TimePoint deadline);

/// Reads one whole frame, or fails at `deadline`. A frame that declares a
/// payload of more than `max_payload_bytes` is refused from its prefix: its
/// body is neither allocated nor read.
/// @return Error::Code::InferenceFailed for a prefix that is not a frame's,
///   or one that declares more than the codec or the caller allows.
[[nodiscard]] Result<ipc::SidecarFrame> read_frame(
    ipc::UnixSocket& sock, ipc::UnixSocket::TimePoint deadline,
    std::size_t max_payload_bytes = ipc::kMaxPayloadLen);

}  // namespace tensorplate::adapters::python_pytorch
