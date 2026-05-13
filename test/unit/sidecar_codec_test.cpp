// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F04-T01: Codec round-trip + framing-violation tests for the
// Python sidecar IPC wire format.

#include "tensorplate/ipc/sidecar_codec.hpp"

#include <gtest/gtest.h>

#include <cstddef>
#include <cstdint>
#include <span>
#include <string>
#include <vector>

#include "tensorplate/core/error.hpp"

namespace tensorplate::ipc {
namespace {

std::vector<std::byte> bytes_of(const std::string& s) {
  std::vector<std::byte> b(s.size());
  for (std::size_t i = 0; i < s.size(); ++i) {
    b[i] = static_cast<std::byte>(s[i]);
  }
  return b;
}

TEST(SidecarCodec, RoundTripNoPayload) {
  SidecarFrame f;
  f.json_header = R"({"schema_version":"0.1","kind":"prime","message_id":"m"})";
  auto enc = encode_frame(f);
  ASSERT_TRUE(enc.has_value());

  std::size_t consumed = 0;
  auto dec = decode_frame(enc.value(), &consumed);
  ASSERT_TRUE(dec.has_value()) << dec.error().message;
  EXPECT_EQ(consumed, enc.value().size());
  EXPECT_EQ(dec.value().json_header, f.json_header);
  EXPECT_TRUE(dec.value().payload.empty());
}

TEST(SidecarCodec, RoundTripWithPayload) {
  SidecarFrame f;
  f.json_header = R"({"schema_version":"0.1","kind":"infer","message_id":"m"})";
  f.payload = bytes_of("hello-world");
  auto enc = encode_frame(f);
  ASSERT_TRUE(enc.has_value());
  auto dec = decode_frame(enc.value(), nullptr);
  ASSERT_TRUE(dec.has_value());
  EXPECT_EQ(dec.value().payload, f.payload);
}

TEST(SidecarCodec, IncompletePrefixReturnsNotReady) {
  std::vector<std::byte> tiny{std::byte{0}, std::byte{0}};
  auto dec = decode_frame(tiny, nullptr);
  ASSERT_FALSE(dec.has_value());
  EXPECT_EQ(dec.error().code, Error::Code::NotReady);
}

TEST(SidecarCodec, IncompleteBodyReturnsNotReady) {
  SidecarFrame f;
  f.json_header = R"({"schema_version":"0.1","kind":"prime","message_id":"m"})";
  f.payload = bytes_of("abcd");
  auto enc = encode_frame(f).value();
  // Drop the last byte.
  std::span<const std::byte> truncated(enc.data(), enc.size() - 1);
  auto dec = decode_frame(truncated, nullptr);
  ASSERT_FALSE(dec.has_value());
  EXPECT_EQ(dec.error().code, Error::Code::NotReady);
}

TEST(SidecarCodec, BadMagicReturnsConfigInvalid) {
  std::vector<std::byte> bad(16, std::byte{0});
  auto dec = decode_frame(bad, nullptr);
  ASSERT_FALSE(dec.has_value());
  EXPECT_EQ(dec.error().code, Error::Code::ConfigInvalid);
}

TEST(SidecarCodec, BadVersionReturnsConfigInvalid) {
  SidecarFrame f;
  f.json_header = R"({"schema_version":"0.1","kind":"prime","message_id":"m"})";
  auto enc = encode_frame(f).value();
  // Bump version field to 0x99.
  enc[7] = std::byte{0x99};
  auto dec = decode_frame(enc, nullptr);
  ASSERT_FALSE(dec.has_value());
  EXPECT_EQ(dec.error().code, Error::Code::ConfigInvalid);
}

TEST(SidecarCodec, OversizedHeaderRejectedOnEncode) {
  SidecarFrame f;
  f.json_header = std::string(kMaxHeaderLen + 1, 'x');
  auto enc = encode_frame(f);
  ASSERT_FALSE(enc.has_value());
  EXPECT_EQ(enc.error().code, Error::Code::ConfigInvalid);
}

TEST(SidecarCodec, DecodeFramesStopsAtPartialFrame) {
  SidecarFrame a;
  a.json_header = R"({"schema_version":"0.1","kind":"prime","message_id":"1"})";
  SidecarFrame b;
  b.json_header = R"({"schema_version":"0.1","kind":"prime","message_id":"2"})";
  auto ea = encode_frame(a).value();
  auto eb = encode_frame(b).value();
  std::vector<std::byte> joined;
  joined.insert(joined.end(), ea.begin(), ea.end());
  joined.insert(joined.end(), eb.begin(), eb.end());
  // Drop the last byte to truncate the second frame.
  joined.pop_back();

  std::size_t consumed = 0;
  auto r = decode_frames(joined, &consumed);
  ASSERT_TRUE(r.has_value());
  ASSERT_EQ(r.value().size(), 1u);
  EXPECT_EQ(consumed, ea.size());
}

TEST(SidecarCodec, DecodeFramesPropagatesMalformedError) {
  std::vector<std::byte> garbage(20, std::byte{0});
  auto r = decode_frames(garbage, nullptr);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

}  // namespace
}  // namespace tensorplate::ipc
