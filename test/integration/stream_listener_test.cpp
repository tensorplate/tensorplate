// SPDX-License-Identifier: Apache-2.0
//
// The gRPC stream listener with a real client over loopback.

#include "serving/stream/stream_listener.hpp"

#include <arpa/inet.h>
#include <grpcpp/grpcpp.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
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
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/serving/config.hpp"
#include "tensorplate/serving/worker.hpp"
#include "tensorplate/stream/v1/session.grpc.pb.h"

#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"
#include "stream_test_dispatch.hpp"

namespace {

using namespace std::chrono_literals;
using tensorplate::SystemSchedulerClock;
using tensorplate::serving::StreamListener;
using tensorplate::serving::StreamListenerOptions;
using tensorplate::testing::StreamTestDispatch;
namespace wire = tensorplate::stream::v1;

constexpr const char* kDeployment = "dep-a";
using Call = grpc::ClientReaderWriter<wire::ClientEvent, wire::ServerEvent>;

wire::ClientEvent open_event(wire::SessionMode mode) {
  wire::ClientEvent event;
  event.set_sequence(1);
  auto& open = *event.mutable_open();
  open.mutable_resolved()->set_deployment_id(kDeployment);
  open.set_mode(mode);
  open.set_language("en");
  if (mode == wire::SESSION_MODE_STT_STREAMING) {
    auto& format = *open.mutable_input_format();
    format.set_encoding(wire::AUDIO_ENCODING_PCM_S16LE);
    format.set_sample_rate_hz(16'000);
    format.set_channels(1);
  } else {
    open.set_voice("voice-a");
  }
  return event;
}

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

/// Polls until `done` or five seconds.
bool eventually(const std::function<bool()>& done) {
  const auto deadline = std::chrono::steady_clock::now() + 5s;
  while (!done()) {
    if (std::chrono::steady_clock::now() >= deadline) {
      return false;
    }
    std::this_thread::sleep_for(5ms);
  }
  return true;
}

/// What a dispatch was shown, for tests that ask how a session ended.
class Seen {
 public:
  void transition(const tensorplate::serving::ManagedSessionTransition& transition) {
    const std::lock_guard guard(mu_);
    if (transition.event == tensorplate::serving::LogicalSessionEvent::Abort) {
      ++aborts_;
    }
  }
  void delivery() {
    const std::lock_guard guard(mu_);
    ++deliveries_;
  }
  [[nodiscard]] int aborts() const {
    const std::lock_guard guard(mu_);
    return aborts_;
  }
  [[nodiscard]] int deliveries() const {
    const std::lock_guard guard(mu_);
    return deliveries_;
  }

 private:
  mutable std::mutex mu_;
  int aborts_ = 0;
  int deliveries_ = 0;
};

/// The synthetic dispatch, with an initialization that can fail and a
/// negotiation that can be held.
class SteeredDispatch final : public tensorplate::serving::SessionDispatch {
 public:
  explicit SteeredDispatch(const tensorplate::SchedulerClock& clock) : inner_(clock) {}

  void attach(tensorplate::serving::SessionManager& manager) override { inner_.attach(manager); }
  void stop() noexcept override { inner_.stop(); }
  [[nodiscard]] tensorplate::Result<tensorplate::serving::SessionTerms> negotiate(
      const tensorplate::serving::SessionRequest& request) const override {
    std::unique_lock lock(mu_);
    negotiating_ = true;
    cv_.notify_all();
    cv_.wait(lock, [&] { return !hold_; });
    lock.unlock();
    return inner_.negotiate(request);
  }
  [[nodiscard]] tensorplate::Result<void> open_session(
      std::uint64_t key, const tensorplate::serving::SessionTerms& terms) override {
    if (fail_initialization) {
      return tensorplate::unexpected(tensorplate::Error::make(tensorplate::Error::Code::Unavailable,
                                                              "no backend", "backend_unavailable"));
    }
    return inner_.open_session(key, terms);
  }
  void on_transition(const tensorplate::serving::ManagedSessionTransition& t) override {
    seen.transition(t);
    inner_.on_transition(t);
  }
  void audio(std::uint64_t key, std::span<const std::byte> frame) override {
    inner_.audio(key, frame);
  }
  void text_segment(std::uint64_t key, std::uint64_t id, std::string text) override {
    inner_.text_segment(key, id, std::move(text));
  }
  void finalize(std::uint64_t key, std::uint64_t sequence) override {
    inner_.finalize(key, sequence);
  }
  void output_delivered(std::uint64_t key, tensorplate::serving::OutputKind kind) override {
    seen.delivery();
    inner_.output_delivered(key, kind);
  }

  void hold_negotiation() {
    const std::lock_guard guard(mu_);
    hold_ = true;
  }
  void wait_until_negotiating() {
    std::unique_lock lock(mu_);
    cv_.wait(lock, [&] { return negotiating_; });
  }
  void release_negotiation() {
    {
      const std::lock_guard guard(mu_);
      hold_ = false;
    }
    cv_.notify_all();
  }

  std::atomic<bool> fail_initialization{false};
  Seen seen;

 private:
  StreamTestDispatch inner_;
  mutable std::mutex mu_;
  mutable std::condition_variable cv_;
  mutable bool negotiating_ = false;
  bool hold_ = false;
};

/// A listener with the synthetic dispatch behind it, or none.
class Served {
 public:
  explicit Served(bool with_dispatch = true, const std::string& host = "127.0.0.1") {
    if (with_dispatch) {
      dispatch_ = std::make_unique<SteeredDispatch>(clock_);
    }
    StreamListenerOptions options;
    options.host = host;
    options.deployment_id = kDeployment;
    options.limits = tensorplate::serving::SessionLimits::create(60s, 10s, 30s, 60min, 1).value();
    listener_ = StreamListener::start(std::move(options), dispatch_.get(), clock_).value();
    channel_ = grpc::CreateChannel("127.0.0.1:" + std::to_string(listener_->bound_port()),
                                   grpc::InsecureChannelCredentials());
    stub_ = wire::SessionService::NewStub(channel_);
  }
  ~Served() { listener_.reset(); }
  Served(const Served&) = delete;
  Served& operator=(const Served&) = delete;

  struct Client {
    std::unique_ptr<grpc::ClientContext> context = std::make_unique<grpc::ClientContext>();
    std::unique_ptr<Call> call;
    std::uint64_t sequence = 1;
    std::string session_id;

    bool send(wire::ClientEvent event) {
      event.set_sequence(++sequence);
      event.set_session_id(session_id);
      return call->Write(event);
    }
    std::optional<wire::ServerEvent> read() {
      wire::ServerEvent event;
      if (!call->Read(&event)) {
        return std::nullopt;
      }
      return event;
    }
  };

  Client connect(std::chrono::milliseconds deadline = 20s) {
    Client client;
    client.context->set_deadline(std::chrono::system_clock::now() + deadline);
    client.call = stub_->Session(client.context.get());
    return client;
  }

  /// A client whose Open was answered by Ready.
  Client ready(wire::SessionMode mode, wire::Ready* ready_out = nullptr,
               std::chrono::milliseconds deadline = 20s) {
    auto client = connect(deadline);
    EXPECT_TRUE(client.call->Write(open_event(mode)));
    const auto ready = client.read();
    EXPECT_TRUE(ready && ready->has_ready());
    if (ready && ready->has_ready()) {
      client.session_id = ready->ready().session_id();
      if (ready_out != nullptr) {
        *ready_out = ready->ready();
      }
    }
    return client;
  }

  /// The status of a call whose Open is `open`, which must be refused.
  grpc::Status refused(const wire::ClientEvent& open) {
    auto client = connect();
    client.call->Write(open);
    client.call->WritesDone();
    wire::ServerEvent event;
    EXPECT_FALSE(client.call->Read(&event)) << "a refused Open has no event";
    return client.call->Finish();
  }

  StreamListener& listener() { return *listener_; }
  SteeredDispatch& dispatch() { return *dispatch_; }

 private:
  SystemSchedulerClock clock_;
  std::unique_ptr<SteeredDispatch> dispatch_;
  std::unique_ptr<StreamListener> listener_;
  std::shared_ptr<grpc::Channel> channel_;
  std::unique_ptr<wire::SessionService::Stub> stub_;
};

/// The status of a call whose session has sent its last event.
grpc::Status end_of(Served::Client& client) {
  EXPECT_FALSE(client.read().has_value()) << "SessionClosed is the last event";
  return client.call->Finish();
}

void expect_closed(const std::optional<wire::ServerEvent>& event, wire::SessionState state,
                   wire::FailureReason reason) {
  ASSERT_TRUE(event.has_value());
  ASSERT_TRUE(event->has_session_closed()) << event->DebugString();
  const auto& closed = event->session_closed();
  EXPECT_EQ(closed.state(), state);
  EXPECT_EQ(closed.admission(), wire::ADMISSION_ADMITTED);
  EXPECT_EQ(closed.last_accepted_sequence(), 0U);
  EXPECT_TRUE(closed.has_totals());
  if (reason == wire::FAILURE_REASON_UNSPECIFIED) {
    EXPECT_FALSE(closed.has_cause());
  } else {
    ASSERT_TRUE(closed.has_cause());
    EXPECT_EQ(closed.cause().reason(), reason);
  }
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

TEST(StreamListener, EventsNotServedYetFailTheSessionAsUnsupported) {
  std::vector<wire::ClientEvent> events(5);
  events[0].mutable_audio()->set_data(std::string(640, '\0'));
  events[1].mutable_text_segment()->set_text("hi");
  events[2].mutable_finalize();
  events[3].mutable_cancel();
  events[4].mutable_status_request();
  for (const auto& event : events) {
    SCOPED_TRACE(event.ShortDebugString());
    Served served;
    auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
    ASSERT_TRUE(client.send(event));
    const auto closed = client.read();
    ASSERT_NO_FATAL_FAILURE(expect_closed(closed, wire::SESSION_STATE_FAILED,
                                          wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY));
    EXPECT_EQ(closed->session_closed().cause().code(), wire::ERROR_CODE_UNSUPPORTED);
    const auto status = end_of(client);
    EXPECT_EQ(status.error_code(), grpc::StatusCode::FAILED_PRECONDITION);
    EXPECT_NE(status.error_message().find("FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY"),
              std::string::npos);
  }
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
    // Accepted as this session's event; Cancel itself is not served yet.
    ASSERT_NO_FATAL_FAILURE(expect_closed(client.read(), wire::SESSION_STATE_FAILED,
                                          wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY));
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

TEST(StreamListener, StopAtTheDeadlineCancelsACallWhoseSessionNeverDrains) {
  SystemSchedulerClock clock;
  HoldingDispatch dispatch;
  StreamListenerOptions options;
  options.deployment_id = kDeployment;
  auto listener = StreamListener::start(options, &dispatch, clock).value();
  const auto stub = wire::SessionService::NewStub(grpc::CreateChannel(
      "127.0.0.1:" + std::to_string(listener->bound_port()), grpc::InsecureChannelCredentials()));
  grpc::ClientContext context;
  context.set_deadline(std::chrono::system_clock::now() + 20s);
  auto call = stub->Session(&context);
  ASSERT_TRUE(call->Write(open_event(wire::SESSION_MODE_STT_STREAMING)));
  wire::ServerEvent event;
  ASSERT_TRUE(call->Read(&event));
  ASSERT_TRUE(event.has_ready());

  const auto began = std::chrono::steady_clock::now();
  listener->stop(began + 300ms);
  const auto took = std::chrono::steady_clock::now() - began;
  EXPECT_GE(took, 300ms) << "the session never drained, so stop waited for the deadline";
  EXPECT_LT(took, 10s);
  // Its call was cancelled, and the dispatch never confirmed the release.
  EXPECT_EQ(listener->live_sessions(), 1U);
  while (call->Read(&event)) {
    EXPECT_FALSE(event.has_ready());
  }
  // The server cancels the call as it shuts down; the client reads that as
  // the server going away or as the cancellation itself.
  const auto code = call->Finish().error_code();
  EXPECT_TRUE(code == grpc::StatusCode::UNAVAILABLE || code == grpc::StatusCode::CANCELLED) << code;
  listener.reset();
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

/// A worker with the stream listener on an ephemeral port, read back from the
/// line the worker logs when it binds.
struct StreamingWorker {
  explicit StreamingWorker(std::chrono::milliseconds drain_window = 5s) {
    tensorplate::ServingConfig config;
    config.deployment.endpoint = kDeployment;
    config.streaming.enabled = true;
    config.shutdown.drain_deadline = drain_window;
    auto created = tensorplate::ServingWorker::create(config);
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

TEST(StreamListener, TheWorkerDrainsStreamsInsideTheWindowThatBeganAtShutdown) {
  StreamingWorker served{2s};
  ASSERT_NE(served.port, 0);
  // A call that has sent nothing keeps the server open until the window ends.
  grpc::ClientContext context;
  context.set_deadline(std::chrono::system_clock::now() + 20s);
  auto call = served.stub->Session(&context);
  // Long enough for the server to have the call before it stops accepting.
  std::this_thread::sleep_for(300ms);
  const auto asked = std::chrono::steady_clock::now();
  served.worker->shutdown("test");
  std::this_thread::sleep_for(1500ms);
  EXPECT_EQ(served.worker->stop(), tensorplate::ServingExitCode::Ok);
  const auto took = std::chrono::steady_clock::now() - asked;
  EXPECT_GE(took, 1900ms) << "the open call was given the window";
  // A window counted from stop() would end 3.5 s after the shutdown.
  EXPECT_LT(took, 3200ms) << "the window was counted from shutdown, not from stop";
  const auto code = call->Finish().error_code();
  EXPECT_TRUE(code == grpc::StatusCode::UNAVAILABLE || code == grpc::StatusCode::CANCELLED) << code;
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
