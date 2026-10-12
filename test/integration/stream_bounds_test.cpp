// SPDX-License-Identifier: Apache-2.0
//
// What the stream listener bounds before a call has a session: the wait for
// its Open, how many calls it waits on, compression, and the window its
// transport announces. Every timeout runs on an injected clock.

#include <arpa/inet.h>
#include <grpcpp/grpcpp.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/stream/v1/session.grpc.pb.h"

#include "fake_scheduler_clock.hpp"
#include "stream_listener_fixture.hpp"

namespace {

using namespace std::chrono_literals;
using tensorplate::testing::FakeSchedulerClock;
using tensorplate::testing::stream_listener::Client;
using tensorplate::testing::stream_listener::eventually;
using tensorplate::testing::stream_listener::open_event;
using tensorplate::testing::stream_listener::Served;
namespace wire = tensorplate::stream::v1;

/// The status of a call that must end with no event.
grpc::Status ended_unanswered(Client& client) {
  EXPECT_FALSE(client.read().has_value()) << "a call that was never admitted has no event";
  return client.call->Finish();
}

void expect_refused(const grpc::Status& status, grpc::StatusCode code, std::string_view reason) {
  EXPECT_EQ(status.error_code(), code) << status.error_message();
  EXPECT_NE(status.error_message().find(reason), std::string::npos) << status.error_message();
}

/// A listener whose clock only the test moves.
struct TimedListener {
  [[nodiscard]] tensorplate::serving::StreamListener& listener() { return served.listener(); }
  /// A call the listener waits on for its Open.
  [[nodiscard]] Client silent_call() {
    const auto waited_on = listener().awaited_calls();
    auto client = served.connect();
    EXPECT_TRUE(eventually([&] { return listener().awaited_calls() == waited_on + 1; }));
    return client;
  }

  FakeSchedulerClock clock;
  Served served{true, "127.0.0.1", &clock};
};

TEST(StreamBounds, ACallThatSendsNoOpenIsRefusedWhenTheHandshakeTimeoutPasses) {
  TimedListener timed;
  auto silent = timed.silent_call();
  timed.clock.advance(5s);
  expect_refused(ended_unanswered(silent), grpc::StatusCode::DEADLINE_EXCEEDED,
                 "open refused: FAILURE_REASON_IDLE_TIMEOUT");
  EXPECT_EQ(timed.listener().awaited_calls(), 0U);
  EXPECT_TRUE(timed.served.dispatch().seen.events().empty()) << "a refused call opened a session";
  EXPECT_TRUE(eventually([&] { return timed.listener().open_calls() == 0; }));
}

// A call that began a millisecond earlier shows when the watch thread has
// looked: the pass that refuses it takes every overdue call at once, so a
// call it leaves waited on had its whole five seconds less that millisecond.
TEST(StreamBounds, AnOpenInsideTheHandshakeTimeoutIsAnsweredAndItsCallNoLongerWaitedOn) {
  TimedListener timed;
  auto earlier = timed.silent_call();
  timed.clock.advance(1ms);
  auto first = timed.silent_call();
  timed.clock.advance(4999ms);
  expect_refused(ended_unanswered(earlier), grpc::StatusCode::DEADLINE_EXCEEDED,
                 "FAILURE_REASON_IDLE_TIMEOUT");
  ASSERT_TRUE(first.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
  const auto ready = first.read();
  ASSERT_TRUE(ready && ready->has_ready()) << "refused before its five seconds were over";
  first.took(ready->ready());
  EXPECT_EQ(timed.listener().awaited_calls(), 0U);

  // A later call, refused in the pass that would have refused the first had
  // its wait not ended: the first began before it and is taken before it.
  auto second = timed.silent_call();
  timed.clock.advance(5s);
  expect_refused(ended_unanswered(second), grpc::StatusCode::DEADLINE_EXCEEDED,
                 "FAILURE_REASON_IDLE_TIMEOUT");
  wire::ClientEvent ping;
  ping.mutable_ping();
  ASSERT_TRUE(first.send(ping));
  const auto pong = first.next();
  ASSERT_TRUE(pong.has_value()) << "the timeout ended a call that had sent its Open";
  EXPECT_TRUE(pong->has_pong());
}

TEST(StreamBounds, OnlyThirtyTwoCallsAreWaitedOnAndTheNextIsRefusedWithoutSendingAnything) {
  TimedListener timed;
  std::vector<Client> waited_on;
  for (int i = 0; i < 32; ++i) {
    waited_on.push_back(timed.served.connect());
  }
  ASSERT_TRUE(eventually([&] { return timed.listener().awaited_calls() == 32; }));
  // It sends nothing, so the refusal cannot follow from anything read.
  auto over = timed.served.connect();
  expect_refused(ended_unanswered(over), grpc::StatusCode::RESOURCE_EXHAUSTED,
                 "open refused: FAILURE_REASON_SESSION_COUNT_LIMIT");
  EXPECT_EQ(timed.listener().live_sessions(), 0U) << "no session holds the slot: the calls do";
  EXPECT_EQ(timed.listener().awaited_calls(), 32U);

  // A call that has sent its first message is no longer waited on: its place
  // is free whatever became of it.
  waited_on.front().call->WritesDone();
  expect_refused(ended_unanswered(waited_on.front()), grpc::StatusCode::INVALID_ARGUMENT,
                 "FAILURE_REASON_INVALID_EVENT");
  auto next = timed.silent_call();
  ASSERT_TRUE(next.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
  const auto ready = next.read();
  EXPECT_TRUE(ready && ready->has_ready());
  for (auto& client : waited_on) {
    client.context->TryCancel();
  }
  next.context->TryCancel();
}

TEST(StreamBounds, ClosingAdmissionRefusesTheCallsWaitedOnAndEveryCallThatArrivesAfter) {
  TimedListener timed;
  auto silent = timed.silent_call();
  timed.listener().stop_admission();
  expect_refused(ended_unanswered(silent), grpc::StatusCode::UNAVAILABLE,
                 "open refused: FAILURE_REASON_ADMISSION_CLOSED");
  // It sends nothing either: the listener no longer waits on a call.
  auto late = timed.served.connect();
  expect_refused(ended_unanswered(late), grpc::StatusCode::UNAVAILABLE,
                 "open refused: FAILURE_REASON_ADMISSION_CLOSED");
  EXPECT_EQ(timed.listener().awaited_calls(), 0U);
  EXPECT_TRUE(timed.served.dispatch().seen.events().empty());
}

// The call's first message was read before admission closed, so no bound
// answers it: the manager refuses the Open it was deciding on.
TEST(StreamBounds, AnOpenBeingAnsweredWhenAdmissionClosesIsRefusedByTheManager) {
  TimedListener timed;
  timed.served.dispatch().hold_negotiation();
  auto client = timed.served.connect();
  ASSERT_TRUE(client.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
  timed.served.dispatch().wait_until_negotiating();
  EXPECT_EQ(timed.listener().awaited_calls(), 0U);
  timed.listener().stop_admission();
  timed.served.dispatch().release_negotiation();
  expect_refused(ended_unanswered(client), grpc::StatusCode::UNAVAILABLE,
                 "open refused: FAILURE_REASON_ADMISSION_CLOSED");
  EXPECT_TRUE(timed.served.dispatch().seen.events().empty());
}

// gRPC ends the call itself when the headers arrive, so the status is the
// transport's and names no failure reason.
TEST(StreamBounds, ACallThatAnnouncesCompressionIsEndedByTheTransportUnanswered) {
  Served served;
  for (const auto algorithm : {GRPC_COMPRESS_GZIP, GRPC_COMPRESS_DEFLATE}) {
    SCOPED_TRACE(algorithm);
    auto client = served.connect(20s, algorithm);
    (void)client.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING));
    const auto status = ended_unanswered(client);
    EXPECT_EQ(status.error_code(), grpc::StatusCode::UNIMPLEMENTED) << status.error_message();
    EXPECT_TRUE(eventually([&] { return served.listener().open_calls() == 0; }));
  }
  EXPECT_TRUE(served.dispatch().seen.events().empty()) << "a compressed Open opened a session";
  auto plain = served.ready(wire::SESSION_MODE_STT_STREAMING);
  EXPECT_FALSE(plain.session_id.empty());
}

/// What a server's HTTP/2 connection preface announces about its windows.
struct AnnouncedWindows {
  /// SETTINGS_INITIAL_WINDOW_SIZE, when the server's SETTINGS frame sets it.
  std::optional<std::uint32_t> stream_window;
  /// The sum of the WINDOW_UPDATE increments for the connection.
  std::uint64_t connection_window_added = 0;
  bool complete = false;
};

/// Opens an HTTP/2 connection to `port` on loopback and reads the server's
/// frames up to its acknowledgement of the client's SETTINGS.
AnnouncedWindows announced_windows(std::uint16_t port) {
  constexpr std::string_view kClientPreface = "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
  constexpr std::array<char, 9> kEmptySettings{0, 0, 0, 4, 0, 0, 0, 0, 0};
  constexpr std::uint8_t kSettings = 4;
  constexpr std::uint8_t kWindowUpdate = 8;
  constexpr std::uint16_t kInitialWindowSize = 4;
  AnnouncedWindows announced;
  const int fd = ::socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) {
    return announced;
  }
  // A server that never acknowledges fails the test instead of hanging it.
  const timeval patience{20, 0};
  (void)::setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &patience, sizeof(patience));
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  address.sin_port = htons(port);
  std::string hello{kClientPreface};
  hello.append(kEmptySettings.data(), kEmptySettings.size());
  const auto read_exactly = [fd](std::size_t size) -> std::optional<std::string> {
    std::string bytes(size, '\0');
    std::size_t have = 0;
    while (have < size) {
      const auto got = ::read(fd, bytes.data() + have, size - have);
      if (got <= 0) {
        return std::nullopt;
      }
      have += static_cast<std::size_t>(got);
    }
    return bytes;
  };
  const auto big_endian = [](std::string_view bytes) {
    std::uint32_t value = 0;
    for (const char byte : bytes) {
      value = (value << 8U) | static_cast<std::uint8_t>(byte);
    }
    return value;
  };
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
  if (::connect(fd, reinterpret_cast<const sockaddr*>(&address), sizeof(address)) == 0 &&
      ::write(fd, hello.data(), hello.size()) == static_cast<ssize_t>(hello.size())) {
    while (!announced.complete) {
      const auto header = read_exactly(9);
      if (!header) {
        break;
      }
      const std::string_view head{*header};
      const auto type = static_cast<std::uint8_t>(head[3]);
      const bool acknowledgement = (static_cast<std::uint8_t>(head[4]) & 1U) != 0;
      const bool for_connection = (big_endian(head.substr(5, 4)) & 0x7fffffffU) == 0;
      const auto payload = read_exactly(big_endian(head.substr(0, 3)));
      if (!payload) {
        break;
      }
      const std::string_view body{*payload};
      if (type == kSettings && acknowledgement) {
        announced.complete = true;
      } else if (type == kSettings) {
        for (std::size_t at = 0; at + 6 <= body.size(); at += 6) {
          if (big_endian(body.substr(at, 2)) == kInitialWindowSize) {
            announced.stream_window = big_endian(body.substr(at + 2, 4));
          }
        }
      } else if (type == kWindowUpdate && for_connection && body.size() == 4) {
        announced.connection_window_added += big_endian(body) & 0x7fffffffU;
      }
    }
  }
  ::close(fd);
  return announced;
}

// The windows a connection starts with: what a client can send to a call the
// listener is not reading. A connection's window starts at 65,535 bytes and
// grows only by a WINDOW_UPDATE (RFC 9113, 6.9.2). gRPC widens both later for
// a message it is receiving, which this does not look at.
TEST(StreamBounds, TheServersPrefaceAnnouncesTheDefaultWindowForAStreamAndTheConnection) {
  Served served;
  const auto announced = announced_windows(served.listener().bound_port());
  ASSERT_TRUE(announced.complete) << "the server's preface was not read to its end";
  EXPECT_EQ(announced.stream_window.value_or(65'535), 65'535U);
  EXPECT_EQ(announced.connection_window_added, 0U);
}

}  // namespace
