// SPDX-License-Identifier: Apache-2.0
//
// What the stream listener bounds before a call has a session: the wait for
// its Open, how many calls it waits on, how many refused calls it keeps,
// compression, what its transport announces, and how long gRPC keeps a
// connection with no call and how many it accepts. Every timeout of the
// listener's own but one runs on an injected clock.

#include <arpa/inet.h>
#include <grpcpp/grpcpp.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
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

/// A listener whose clock only the test moves. The clock's instants lie an
/// hour ahead of the steady clock, so no wait of the listener ends by itself
/// during a test: it looks at the clock when it is told the clock has moved.
struct TimedListener {
  explicit TimedListener(std::optional<std::size_t> max_refused_calls = std::nullopt)
      : served(true, "127.0.0.1", &clock, max_refused_calls) {}

  [[nodiscard]] tensorplate::serving::StreamListener& listener() { return served.listener(); }
  void advance(tensorplate::SchedulerClock::Duration by) {
    clock.advance(by);
    listener().notify_clock_advanced();
  }
  /// A call the listener waits on for its Open.
  [[nodiscard]] Client silent_call() {
    const auto waited_on = listener().awaited_calls();
    auto client = served.connect();
    EXPECT_TRUE(eventually([&] { return listener().awaited_calls() == waited_on + 1; }));
    return client;
  }

  FakeSchedulerClock clock;
  Served served;
};

TEST(StreamBounds, ACallThatSendsNoOpenIsRefusedWhenTheHandshakeTimeoutPasses) {
  TimedListener timed;
  auto silent = timed.silent_call();
  timed.advance(5s);
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
  timed.advance(1ms);
  auto first = timed.silent_call();
  timed.advance(4999ms);
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
  timed.advance(5s);
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

// A clock moved by hand never asks the watch thread to wake by itself when a
// wait ends, which is all it does on the system clock. This test waits the
// five seconds; what it checks is the refusal, not how long it took.
TEST(StreamBounds, OnTheSystemClockTheWatchThreadWakesByItselfWhenAHandshakeTimeoutPasses) {
  Served served;
  auto silent = served.connect();
  expect_refused(ended_unanswered(silent), grpc::StatusCode::DEADLINE_EXCEEDED,
                 "open refused: FAILURE_REASON_IDLE_TIMEOUT");
  EXPECT_TRUE(eventually([&] { return served.listener().open_calls() == 0; }));
}

// A refused call holds a place from its refusal until it is done, and no
// longer: a listener with one place refuses calls one after another, and
// each reads its reason.
TEST(StreamBounds, ARefusedCallGivesItsPlaceBackWhenItIsDone) {
  TimedListener timed{1};
  timed.listener().stop_admission();
  for (int call = 0; call < 3; ++call) {
    SCOPED_TRACE(call);
    auto late = timed.served.connect();
    expect_refused(ended_unanswered(late), grpc::StatusCode::UNAVAILABLE,
                   "open refused: FAILURE_REASON_ADMISSION_CLOSED");
    ASSERT_TRUE(eventually([&] { return timed.listener().open_calls() == 0; }));
  }
  EXPECT_EQ(timed.listener().refused_calls(), 0U);
}

// A call refused with no place left is cancelled: its client reads CANCELLED
// and no reason. No test here fills the listener's own 4,096 places with
// real calls; a listener given none runs the same path from each place a
// bound refuses: the watch thread, the closing of admission, and the method
// handler, where a call refused on arrival is ended.
TEST(StreamBounds, ACallRefusedWithNoPlaceLeftIsCancelledAndReadsNoReason) {
  TimedListener timed{0};
  auto silent = timed.silent_call();
  timed.advance(5s);
  const auto overdue = ended_unanswered(silent);
  EXPECT_EQ(overdue.error_code(), grpc::StatusCode::CANCELLED) << overdue.error_message();
  EXPECT_EQ(overdue.error_message().find("open refused"), std::string::npos)
      << overdue.error_message();

  auto waited_on = timed.silent_call();
  timed.listener().stop_admission();
  const auto at_close = ended_unanswered(waited_on);
  EXPECT_EQ(at_close.error_code(), grpc::StatusCode::CANCELLED) << at_close.error_message();
  EXPECT_EQ(at_close.error_message().find("open refused"), std::string::npos)
      << at_close.error_message();

  auto late = timed.served.connect();
  const auto on_arrival = ended_unanswered(late);
  EXPECT_EQ(on_arrival.error_code(), grpc::StatusCode::CANCELLED) << on_arrival.error_message();
  EXPECT_EQ(on_arrival.error_message().find("open refused"), std::string::npos)
      << on_arrival.error_message();
  EXPECT_TRUE(eventually([&] { return timed.listener().open_calls() == 0; }));
  EXPECT_EQ(timed.listener().refused_calls(), 0U);
  EXPECT_TRUE(timed.served.dispatch().seen.events().empty());
}

// One channel with as many calls open as a connection carries, against a
// listener that refuses every Open, round after round without waiting for the
// listener between them. A place is held from the refusal until gRPC reports
// the call done, also for a peer that reads, so such a channel holds up to 64
// places at one time. With the listener's own places no call is cancelled:
// each reads a refusal with its reason, the one its Open is answered with or,
// for a call that arrives while 32 are waited on, the one for that limit.
TEST(StreamBounds, AChannelAtTheStreamLimitReadsARefusalWithItsReasonForEveryCall) {
  constexpr std::size_t kCalls = 64;
  constexpr int kRounds = 20;
  Served served{false};
  for (int round = 0; round < kRounds; ++round) {
    SCOPED_TRACE(round);
    std::vector<grpc::Status> statuses(kCalls);
    std::atomic<bool> open{false};
    std::vector<std::thread> callers;
    for (auto& status : statuses) {
      callers.emplace_back([&served, &open, &status] {
        while (!open) {
          std::this_thread::yield();
        }
        status = served.refused(open_event(wire::SESSION_MODE_STT_STREAMING));
      });
    }
    open = true;
    for (auto& caller : callers) {
      caller.join();
    }
    for (const auto& status : statuses) {
      const auto& message = status.error_message();
      if (status.error_code() == grpc::StatusCode::RESOURCE_EXHAUSTED) {
        EXPECT_EQ(message, "open refused: FAILURE_REASON_SESSION_COUNT_LIMIT");
      } else {
        EXPECT_EQ(status.error_code(), grpc::StatusCode::FAILED_PRECONDITION) << message;
        EXPECT_EQ(message, "open refused: FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY");
      }
    }
  }
  EXPECT_TRUE(eventually([&] { return served.listener().open_calls() == 0; }));
  EXPECT_EQ(served.listener().refused_calls(), 0U);
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

/// What a server's HTTP/2 connection preface announces about its windows and
/// its streams.
struct AnnouncedWindows {
  /// SETTINGS_INITIAL_WINDOW_SIZE, when the server's SETTINGS frame sets it.
  std::optional<std::uint32_t> stream_window;
  /// SETTINGS_MAX_CONCURRENT_STREAMS, when the frame sets it.
  std::optional<std::uint32_t> max_concurrent_streams;
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
  constexpr std::uint16_t kMaxConcurrentStreams = 3;
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
          const auto setting = big_endian(body.substr(at, 2));
          const auto value = big_endian(body.substr(at + 2, 4));
          if (setting == kInitialWindowSize) {
            announced.stream_window = value;
          } else if (setting == kMaxConcurrentStreams) {
            announced.max_concurrent_streams = value;
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

// gRPC's own bound on a connection: it refuses a stream beyond these before
// the listener has a call for it. Sixty-four leaves room for the thirty-two
// calls waited on and for a call that is told it came on top of them.
TEST(StreamBounds, TheServersPrefaceLimitsAConnectionToSixtyFourConcurrentStreams) {
  Served served;
  const auto announced = announced_windows(served.listener().bound_port());
  ASSERT_TRUE(announced.complete) << "the server's preface was not read to its end";
  ASSERT_TRUE(announced.max_concurrent_streams.has_value()) << "the server announced no limit";
  EXPECT_EQ(*announced.max_concurrent_streams, 64U);
}

/// A channel with a connection of its own: channels to one address otherwise
/// share gRPC's subchannels, and with them a connection.
struct Connection {
  explicit Connection(std::uint16_t port) {
    grpc::ChannelArguments arguments;
    arguments.SetInt(GRPC_ARG_USE_LOCAL_SUBCHANNEL_POOL, 1);
    channel = grpc::CreateCustomChannel("127.0.0.1:" + std::to_string(port),
                                        grpc::InsecureChannelCredentials(), arguments);
    stub = wire::SessionService::NewStub(channel);
  }

  /// A call that has sent nothing. With `wait_for_ready` it waits for a
  /// connection instead of failing while the channel has none.
  [[nodiscard]] Client call(bool wait_for_ready = false) const {
    Client client;
    client.context->set_deadline(std::chrono::system_clock::now() + 20s);
    client.context->set_wait_for_ready(wait_for_ready);
    client.call = stub->Session(client.context.get());
    return client;
  }

  std::shared_ptr<grpc::Channel> channel;
  std::unique_ptr<wire::SessionService::Stub> stub;
};

std::chrono::system_clock::time_point in(std::chrono::milliseconds wait) {
  return std::chrono::system_clock::now() + wait;
}

/// Whether `client`'s Open is answered by Ready.
bool answered_by_ready(Client& client) {
  if (!client.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING))) {
    return false;
  }
  const auto ready = client.read();
  if (!ready || !ready->has_ready()) {
    return false;
  }
  client.took(ready->ready());
  return true;
}

void close_session(Client& client) {
  EXPECT_TRUE(client.call->WritesDone());
  const auto closed = client.read();
  EXPECT_TRUE(closed && closed->has_session_closed());
  EXPECT_FALSE(client.read().has_value());
  EXPECT_TRUE(client.call->Finish().ok());
}

/// The status of a call on a connection the server closed as it accepted it.
grpc::Status unconnected(const Connection& connection) {
  auto client = connection.call();
  (void)client.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING));
  wire::ServerEvent event;
  EXPECT_FALSE(client.call->Read(&event));
  return client.call->Finish();
}

TEST(StreamBounds, UnsetConnectionLimitsAreTheListenersOwn) {
  const auto own = tensorplate::serving::connection_bounds({});
  EXPECT_EQ(own.idle, 67min);
  EXPECT_EQ(own.connections, 256);
  tensorplate::serving::StreamListenerOptions options;
  options.max_connection_idle = 5ms;
  options.max_connections = 2;
  const auto set = tensorplate::serving::connection_bounds(options);
  EXPECT_EQ(set.idle, 5ms);
  EXPECT_EQ(set.connections, 2);
}

// gRPC ends a connection that has had no call open for the idle limit, and
// the client's next call connects again by itself.
TEST(StreamBounds, AConnectionWithNoCallOpenIsClosedAtTheIdleLimitAndTheNextCallConnectsAgain) {
  Served served{true, "127.0.0.1", nullptr, std::nullopt, 1, {.idle = 200ms}};
  const Connection connection{served.listener().bound_port()};
  ASSERT_TRUE(connection.channel->WaitForConnected(in(20s)));
  // The server's GOAWAY takes the channel off READY.
  ASSERT_TRUE(connection.channel->WaitForStateChange(GRPC_CHANNEL_READY, in(20s)))
      << "the connection outlived the idle limit";
  auto client = connection.call();
  ASSERT_TRUE(answered_by_ready(client));
  close_session(client);
}

// The idle limit counts only time with no call open: a session held across
// it keeps its connection.
TEST(StreamBounds, ASessionHeldPastTheIdleLimitKeepsItsConnection) {
  Served served{true, "127.0.0.1", nullptr, std::nullopt, 1, {.idle = 1s}};
  const Connection connection{served.listener().bound_port()};
  auto client = connection.call();
  ASSERT_TRUE(answered_by_ready(client));
  EXPECT_FALSE(connection.channel->WaitForStateChange(GRPC_CHANNEL_READY, in(3s)))
      << "the connection of an open call was closed";
  wire::ClientEvent ping;
  ping.mutable_ping();
  ASSERT_TRUE(client.send(ping));
  const auto pong = client.read();
  EXPECT_TRUE(pong && pong->has_pong());
  close_session(client);
}

// gRPC accepts no more connections than the cap and closes one past it as it
// accepts it; a connection that closes gives its place back.
TEST(StreamBounds, ConnectionsUpToTheCapAreServedAndOnePastItIsClosedAsItIsAccepted) {
  constexpr int kCap = 3;
  Served served{true, "127.0.0.1", nullptr, std::nullopt, kCap, {.connections = kCap}};
  const auto port = served.listener().bound_port();
  std::vector<std::unique_ptr<Connection>> connections;
  std::vector<Client> sessions;
  for (int held = 0; held < kCap; ++held) {
    connections.push_back(std::make_unique<Connection>(port));
    sessions.push_back(connections.back()->call());
    ASSERT_TRUE(answered_by_ready(sessions.back())) << "connection " << held;
  }
  const Connection past{port};
  const auto refused = unconnected(past);
  EXPECT_EQ(refused.error_code(), grpc::StatusCode::UNAVAILABLE) << refused.error_message();

  close_session(sessions.front());
  sessions.erase(sessions.begin());
  connections.erase(connections.begin());
  auto admitted = past.call(true);
  ASSERT_TRUE(answered_by_ready(admitted)) << "the closed connection kept its place";
  close_session(admitted);
  for (auto& session : sessions) {
    close_session(session);
  }
}

// The listener's own cap, 256: each of that many connections is served a call
// while all are held, and the next is closed as it is accepted.
TEST(StreamBounds, TheListenersOwnCapServesTwoHundredFiftySixConnectionsAndClosesTheNext) {
  constexpr int kOwnCap = 256;
  Served served{false};
  const auto port = served.listener().bound_port();
  std::vector<std::unique_ptr<Connection>> held;
  for (int connection = 0; connection < kOwnCap; ++connection) {
    held.push_back(std::make_unique<Connection>(port));
    auto client = held.back()->call();
    ASSERT_TRUE(client.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
    wire::ServerEvent event;
    EXPECT_FALSE(client.call->Read(&event));
    ASSERT_EQ(client.call->Finish().error_code(), grpc::StatusCode::FAILED_PRECONDITION)
        << "connection " << connection;
  }
  const Connection past{port};
  const auto refused = unconnected(past);
  EXPECT_EQ(refused.error_code(), grpc::StatusCode::UNAVAILABLE) << refused.error_message();
}

}  // namespace
