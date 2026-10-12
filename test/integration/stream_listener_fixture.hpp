// SPDX-License-Identifier: Apache-2.0
#pragma once

// What the stream listener's tests share: a listener over loopback with the
// synthetic session dispatch behind it, and a client that keeps a stream's
// accounts the way the schema asks a client to.

#include <grpcpp/grpcpp.h>
#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/stream/v1/session.grpc.pb.h"

#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"
#include "serving/stream/stream_listener.hpp"
#include "synthetic_session_dispatch.hpp"

namespace tensorplate::testing::stream_listener {

namespace wire = ::tensorplate::stream::v1;

inline constexpr const char* kDeployment = "dep-a";
using Call = grpc::ClientReaderWriter<wire::ClientEvent, wire::ServerEvent>;

inline wire::ClientEvent open_event(wire::SessionMode mode) {
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

/// Polls until `done` or five seconds.
inline bool eventually(const std::function<bool()>& done) {
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds{5};
  while (!done()) {
    if (std::chrono::steady_clock::now() >= deadline) {
      return false;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds{5});
  }
  return true;
}

/// What a dispatch was shown and asked, for tests that pace themselves from
/// the server's side or ask how a session ended.
class Seen {
 public:
  void transition(const serving::ManagedSessionTransition& transition) {
    const std::lock_guard guard(mu_);
    if (transition.event) {
      events_.push_back(*transition.event);
    }
    if (transition.event == serving::LogicalSessionEvent::Abort) {
      ++aborts_;
    }
    if (transition.effects.contains(serving::LogicalSessionEffect::AcceptInput)) {
      ++inputs_;
    }
    waiting_segments_ = transition.waiting_segments.used();
    state_ = transition.state;
    cause_ = transition.cause ? transition.cause->context.value_or("") : "";
    output_ = transition.output;
  }
  void delivery(serving::OutputKind kind) {
    const std::lock_guard guard(mu_);
    ++deliveries_;
    if (kind == serving::OutputKind::Audio) {
      ++audio_deliveries_;
    }
  }
  void finalize(std::uint64_t client_sequence) {
    const std::lock_guard guard(mu_);
    finalizes_.push_back(client_sequence);
  }

  [[nodiscard]] int aborts() const {
    const std::lock_guard guard(mu_);
    return aborts_;
  }
  [[nodiscard]] int deliveries() const {
    const std::lock_guard guard(mu_);
    return deliveries_;
  }
  [[nodiscard]] std::uint64_t audio_deliveries() const {
    const std::lock_guard guard(mu_);
    return audio_deliveries_;
  }
  /// Audio frames or text segments the manager accepted.
  [[nodiscard]] std::uint64_t inputs() const {
    const std::lock_guard guard(mu_);
    return inputs_;
  }
  /// Text segments waiting to start, as the last transition showed them.
  [[nodiscard]] std::uint64_t waiting_segments() const {
    const std::lock_guard guard(mu_);
    return waiting_segments_;
  }
  /// Every event the manager applied for a session, in the sink's order.
  [[nodiscard]] std::vector<serving::LogicalSessionEvent> events() const {
    const std::lock_guard guard(mu_);
    return events_;
  }
  [[nodiscard]] bool saw(serving::LogicalSessionEvent event) const {
    const std::lock_guard guard(mu_);
    return std::find(events_.begin(), events_.end(), event) != events_.end();
  }
  [[nodiscard]] serving::LogicalSessionState state() const {
    const std::lock_guard guard(mu_);
    return state_;
  }
  /// The reason of the end cause the last transition carried; empty for none.
  [[nodiscard]] std::string cause() const {
    const std::lock_guard guard(mu_);
    return cause_;
  }
  /// The client sequences of the Finalizes handed over.
  [[nodiscard]] std::vector<std::uint64_t> finalizes() const {
    const std::lock_guard guard(mu_);
    return finalizes_;
  }
  [[nodiscard]] std::shared_ptr<serving::BoundedOutputQueue> output() const {
    const std::lock_guard guard(mu_);
    return output_;
  }

 private:
  mutable std::mutex mu_;
  std::vector<serving::LogicalSessionEvent> events_;
  std::vector<std::uint64_t> finalizes_;
  serving::LogicalSessionState state_ = serving::LogicalSessionState::Opening;
  std::string cause_;
  std::shared_ptr<serving::BoundedOutputQueue> output_;
  int aborts_ = 0;
  int deliveries_ = 0;
  std::uint64_t audio_deliveries_ = 0;
  std::uint64_t inputs_ = 0;
  std::uint64_t waiting_segments_ = 0;
};

/// The synthetic dispatch, with an initialization that can fail, a
/// negotiation that can be held and a record of what the listener did.
class SteeredDispatch final : public serving::SessionDispatch {
 public:
  explicit SteeredDispatch(const SchedulerClock& clock) : inner_(clock) {}

  void attach(serving::SessionManager& manager) override { inner_.attach(manager); }
  void stop() noexcept override { inner_.stop(); }
  [[nodiscard]] Result<serving::SessionTerms> negotiate(
      const serving::SessionRequest& request) const override {
    std::unique_lock lock(mu_);
    negotiating_ = true;
    cv_.notify_all();
    cv_.wait(lock, [&] { return !hold_; });
    lock.unlock();
    return inner_.negotiate(request);
  }
  [[nodiscard]] Result<void> open_session(std::uint64_t key,
                                          const serving::SessionTerms& terms) override {
    if (fail_initialization) {
      return unexpected(Error::make(Error::Code::Unavailable, "no backend", "backend_unavailable"));
    }
    return inner_.open_session(key, terms);
  }
  void on_transition(const serving::ManagedSessionTransition& t) override {
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
    seen.finalize(sequence);
    inner_.finalize(key, sequence);
  }
  void output_delivered(std::uint64_t key, serving::OutputKind kind) override {
    seen.delivery(kind);
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

  /// The gates and the yield of the dispatch behind.
  [[nodiscard]] SyntheticSessionDispatch& synthetic() noexcept { return inner_; }

  std::atomic<bool> fail_initialization{false};
  Seen seen;

 private:
  SyntheticSessionDispatch inner_;
  mutable std::mutex mu_;
  mutable std::condition_variable cv_;
  mutable bool negotiating_ = false;
  bool hold_ = false;
};

/// One call, and the accounts a client keeps of its stream: its credit from
/// Ready and then from each Accepted, less what it has sent since.
struct Client {
  std::unique_ptr<grpc::ClientContext> context = std::make_unique<grpc::ClientContext>();
  std::unique_ptr<Call> call;
  std::uint64_t sequence = 1;
  std::string session_id;
  /// Every Accepted read, in order.
  std::vector<wire::Accepted> accepted;
  /// Every audio byte sent, and how many samples that is.
  std::string audio_sent;
  std::uint64_t samples_sent = 0;

  bool send(wire::ClientEvent event) {
    event.set_sequence(++sequence);
    event.set_session_id(session_id);
    return call->Write(event);
  }

  /// The next event on the wire.
  std::optional<wire::ServerEvent> read() {
    wire::ServerEvent event;
    if (!call->Read(&event)) {
      return std::nullopt;
    }
    if (event.has_accepted()) {
      accepted.push_back(event.accepted());
      credit_ = event.accepted().input_credit();
      while (!unreported_.empty() &&
             unreported_.front().first <= event.accepted().accepted_sequence()) {
        unreported_.pop_front();
      }
    }
    return event;
  }

  /// The next event that is not an Accepted, those read while pacing first.
  std::optional<wire::ServerEvent> next() {
    for (;;) {
      if (!stashed_.empty()) {
        auto event = std::move(stashed_.front());
        stashed_.pop_front();
        return event;
      }
      auto event = read();
      if (!event || !event->has_accepted()) {
        return event;
      }
    }
  }

  void took(const wire::Ready& ready) {
    session_id = ready.session_id();
    credit_ = ready.status().input_credit();
  }

  /// One frame of `samples` 16-bit samples at the offset the stream is at,
  /// sent once the credit has room. False if the stream ended first.
  bool send_audio(std::uint32_t samples) {
    wire::ClientEvent event;
    auto& audio = *event.mutable_audio();
    std::string data(std::size_t{samples} * 2, '\0');
    for (std::size_t index = 0; index < data.size(); ++index) {
      data[index] = static_cast<char>((audio_sent.size() + index) * 131U % 251U);
    }
    audio.set_sample_count(samples);
    audio.set_sample_offset(samples_sent);
    if (!wait_for_room(data.size(), false)) {
      return false;
    }
    audio_sent += data;
    samples_sent += samples;
    audio.set_data(std::move(data));
    return send_input(std::move(event), std::size_t{samples} * 2);
  }

  /// `samples` of audio in frames of `frame` samples, the last one shorter.
  bool send_audio(std::uint64_t samples, std::uint32_t frame) {
    while (samples > 0) {
      const auto next_frame = static_cast<std::uint32_t>(std::min<std::uint64_t>(samples, frame));
      if (!send_audio(next_frame)) {
        return false;
      }
      samples -= next_frame;
    }
    return true;
  }

  /// One text segment, sent once both budgets have room.
  bool send_text(std::uint64_t segment_id, const std::string& text) {
    wire::ClientEvent event;
    event.mutable_text_segment()->set_segment_id(segment_id);
    event.mutable_text_segment()->set_text(text);
    return wait_for_room(text.size(), true) && send_input(std::move(event), text.size());
  }

  /// Sends a Finalize and returns its sequence.
  std::uint64_t finalize() {
    wire::ClientEvent event;
    event.mutable_finalize();
    EXPECT_TRUE(send(std::move(event)));
    return sequence;
  }

 private:
  bool send_input(wire::ClientEvent event, std::uint64_t bytes) {
    if (!send(std::move(event))) {
      return false;
    }
    unreported_.emplace_back(sequence, bytes);
    return true;
  }

  /// Reads until what the server last reported, less what was sent since,
  /// leaves room for `bytes` (and for one more waiting segment).
  bool wait_for_room(std::uint64_t bytes, bool segment) {
    for (;;) {
      std::uint64_t sent_since = 0;
      for (const auto& [unused, sent] : unreported_) {
        sent_since += sent;
      }
      const auto& waiting = credit_.waiting_segments();
      const bool bytes_fit = credit_.bytes().used() + sent_since + bytes <= credit_.bytes().limit();
      const bool segment_fits =
          !segment || waiting.used() + unreported_.size() + 1 <= waiting.limit();
      if (bytes_fit && segment_fits) {
        return true;
      }
      auto event = read();
      if (!event) {
        return false;
      }
      if (!event->has_accepted()) {
        stashed_.push_back(std::move(*event));
      }
    }
  }

  wire::InputCredit credit_;
  /// Input sent that no Accepted covers yet: its sequence and bytes.
  std::deque<std::pair<std::uint64_t, std::uint64_t>> unreported_;
  std::deque<wire::ServerEvent> stashed_;
};

/// A listener with the synthetic dispatch behind it, or none.
class Served {
 public:
  /// `clock`, when given, is the listener's and its manager's.
  explicit Served(bool with_dispatch = true, const std::string& host = "127.0.0.1",
                  const SchedulerClock* clock = nullptr)
      : clock_(clock != nullptr ? *clock : system_clock_) {
    if (with_dispatch) {
      dispatch_ = std::make_unique<SteeredDispatch>(clock_);
    }
    serving::StreamListenerOptions options;
    options.host = host;
    options.deployment_id = kDeployment;
    options.limits =
        serving::SessionLimits::create(std::chrono::seconds{60}, std::chrono::seconds{10},
                                       std::chrono::seconds{30}, std::chrono::minutes{60}, 1)
            .value();
    listener_ = serving::StreamListener::start(std::move(options), dispatch_.get(), clock_).value();
    use_channel({});
  }
  ~Served() { listener_.reset(); }
  Served(const Served&) = delete;
  Served& operator=(const Served&) = delete;

  /// Calls made from now on use a new channel with `arguments`.
  void use_channel(const grpc::ChannelArguments& arguments) {
    channel_ = grpc::CreateCustomChannel("127.0.0.1:" + std::to_string(listener_->bound_port()),
                                         grpc::InsecureChannelCredentials(), arguments);
    stub_ = wire::SessionService::NewStub(channel_);
  }

  Client connect(std::chrono::milliseconds deadline = std::chrono::seconds{20}) {
    Client client;
    client.context->set_deadline(std::chrono::system_clock::now() + deadline);
    client.call = stub_->Session(client.context.get());
    return client;
  }

  /// A client whose Open was answered by Ready.
  Client ready(wire::SessionMode mode, wire::Ready* ready_out = nullptr,
               std::chrono::milliseconds deadline = std::chrono::seconds{20}) {
    auto client = connect(deadline);
    EXPECT_TRUE(client.call->Write(open_event(mode)));
    const auto ready = client.read();
    EXPECT_TRUE(ready && ready->has_ready());
    if (ready && ready->has_ready()) {
      client.took(ready->ready());
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

  serving::StreamListener& listener() { return *listener_; }
  SteeredDispatch& dispatch() { return *dispatch_; }
  /// The channel calls are made on, for a client the fixture does not build.
  [[nodiscard]] std::shared_ptr<grpc::Channel> channel() const { return channel_; }

 private:
  SystemSchedulerClock system_clock_;
  const SchedulerClock& clock_;
  std::unique_ptr<SteeredDispatch> dispatch_;
  std::unique_ptr<serving::StreamListener> listener_;
  std::shared_ptr<grpc::Channel> channel_;
  std::unique_ptr<wire::SessionService::Stub> stub_;
};

/// The status of a call whose session has sent its last event.
inline grpc::Status end_of(Client& client) {
  EXPECT_FALSE(client.read().has_value()) << "SessionClosed is the last event";
  return client.call->Finish();
}

/// Checks a SessionClosed that ends a session which accepted no input.
inline void expect_closed(const std::optional<wire::ServerEvent>& event, wire::SessionState state,
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

}  // namespace tensorplate::testing::stream_listener
