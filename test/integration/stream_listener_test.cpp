// SPDX-License-Identifier: Apache-2.0
//
// The gRPC stream listener with a real client over loopback.

#include "serving/stream/stream_listener.hpp"

#include <arpa/inet.h>
#include <grpcpp/grpcpp.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <filesystem>
#include <functional>
#include <memory>
#include <mutex>
#include <nlohmann/json.hpp>
#include <optional>
#include <span>
#include <sstream>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/serving/config.hpp"
#include "tensorplate/serving/serialization.hpp"
#include "tensorplate/serving/worker.hpp"
#include "tensorplate/stream/v1/session.grpc.pb.h"

#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"
#include "serving_http_client.hpp"
#include "stream_listener_fixture.hpp"

namespace {

using namespace std::chrono_literals;
using tensorplate::SystemSchedulerClock;
using tensorplate::serving::StreamListener;
using tensorplate::serving::StreamListenerOptions;
using tensorplate::testing::stream_listener::Call;
using tensorplate::testing::stream_listener::end_of;
using tensorplate::testing::stream_listener::eventually;
using tensorplate::testing::stream_listener::expect_closed;
using tensorplate::testing::stream_listener::kDeployment;
using tensorplate::testing::stream_listener::open_event;
using tensorplate::testing::stream_listener::Seen;
using tensorplate::testing::stream_listener::Served;
namespace wire = tensorplate::stream::v1;

std::size_t thread_count() {
  std::size_t count = 0;
  for ([[maybe_unused]] const auto& entry :
       std::filesystem::directory_iterator("/proc/self/task")) {
    ++count;
  }
  return count;
}

/// Whether a TCP connection to `port` on the loopback of `family` is accepted.
bool accepts(int family, std::uint16_t port) {
  const int fd = ::socket(family, SOCK_STREAM, 0);
  if (fd < 0) {
    return false;
  }
  int connected = -1;
  if (family == AF_INET) {
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = htons(port);
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
    connected = ::connect(fd, reinterpret_cast<const sockaddr*>(&address), sizeof(address));
  } else {
    sockaddr_in6 address{};
    address.sin6_family = AF_INET6;
    address.sin6_addr = in6addr_loopback;
    address.sin6_port = htons(port);
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
    connected = ::connect(fd, reinterpret_cast<const sockaddr*>(&address), sizeof(address));
  }
  ::close(fd);
  return connected == 0;
}

TEST(StreamListener, SpeechToTextOpenIsAnsweredByReady) {
  Served served;
  wire::Ready ready;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING, &ready);
  EXPECT_FALSE(client.session_id.empty());
  // The worker serves no generation and no descriptor: Ready names neither.
  EXPECT_EQ(ready.generation(), 0U);
  EXPECT_TRUE(ready.descriptor_digest().empty());
  EXPECT_EQ(ready.capabilities_size(), 0);
  const auto& limits = ready.limits();
  EXPECT_EQ(limits.idle_timeout_ms(), 60'000U);
  EXPECT_EQ(limits.heartbeat_interval_ms(), 10'000U);
  EXPECT_EQ(limits.liveness_timeout_ms(), 30'000U);
  EXPECT_EQ(limits.max_duration_ms(), 3'600'000U);
  EXPECT_EQ(limits.finalize_deadline_ms(), 10'000U);
  EXPECT_EQ(limits.output_stall_timeout_ms(), 5'000U);
  ASSERT_TRUE(limits.has_speech_to_text());
  EXPECT_FALSE(limits.has_text_to_speech());
  EXPECT_EQ(limits.speech_to_text().min_frame_ms(), 20U);
  EXPECT_EQ(limits.speech_to_text().max_frame_ms(), 320U);
  EXPECT_EQ(limits.speech_to_text().max_utterance_ms(), 30'000U);
  EXPECT_EQ(ready.remaining_duration_ms(), 3'600'000U);
  ASSERT_TRUE(ready.has_input_format());
  EXPECT_FALSE(ready.has_output_format());
  EXPECT_EQ(ready.input_format().encoding(), wire::AUDIO_ENCODING_PCM_S16LE);
  EXPECT_EQ(ready.input_format().sample_rate_hz(), 16'000U);
  EXPECT_EQ(ready.input_format().channels(), 1U);
  const auto& status = ready.status();
  EXPECT_EQ(status.state(), wire::SESSION_STATE_ACTIVE);
  EXPECT_EQ(status.input_queue_depth(), 0U);
  // One second of the input format.
  EXPECT_EQ(status.input_credit().bytes().limit(), 32'000U);
  EXPECT_EQ(status.input_credit().bytes().used(), 0U);
  EXPECT_EQ(status.input_credit().waiting_segments().limit(), 0U);
  EXPECT_EQ(status.output_metadata_bytes().limit(), 16'384U);
}

TEST(StreamListener, TextToSpeechReadyCarriesOutputFormatAndSegmentCredit) {
  Served served;
  wire::Ready ready;
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING, &ready);
  ASSERT_TRUE(ready.has_output_format());
  EXPECT_FALSE(ready.has_input_format());
  EXPECT_EQ(ready.output_format().sample_rate_hz(), 24'000U);
  ASSERT_TRUE(ready.limits().has_text_to_speech());
  EXPECT_FALSE(ready.limits().has_speech_to_text());
  EXPECT_EQ(ready.limits().text_to_speech().max_segment_text_bytes(), 4'096U);
  EXPECT_EQ(ready.limits().text_to_speech().max_segment_audio_ms(), 30'000U);
  EXPECT_EQ(ready.limits().text_to_speech().max_synthesis_text_bytes(), 3U * 4'096U);
  EXPECT_EQ(ready.limits().text_to_speech().max_synthesis_audio_ms(), 90'000U);
  EXPECT_EQ(ready.limits().finalize_deadline_ms(), 120'000U);
  const auto& credit = ready.status().input_credit();
  EXPECT_EQ(credit.waiting_segments().limit(), 2U);
  EXPECT_EQ(credit.waiting_segments().used(), 0U);
  EXPECT_EQ(credit.bytes().limit(), 8'192U);
}

TEST(StreamListener, PingIsAnsweredByPongInSequence) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  for (std::uint64_t expected = 2; expected <= 4; ++expected) {
    wire::ClientEvent ping;
    ping.mutable_ping();
    ASSERT_TRUE(client.send(ping));
    const auto pong = client.read();
    ASSERT_TRUE(pong.has_value());
    EXPECT_TRUE(pong->has_pong());
    EXPECT_EQ(pong->sequence(), expected);
  }
  // Ready and three Pongs: the dispatch hears of each delivery.
  EXPECT_TRUE(eventually([&] { return served.dispatch().seen.deliveries() == 4; }));
}

TEST(StreamListener, HalfCloseClosesTheSessionWithoutCause) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.call->WritesDone());
  const auto closed = client.read();
  ASSERT_NO_FATAL_FAILURE(
      expect_closed(closed, wire::SESSION_STATE_CLOSED, wire::FAILURE_REASON_UNSPECIFIED));
  EXPECT_EQ(closed->sequence(), 2U);
  EXPECT_FALSE(client.read().has_value());
  EXPECT_TRUE(client.call->Finish().ok());
}

TEST(StreamListener, EventsOutOfSequenceOrForAnotherSessionFailIt) {
  for (const bool wrong_id : {false, true}) {
    SCOPED_TRACE(wrong_id ? "another session id" : "a sequence gap");
    Served served;
    auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
    wire::ClientEvent ping;
    ping.mutable_ping();
    if (wrong_id) {
      client.session_id += "x";
    } else {
      ++client.sequence;
    }
    ASSERT_TRUE(client.send(ping));
    ASSERT_NO_FATAL_FAILURE(expect_closed(client.read(), wire::SESSION_STATE_FAILED,
                                          wire::FAILURE_REASON_INVALID_EVENT));
    EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::INVALID_ARGUMENT);
  }
}

TEST(StreamListener, ASecondOpenFailsTheSession) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.send(open_event(wire::SESSION_MODE_STT_STREAMING)));
  ASSERT_NO_FATAL_FAILURE(
      expect_closed(client.read(), wire::SESSION_STATE_FAILED, wire::FAILURE_REASON_INVALID_EVENT));
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::INVALID_ARGUMENT);
}

TEST(StreamListener, OnlyACancelMayOmitTheSessionId) {
  {
    Served served;
    auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
    client.session_id.clear();
    wire::ClientEvent cancel;
    cancel.mutable_cancel();
    ASSERT_TRUE(client.send(cancel));
    // Taken as this session's event, and served as any Cancel.
    const auto accepted = client.read();
    ASSERT_TRUE(accepted.has_value());
    EXPECT_TRUE(accepted->has_cancel_accepted()) << accepted->DebugString();
    ASSERT_NO_FATAL_FAILURE(expect_closed(client.read(), wire::SESSION_STATE_CLOSED,
                                          wire::FAILURE_REASON_CLIENT_CANCELLED));
  }
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  client.session_id.clear();
  wire::ClientEvent ping;
  ping.mutable_ping();
  ASSERT_TRUE(client.send(ping));
  ASSERT_NO_FATAL_FAILURE(
      expect_closed(client.read(), wire::SESSION_STATE_FAILED, wire::FAILURE_REASON_INVALID_EVENT));
}

TEST(StreamListener, AnEventWithNoKnownBodyFailsTheSession) {
  for (const bool unknown_field : {false, true}) {
    SCOPED_TRACE(unknown_field ? "a body this server does not know" : "no body");
    Served served;
    auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
    wire::ClientEvent event;
    if (unknown_field) {
      event.GetReflection()->MutableUnknownFields(&event)->AddLengthDelimited(99, "later");
    }
    ASSERT_TRUE(client.send(event));
    ASSERT_NO_FATAL_FAILURE(expect_closed(client.read(), wire::SESSION_STATE_FAILED,
                                          wire::FAILURE_REASON_INVALID_EVENT));
    EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::INVALID_ARGUMENT);
  }
}

TEST(StreamListener, AHalfCloseBeforeOpenIsRefused) {
  Served served;
  auto client = served.connect();
  ASSERT_TRUE(client.call->WritesDone());
  EXPECT_FALSE(client.read().has_value());
  const auto status = client.call->Finish();
  EXPECT_EQ(status.error_code(), grpc::StatusCode::INVALID_ARGUMENT);
  EXPECT_NE(status.error_message().find("FAILURE_REASON_INVALID_EVENT"), std::string::npos);
  EXPECT_EQ(served.listener().live_sessions(), 0U);
}

TEST(StreamListener, ACancelThatRacesOpenLeavesNoSession) {
  Served served;
  served.dispatch().hold_negotiation();
  auto client = served.connect();
  ASSERT_TRUE(client.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
  served.dispatch().wait_until_negotiating();
  client.context->TryCancel();
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::CANCELLED);
  // Long enough for the server to have seen the cancellation before it admits.
  std::this_thread::sleep_for(300ms);
  served.dispatch().release_negotiation();
  EXPECT_TRUE(eventually([&] { return served.dispatch().seen.aborts() == 1; }))
      << "the session admitted behind the cancel was not aborted";
  EXPECT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
  // The slot is free again.
  auto next = served.ready(wire::SESSION_MODE_STT_STREAMING);
  EXPECT_FALSE(next.session_id.empty());
}

TEST(StreamListener, AFailedInitializationSendsSessionClosedAloneAndItsStatus) {
  Served served;
  served.dispatch().fail_initialization = true;
  auto client = served.connect();
  ASSERT_TRUE(client.call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
  const auto closed = client.read();
  ASSERT_NO_FATAL_FAILURE(
      expect_closed(closed, wire::SESSION_STATE_FAILED, wire::FAILURE_REASON_BACKEND_UNAVAILABLE));
  EXPECT_EQ(closed->sequence(), 1U);
  const auto status = end_of(client);
  EXPECT_EQ(status.error_code(), grpc::StatusCode::UNAVAILABLE);
  EXPECT_NE(status.error_message().find("session ended: FAILURE_REASON_BACKEND_UNAVAILABLE"),
            std::string::npos)
      << status.error_message();
  EXPECT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
}

TEST(StreamListener, RefusedOpensEndTheCallWithTheirStatus) {
  struct Case {
    const char* name;
    std::function<void(wire::ClientEvent&)> change;
    grpc::StatusCode code;
    const char* reason;
  };
  const std::vector<Case> cases{
      {"selector", [](auto& e) { e.mutable_open()->mutable_selector()->set_model("m"); },
       grpc::StatusCode::FAILED_PRECONDITION, "FAILURE_REASON_TARGET_UNRESOLVED"},
      {"another deployment",
       [](auto& e) { e.mutable_open()->mutable_resolved()->set_deployment_id("dep-b"); },
       grpc::StatusCode::FAILED_PRECONDITION, "FAILURE_REASON_TARGET_MISMATCH"},
      {"a descriptor digest",
       [](auto& e) {
         e.mutable_open()->mutable_resolved()->set_descriptor_digest("sha256:" +
                                                                     std::string(64, '0'));
       },
       grpc::StatusCode::FAILED_PRECONDITION, "FAILURE_REASON_TARGET_MISMATCH"},
      {"a generation", [](auto& e) { e.mutable_open()->mutable_resolved()->set_generation(7); },
       grpc::StatusCode::FAILED_PRECONDITION, "FAILURE_REASON_STALE_GENERATION"},
      {"no target", [](auto& e) { e.mutable_open()->clear_target(); },
       grpc::StatusCode::INVALID_ARGUMENT, "FAILURE_REASON_INVALID_EVENT"},
      {"a malformed deployment id",
       [](auto& e) { e.mutable_open()->mutable_resolved()->set_deployment_id(".."); },
       grpc::StatusCode::INVALID_ARGUMENT, "FAILURE_REASON_INVALID_EVENT"},
      {"sequence 2", [](auto& e) { e.set_sequence(2); }, grpc::StatusCode::INVALID_ARGUMENT,
       "FAILURE_REASON_INVALID_EVENT"},
      {"not an Open", [](auto& e) { e.mutable_ping(); }, grpc::StatusCode::INVALID_ARGUMENT,
       "FAILURE_REASON_INVALID_EVENT"},
      {"no mode", [](auto& e) { e.mutable_open()->set_mode(wire::SESSION_MODE_UNSPECIFIED); },
       grpc::StatusCode::INVALID_ARGUMENT, "FAILURE_REASON_INVALID_EVENT"},
      {"an unknown mode",
       [](auto& e) { e.mutable_open()->set_mode(static_cast<wire::SessionMode>(9)); },
       grpc::StatusCode::FAILED_PRECONDITION, "FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY"},
      {"a format not served",
       [](auto& e) { e.mutable_open()->mutable_input_format()->set_sample_rate_hz(8'000); },
       grpc::StatusCode::FAILED_PRECONDITION, "FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY"},
      {"no language", [](auto& e) { e.mutable_open()->clear_language(); },
       grpc::StatusCode::INVALID_ARGUMENT, "FAILURE_REASON_INVALID_EVENT"},
  };
  Served served;
  for (const auto& entry : cases) {
    SCOPED_TRACE(entry.name);
    auto open = open_event(wire::SESSION_MODE_STT_STREAMING);
    entry.change(open);
    const auto status = served.refused(open);
    EXPECT_EQ(status.error_code(), entry.code);
    EXPECT_NE(status.error_message().find(entry.reason), std::string::npos)
        << status.error_message();
    // How a refusal carries its OpenRefused is not decided: no detail yet.
    EXPECT_TRUE(status.error_details().empty());
  }
  EXPECT_EQ(served.listener().live_sessions(), 0U);
}

TEST(StreamListener, WithoutADispatchEveryOpenIsRefusedAsUnsupported) {
  Served served{false};
  for (const auto mode : {wire::SESSION_MODE_STT_STREAMING, wire::SESSION_MODE_TTS_STREAMING}) {
    const auto status = served.refused(open_event(mode));
    EXPECT_EQ(status.error_code(), grpc::StatusCode::FAILED_PRECONDITION);
    EXPECT_NE(status.error_message().find("FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY"),
              std::string::npos);
  }
}

TEST(StreamListener, ASecondSessionIsRefusedWhileOneIsLive) {
  Served served;
  auto first = served.ready(wire::SESSION_MODE_STT_STREAMING);
  const auto status = served.refused(open_event(wire::SESSION_MODE_TTS_STREAMING));
  EXPECT_EQ(status.error_code(), grpc::StatusCode::RESOURCE_EXHAUSTED);
  EXPECT_NE(status.error_message().find("FAILURE_REASON_SESSION_COUNT_LIMIT"), std::string::npos);
  EXPECT_EQ(served.listener().live_sessions(), 1U);
  // The refused call left nothing behind: the slot is served again once the
  // first session has given it back.
  ASSERT_TRUE(first.call->WritesDone());
  ASSERT_NO_FATAL_FAILURE(
      expect_closed(first.read(), wire::SESSION_STATE_CLOSED, wire::FAILURE_REASON_UNSPECIFIED));
  EXPECT_TRUE(end_of(first).ok());
  ASSERT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
  auto next = served.ready(wire::SESSION_MODE_TTS_STREAMING);
  EXPECT_FALSE(next.session_id.empty());
}

TEST(StreamListener, StopDrainsALiveSessionThenRefusesAndCloses) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  served.listener().stop_admission();
  ASSERT_NO_FATAL_FAILURE(expect_closed(client.read(), wire::SESSION_STATE_CLOSED,
                                        wire::FAILURE_REASON_WORKER_SHUTDOWN));
  const auto status = end_of(client);
  EXPECT_EQ(status.error_code(), grpc::StatusCode::UNAVAILABLE);
  EXPECT_NE(status.error_message().find("FAILURE_REASON_WORKER_SHUTDOWN"), std::string::npos);
  const auto refused = served.refused(open_event(wire::SESSION_MODE_STT_STREAMING));
  EXPECT_EQ(refused.error_code(), grpc::StatusCode::UNAVAILABLE);
  EXPECT_NE(refused.error_message().find("FAILURE_REASON_ADMISSION_CLOSED"), std::string::npos);
  const auto began = std::chrono::steady_clock::now();
  served.listener().stop(began + 5s);
  EXPECT_LT(std::chrono::steady_clock::now() - began, 5s) << "nothing was left to wait for";
  EXPECT_EQ(served.listener().live_sessions(), 0U) << "the drained session gave its slot back";
}

/// A dispatch that grants speech-to-text and never finishes a drain or
/// confirms a release, so a session outlives any drain deadline.
class HoldingDispatch final : public tensorplate::serving::SessionDispatch {
 public:
  void attach(tensorplate::serving::SessionManager& /*manager*/) override {}
  void stop() noexcept override {}
  [[nodiscard]] tensorplate::Result<tensorplate::serving::SessionTerms> negotiate(
      const tensorplate::serving::SessionRequest& request) const override {
    tensorplate::serving::SessionTerms terms;
    terms.input_kind = request.input_kind;
    terms.language = request.language;
    terms.audio_format = request.input_format;
    terms.min_frame = 20ms;
    terms.max_frame = 320ms;
    terms.max_utterance = 30s;
    return terms;
  }
  [[nodiscard]] tensorplate::Result<void> open_session(
      std::uint64_t /*session_key*/, const tensorplate::serving::SessionTerms& /*terms*/) override {
    return {};
  }
  void on_transition(const tensorplate::serving::ManagedSessionTransition& t) override {
    seen.transition(t);
  }
  void audio(std::uint64_t /*key*/, std::span<const std::byte> /*frame*/) override {}
  void text_segment(std::uint64_t /*key*/, std::uint64_t /*id*/, std::string /*text*/) override {}
  void finalize(std::uint64_t /*key*/, std::uint64_t /*sequence*/) override {}
  void output_delivered(std::uint64_t /*key*/, tensorplate::serving::OutputKind /*kind*/) override {
  }

  Seen seen;
};

/// A listener behind the holding dispatch, with one session that is ready.
struct HeldSession {
  explicit HeldSession(std::chrono::milliseconds deadline = 20s) {
    StreamListenerOptions options;
    options.deployment_id = kDeployment;
    listener = StreamListener::start(options, &dispatch, clock).value();
    stub = wire::SessionService::NewStub(grpc::CreateChannel(
        "127.0.0.1:" + std::to_string(listener->bound_port()), grpc::InsecureChannelCredentials()));
    context.set_deadline(std::chrono::system_clock::now() + deadline);
    call = stub->Session(&context);
    EXPECT_TRUE(call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
    wire::ServerEvent event;
    EXPECT_TRUE(call->Read(&event));
    EXPECT_TRUE(event.has_ready());
  }

  SystemSchedulerClock clock;
  HoldingDispatch dispatch;
  std::unique_ptr<StreamListener> listener;
  std::unique_ptr<wire::SessionService::Stub> stub;
  grpc::ClientContext context;
  std::unique_ptr<Call> call;
};

// The session cannot end by draining here, so only the cancel path ends it.
TEST(StreamListener, AClientCancelAbortsItsLiveSession) {
  HeldSession held;
  held.context.TryCancel();
  EXPECT_EQ(held.call->Finish().error_code(), grpc::StatusCode::CANCELLED);
  EXPECT_TRUE(eventually([&] { return held.dispatch.seen.aborts() == 1; }));
}

TEST(StreamListener, AClientDeadlineAbortsItsLiveSession) {
  HeldSession held{2s};
  wire::ServerEvent event;
  EXPECT_FALSE(held.call->Read(&event));
  EXPECT_EQ(held.call->Finish().error_code(), grpc::StatusCode::DEADLINE_EXCEEDED);
  EXPECT_TRUE(eventually([&] { return held.dispatch.seen.aborts() == 1; }));
}

/// What a client that reads gets of a session the stop failed: SessionClosed,
/// then the status of `worker_shutdown`. The dispatch never confirmed the
/// release, so the session still holds its slot.
void expect_failed_by_the_stop(HeldSession& held) {
  EXPECT_EQ(held.listener->live_sessions(), 1U);
  EXPECT_EQ(held.dispatch.seen.cause(), "worker_shutdown");
  wire::ServerEvent event;
  ASSERT_TRUE(held.call->Read(&event)) << "the call ended without SessionClosed";
  ASSERT_TRUE(event.has_session_closed()) << event.ShortDebugString();
  const auto& closed = event.session_closed();
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_WORKER_SHUTDOWN);
  EXPECT_EQ(closed.admission(), wire::ADMISSION_ADMITTED);
  EXPECT_FALSE(held.call->Read(&event)) << "SessionClosed is the last event";
  const auto status = held.call->Finish();
  EXPECT_EQ(status.error_code(), grpc::StatusCode::UNAVAILABLE) << status.error_message();
  EXPECT_NE(status.error_message().find("FAILURE_REASON_WORKER_SHUTDOWN"), std::string::npos)
      << status.error_message();
  EXPECT_EQ(held.dispatch.seen.aborts(), 0) << "the call was cancelled before its session failed";
}

// The session never drains. The stop fails it with the reserve of the window
// left, and gRPC lets its call write the terminal event before the window's
// end. The client reads only once the stop has returned.
TEST(StreamListener, AStopFailsASessionThatOutlastsTheDrainWindowAndWritesItsTerminalEvent) {
  HeldSession held;
  held.listener->stop(std::chrono::steady_clock::now() +
                      2 * tensorplate::serving::kTerminalEventReserve);
  expect_failed_by_the_stop(held);
}

// A window no longer than the reserve: the session is failed at once, and its
// call has the whole window for its terminal event.
TEST(StreamListener, WithADrainWindowNoLongerThanTheReserveTheStopFailsTheSessionAtOnce) {
  HeldSession held;
  held.listener->stop(std::chrono::steady_clock::now() +
                      tensorplate::serving::kTerminalEventReserve);
  expect_failed_by_the_stop(held);
}

TEST(StreamListener, BindsLoopbackOnlyAndOwnsItsPort) {
  SystemSchedulerClock clock;
  StreamListenerOptions outside;
  outside.host = "0.0.0.0";
  outside.deployment_id = kDeployment;
  const auto refused = StreamListener::start(outside, nullptr, clock);
  ASSERT_FALSE(refused.has_value());
  EXPECT_EQ(refused.error().code, tensorplate::Error::Code::Unsupported);

  StreamListenerOptions first;
  first.deployment_id = kDeployment;
  const std::size_t before = thread_count();
  auto listener = StreamListener::start(first, nullptr, clock);
  ASSERT_TRUE(listener.has_value());
  ::testing::Test::RecordProperty("threads_added_by_listener",
                                  static_cast<int>(thread_count() - before));
  StreamListenerOptions second = first;
  second.port = (*listener)->bound_port();
  const auto taken = StreamListener::start(second, nullptr, clock);
  ASSERT_FALSE(taken.has_value());
  EXPECT_EQ(taken.error().code, tensorplate::Error::Code::Unavailable);
}

TEST(StreamListener, LocalhostBindsTheIpv4LoopbackOnly) {
  // Without an IPv6 loopback nothing here could tell a resolved bind apart.
  SystemSchedulerClock clock;
  StreamListenerOptions ipv6;
  ipv6.host = "::1";
  ipv6.deployment_id = kDeployment;
  const auto on_ipv6 = StreamListener::start(ipv6, nullptr, clock);
  if (!on_ipv6.has_value() || !accepts(AF_INET6, (*on_ipv6)->bound_port())) {
    GTEST_SKIP() << "this host has no IPv6 loopback";
  }
  Served served{false, "localhost"};
  const auto port = served.listener().bound_port();
  EXPECT_TRUE(accepts(AF_INET, port));
  EXPECT_FALSE(accepts(AF_INET6, port)) << "localhost was resolved, not mapped";
}

/// An HTTP/2 connection to `port` on loopback that opens no call. Once the
/// server has acknowledged its SETTINGS it reads nothing more, so it answers
/// nothing the server sends, until it is closed.
class SilentConnection {
 public:
  explicit SilentConnection(std::uint16_t port) : fd_(::socket(AF_INET, SOCK_STREAM, 0)) {
    constexpr std::uint8_t kSettings = 4;
    // The client preface, then a SETTINGS frame that sets nothing.
    std::string hello{"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"};
    hello.append({0, 0, 0, static_cast<char>(kSettings), 0, 0, 0, 0, 0});
    // A server that never acknowledges fails the test instead of hanging it.
    const timeval patience{20, 0};
    (void)::setsockopt(fd_, SOL_SOCKET, SO_RCVTIMEO, &patience, sizeof(patience));
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = htons(port);
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
    if (::connect(fd_, reinterpret_cast<const sockaddr*>(&address), sizeof(address)) != 0 ||
        ::write(fd_, hello.data(), hello.size()) != static_cast<ssize_t>(hello.size())) {
      return;
    }
    // Frames: a 24-bit length, a type, flags, a stream id, then the payload.
    for (std::string frame; read_exactly(frame, 9);) {
      const auto length = (static_cast<std::size_t>(static_cast<std::uint8_t>(frame[0])) << 16U) |
                          (static_cast<std::size_t>(static_cast<std::uint8_t>(frame[1])) << 8U) |
                          static_cast<std::uint8_t>(frame[2]);
      const bool settings = static_cast<std::uint8_t>(frame[3]) == kSettings;
      const bool acknowledgement = (static_cast<std::uint8_t>(frame[4]) & 1U) != 0;
      if (!read_exactly(frame, length)) {
        return;
      }
      if (settings && acknowledgement) {
        established_ = true;
        return;
      }
    }
  }
  ~SilentConnection() { close(); }
  SilentConnection(const SilentConnection&) = delete;
  SilentConnection& operator=(const SilentConnection&) = delete;

  /// The server has read this connection's preface and answered it.
  [[nodiscard]] bool established() const noexcept { return established_; }

  void close() noexcept {
    if (fd_ >= 0) {
      ::close(fd_);
      fd_ = -1;
    }
  }

 private:
  bool read_exactly(std::string& bytes, std::size_t size) const {
    bytes.assign(size, '\0');
    std::size_t have = 0;
    while (have < size) {
      const auto got = ::read(fd_, bytes.data() + have, size - have);
      if (got <= 0) {
        return false;
      }
      have += static_cast<std::size_t>(got);
    }
    return true;
  }

  int fd_;
  bool established_ = false;
};

/// A worker with the stream listener on an ephemeral port, read back from the
/// line the worker logs when it binds.
struct StreamingWorker {
  /// `registry`, when given, resolves the backend `config` names.
  explicit StreamingWorker(tensorplate::ServingConfig config = {},
                           tensorplate::BackendRegistry* registry = nullptr) {
    config.deployment.endpoint = kDeployment;
    config.streaming.enabled = true;
    auto created = registry != nullptr ? tensorplate::ServingWorker::create(config, *registry)
                                       : tensorplate::ServingWorker::create(config);
    EXPECT_TRUE(created.has_value());
    worker = std::move(created).value();
    ::testing::internal::CaptureStderr();
    const auto started = worker->start();
    std::istringstream log{::testing::internal::GetCapturedStderr()};
    EXPECT_TRUE(started.has_value());
    for (std::string line; std::getline(log, line);) {
      const auto entry = nlohmann::json::parse(line, nullptr, false);
      if (entry.is_object() && entry.value("message", "") == "stream listener bound") {
        port = entry.at("fields").at("port").get<std::uint16_t>();
        serves_sessions = entry.at("fields").at("serves_sessions").get<bool>();
      }
    }
    stub = wire::SessionService::NewStub(grpc::CreateChannel("127.0.0.1:" + std::to_string(port),
                                                             grpc::InsecureChannelCredentials()));
  }

  /// The status of a call that sends `open` and closes.
  [[nodiscard]] grpc::Status open(const wire::ClientEvent& event) const {
    grpc::ClientContext context;
    context.set_deadline(std::chrono::system_clock::now() + 10s);
    auto call = stub->Session(&context);
    call->Write(event);
    call->WritesDone();
    wire::ServerEvent reply;
    EXPECT_FALSE(call->Read(&reply));
    return call->Finish();
  }

  std::unique_ptr<tensorplate::ServingWorker> worker;
  std::uint16_t port = 0;
  bool serves_sessions = true;
  std::unique_ptr<wire::SessionService::Stub> stub;
};

TEST(StreamListener, TheWorkerServesNoStreamModeYetAndStopsCleanly) {
  StreamingWorker served;
  ASSERT_NE(served.port, 0) << "the worker logs the port it bound";
  EXPECT_FALSE(served.serves_sessions);
  const auto status = served.open(open_event(wire::SESSION_MODE_STT_STREAMING));
  EXPECT_EQ(status.error_code(), grpc::StatusCode::FAILED_PRECONDITION);
  EXPECT_NE(status.error_message().find("FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY"),
            std::string::npos);
  served.worker->shutdown("test");
  EXPECT_EQ(served.worker->stop(), tensorplate::ServingExitCode::Ok);
}

TEST(StreamListener, TheWorkerClosesStreamAdmissionAtShutdownAndTheListenerAtStop) {
  StreamingWorker served;
  ASSERT_NE(served.port, 0);
  served.worker->shutdown("test");
  // Still listening, and saying why it takes no session.
  const auto refused = served.open(open_event(wire::SESSION_MODE_STT_STREAMING));
  EXPECT_EQ(refused.error_code(), grpc::StatusCode::UNAVAILABLE);
  EXPECT_NE(refused.error_message().find("FAILURE_REASON_ADMISSION_CLOSED"), std::string::npos)
      << refused.error_message();
  EXPECT_EQ(served.worker->stop(), tensorplate::ServingExitCode::Ok);
  EXPECT_FALSE(accepts(AF_INET, served.port));
  EXPECT_EQ(served.worker->stop(), tensorplate::ServingExitCode::Ok) << "stop is safe to repeat";
}

/// Holds every infer call until the test opens the gate.
struct InferGate {
  std::mutex mutex;
  std::condition_variable cv;
  bool entered = false;
  bool open = false;

  void release() {
    {
      const std::lock_guard lock(mutex);
      open = true;
    }
    cv.notify_all();
  }
};

class GatedSession final : public tensorplate::ExecutionSession {
 public:
  GatedSession(tensorplate::ExecutionSessionRuntimeHooks hooks, std::shared_ptr<InferGate> gate)
      : ExecutionSession(hooks), gate_(std::move(gate)) {}

  [[nodiscard]] std::string_view backend_name() const noexcept override { return "gated"; }

 protected:
  tensorplate::Result<void> do_load(const tensorplate::ModelSpec& /*spec*/) override { return {}; }
  tensorplate::Result<void> do_prime() override { return {}; }
  tensorplate::Result<std::vector<tensorplate::NamedOutput>> do_infer(
      const tensorplate::InferRequest& /*request*/) override {
    std::unique_lock lock(gate_->mutex);
    gate_->entered = true;
    gate_->cv.notify_all();
    gate_->cv.wait(lock, [this] { return gate_->open; });
    return tensorplate::unexpected(tensorplate::Error::Code::InferenceFailed,
                                   "gated fixture produces no outputs");
  }

 private:
  std::shared_ptr<InferGate> gate_;
};

std::string infer_body() {
  const std::vector<std::byte> payload(4);
  nlohmann::json input;
  input["name"] = "image";
  input["tensor"] = {{"dtype", "uint8"}, {"shape", {2, 2}}, {"byte_size", 4}};
  input["payload_b64"] = tensorplate::serving::base64_encode(payload.data(), payload.size());
  nlohmann::json body;
  body["schema_version"] = "0.1";
  body["request_id"] = "held";
  body["endpoint"] = kDeployment;
  body["inputs"] = nlohmann::json::array({input});
  return body.dump();
}

/// The `drain_ends_ns` of the worker's log line `message`, on the steady clock.
std::optional<std::chrono::steady_clock::time_point> logged_drain_end(const std::string& log,
                                                                      std::string_view message) {
  std::istringstream lines{log};
  for (std::string line; std::getline(lines, line);) {
    const auto entry = nlohmann::json::parse(line, nullptr, false);
    if (entry.is_object() && entry.value("message", "") == message) {
      return std::chrono::steady_clock::time_point{
          std::chrono::nanoseconds{entry.at("fields").at("drain_ends_ns").get<std::int64_t>()}};
    }
  }
  return std::nullopt;
}

// A stop ends the stream listener before it waits for a unary request in
// flight, which therefore cannot use up the window stream sessions drain in.
// The HTTP port still answers once shutdown is asked, and with the stream
// port closed for as long as the stream listener is stopping; then it closes
// too, before the request in flight has been waited for.
// Both drains end with the one window that began when shutdown was asked.
// The worker gives its listener no dispatch, so no session exists here: what
// a listener's stop does to its sessions is shown on the listener itself.
TEST(StreamListener, TheWorkersStopEndsItsStreamListenerWhileAUnaryRequestIsStillInFlight) {
  auto gate = std::make_shared<InferGate>();
  tensorplate::BackendRegistry registry;
  auto capability =
      tensorplate::BackendCapability::create("gated", {tensorplate::PrecisionHint::Auto});
  ASSERT_TRUE(capability.has_value()) << capability.error().message;
  ASSERT_TRUE(registry
                  .register_backend(tensorplate::BackendEntry{
                      "gated",
                      capability.value(),
                      [gate](tensorplate::ExecutionSessionRuntimeHooks hooks)
                          -> tensorplate::Result<std::unique_ptr<tensorplate::ExecutionSession>> {
                        return std::unique_ptr<tensorplate::ExecutionSession>(
                            new GatedSession(hooks, gate));
                      },
                  })
                  .has_value());
  tensorplate::ServingConfig config;
  config.deployment.use_mock_session = false;
  config.deployment.backend = "gated";
  config.deployment.model =
      tensorplate::ModelSpec::create("gated-model", tensorplate::ModelClass::Custom,
                                     "fixture://gated", "gated")
          .value();
  config.shutdown.drain_deadline = 10min;
  StreamingWorker served{config, &registry};
  ASSERT_NE(served.port, 0);

  const auto http_port = served.worker->bound_port();
  std::thread caller([http_port] {
    tensorplate::testing::HttpClient client("127.0.0.1", http_port);
    (void)client.post("/infer", infer_body());
  });
  std::atomic<bool> stop_returned{false};
  std::thread stopper;
  struct OpenAndJoin {
    InferGate& gate;
    std::thread& caller;
    std::thread& stopper;
    ~OpenAndJoin() {
      gate.release();
      for (auto* thread : {&caller, &stopper}) {
        if (thread->joinable()) {
          thread->join();
        }
      }
    }
  } open_and_join{*gate, caller, stopper};
  {
    std::unique_lock lock(gate->mutex);
    ASSERT_TRUE(gate->cv.wait_for(lock, 10s, [&] { return gate->entered; }));
  }

  ::testing::internal::CaptureStderr();
  const auto before_shutdown = std::chrono::steady_clock::now();
  served.worker->shutdown("test");
  const auto after_shutdown = std::chrono::steady_clock::now();
  // Stream admission is closed at once, with the request still in flight.
  const auto refused = served.open(open_event(wire::SESSION_MODE_STT_STREAMING));
  EXPECT_NE(refused.error_message().find("FAILURE_REASON_ADMISSION_CLOSED"), std::string::npos)
      << refused.error_message();
  // The HTTP side still answers beside the request it holds: 503, stopping.
  EXPECT_EQ(tensorplate::testing::HttpClient("127.0.0.1", http_port).get("/health").status, 503);
  // The gate between the two ports. gRPC's shutdown closes the port, sends
  // each connection a ping and waits for the connection to end; this one does
  // not answer, so the stream listener's stop stays there until the test ends
  // the connection (gRPC itself would after 20 s).
  SilentConnection unanswering{served.port};
  ASSERT_TRUE(unanswering.established());
  auto exit_code = tensorplate::ServingExitCode::Internal;
  stopper = std::thread([&] {
    exit_code = served.worker->stop();
    stop_returned = true;
  });
  // The request is still held at its gate, which only this test opens.
  ASSERT_TRUE(eventually([&] { return !accepts(AF_INET, served.port); }))
      << "the stop waited for the unary request before it reached the stream listener";
  // The stream listener is still stopping, so the HTTP listener's stop has
  // not begun: its port accepts and answers.
  ASSERT_TRUE(accepts(AF_INET, http_port))
      << "the HTTP port closed while the stream listener was still stopping";
  EXPECT_EQ(tensorplate::testing::HttpClient("127.0.0.1", http_port).get("/health").status, 503);
  EXPECT_FALSE(stop_returned);
  unanswering.close();
  // The HTTP listener's stop comes next. It closes the port first and then
  // waits for the request, so nothing answers on it for the rest of the stop.
  ASSERT_TRUE(eventually([&] { return !accepts(AF_INET, http_port); }))
      << "the HTTP port stayed open after the stream listener had stopped";
  EXPECT_FALSE(stop_returned) << "the stop did not wait for the request in flight";
  gate->release();
  stopper.join();
  const auto log = ::testing::internal::GetCapturedStderr();
  EXPECT_EQ(exit_code, tensorplate::ServingExitCode::Ok);
  EXPECT_FALSE(accepts(AF_INET, http_port)) << "the HTTP listener outlived the stop";
  for (const auto* drain : {"stream listener stopping", "draining requests"}) {
    SCOPED_TRACE(drain);
    const auto end = logged_drain_end(log, drain);
    ASSERT_TRUE(end.has_value()) << log;
    EXPECT_GE(*end, before_shutdown + config.shutdown.drain_deadline);
    EXPECT_LE(*end, after_shutdown + config.shutdown.drain_deadline)
        << "the window began after shutdown was asked";
  }
}

TEST(StreamListener, ShutdownFromAnotherThreadWhileTheWorkerServes) {
  for (int round = 0; round < 20; ++round) {
    tensorplate::ServingConfig config;
    config.enable_stderr_logs = false;
    config.deployment.endpoint = kDeployment;
    config.streaming.enabled = true;
    auto worker = tensorplate::ServingWorker::create(config).value();
    ASSERT_TRUE(worker->start().has_value());
    auto exit_code = tensorplate::ServingExitCode::Internal;
    std::thread serving([&] { exit_code = worker->serve_forever(); });
    worker->shutdown("test");
    serving.join();
    EXPECT_EQ(exit_code, tensorplate::ServingExitCode::Ok);
  }
}

}  // namespace
