// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F04-T01 / T02: Sidecar IPC codec - public header.
//
// Wire format
//   Every sidecar IPC message on the Unix-domain socket is framed as:
//
//     +-----+---------+------------+-------------+--------------+-----------------+
//     | u32 |  u32    |  u32       |  u32        |  N bytes     |  M bytes        |
//     | mag |  ver    |  hdr_len   |  payload_len|  JSON header |  tensor payload |
//     +-----+---------+------------+-------------+--------------+-----------------+
//
//   - All u32 fields are big-endian.
//   - Magic is `'T' 'P' 'S' 'C'` (0x54505343).
//   - Version is `kProtocolWireVersion` (currently 1).
//   - Header is the JSON object described by
//     `protocol/schemas/python_pytorch_ipc.json`.
//   - Tensor payload is raw bytes; offsets and lengths inside the
//     payload are recorded in the header's `tensors[]` array. Empty
//     payload is permitted on messages that carry only control data.
//
// This codec deliberately does not depend on `tp_runtime` core types.
// It is used by the sidecar adapter (V01-E05-F05) on the C++ side and
// is mirrored in `backends/python_pytorch/.../codec.py` on the Python
// side.

#pragma once

#include <cstddef>
#include <cstdint>
#include <span>
#include <string>
#include <vector>

#include "tensorplate/core/result.hpp"

namespace tensorplate::ipc {

/// Sidecar wire-protocol version. Bumped when the on-wire envelope
/// changes (independent of the JSON message version inside the
/// `schema_version` field, which is the inner protocol version).
inline constexpr std::uint32_t kProtocolWireVersion = 1;

/// Sidecar frame magic ('T','P','S','C' big-endian).
inline constexpr std::uint32_t kFrameMagic = 0x54505343u;

/// Inclusive maxima for malformed-frame rejection. Generous but
/// bounded; sidecars sending more must split the message.
inline constexpr std::uint32_t kMaxHeaderLen = 1u << 20;     // 1 MiB
inline constexpr std::uint32_t kMaxPayloadLen = 256u << 20;  // 256 MiB

/// Decoded sidecar frame. Header is the JSON string as transmitted on
/// the wire so the caller can parse with the same JSON library used
/// throughout the runtime.
struct SidecarFrame {
  std::string json_header;
  std::vector<std::byte> payload;
};

/// Encode `frame` into a contiguous byte buffer ready to write to a
/// socket. Returns `Error::Code::ConfigInvalid` if header or payload
/// exceed `kMaxHeaderLen` / `kMaxPayloadLen`.
[[nodiscard]] Result<std::vector<std::byte>> encode_frame(const SidecarFrame& frame);

/// Decode the first frame from `bytes`. On success returns the frame
/// and writes the number of bytes consumed to `*consumed`. Returns
/// `Error::Code::ConfigInvalid` for any framing violation (bad magic,
/// bad version, header/payload too large, truncated buffer).
///
/// The codec is stream-friendly: it returns `Error::Code::NotReady`
/// (with the message "incomplete frame") when the buffer holds a
/// prefix of a valid frame but the rest has not arrived yet. Callers
/// distinguish "need more bytes" from "malformed frame" by inspecting
/// the typed error code.
[[nodiscard]] Result<SidecarFrame> decode_frame(std::span<const std::byte> bytes,
                                                std::size_t* consumed);

/// Convenience: decode all frames in `bytes`. Stops at the first
/// incomplete frame (returns the remainder via `*consumed`).
[[nodiscard]] Result<std::vector<SidecarFrame>> decode_frames(std::span<const std::byte> bytes,
                                                              std::size_t* consumed);

}  // namespace tensorplate::ipc
