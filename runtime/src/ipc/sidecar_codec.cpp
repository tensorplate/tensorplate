// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F04-T01 / T02: Sidecar IPC codec implementation.

#include "tensorplate/ipc/sidecar_codec.hpp"

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <span>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::ipc {

namespace {

constexpr std::size_t kFramePrefixBytes = 16;

void write_be_u32(std::byte* out, std::uint32_t value) noexcept {
  out[0] = static_cast<std::byte>((value >> 24) & 0xFFu);
  out[1] = static_cast<std::byte>((value >> 16) & 0xFFu);
  out[2] = static_cast<std::byte>((value >> 8) & 0xFFu);
  out[3] = static_cast<std::byte>(value & 0xFFu);
}

std::uint32_t read_be_u32(const std::byte* in) noexcept {
  return (static_cast<std::uint32_t>(in[0]) << 24) |
         (static_cast<std::uint32_t>(in[1]) << 16) |
         (static_cast<std::uint32_t>(in[2]) << 8) |
         static_cast<std::uint32_t>(in[3]);
}

}  // namespace

Result<std::vector<std::byte>> encode_frame(const SidecarFrame& frame) {
  if (frame.json_header.size() > kMaxHeaderLen) {
    return unexpected(Error::Code::ConfigInvalid, "sidecar frame header exceeds kMaxHeaderLen");
  }
  if (frame.payload.size() > kMaxPayloadLen) {
    return unexpected(Error::Code::ConfigInvalid, "sidecar frame payload exceeds kMaxPayloadLen");
  }

  const auto hdr_len = static_cast<std::uint32_t>(frame.json_header.size());
  const auto pld_len = static_cast<std::uint32_t>(frame.payload.size());

  std::vector<std::byte> buf;
  buf.resize(kFramePrefixBytes + hdr_len + pld_len);

  write_be_u32(buf.data() + 0, kFrameMagic);
  write_be_u32(buf.data() + 4, kProtocolWireVersion);
  write_be_u32(buf.data() + 8, hdr_len);
  write_be_u32(buf.data() + 12, pld_len);

  if (hdr_len > 0) {
    std::memcpy(buf.data() + kFramePrefixBytes, frame.json_header.data(), hdr_len);
  }
  if (pld_len > 0) {
    std::memcpy(buf.data() + kFramePrefixBytes + hdr_len, frame.payload.data(), pld_len);
  }
  return buf;
}

Result<SidecarFrame> decode_frame(std::span<const std::byte> bytes, std::size_t* consumed) {
  if (consumed != nullptr) {
    *consumed = 0;
  }
  if (bytes.size() < kFramePrefixBytes) {
    return unexpected(Error::Code::NotReady, "incomplete frame: prefix");
  }
  const std::uint32_t magic = read_be_u32(bytes.data() + 0);
  if (magic != kFrameMagic) {
    return unexpected(Error::Code::ConfigInvalid, "sidecar frame magic mismatch");
  }
  const std::uint32_t version = read_be_u32(bytes.data() + 4);
  if (version != kProtocolWireVersion) {
    return unexpected(Error::Code::ConfigInvalid,
                      "sidecar frame protocol version " + std::to_string(version) +
                          " is not supported (expected " +
                          std::to_string(kProtocolWireVersion) + ")");
  }
  const std::uint32_t hdr_len = read_be_u32(bytes.data() + 8);
  const std::uint32_t pld_len = read_be_u32(bytes.data() + 12);

  if (hdr_len > kMaxHeaderLen) {
    return unexpected(Error::Code::ConfigInvalid, "sidecar frame header exceeds kMaxHeaderLen");
  }
  if (pld_len > kMaxPayloadLen) {
    return unexpected(Error::Code::ConfigInvalid, "sidecar frame payload exceeds kMaxPayloadLen");
  }

  const std::size_t total = kFramePrefixBytes + hdr_len + pld_len;
  if (bytes.size() < total) {
    return unexpected(Error::Code::NotReady, "incomplete frame: body");
  }

  SidecarFrame frame;
  frame.json_header.assign(reinterpret_cast<const char*>(bytes.data() + kFramePrefixBytes),
                           hdr_len);
  frame.payload.assign(bytes.data() + kFramePrefixBytes + hdr_len,
                       bytes.data() + kFramePrefixBytes + hdr_len + pld_len);

  if (consumed != nullptr) {
    *consumed = total;
  }
  return frame;
}

Result<std::vector<SidecarFrame>> decode_frames(std::span<const std::byte> bytes,
                                                std::size_t* consumed) {
  std::vector<SidecarFrame> frames;
  std::size_t offset = 0;
  while (offset < bytes.size()) {
    std::size_t step = 0;
    auto f = decode_frame(bytes.subspan(offset), &step);
    if (!f.has_value()) {
      // NotReady (partial frame) is a stop signal, not an error.
      if (f.error().code == Error::Code::NotReady) {
        break;
      }
      return unexpected(f.error());
    }
    frames.push_back(std::move(f).value());
    offset += step;
  }
  if (consumed != nullptr) {
    *consumed = offset;
  }
  return frames;
}

}  // namespace tensorplate::ipc
