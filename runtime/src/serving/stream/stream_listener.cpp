// SPDX-License-Identifier: Apache-2.0
#include "serving/stream/stream_listener.hpp"

#include <grpcpp/grpcpp.h>
#include <grpcpp/support/server_callback.h>

#include <algorithm>
#include <chrono>
#include <map>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>

#include "tensorplate/stream/v1/session.grpc.pb.h"

#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"
#include "serving/session/timers.hpp"
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
/// The replace key of a Pong: an unsent Pong stands for every Ping before it.
constexpr std::uint64_t kPongReplaceKey = 1;
constexpr int kMaxReceiveMessageBytes = 64 * 1024;

using stream::CallOutput;
using stream::WireEvent;

bool host_is_loopback(std::string_view host) noexcept {
  return host == "127.0.0.1" || host == "::1" || host == "localhost";
}

std::uint32_t whole_ms(std::chrono::nanoseconds duration) noexcept {
  const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(duration).count();
  return static_cast<std::uint32_t>(std::clamp<std::int64_t>(ms, 0, UINT32_MAX));
}

OutputItem wire_item(ServerEvent event, OutputKind kind, std::uint64_t replace_key = 0) {
  const auto bytes = std::max<std::uint64_t>(1, event.ByteSizeLong());
  return OutputItem{kind, bytes, replace_key, std::make_unique<WireEvent>(std::move(event))};
}

wire::Usage usage(const LogicalSessionUsage& from) {
  wire::Usage out;
  out.set_used(from.used());
  out.set_limit(from.limit());
  return out;
}

/// What a call's reactor and the manager's sink share about its session.
struct CallState {
  std::mutex mu;
  std::optional<std::uint64_t> key;
  std::string session_id;
  SessionTerms terms;
  std::uint32_t waiting_segment_limit = 0;
  std::uint64_t last_accepted_sequence = 0;
  bool cancelled = false;
  /// Outlives the reactor, which it stops calling once the call is done.
  std::shared_ptr<CallOutput> out;
};

}  // namespace

struct StreamListener::Impl final : wire::SessionService::CallbackService {
  Impl(StreamListenerOptions options_in, SessionDispatch* dispatch_in,
       const SchedulerClock& clock_in)
      : options(std::move(options_in)), dispatch(dispatch_in), clock(clock_in) {}

  grpc::ServerBidiReactor<ClientEvent, ServerEvent>* Session(
      grpc::CallbackServerContext* context) override;

  void on_transition(const ManagedSessionTransition& transition);
  void close_admission() noexcept;
  [[nodiscard]] bool admitting() const;

  StreamListenerOptions options;
  SessionDispatch* dispatch;
  const SchedulerClock& clock;
  std::unique_ptr<SessionManager> manager;
  std::unique_ptr<grpc::Server> server;
  std::uint16_t port = 0;

  std::mutex calls_mu;
  std::map<std::uint64_t, std::shared_ptr<CallState>> calls;

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
class SessionCall final : public grpc::ServerBidiReactor<ClientEvent, ServerEvent>,
                          public stream::CallWriter {
 public:
  explicit SessionCall(StreamListener::Impl& listener)
      : listener_(listener), state_(std::make_shared<CallState>()) {
    state_->out = std::make_shared<CallOutput>(*this, listener.clock);
    StartRead(&in_);
  }

  void OnReadDone(bool ok) override {
    if (!ok) {
      half_closed();
      return;
    }
    if (handle(in_)) {
      StartRead(&in_);
    }
  }

  void OnWriteDone(bool ok) override { state_->out->write_done(ok); }

  void OnCancel() override {
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

  void OnDone() override {
    state_->out->detach();
    delete this;
  }

  void start_write(const ServerEvent& event) override { StartWrite(&event); }
  void finish(const grpc::Status& status) override { Finish(status); }

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
        (void)listener_.manager->apply(*key, LogicalSessionEvent::Ping);
        return true;
      case ClientEvent::kOpen:
        fail(*key,
             Error::make(Error::Code::ConfigInvalid, "a session is opened once", "invalid_event"));
        return false;
      case ClientEvent::BODY_NOT_SET:
        fail(*key, Error::make(Error::Code::ConfigInvalid, "client event with no known body",
                               "invalid_event"));
        return false;
      default:
        fail(*key,
             Error::make(Error::Code::Unsupported, "this server does not serve that event yet",
                         "backend_unsupported_capability"));
        return false;
    }
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
    if (!listener_.admitting()) {
      refuse(Error::make(Error::Code::NotReady, "session admission is closed", "admission_closed"));
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
    {
      const std::lock_guard guard(state_->mu);
      state_->terms = *terms;
      state_->waiting_segment_limit = budgets->waiting_items();
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
    // The call was cancelled before its session had a key to abort.
    if (cancelled_key) {
      abort(*cancelled_key);
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

  void half_closed() {
    if (const auto key = session_key()) {
      (void)listener_.manager->apply(*key, LogicalSessionEvent::HalfClose);
      return;
    }
    refuse(
        Error::make(Error::Code::ConfigInvalid, "the client closed before Open", "invalid_event"));
  }

  void fail(std::uint64_t key, Error cause) const {
    (void)listener_.manager->apply(key, LogicalSessionEvent::Fail, std::move(cause));
  }

  void refuse(const Error& cause) {
    wire::OpenRefused refused;
    *refused.mutable_cause() = stream::end_cause(cause);
    refused.set_admission(wire::ADMISSION_NOT_ADMITTED);
    state_->out->finish(stream::refused_open_status(refused));
  }

  [[nodiscard]] std::string session_id() const {
    const std::lock_guard guard(state_->mu);
    return state_->session_id;
  }

  StreamListener::Impl& listener_;
  std::shared_ptr<CallState> state_;
  ClientEvent in_;
  std::uint64_t expected_sequence_ = 1;
};

}  // namespace

grpc::ServerBidiReactor<ClientEvent, ServerEvent>* StreamListener::Impl::Session(
    grpc::CallbackServerContext* /*context*/) {
  return new SessionCall(*this);
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
  if (first) {
    state->out->bind(output, [target = dispatch, key = transition.session_key](OutputKind kind) {
      if (target != nullptr) {
        target->output_delivered(key, kind);
      }
    });
  }
  std::string session_id;
  SessionTerms terms;
  std::uint32_t waiting_segment_limit = 0;
  std::uint64_t last_accepted_sequence = 0;
  {
    const std::lock_guard guard(state->mu);
    session_id = state->session_id;
    terms = state->terms;
    waiting_segment_limit = state->waiting_segment_limit;
    last_accepted_sequence = state->last_accepted_sequence;
  }
  const auto offer = [&](ServerEvent event, OutputKind kind, std::uint64_t replace_key = 0) {
    auto item = wire_item(std::move(event), kind, replace_key);
    (void)output->offer(item, clock.now());
  };
  if (transition.event) {
    transition.effects.for_each([&](LogicalSessionEffect effect) {
      ServerEvent event;
      switch (effect) {
        case LogicalSessionEffect::EmitCancelAccepted:
          event.mutable_cancel_accepted();
          offer(std::move(event), OutputKind::Control);
          break;
        case LogicalSessionEffect::EmitReady: {
          auto& ready = *event.mutable_ready();
          ready.set_session_id(session_id);
          const bool audio = terms.input_kind == SessionInputKind::Audio;
          auto& limits = *ready.mutable_limits();
          limits.set_idle_timeout_ms(whole_ms(options.limits.idle_timeout()));
          limits.set_heartbeat_interval_ms(whole_ms(options.limits.heartbeat_interval()));
          limits.set_liveness_timeout_ms(whole_ms(options.limits.liveness_timeout()));
          limits.set_max_duration_ms(whole_ms(options.limits.max_duration()));
          limits.set_finalize_deadline_ms(whole_ms(finalize_deadline(terms.input_kind)));
          limits.set_output_stall_timeout_ms(whole_ms(SessionBudgets::kOutputNoProgressTimeout));
          if (audio) {
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
          ready.set_remaining_duration_ms(whole_ms(options.limits.max_duration()));
          const auto& status = transition.status;
          auto& wire_status = *ready.mutable_status();
          wire_status.set_state(stream::wire_state(status.state));
          wire_status.set_input_queue_depth(status.input_queue_depth);
          auto& credit = *wire_status.mutable_input_credit();
          *credit.mutable_bytes() = usage(status.input_credit_bytes);
          // A session that has accepted nothing has no segment waiting.
          credit.mutable_waiting_segments()->set_limit(waiting_segment_limit);
          *wire_status.mutable_output_pcm_bytes() = usage(status.output_pcm_bytes);
          *wire_status.mutable_output_metadata_bytes() = usage(status.output_metadata_bytes);
          offer(std::move(event), OutputKind::Control);
          break;
        }
        case LogicalSessionEffect::EmitReply:
          if (*transition.event == LogicalSessionEvent::Ping) {
            event.mutable_pong();
            offer(std::move(event), OutputKind::Control, kPongReplaceKey);
          }
          break;
        case LogicalSessionEffect::EmitTerminal: {
          auto& closed = *event.mutable_session_closed();
          closed.set_state(stream::wire_state(transition.state));
          if (transition.cause) {
            *closed.mutable_cause() = stream::end_cause(*transition.cause);
          }
          closed.set_admission(wire::ADMISSION_ADMITTED);
          closed.set_last_accepted_sequence(last_accepted_sequence);
          closed.mutable_totals();
          state->out->set_end_status(stream::session_end_status(closed));
          offer(std::move(event), OutputKind::Terminal);
          break;
        }
        default:
          break;
      }
    });
  }
  if (first) {
    (void)output->set_consumer([weak = std::weak_ptr<CallOutput>(state->out)] {
      if (const auto shared = weak.lock()) {
        shared->pump();
      }
    });
  }
}

void StreamListener::Impl::close_admission() noexcept {
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

bool StreamListener::Impl::admitting() const {
  const std::lock_guard guard(manager_mu);
  return !admission_stopped;
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
  raw->server = builder.BuildAndStart();
  if (!raw->server || selected <= 0) {
    if (dispatch != nullptr) {
      dispatch->stop();
    }
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
  impl_->server.reset();
  const std::lock_guard guard(impl_->manager_mu);
  impl_->held_at_stop = impl_->manager->held_slots();
  impl_->manager.reset();
}

std::size_t StreamListener::live_sessions() const noexcept {
  const std::lock_guard lock(impl_->manager_mu);
  return impl_->manager ? impl_->manager->held_slots() : impl_->held_at_stop;
}

}  // namespace tensorplate::serving
