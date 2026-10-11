// SPDX-License-Identifier: Apache-2.0

#include "sidecar_frame_io.hpp"

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <span>
#include <string>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

namespace tensorplate::adapters::python_pytorch {

std::string next_message_id() {
  static std::atomic<std::uint64_t> seq{0};
  return "tp-msg-" + std::to_string(seq.fetch_add(1, std::memory_order_relaxed));
}

Result<void> write_frame(ipc::UnixSocket& sock, const ipc::SidecarFrame& frame,
                         ipc::UnixSocket::TimePoint deadline) {
  auto enc = ipc::encode_frame(frame);
  if (!enc.has_value()) {
    return unexpected(enc.error());
  }
  return sock.write_all(std::span<const std::byte>(enc.value()), deadline);
}

Result<ipc::SidecarFrame> read_frame(ipc::UnixSocket& sock, ipc::UnixSocket::TimePoint deadline,
                                     std::size_t max_payload_bytes) {
  std::vector<std::byte> buf(16);
  if (auto r = sock.read_exact(std::span<std::byte>(buf.data(), buf.size()), deadline);
      !r.has_value()) {
    return unexpected(r.error());
  }
  const auto* p = buf.data();
  auto u32 = [&](std::size_t off) {
    return (static_cast<std::uint32_t>(p[off]) << 24) |
           (static_cast<std::uint32_t>(p[off + 1]) << 16) |
           (static_cast<std::uint32_t>(p[off + 2]) << 8) | static_cast<std::uint32_t>(p[off + 3]);
  };
  if (u32(0) != ipc::kFrameMagic) {
    return unexpected(Error::Code::InferenceFailed, "sidecar frame magic mismatch");
  }
  if (u32(4) != ipc::kProtocolWireVersion) {
    return unexpected(Error::Code::InferenceFailed,
                      "sidecar wire version " + std::to_string(u32(4)) + " unsupported");
  }
  const std::uint32_t hdr_len = u32(8);
  const std::uint32_t pld_len = u32(12);
  if (hdr_len > ipc::kMaxHeaderLen || pld_len > ipc::kMaxPayloadLen ||
      pld_len > max_payload_bytes) {
    return unexpected(Error::Code::InferenceFailed, "sidecar frame size exceeds limit");
  }
  buf.resize(16 + hdr_len + pld_len);
  if (hdr_len + pld_len > 0) {
    if (auto r =
            sock.read_exact(std::span<std::byte>(buf.data() + 16, hdr_len + pld_len), deadline);
        !r.has_value()) {
      return unexpected(r.error());
    }
  }
  return ipc::decode_frame(buf, nullptr);
}

}  // namespace tensorplate::adapters::python_pytorch
