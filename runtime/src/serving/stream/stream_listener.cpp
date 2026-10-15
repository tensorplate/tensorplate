// SPDX-License-Identifier: Apache-2.0
#include "serving/stream/stream_listener.hpp"

#include <grpcpp/grpcpp.h>
#include <grpcpp/impl/proto_utils.h>
#include <grpcpp/support/byte_buffer.h>
#include <grpcpp/support/server_callback.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <map>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <system_error>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/stream/v1/session.grpc.pb.h"

#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"
#include "serving/session/timers.hpp"
#include "serving/stream/stream_handshake.hpp"
#include "serving/stream/stream_mapping.hpp"
#include "serving/stream/stream_output.hpp"

namespace tensorplate::serving {
namespace {

using stream::wire::ClientEvent;
using stream::wire::ServerEvent;
namespace wire = stream::wire;

/// The generation the manager binds sessions to on a worker that serves none.
/// It never reaches the wire: an Open names generation 0 and Ready omits it.
constexpr std::uint64_t kUnversionedBinding = 1;
constexpr int kMaxReceiveMessageBytes = 64 * 1024;
/// Above kMaxOpenHandshakes, so that one connection can still be told that
/// too many of its calls have yet to send their Open.
constexpr int kMaxConcurrentStreams = 64;

using stream::CallOutput;

bool host_is_loopback(std::string_view host) noexcept {
  return host == "127.0.0.1" || host == "::1" || host == "localhost";
}

std::uint32_t whole_ms(std::chrono::nanoseconds duration) noexcept {
  const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(duration).count();
  return static_cast<std::uint32_t>(std::clamp<std::int64_t>(ms, 0, UINT32_MAX));
}

/// The Ready of a session granted `terms`, with the status of the transition
/// that admitted it.
ServerEvent ready_event(const SessionLimits& session_limits, const std::string& session_id,
                        const SessionTerms& terms, const ManagedSessionTransition& transition) {
  ServerEvent event;
  auto& ready = *event.mutable_ready();
  ready.set_session_id(session_id);
  auto& limits = *ready.mutable_limits();
  limits.set_idle_timeout_ms(whole_ms(session_limits.idle_timeout()));
  limits.set_heartbeat_interval_ms(whole_ms(session_limits.heartbeat_interval()));
  limits.set_liveness_timeout_ms(whole_ms(session_limits.liveness_timeout()));
  limits.set_max_duration_ms(whole_ms(session_limits.max_duration()));
  limits.set_finalize_deadline_ms(whole_ms(finalize_deadline(terms.input_kind)));
  limits.set_output_stall_timeout_ms(whole_ms(SessionBudgets::kOutputNoProgressTimeout));
  if (terms.input_kind == SessionInputKind::Audio) {
    auto& stt = *limits.mutable_speech_to_text();
    stt.set_min_frame_ms(whole_ms(terms.min_frame));
    stt.set_max_frame_ms(whole_ms(terms.max_frame));
    stt.set_max_utterance_ms(whole_ms(terms.max_utterance));
    *ready.mutable_input_format() = stream::wire_format(terms.audio_format);
  } else {
    auto& tts = *limits.mutable_text_to_speech();
    tts.set_max_segment_text_bytes(terms.max_segment_text_bytes);
    tts.set_max_segment_audio_ms(whole_ms(terms.max_segment_audio));
    tts.set_max_synthesis_text_bytes(terms.max_synthesis_text_bytes);
    tts.set_max_synthesis_audio_ms(whole_ms(terms.max_synthesis_audio));
    *ready.mutable_output_format() = stream::wire_format(terms.audio_format);
  }
  // Ready follows admission at once: the whole duration is left.
  ready.set_remaining_duration_ms(whole_ms(session_limits.max_duration()));
  *ready.mutable_status() = stream::wire_status(transition.status, transition.waiting_segments);
  return event;
}

/// The status of a call refused for `cause`: it was never admitted.
grpc::Status refusal(const Error& cause) {
  wire::OpenRefused refused;
  *refused.mutable_cause() = stream::end_cause(cause);
  refused.set_admission(wire::ADMISSION_NOT_ADMITTED);
  return stream::refused_open_status(refused);
}

Error admission_closed() {
  return Error::make(Error::Code::NotReady, "session admission is closed", "admission_closed");
}

CallOutput::Delivered delivered_to(SessionDispatch* dispatch, std::uint64_t key) {
  if (dispatch == nullptr) {
    return {};
  }
  return [dispatch, key](OutputKind kind) { dispatch->output_delivered(key, kind); };
}

/// What a call's reactor and the manager's sink share about its session.
struct CallState {
  std::mutex mu;
  std::optional<std::uint64_t> key;
  std::string session_id;
  SessionTerms terms;
  /// The input the reader is offering the manager. The sink, which runs
  /// inside that call, counts it when the manager accepts it.
  struct Offered {
    std::uint64_t sequence = 0;
    std::uint64_t audio_samples = 0;
    std::uint64_t text_bytes = 0;
  };
  Offered offered;
  std::uint64_t last_accepted_sequence = 0;
  std::uint64_t input_audio_samples = 0;
  std::uint64_t input_text_bytes = 0;
  bool cancelled = false;
  /// Outlives the reactor, which it stops calling once the call is done.
  std::shared_ptr<CallOutput> out;
};

}  // namespace

/// The method is registered raw: gRPC hands over each message as bytes, so a
/// message that does not parse reaches the call instead of failing its read.
struct StreamListener::Impl final
    : wire::SessionService::WithRawCallbackMethod_Session<wire::SessionService::Service> {
  Impl(StreamListenerOptions options_in, SessionDispatch* dispatch_in,
       const SchedulerClock& clock_in)
      : options(std::move(options_in)),
        dispatch(dispatch_in),
        clock(clock_in),
        refused(options.max_refused_calls.value_or(stream::kMaxRefusedCalls)) {}

  grpc::ServerBidiReactor<grpc::ByteBuffer, grpc::ByteBuffer>* Session(
      grpc::CallbackServerContext* context) override;

  void on_transition(const ManagedSessionTransition& transition);
  void close_admission() noexcept;
  /// Ends a call that was never admitted, for `cause`.
  void refuse(CallOutput& call, const Error& cause) { call.refuse(refused, refusal(cause)); }
  /// Waits for the call's first message, with `start_read` run once the wait
  /// has begun, or says why the call is refused with no read started.
  template <typename StartRead>
  [[nodiscard]] std::optional<Error> await_open(const std::shared_ptr<CallOutput>& call,
                                                StartRead&& start_read);
  void watch(std::shared_ptr<CallOutput> ended_call);
  void wake_watcher();
  void watch_calls();
  void stop_watching() noexcept;

  StreamListenerOptions options;
  SessionDispatch* dispatch;
  const SchedulerClock& clock;
  std::unique_ptr<SessionManager> manager;
  std::unique_ptr<grpc::Server> server;
  std::uint16_t port = 0;

  std::mutex calls_mu;
  std::map<std::uint64_t, std::shared_ptr<CallState>> calls;
  /// Reactors not done yet.
  std::atomic<std::size_t> open_calls{0};

  /// Calls whose first message has not been read.
  stream::OpenHandshakes handshakes;
  /// Refused calls that are not done.
  stream::RefusedCalls refused;

  /// Calls whose session has ended, each until its reactor is done.
  std::mutex ended_mu;
  std::condition_variable ended_cv;
  std::vector<std::shared_ptr<CallOutput>> ended;
  /// A call was added to `ended` or to `handshakes`, or the clock was moved.
  bool watch_changed = false;
  bool watch_stopped = false;
  std::thread watcher;

  /// Serializes stop(); held through its wait.
  std::mutex lifecycle_mu;
  bool stopped = false;
  /// Sessions that still held a slot when stop() released the manager.
  std::size_t held_at_stop = 0;
  /// Guards `manager` against its reset and `admission_stopped`; held briefly.
  mutable std::mutex manager_mu;
  bool admission_stopped = false;
};

namespace {

/// One Session call: one logical session.
class SessionCall final : public grpc::ServerBidiReactor<grpc::ByteBuffer, grpc::ByteBuffer>,
                          public stream::CallWriter {
 public:
  SessionCall(StreamListener::Impl& listener, grpc::CallbackServerContext& context)
      : listener_(listener), context_(context), state_(std::make_shared<CallState>()) {
    ++listener_.open_calls;
    state_->out = std::make_shared<CallOutput>(*this, listener.clock);
    // A call the listener does not wait on is refused with no read started.
    if (const auto refused = listener_.await_open(state_->out, [this] { StartRead(&in_); })) {
      refuse(*refused);
    }
  }

  void OnReadDone(bool ok) override {
    // The first read ends the wait for Open. A call that a bound refused
    // meanwhile is answered already: nothing of it is handled or read.
    if (std::exchange(awaiting_open_, false) && !listener_.handshakes.end(*state_->out)) {
      return;
    }
    if (!ok) {
      // gRPC 1.81.1 marks the context cancelled before it reports a read that
      // failed for a cancel or a lost connection, but not for the stream's end.
      if (context_.IsCancelled()) {
        cancelled();
      } else {
        half_closed();
      }
      return;
    }
    // The parse gRPC runs for a typed method, which reports its failure as a
    // failed read.
    if (!grpc::SerializationTraits<ClientEvent>::Deserialize(&in_, &event_).ok()) {
      unparsed();
      return;
    }
    if (handle(event_)) {
      StartRead(&in_);
    }
  }

  void OnWriteDone(bool ok) override { state_->out->write_done(ok); }

  void OnCancel() override { cancelled(); }

  void OnDone() override {
    state_->out->detach();
    // A call that ended on its own, for output it could not send, would leave
    // its session live until a timer ended it. Ignored once the session's
    // outcome is fixed.
    if (const auto key = session_key()) {
      fail(*key,
           Error::make(Error::Code::Internal, "the call ended before its session", "internal"));
    }
    --listener_.open_calls;
    delete this;
  }

  void start_write(const ServerEvent& event) override {
    bool owned = false;
    // gRPC serializes into an empty buffer only.
    out_.Clear();
    if (grpc::SerializationTraits<ServerEvent>::Serialize(event, &out_, &owned).ok()) {
      StartWrite(&out_);
      return;
    }
    // Only an event past protobuf's 2 GiB limit: its write is answered as failed.
    state_->out->finish({grpc::StatusCode::INTERNAL, "output this server cannot send"});
    state_->out->write_done(false);
  }
  void finish(const grpc::Status& status) override { Finish(status); }
  // The context lives until OnDone has returned, and detach() runs in it.
  void cancel() override { context_.TryCancel(); }

 private:
  [[nodiscard]] std::optional<std::uint64_t> session_key() const {
    const std::lock_guard guard(state_->mu);
    return state_->key;
  }

  /// False when no further event is read.
  bool handle(const ClientEvent& event) {
    const auto key = session_key();
    if (!key) {
      return open(event);
    }
    // A Cancel may be sent before the client has read Ready, without the id.
    const bool id_matches =
        event.session_id() == session_id() ||
        (event.body_case() == ClientEvent::kCancel && event.session_id().empty());
    if (event.sequence() != expected_sequence_ || event.sequence() > stream::kMaxWireInteger ||
        !id_matches) {
      fail(*key,
           Error::make(Error::Code::ConfigInvalid,
                       "client event out of sequence or for another session", "invalid_event"));
      return false;
    }
    ++expected_sequence_;
    switch (event.body_case()) {
      case ClientEvent::kPing:
        return apply(*key, LogicalSessionEvent::Ping);
      case ClientEvent::kStatusRequest:
        return apply(*key, LogicalSessionEvent::StatusRequest);
      case ClientEvent::kCancel:
        return apply(*key, LogicalSessionEvent::Cancel);
      case ClientEvent::kFinalize:
        return finalize(*key, event.sequence());
      case ClientEvent::kAudio:
        return audio(*key, event.sequence(), event.audio());
      case ClientEvent::kTextSegment:
        return text_segment(*key, event.sequence(), event.text_segment());
      case ClientEvent::kOpen:
        fail(*key,
             Error::make(Error::Code::ConfigInvalid, "a session is opened once", "invalid_event"));
        return false;
      case ClientEvent::BODY_NOT_SET:
        break;
    }
    fail(*key, Error::make(Error::Code::ConfigInvalid, "client event with no known body",
                           "invalid_event"));
    return false;
  }

  /// Applies a client event that carries nothing. False when the manager
  /// refused it, which failed the session.
  bool apply(std::uint64_t key, LogicalSessionEvent event) const {
    return listener_.manager->apply(key, event).has_value();
  }

  /// The hand-over follows the result of this call, never the sink: the
  /// `start_finalize` an automatic endpoint shows the sink is the dispatch's
  /// own.
  bool finalize(std::uint64_t key, std::uint64_t sequence) {
    const auto applied = listener_.manager->apply(key, LogicalSessionEvent::Finalize);
    if (!applied) {
      return false;
    }
    if (applied->effects.contains(LogicalSessionEffect::StartFinalize)) {
      history_.short_frame = false;
      history_.synthesis_text_bytes = 0;
      listener_.dispatch->finalize(key, sequence);
    } else if (applied->state == LogicalSessionState::Draining) {
      // Not taken by a drain: the history keeps a mark only it could clear.
      input_not_taken_ = true;
    }
    return true;
  }

  bool audio(std::uint64_t key, std::uint64_t sequence, const wire::Audio& audio) {
    if (!check(key, [&] { return stream::check_audio(audio, terms_, history_); })) {
      return false;
    }
    const auto& data = audio.data();
    const auto taken = offer(key, {sequence, audio.sample_count(), 0}, data.size());
    if (taken == Taken::Yes) {
      history_.samples += audio.sample_count();
      history_.short_frame = stream::is_short_frame(audio.sample_count(), terms_);
      listener_.dispatch->audio(key, std::as_bytes(std::span{data.data(), data.size()}));
    }
    return taken != Taken::Refused;
  }

  bool text_segment(std::uint64_t key, std::uint64_t sequence, const wire::TextSegment& segment) {
    if (!check(key, [&] { return stream::check_text_segment(segment, terms_, history_); })) {
      return false;
    }
    const auto& text = segment.text();
    const auto taken = offer(key, {sequence, 0, text.size()}, text.size());
    if (taken == Taken::Yes) {
      history_.last_segment_id = segment.segment_id();
      history_.synthesis_text_bytes += text.size();
      listener_.dispatch->text_segment(key, segment.segment_id(), text);
    }
    return taken != Taken::Refused;
  }

  /// Runs the check of one input and fails the session with what it refuses.
  /// Once a drain has not taken an input or a Finalize nothing is checked:
  /// the history it would be checked against has stopped with the drain.
  template <typename Check>
  bool check(std::uint64_t key, Check&& checked) const {
    if (input_not_taken_) {
      return true;
    }
    if (auto result = std::forward<Check>(checked)(); !result) {
      fail(key, result.error());
      return false;
    }
    return true;
  }

  enum class Taken : std::uint8_t { Yes, No, Refused };

  /// Offers one checked input of `bytes` to the manager. Refused means the
  /// manager failed the session.
  Taken offer(std::uint64_t key, const CallState::Offered& offered, std::uint64_t bytes) {
    {
      const std::lock_guard guard(state_->mu);
      state_->offered = offered;
    }
    const auto accepted = listener_.manager->accept_input(key, bytes);
    if (!accepted) {
      return Taken::Refused;
    }
    if (!accepted->effects.contains(LogicalSessionEffect::AcceptInput)) {
      // A drain took nothing, and a draining session takes nothing again.
      input_not_taken_ = true;
      return Taken::No;
    }
    return Taken::Yes;
  }

  bool open(const ClientEvent& event) {
    if (event.body_case() != ClientEvent::kOpen || event.sequence() != 1 ||
        !event.session_id().empty()) {
      refuse(Error::make(Error::Code::ConfigInvalid, "the first event must be Open, sequence 1",
                         "invalid_event"));
      return false;
    }
    const auto& open = event.open();
    if (auto target =
            stream::check_target(open, stream::ServedTarget{listener_.options.deployment_id});
        !target) {
      refuse(target.error());
      return false;
    }
    auto request = stream::session_request(open);
    if (!request) {
      refuse(request.error());
      return false;
    }
    SessionDispatch* dispatch = listener_.dispatch;
    if (dispatch == nullptr) {
      refuse(Error::make(Error::Code::Unsupported, "the deployment declares no stream mode",
                         "backend_unsupported_capability"));
      return false;
    }
    auto terms = dispatch->negotiate(*request);
    if (!terms) {
      refuse(terms.error());
      return false;
    }
    auto budgets = budgets_for(*terms);
    if (!budgets) {
      refuse(budgets.error());
      return false;
    }
    terms_ = *terms;
    {
      const std::lock_guard guard(state_->mu);
      state_->terms = *terms;
    }
    const auto opened = listener_.manager->open(kUnversionedBinding, *budgets,
                                                [&](std::uint64_t key) -> Result<void> {
                                                  {
                                                    const std::lock_guard guard(state_->mu);
                                                    state_->key = key;
                                                    state_->session_id = "s" + std::to_string(key);
                                                  }
                                                  {
                                                    const std::lock_guard guard(listener_.calls_mu);
                                                    listener_.calls[key] = state_;
                                                  }
                                                  return dispatch->open_session(key, *terms);
                                                });
    // A refusal no transition reported leaves no session and sends no event.
    if (!opened && !state_->out->bound()) {
      forget_session();
      refuse(opened.error());
      return false;
    }
    expected_sequence_ = 2;
    std::optional<std::uint64_t> cancelled_key;
    {
      const std::lock_guard guard(state_->mu);
      if (state_->cancelled) {
        cancelled_key = state_->key;
      }
    }
    // The call was cancelled before its session had a key to abort; it is
    // finished, so nothing more is read.
    if (cancelled_key) {
      abort(*cancelled_key);
      return false;
    }
    // A failed initialization was admitted: its terminal event ends the call.
    return opened.has_value();
  }

  void forget_session() {
    std::optional<std::uint64_t> key;
    {
      const std::lock_guard guard(state_->mu);
      key = std::exchange(state_->key, std::nullopt);
      state_->session_id.clear();
    }
    if (key) {
      const std::lock_guard guard(listener_.calls_mu);
      listener_.calls.erase(*key);
    }
  }

  void abort(std::uint64_t key) const {
    (void)listener_.manager->apply(
        key, LogicalSessionEvent::Abort,
        Error::make(Error::Code::Cancelled, "the call was cancelled", "client_cancelled"));
  }

  /// The call was cancelled or its connection lost: its session is abandoned,
  /// whichever of its failed read and OnCancel tells first.
  void cancelled() {
    std::optional<std::uint64_t> key;
    {
      const std::lock_guard guard(state_->mu);
      state_->cancelled = true;
      key = state_->key;
    }
    if (key) {
      abort(*key);
    }
    state_->out->finish(grpc::Status::CANCELLED);
  }

  void half_closed() {
    if (const auto key = session_key()) {
      (void)listener_.manager->apply(*key, LogicalSessionEvent::HalfClose);
      return;
    }
    refuse(
        Error::make(Error::Code::ConfigInvalid, "the client closed before Open", "invalid_event"));
  }

  /// A message that is no client event; nothing after it is read.
  void unparsed() {
    auto cause = Error::make(Error::Code::ConfigInvalid, "client message that does not parse",
                             "invalid_event");
    if (const auto key = session_key()) {
      fail(*key, std::move(cause));
      return;
    }
    refuse(cause);
  }

  void fail(std::uint64_t key, Error cause) const {
    (void)listener_.manager->apply(key, LogicalSessionEvent::Fail, std::move(cause));
  }

  void refuse(const Error& cause) { listener_.refuse(*state_->out, cause); }

  [[nodiscard]] std::string session_id() const {
    const std::lock_guard guard(state_->mu);
    return state_->session_id;
  }

  StreamListener::Impl& listener_;
  grpc::CallbackServerContext& context_;
  std::shared_ptr<CallState> state_;
  grpc::ByteBuffer in_;
  ClientEvent event_;
  /// The write in flight, serialized; kept until it is answered.
  grpc::ByteBuffer out_;
  std::uint64_t expected_sequence_ = 1;
  /// The reader's own: gRPC runs one read reaction of a call at a time.
  bool awaiting_open_ = true;
  SessionTerms terms_;
  stream::InputHistory history_;
  bool input_not_taken_ = false;
};

}  // namespace

grpc::ServerBidiReactor<grpc::ByteBuffer, grpc::ByteBuffer>* StreamListener::Impl::Session(
    grpc::CallbackServerContext* context) {
  return new SessionCall(*this, *context);
}

void StreamListener::Impl::on_transition(const ManagedSessionTransition& transition) {
  if (dispatch != nullptr) {
    dispatch->on_transition(transition);
  }
  std::shared_ptr<CallState> state;
  {
    const std::lock_guard guard(calls_mu);
    const auto it = calls.find(transition.session_key);
    if (it == calls.end()) {
      return;
    }
    state = it->second;
    if (transition.effects.contains(LogicalSessionEffect::ReleaseSlot)) {
      calls.erase(it);
    }
  }
  const auto& output = transition.output;
  const bool first = !state->out->bound();
  std::string session_id;
  SessionTerms terms;
  std::uint64_t last_accepted_sequence = 0;
  std::uint64_t input_audio_samples = 0;
  std::uint64_t input_text_bytes = 0;
  {
    const std::lock_guard guard(state->mu);
    // Counted here, inside the manager's serialized section, so that the
    // terminal event built in the same section never misses an input.
    if (transition.event == LogicalSessionEvent::Data &&
        transition.effects.contains(LogicalSessionEffect::AcceptInput)) {
      state->last_accepted_sequence = state->offered.sequence;
      state->input_audio_samples += state->offered.audio_samples;
      state->input_text_bytes += state->offered.text_bytes;
    }
    session_id = state->session_id;
    terms = state->terms;
    last_accepted_sequence = state->last_accepted_sequence;
    input_audio_samples = state->input_audio_samples;
    input_text_bytes = state->input_text_bytes;
  }
  const auto offer = [&](ServerEvent event) {
    auto item = stream::lifecycle_item(std::move(event));
    (void)output->offer(item, clock.now());
  };
  // Credit is reported from the sink only: its status is never older than
  // one it showed before.
  const auto offer_accepted = [&] {
    ServerEvent event;
    auto& accepted = *event.mutable_accepted();
    accepted.set_accepted_sequence(last_accepted_sequence);
    *accepted.mutable_input_credit() =
        stream::wire_credit(transition.status, transition.waiting_segments);
    offer(std::move(event));
  };
  if (transition.event) {
    transition.effects.for_each([&](LogicalSessionEffect effect) {
      ServerEvent event;
      switch (effect) {
        case LogicalSessionEffect::EmitCancelAccepted:
          event.mutable_cancel_accepted();
          offer(std::move(event));
          break;
        case LogicalSessionEffect::EmitReady:
          offer(ready_event(options.limits, session_id, terms, transition));
          break;
        case LogicalSessionEffect::AcceptInput:
          offer_accepted();
          break;
        case LogicalSessionEffect::EmitReply:
          if (*transition.event == LogicalSessionEvent::Ping) {
            event.mutable_pong();
          } else {
            *event.mutable_status() =
                stream::wire_status(transition.status, transition.waiting_segments);
          }
          offer(std::move(event));
          break;
        case LogicalSessionEffect::EmitTerminal: {
          auto& closed = *event.mutable_session_closed();
          closed.set_state(stream::wire_state(transition.state));
          if (transition.cause) {
            *closed.mutable_cause() = stream::end_cause(*transition.cause);
          }
          closed.set_admission(wire::ADMISSION_ADMITTED);
          closed.set_last_accepted_sequence(last_accepted_sequence);
          // The write path adds what it wrote.
          auto& totals = *closed.mutable_totals();
          totals.set_input_audio_samples(input_audio_samples);
          totals.set_input_text_bytes(input_text_bytes);
          state->out->set_end_status(stream::session_end_status(closed));
          state->out->session_ended(clock.now());
          offer(std::move(event));
          watch(state->out);
          break;
        }
        default:
          break;
      }
    });
  } else if (transition.state == LogicalSessionState::Active ||
             transition.state == LogicalSessionState::Finalizing) {
    // Returned credit, announced with the sequence of the Accepted before.
    // It keeps returning while a session ends, when no client can use it.
    offer_accepted();
  }
  if (first) {
    state->out->bind(output, delivered_to(dispatch, transition.session_key), terms.audio_format);
    (void)output->set_consumer([weak = std::weak_ptr<CallOutput>(state->out)] {
      if (const auto shared = weak.lock()) {
        shared->pump();
      }
    });
  }
}

void StreamListener::Impl::close_admission() noexcept {
  {
    const std::lock_guard guard(manager_mu);
    if (admission_stopped || !manager) {
      return;
    }
    admission_stopped = true;
    try {
      (void)manager->stop_admission_and_drain(
          Error::make(Error::Code::Unavailable, "the worker is shutting down", "worker_shutdown"));
    } catch (...) {  // NOLINT(bugprone-empty-catch): shutdown continues without the drain.
    }
  }
  try {
    // A call that has not sent its Open can no longer be admitted.
    for (const auto& call : handshakes.close()) {
      refuse(*call, admission_closed());
    }
  } catch (...) {  // NOLINT(bugprone-empty-catch): the stop cancels the calls left open.
  }
}

template <typename StartRead>
std::optional<Error> StreamListener::Impl::await_open(const std::shared_ptr<CallOutput>& call,
                                                      StartRead&& start_read) {
  switch (handshakes.begin(call, clock.now(), std::forward<StartRead>(start_read))) {
    case stream::OpenHandshakes::Begun::Closed:
      return admission_closed();
    case stream::OpenHandshakes::Begun::Full:
      return Error::make(Error::Code::ResourceExhausted, "too many calls have not sent their Open",
                         "session_count_limit");
    case stream::OpenHandshakes::Begun::Yes:
      break;
  }
  // The watch thread sleeps until the earliest wait it knows of is over.
  wake_watcher();
  return std::nullopt;
}

void StreamListener::Impl::watch(std::shared_ptr<CallOutput> ended_call) {
  {
    const std::lock_guard guard(ended_mu);
    ended.push_back(std::move(ended_call));
  }
  wake_watcher();
}

void StreamListener::Impl::wake_watcher() {
  {
    const std::lock_guard guard(ended_mu);
    watch_changed = true;
  }
  ended_cv.notify_one();
}

void StreamListener::Impl::watch_calls() {
  std::unique_lock lock(ended_mu);
  while (!watch_stopped) {
    watch_changed = false;
    auto watched = ended;
    lock.unlock();
    // Unlocked: a refusal and a cancel call into gRPC, and the sink adds under
    // this lock.
    const auto now = clock.now();
    for (const auto& call : handshakes.take_overdue(now)) {
      refuse(*call, Error::make(Error::Code::Timeout, "no Open within the open handshake timeout",
                                "idle_timeout"));
    }
    auto next = handshakes.next_due();
    std::erase_if(watched, [&](const std::shared_ptr<CallOutput>& call) {
      const auto due = call->cancel_if_stalled(now);
      if (due && (!next || *due < *next)) {
        next = due;
      }
      return due.has_value();
    });
    lock.lock();
    // What is left of `watched` needs no more watching.
    std::erase_if(ended, [&](const std::shared_ptr<CallOutput>& call) {
      return std::find(watched.begin(), watched.end(), call) != watched.end();
    });
    const auto woken = [&] { return watch_stopped || watch_changed; };
    if (!next) {
      ended_cv.wait(lock, woken);
    } else {
      // An instant of the listener's clock is one of the steady clock; a
      // clock moved by hand says so through notify_clock_advanced().
      ended_cv.wait_until(lock, *next, woken);
    }
  }
}

void StreamListener::Impl::stop_watching() noexcept {
  {
    const std::lock_guard guard(ended_mu);
    watch_stopped = true;
  }
  ended_cv.notify_one();
  if (watcher.joinable()) {
    watcher.join();
  }
}

StreamListener::StreamListener(std::unique_ptr<Impl> impl) noexcept : impl_(std::move(impl)) {}

StreamListener::~StreamListener() {
  stop(std::chrono::steady_clock::now());
}

Result<std::unique_ptr<StreamListener>> StreamListener::start(StreamListenerOptions options,
                                                              SessionDispatch* dispatch,
                                                              const SchedulerClock& clock) {
  if (options.host.empty() || (!host_is_loopback(options.host) && !options.allow_non_loopback)) {
    return unexpected(Error::make(
        Error::Code::Unsupported,
        "stream listener: non-loopback bind not permitted: " + options.host, "non_loopback_bind"));
  }
  auto impl = std::make_unique<Impl>(std::move(options), dispatch, clock);
  Impl* raw = impl.get();
  auto manager = SessionManager::create(
      raw->options.limits, kUnversionedBinding, clock,
      [raw](const ManagedSessionTransition& transition) { raw->on_transition(transition); });
  if (!manager) {
    return unexpected(manager.error());
  }
  raw->manager = std::move(manager).value();
  // First, while a failure leaves nothing started to undo.
  try {
    raw->watcher = std::thread([raw] { raw->watch_calls(); });
  } catch (const std::system_error&) {
    return unexpected(Error::make(Error::Code::Internal,
                                  "stream listener's watch thread could not start",
                                  "watch_thread_unavailable"));
  }
  if (dispatch != nullptr) {
    dispatch->attach(*raw->manager);
  }
  const std::string address = stream::listen_address(raw->options.host, raw->options.port);
  int selected = 0;
  grpc::ServerBuilder builder;
  builder.AddListeningPort(address, grpc::InsecureServerCredentials(), &selected);
  builder.RegisterService(raw);
  builder.SetMaxReceiveMessageSize(kMaxReceiveMessageBytes);
  // A second process must not share the port.
  builder.AddChannelArgument(GRPC_ARG_ALLOW_REUSEPORT, 0);
  // Identity only: gRPC ends a call that announces a compression with
  // UNIMPLEMENTED when its headers arrive, before it inflates a message.
  for (int algorithm = GRPC_COMPRESS_NONE + 1; algorithm < GRPC_COMPRESS_ALGORITHMS_COUNT;
       ++algorithm) {
    builder.SetCompressionAlgorithmSupportStatus(static_cast<grpc_compression_algorithm>(algorithm),
                                                 false);
  }
  // With its bandwidth probe gRPC announces a 4 MiB window at once. Without,
  // a call can leave the HTTP/2 default of 65,535 bytes unread in the
  // transport, and so can a connection.
  builder.AddChannelArgument(GRPC_ARG_HTTP2_BDP_PROBE, 0);
  // gRPC refuses a stream beyond these before a call exists for it.
  builder.AddChannelArgument(GRPC_ARG_MAX_CONCURRENT_STREAMS, kMaxConcurrentStreams);
  raw->server = builder.BuildAndStart();
  if (!raw->server || selected <= 0) {
    if (dispatch != nullptr) {
      dispatch->stop();
    }
    raw->stop_watching();
    return unexpected(Error::make(Error::Code::Unavailable,
                                  "stream listener could not bind " + address,
                                  "stream_bind_failed"));
  }
  raw->port = static_cast<std::uint16_t>(selected);
  return std::unique_ptr<StreamListener>(new StreamListener(std::move(impl)));
}

std::uint16_t StreamListener::bound_port() const noexcept {
  return impl_->port;
}

void StreamListener::stop_admission() noexcept {
  impl_->close_admission();
}

void StreamListener::stop(std::chrono::steady_clock::time_point deadline) noexcept {
  const std::lock_guard lock(impl_->lifecycle_mu);
  if (impl_->stopped) {
    return;
  }
  impl_->stopped = true;
  impl_->close_admission();
  while (impl_->manager->held_slots() > 0 && std::chrono::steady_clock::now() < deadline) {
    std::this_thread::sleep_for(std::chrono::milliseconds{10});
  }
  // Calls still open at the deadline are cancelled, and each aborts its session.
  const auto left = std::max(deadline - std::chrono::steady_clock::now(),
                             std::chrono::steady_clock::duration::zero());
  impl_->server->Shutdown(std::chrono::system_clock::now() +
                          std::chrono::duration_cast<std::chrono::system_clock::duration>(left));
  if (impl_->dispatch != nullptr) {
    impl_->dispatch->stop();
  }
  // Every reactor is done: nothing is left to cancel.
  impl_->stop_watching();
  impl_->server.reset();
  const std::lock_guard guard(impl_->manager_mu);
  impl_->held_at_stop = impl_->manager->held_slots();
  impl_->manager.reset();
}

std::size_t StreamListener::open_calls() const noexcept {
  return impl_->open_calls.load();
}

std::size_t StreamListener::awaited_calls() const noexcept {
  return impl_->handshakes.size();
}

std::size_t StreamListener::refused_calls() const noexcept {
  return impl_->refused.size();
}

void StreamListener::notify_clock_advanced() noexcept {
  impl_->wake_watcher();
}

std::size_t StreamListener::live_sessions() const noexcept {
  const std::lock_guard lock(impl_->manager_mu);
  return impl_->manager ? impl_->manager->held_slots() : impl_->held_at_stop;
}

}  // namespace tensorplate::serving
