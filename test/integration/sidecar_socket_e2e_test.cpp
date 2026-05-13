// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F04-T02: Cross-process integration test for the C++ Unix-
// socket helpers. Forks a child process that binds + accepts and
// exchanges one sidecar frame through the helpers.

#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

#include <gtest/gtest.h>

#include <sys/wait.h>
#include <unistd.h>

#include <chrono>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <span>
#include <string>
#include <thread>
#include <vector>

#include "tensorplate/core/error.hpp"

namespace tensorplate::ipc {
namespace {

std::string make_socket_path(const std::string& tag) {
  const auto tmp = std::filesystem::temp_directory_path();
  return (tmp / ("tp_sidecar_" + tag + "_" + std::to_string(::getpid()) + ".sock")).string();
}

UnixSocket::TimePoint default_deadline() {
  return UnixSocket::Clock::now() + std::chrono::seconds(5);
}

TEST(SidecarSocket, RoundTripsOneFrameAcrossProcesses) {
  const std::string path = make_socket_path("roundtrip");
  std::filesystem::remove(path);

  // Build the listener in the parent before forking so the child does
  // not race against bind().
  auto listener_r = UnixSocket::create_stream();
  ASSERT_TRUE(listener_r.has_value()) << listener_r.error().message;
  auto listener = std::move(listener_r).value();
  ASSERT_TRUE(listener.bind_and_listen(path).has_value());

  const pid_t pid = ::fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    // Child: connect, write a frame, read a frame, exit.
    auto sock_r = UnixSocket::create_stream();
    if (!sock_r.has_value()) std::_Exit(11);
    auto sock = std::move(sock_r).value();
    if (!sock.connect(path, default_deadline()).has_value()) std::_Exit(12);

    SidecarFrame frame;
    frame.json_header = R"({"schema_version":"0.1","kind":"prime","message_id":"c1"})";
    auto enc_r = encode_frame(frame);
    if (!enc_r.has_value()) std::_Exit(13);
    if (!sock.write_all(enc_r.value(), default_deadline()).has_value()) std::_Exit(14);

    // Read the response prefix + body. The parent sends a 16-byte
    // prefixed frame; for simplicity we read into a large buffer and
    // attempt decode in a loop.
    std::vector<std::byte> buf(4096);
    auto read_r = sock.read_exact(std::span<std::byte>(buf.data(), 16), default_deadline());
    if (!read_r.has_value()) std::_Exit(15);
    // Body follows the prefix; parent sends a 0-byte payload.
    std::size_t consumed = 0;
    auto dec = decode_frame(std::span<const std::byte>(buf.data(), 16), &consumed);
    if (!dec.has_value() && dec.error().code != Error::Code::NotReady) std::_Exit(16);
    // The response prefix says hdr_len = N; read that next.
    // To keep this test simple we expect the parent's response body
    // to fit in the original 4 KiB buf.
    if (!dec.has_value()) {
      // Need more bytes; recompute body size from prefix and read.
      // header_len + payload_len are at bytes 8..15.
      const auto* p = buf.data();
      auto u32 = [&](std::size_t off) {
        return (static_cast<std::uint32_t>(p[off]) << 24) |
               (static_cast<std::uint32_t>(p[off + 1]) << 16) |
               (static_cast<std::uint32_t>(p[off + 2]) << 8) |
               static_cast<std::uint32_t>(p[off + 3]);
      };
      const std::uint32_t hdr_len = u32(8);
      const std::uint32_t pld_len = u32(12);
      auto read2 = sock.read_exact(std::span<std::byte>(buf.data() + 16, hdr_len + pld_len),
                                   default_deadline());
      if (!read2.has_value()) std::_Exit(17);
      auto dec2 = decode_frame(std::span<const std::byte>(buf.data(), 16 + hdr_len + pld_len),
                               nullptr);
      if (!dec2.has_value()) std::_Exit(18);
    }
    std::_Exit(0);
  }

  // Parent: accept the child, decode its frame, send a response.
  auto client_r = listener.accept(default_deadline());
  ASSERT_TRUE(client_r.has_value()) << client_r.error().message;
  auto client = std::move(client_r).value();

  std::vector<std::byte> in(4096);
  auto read_prefix = client.read_exact(std::span<std::byte>(in.data(), 16), default_deadline());
  ASSERT_TRUE(read_prefix.has_value());
  // Read remaining hdr+pld.
  const auto* p = in.data();
  auto u32 = [&](std::size_t off) {
    return (static_cast<std::uint32_t>(p[off]) << 24) |
           (static_cast<std::uint32_t>(p[off + 1]) << 16) |
           (static_cast<std::uint32_t>(p[off + 2]) << 8) |
           static_cast<std::uint32_t>(p[off + 3]);
  };
  const std::uint32_t hdr_len = u32(8);
  const std::uint32_t pld_len = u32(12);
  auto read_body = client.read_exact(std::span<std::byte>(in.data() + 16, hdr_len + pld_len),
                                     default_deadline());
  ASSERT_TRUE(read_body.has_value());
  auto frame_r =
      decode_frame(std::span<const std::byte>(in.data(), 16 + hdr_len + pld_len), nullptr);
  ASSERT_TRUE(frame_r.has_value());
  EXPECT_NE(frame_r.value().json_header.find("\"message_id\":\"c1\""), std::string::npos);

  // Send a tiny response frame back.
  SidecarFrame resp;
  resp.json_header = R"({"schema_version":"0.1","kind":"prime_response","message_id":"c1","status":"ok"})";
  auto resp_enc = encode_frame(resp);
  ASSERT_TRUE(resp_enc.has_value());
  ASSERT_TRUE(client.write_all(resp_enc.value(), default_deadline()).has_value());

  int status = 0;
  ASSERT_EQ(::waitpid(pid, &status, 0), pid);
  EXPECT_TRUE(WIFEXITED(status));
  EXPECT_EQ(WEXITSTATUS(status), 0) << "child exited with code " << WEXITSTATUS(status);

  std::filesystem::remove(path);
}

TEST(SidecarSocket, ConnectFailsOnMissingPath) {
  auto sock = UnixSocket::create_stream();
  ASSERT_TRUE(sock.has_value());
  auto r = sock.value().connect("/nonexistent/tp_sidecar_missing.sock",
                                UnixSocket::Clock::now() + std::chrono::milliseconds(200));
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::LoadFailed);
}

TEST(SidecarSocket, OversizedPathRejected) {
  auto sock = UnixSocket::create_stream().value();
  std::string huge(200, 'x');
  auto r = sock.connect(huge, UnixSocket::Clock::now() + std::chrono::milliseconds(200));
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

}  // namespace
}  // namespace tensorplate::ipc
