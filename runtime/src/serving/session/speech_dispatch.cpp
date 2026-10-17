// SPDX-License-Identifier: Apache-2.0
#include "serving/session/speech_dispatch.hpp"

#include <algorithm>
#include <cctype>
#include <chrono>
#include <condition_variable>
#include <string_view>
#include <system_error>
#include <utility>
#include <variant>

#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"

namespace tensorplate::serving {
namespace {
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;

/// The internal timeline: the audio voice detection and every input job
/// read, whatever the session's input format.
constexpr StreamAudioFormat kInternalFormat{StreamAudioEncoding::PcmS16Le, kJobInputSampleRateHz,
                                            1};
/// The one input format served: the internal one, so a frame is its own
/// internal audio.
constexpr StreamAudioFormat kInputFormat = kInternalFormat;
constexpr StreamAudioFormat kOutputFormat{StreamAudioEncoding::PcmS16Le, kJobOutputSampleRateHz, 1};
/// The longest utterance and the longest audio of a segment: what one job's
/// input and one job's result may hold.
constexpr std::uint64_t kLongestSeconds = 30;
static_assert(kInternalFormat.bytes_per_second() * kLongestSeconds == kMaxAudioFramesBytes);
static_assert(kOutputFormat.bytes_per_second() * kLongestSeconds == kMaxAudioChunkBytes);
/// The waiting segments and the one being delivered.
constexpr std::uint32_t kSynthesisSegments = SessionBudgets::kWaitingTextSegments + 1;
constexpr std::uint16_t kNormalSpeed = 1000;
/// The utterances of a session that ended and have no final yet: the one
/// being decoded and those behind it. One more fails the session.
constexpr std::size_t kEndedUtterances = 3;
/// The utterance key of every voice-activity job of a session: the backend
/// keeps one state over the session's audio, across endpoints and Finalizes.
/// A job that ends unscored ends its session, so no job needs a new key.
constexpr std::uint64_t kVoiceActivityUtterance = 1;
/// A voice-detected utterance is split where the session's longest one ends.
static_assert(VadProfile{}.max_utterance == std::chrono::seconds{kLongestSeconds});
/// The shortest timed wait for a bound not yet due: a clock moved by hand can
/// stand just short of one for as long as a test likes.
constexpr SchedulerClock::Duration kShortestWait = std::chrono::milliseconds{1};

constexpr std::uint64_t kInternalWidth = kInternalFormat.bytes_per_sample();

/// The longest utterance `terms` grant, in internal samples.
std::uint64_t longest_utterance(const SessionTerms& terms) {
  return static_cast<std::uint64_t>(terms.max_utterance.count()) * kInternalFormat.sample_rate_hz /
         1000;
}

/// Task output a drain waits to see delivered: a partial transcript that
/// gave way unsent never is.
bool awaits_delivery(OutputKind kind) {
  return kind == OutputKind::Audio || kind == OutputKind::Result;
}

bool blank(const std::string& text) {
  return std::all_of(text.begin(), text.end(),
                     [](unsigned char c) { return std::isspace(c) != 0; });
}

Error not_served(const char* message, const char* context) {
  return Error::make(Error::Code::Unsupported, message, context);
}

/// Input the manager accepted that was not handed over yet, or is still
/// being worked on, keeps its depth above `held`: the frames handed over
/// whose credit the dispatch holds.
bool input_owed(const SessionManager& manager, std::uint64_t key, std::size_t held) {
  const auto status = manager.status(key);
  return !status || status->input_queue_depth > held;
}

Error wrong_input_kind() {
  return Error::make(Error::Code::Internal, "input of a kind the session does not take",
                     "wrong_input_kind");
}

/// The stream's reason for a backend error that answers any job of the
/// session, not one job's work: a session it unloaded, or a job class or an
/// option it refuses.
std::optional<std::string> refusal_reason(const Error& error) {
  const std::string_view reason = error.context ? std::string_view{*error.context} : "";
  if (reason == "session_unloaded") {
    return "backend_unavailable";
  }
  if (reason == "job_class_unsupported" || reason == "job_not_permitted") {
    return "backend_unsupported_capability";
  }
  return std::nullopt;
}

/// The bridge's word for a job that ended because the backend was lost.
bool backend_was_reset(const Error& error) {
  return error.code == Error::Code::Unavailable && error.context == "backend_reset";
}

/// `per_second` for each second of `bytes` of audio at `bytes_per_second`.
SchedulerClock::Duration per_audio(SchedulerClock::Duration per_second, std::uint64_t bytes,
                                   std::uint64_t bytes_per_second) {
  return SchedulerClock::Duration{per_second.count() * static_cast<std::int64_t>(bytes) /
                                  static_cast<std::int64_t>(bytes_per_second)};
}
}  // namespace

SpeechJobDispatch::HeldBuffer::HeldBuffer(HeldBuffer&& other) noexcept
    : buffers_(std::exchange(other.buffers_, nullptr)), ref_(other.ref_) {}

SpeechJobDispatch::HeldBuffer& SpeechJobDispatch::HeldBuffer::operator=(
    HeldBuffer&& other) noexcept {
  // What this held goes with `other`.
  std::swap(buffers_, other.buffers_);
  std::swap(ref_, other.ref_);
  return *this;
}

SpeechJobDispatch::HeldBuffer::~HeldBuffer() {
  reset();
}

void SpeechJobDispatch::HeldBuffer::reset() noexcept {
  if (buffers_ != nullptr) {
    (void)std::exchange(buffers_, nullptr)->release(ref_);
  }
}

struct SpeechJobDispatch::Moved {
  std::optional<Event> event;
  LogicalSessionEffects effects;
  std::shared_ptr<BoundedOutputQueue> output;
};

struct SpeechJobDispatch::JobEnded {
  std::uint64_t job_id = 0;
  std::variant<TranscriptResult, SpeechAudio, VadResult, Error> outcome;
};

struct SpeechJobDispatch::Command {
  struct Opened {
    SessionTerms terms;
  };
  struct AudioIn {
    std::vector<std::byte> frame;
  };
  struct FinalizeIn {
    std::uint64_t client_sequence = 0;
  };
  struct Delivered {
    bool awaited = false;
  };
  struct SessionReleased {};
  struct JobReleased {
    std::uint64_t job_id = 0;
  };

  std::uint64_t session_key = 0;
  std::variant<Opened, Moved, AudioIn, WaitingSegment, FinalizeIn, Delivered, JobEnded,
               SessionReleased, JobReleased>
      body;
};

/// What the bridge's callbacks and the dispatch's thread share. The bridge
/// keeps it as its sink, so it outlives a stopped or destroyed dispatch and
/// still settles the buffers of the events that arrive then.
struct SpeechJobDispatch::Core final : JobEventSink {
  explicit Core(BufferManager& buffers_in) noexcept : buffers(buffers_in) {}

  void on_accepted(const JobEvent& /*event*/) noexcept override {}
  // No job of the dispatch asks for progress, and the bridge delivers none
  // that was not asked for: a fragment is another submitter's, and dropped.
  void on_progress(const JobEvent& event) noexcept override {
    const auto& fragment = event.result();
    if (const auto* chunk =
            fragment.has_value() ? std::get_if<AudioChunkResult>(&fragment->payload()) : nullptr;
        chunk != nullptr) {
      HeldBuffer{buffers, chunk->pcm.buffer}.reset();
    }
  }
  // Queueing can only fail for want of memory, which ends the worker here as
  // it would on any other thread.
  // NOLINTNEXTLINE(bugprone-exception-escape)
  void on_completed(const JobEvent& event) noexcept override { job_ended(event); }
  // NOLINTNEXTLINE(bugprone-exception-escape)
  void on_failed(const JobEvent& event) noexcept override { job_ended(event); }
  void on_cancel_acknowledged(const JobEvent& /*event*/) noexcept override {}
  // NOLINTNEXTLINE(bugprone-exception-escape)
  void on_released(const JobEvent& event) noexcept override {
    (void)take_input(event.identity().job_id);
    Command command;
    command.session_key = event.identity().session_key;
    command.body = Command::JobReleased{event.identity().job_id};
    post(std::move(command));
  }
  // NOLINTNEXTLINE(bugprone-exception-escape)
  void on_session_released(std::uint64_t session_key,
                           std::uint64_t /*generation*/) noexcept override {
    Command command;
    command.session_key = session_key;
    command.body = Command::SessionReleased{};
    post(std::move(command));
  }

  void job_ended(const JobEvent& event) {
    JobEnded ended;
    ended.job_id = event.identity().job_id;
    const auto& result = event.result();
    const auto* transcript =
        result.has_value() ? std::get_if<TranscriptResult>(&result->payload()) : nullptr;
    const auto* chunk =
        result.has_value() ? std::get_if<AudioChunkResult>(&result->payload()) : nullptr;
    const auto* vad = result.has_value() ? std::get_if<VadResult>(&result->payload()) : nullptr;
    if (transcript != nullptr) {
      ended.outcome = *transcript;
    } else if (chunk != nullptr) {
      ended.outcome = SpeechAudio{HeldBuffer{buffers, chunk->pcm.buffer}, chunk->pcm.byte_offset,
                                  chunk->pcm.byte_size};
    } else if (vad != nullptr) {
      ended.outcome = *vad;
    } else {
      ended.outcome = event.error().value_or(Error::make(
          Error::Code::Internal, "a job ended with no speech result", "unexpected_job_result"));
    }
    Command command;
    command.session_key = event.identity().session_key;
    command.body = std::move(ended);
    post(std::move(command));
  }

  /// A command that arrives after stop() is dropped here, outside the lock,
  /// and its result buffer with it.
  void post(Command command) {
    {
      const std::lock_guard guard(mu);
      if (!stopping) {
        commands.push_back(std::move(command));
      }
    }
    cv.notify_one();
  }

  void hold_input(std::uint64_t job_id, HeldBuffer input) {
    const std::lock_guard guard(mu);
    inputs.insert_or_assign(job_id, std::move(input));
  }

  /// The caller drops the buffer outside the lock.
  [[nodiscard]] HeldBuffer take_input(std::uint64_t job_id) {
    const std::lock_guard guard(mu);
    auto node = inputs.extract(job_id);
    return node.empty() ? HeldBuffer{} : std::move(node.mapped());
  }

  BufferManager& buffers;
  std::mutex mu;
  std::condition_variable cv;
  std::condition_variable idle_cv;
  SessionManager* manager = nullptr;
  std::deque<Command> commands;
  /// The input buffer of each job the backend has not reported released.
  std::map<std::uint64_t, HeldBuffer> inputs;
  bool stopping = false;
  bool working = false;
  /// The bounds are to be checked against a clock moved by hand.
  bool clock_advanced = false;
  SpeechDispatchStats published;
};

Result<std::unique_ptr<SpeechJobDispatch>> SpeechJobDispatch::create(
    std::shared_ptr<BoundedJobBridge> bridge, BufferManager& buffers, const SchedulerClock& clock,
    SpeechDispatchOptions options) {
  if (bridge == nullptr) {
    return unexpected(Error::make(Error::Code::ConfigInvalid, "speech dispatch needs a job bridge",
                                  "job_bridge_null"));
  }
  if (options.generation == 0) {
    return unexpected(Error::make(Error::Code::ConfigInvalid,
                                  "speech dispatch needs a nonzero generation", "generation_zero"));
  }
  std::optional<VadPolicy> detection;
  if (bridge->runs(JobClass::VadFrames)) {
    auto policy = VadPolicy::create(VadProfile{});
    if (!policy) {
      return unexpected(policy.error());
    }
    detection = *policy;
  }
  auto core = std::make_shared<Core>(buffers);
  std::unique_ptr<SpeechJobDispatch> dispatch;
  try {
    dispatch = std::unique_ptr<SpeechJobDispatch>(
        new SpeechJobDispatch(std::move(bridge), core, clock, std::move(options), detection));
  } catch (const std::system_error&) {
    return unexpected(Error::make(Error::Code::Internal, "speech dispatch thread could not start",
                                  "dispatch_thread_unavailable"));
  }
  if (auto set = dispatch->bridge_->set_event_sink(std::move(core)); !set) {
    return unexpected(set.error());
  }
  return dispatch;
}

SpeechJobDispatch::SpeechJobDispatch(std::shared_ptr<BoundedJobBridge> bridge,
                                     std::shared_ptr<Core> core, const SchedulerClock& clock,
                                     SpeechDispatchOptions options,
                                     std::optional<VadPolicy> detection)
    : bridge_(std::move(bridge)),
      core_(std::move(core)),
      clock_(clock),
      options_(std::move(options)),
      detection_(detection),
      thread_([this] { run(); }) {}

SpeechJobDispatch::~SpeechJobDispatch() {
  stop();
}

SpeechDispatchStats SpeechJobDispatch::stats() const {
  const std::lock_guard guard(core_->mu);
  return core_->published;
}

bool SpeechJobDispatch::wait_idle(std::chrono::milliseconds patience) {
  std::unique_lock lock(core_->mu);
  return core_->idle_cv.wait_for(lock, patience, [&] {
    return core_->commands.empty() && !core_->working && !core_->clock_advanced;
  });
}

void SpeechJobDispatch::notify_clock_advanced() {
  {
    const std::lock_guard guard(core_->mu);
    core_->clock_advanced = true;
  }
  core_->cv.notify_all();
}

void SpeechJobDispatch::attach(SessionManager& manager) {
  {
    const std::lock_guard guard(core_->mu);
    core_->manager = &manager;
  }
  core_->cv.notify_all();
}

void SpeechJobDispatch::stop() noexcept {
  std::deque<Command> dropped;
  {
    const std::lock_guard guard(core_->mu);
    core_->stopping = true;
    dropped.swap(core_->commands);
  }
  core_->cv.notify_all();
  core_->idle_cv.notify_all();
  std::call_once(joined_, [this] {
    thread_.join();
    // An unload cancels nothing: what is still open is released here.
    for (const auto& [key, session] : sessions_) {
      if (!session.releasing) {
        (void)bridge_->release_session(key, options_.generation);
      }
    }
    // A release the bridge refused cancelled nothing either.
    const auto cancel_if_refused = [this](const InFlight& job) {
      const auto owner = sessions_.find(job.session_key);
      if (owner != sessions_.end() && owner->second.release_refused) {
        (void)bridge_->cancel(job.job_id);
      }
    };
    if (in_flight_) {
      cancel_if_refused(*in_flight_);
    }
    for (const auto& [job_id, job] : vad_in_flight_) {
      cancel_if_refused(job);
    }
    sessions_.clear();
    lane_.clear();
    partial_lane_.clear();
    vad_lane_.clear();
  });
}

Result<SessionTerms> SpeechJobDispatch::negotiate(const SessionRequest& request) const {
  const bool takes_audio = request.input_kind == SessionInputKind::Audio;
  if (takes_audio ? !options_.audio_input : !options_.text_input) {
    return unexpected(not_served("input kind is not served", "unsupported_input_kind"));
  }
  // The options a job of the session would carry, checked by a job request's
  // own rules: a synthesis takes both, and a decode takes no voice.
  const auto probe = JobRequest::create(
      JobIdentity{1, 1, 1}, JobClass::TtsSynthesis, TextSegment{"a"},
      JobOptions{request.language, takes_audio ? std::string{"a"} : request.voice, kNormalSpeed});
  if (!probe) {
    return unexpected(probe.error().context == "language_invalid"
                          ? not_served("language is not served", "unsupported_language")
                          : not_served("voice is not served", "unsupported_voice"));
  }
  SessionTerms terms;
  terms.input_kind = request.input_kind;
  terms.language = request.language;
  if (takes_audio) {
    if (request.input_format != kInputFormat) {
      return unexpected(not_served("audio input format is not served", "unsupported_audio_format"));
    }
    terms.audio_format = kInputFormat;
    terms.min_frame = std::chrono::milliseconds{20};
    terms.max_frame = std::chrono::milliseconds{320};
    terms.max_utterance = std::chrono::seconds{kLongestSeconds};
    return terms;
  }
  terms.voice = request.voice;
  terms.audio_format = kOutputFormat;
  terms.max_segment_text_bytes = static_cast<std::uint32_t>(kMaxTextSegmentBytes);
  terms.max_segment_audio = std::chrono::seconds{kLongestSeconds};
  terms.max_synthesis_text_bytes = kSynthesisSegments * terms.max_segment_text_bytes;
  terms.max_synthesis_audio = kSynthesisSegments * terms.max_segment_audio;
  return terms;
}

Result<void> SpeechJobDispatch::open_session(std::uint64_t session_key, const SessionTerms& terms) {
  core_->post({session_key, Command::Opened{terms}});
  return {};
}

void SpeechJobDispatch::on_transition(const ManagedSessionTransition& transition) {
  core_->post(
      {transition.session_key, Moved{transition.event, transition.effects, transition.output}});
}

void SpeechJobDispatch::audio(std::uint64_t session_key, std::span<const std::byte> frame) {
  core_->post({session_key, Command::AudioIn{{frame.begin(), frame.end()}}});
}

void SpeechJobDispatch::text_segment(std::uint64_t session_key, std::uint64_t segment_id,
                                     std::string text) {
  core_->post({session_key, WaitingSegment{segment_id, std::move(text)}});
}

void SpeechJobDispatch::finalize(std::uint64_t session_key, std::uint64_t client_sequence) {
  core_->post({session_key, Command::FinalizeIn{client_sequence}});
}

void SpeechJobDispatch::output_delivered(std::uint64_t session_key, OutputKind kind) {
  core_->post({session_key, Command::Delivered{awaits_delivery(kind)}});
}

struct SpeechJobDispatch::Step {
  std::optional<Command> command;
  /// No command: the bounds are to be checked.
  bool check = false;
};

void SpeechJobDispatch::run() {
  std::unique_lock lock(core_->mu);
  while (wait_for_step(lock)) {
    Step step = take_step();
    SessionManager& manager = *core_->manager;
    core_->working = true;
    // The manager and the bridge are called unlocked: both call back here.
    lock.unlock();
    if (step.command) {
      handle(manager, std::move(*step.command));
    } else if (step.check) {
      check_bounds(manager);
    }
    waiting_.bound = next_bound();
    stats_.sessions = sessions_.size();
    ++stats_.steps;
    lock.lock();
    core_->published = stats_;
    core_->working = false;
    core_->idle_cv.notify_all();
  }
}

bool SpeechJobDispatch::wait_for_step(std::unique_lock<std::mutex>& lock) {
  const auto woken = [&] {
    return core_->stopping ||
           (core_->manager != nullptr && (!core_->commands.empty() || core_->clock_advanced));
  };
  for (;;) {
    if (waiting_.before_check) {
      // The check is due: nothing to wait for.
    } else if (waiting_.bound) {
      // A timed wait ends short of its bound on a clock slower than the
      // steady clock, or moved by hand: what is left is waited again.
      const auto left = *waiting_.bound - clock_.now();
      const auto wait = left > SchedulerClock::Duration::zero() ? std::max(left, kShortestWait)
                                                                : SchedulerClock::Duration::zero();
      if (!core_->cv.wait_for(lock, wait, woken) && clock_.now() < *waiting_.bound) {
        continue;
      }
    } else {
      core_->cv.wait(lock, woken);
    }
    return !core_->stopping;
  }
}

SpeechJobDispatch::Step SpeechJobDispatch::take_step() {
  if (!waiting_.before_check &&
      (core_->clock_advanced || (waiting_.bound && clock_.now() >= *waiting_.bound))) {
    // A job's end already queued is not taken for a deadline missed, and what
    // arrives from here on does not put the check off.
    core_->clock_advanced = false;
    waiting_.before_check = core_->commands.size();
  }
  Step step;
  if (!core_->commands.empty() && waiting_.before_check.value_or(1) > 0) {
    step.command.emplace(std::move(core_->commands.front()));
    core_->commands.pop_front();
    if (waiting_.before_check) {
      --*waiting_.before_check;
    }
  }
  step.check = !step.command && waiting_.before_check.has_value();
  if (step.check) {
    waiting_.before_check.reset();
  }
  return step;
}

void SpeechJobDispatch::handle(SessionManager& manager, Command command) {
  const std::uint64_t key = command.session_key;
  if (auto* opened = std::get_if<Command::Opened>(&command.body)) {
    Session& session = sessions_[key];
    session.terms = std::move(opened->terms);
    if (detection_ && session.terms.input_kind == SessionInputKind::Audio) {
      session.detection.emplace(*detection_);
    }
    return;
  }
  if (auto* ended = std::get_if<JobEnded>(&command.body)) {
    job_ended(manager, key, *ended);
  } else if (std::holds_alternative<Command::SessionReleased>(command.body)) {
    // The bridge acknowledges only a release it was asked for.
    if (const auto it = sessions_.find(key); it != sessions_.end()) {
      stats_.longest_release_wait =
          std::max(stats_.longest_release_wait, clock_.now() - it->second.release_asked);
      acknowledge(manager, key);
    }
  } else if (const auto* transition = std::get_if<Moved>(&command.body)) {
    moved(manager, key, *transition);
  } else if (const auto* released = std::get_if<Command::JobReleased>(&command.body)) {
    // The backend counts a job open until it is released, and so do the lanes.
    if (unreleased_vad_.erase(released->job_id) == 0 && unreleased_job_ == released->job_id) {
      unreleased_job_.reset();
    }
  }
  if (const auto it = sessions_.find(key); it != sessions_.end()) {
    hand_over(manager, key, it->second, command);
    pump(manager, key, it->second);
  }
  start_next(manager);
}

void SpeechJobDispatch::hand_over(SessionManager& manager, std::uint64_t key, Session& session,
                                  Command& command) {
  const bool takes_audio = session.terms.input_kind == SessionInputKind::Audio;
  if (const auto* input = std::get_if<Command::AudioIn>(&command.body)) {
    // On a session that takes text the credit refuses this as
    // wrong_input_kind; with voice detection it returns once it was scored,
    // or at once, the frame dropped, when the session has ended.
    if (session.detection && !session.ended) {
      keep_audio(session, intake(session, input->frame), input->frame.size());
    } else if (manager.release_audio_input(key, 1, input->frame.size())) {
      take_audio(manager, key, session, intake(session, input->frame));
    } else {
      session.ended = true;
    }
  } else if (auto* segment = std::get_if<WaitingSegment>(&command.body)) {
    if (takes_audio) {
      fail(manager, key, session, wrong_input_kind());
    } else {
      session.waiting.push_back(std::move(*segment));
    }
  } else if (const auto* asked = std::get_if<Command::FinalizeIn>(&command.body)) {
    session.owed_finalize -= std::min(session.owed_finalize, 1U);
    if (!takes_audio) {
      session.finalize_sequence = asked->client_sequence;
    } else if (session.detection) {
      session.detection->pending.push_back(PendingEnd{
          session.samples_accepted, EndpointReason::ClientFinalize, asked->client_sequence});
    } else if (may_end_utterance(manager, key, session)) {
      (void)end_utterance(manager, key, session, session.open_start, session.samples_accepted,
                          EndpointReason::ClientFinalize, asked->client_sequence);
    }
  } else if (const auto* delivered = std::get_if<Command::Delivered>(&command.body)) {
    // Any delivery can make room for a held item; only task output counts.
    session.undelivered -= delivered->awaited ? 1 : 0;
  }
}

void SpeechJobDispatch::moved(SessionManager& manager, std::uint64_t key, const Moved& moved) {
  const auto it = sessions_.find(key);
  if (moved.effects.contains(Effect::RequestCleanup)) {
    if (it == sessions_.end()) {
      // Never opened here, so the backend holds nothing for it.
      (void)manager.apply(key, Event::ReleaseAcknowledged);
    } else {
      release(manager, key, it->second);
    }
    return;
  }
  if (it == sessions_.end()) {
    return;
  }
  Session& session = it->second;
  session.output = moved.output;
  if (moved.effects.contains(Effect::StartFinalize)) {
    // An automatic endpoint starts a finalization too; only a client's
    // Finalize is handed over.
    if (moved.event == Event::Finalize) {
      ++session.owed_finalize;
    }
    if (session.completions_unseen == 0) {
      session.finalizing = true;
    }
  }
  if (moved.event == Event::FinalizeCompleted && session.completions_unseen > 0) {
    --session.completions_unseen;
  }
  if (moved.effects.contains(Effect::StartDrain)) {
    session.draining = true;
    session.close_open_utterance = moved.event == Event::HalfClose;
  }
}

void SpeechJobDispatch::release(SessionManager& manager, std::uint64_t key, Session& session) {
  session.releasing = true;
  session.ended = true;
  // What it still held of its input, its output and the backend's audio.
  session.held.clear();
  session.waiting.clear();
  session.active.reset();
  session.ended_utterances.clear();
  std::vector<std::byte>().swap(session.open_audio);
  session.release_asked = clock_.now();
  session.release_bound_at = release_due(key, session);
  // The bridge cancels the session's unfinished job with it.
  const auto released = bridge_->release_session(key, options_.generation);
  if (released) {
    return;
  }
  if (released.error().code == Error::Code::Unavailable) {
    // The backend holds nothing any more: that is a released session.
    acknowledge(manager, key);
    return;
  }
  session.release_refused = true;
  ++stats_.releases_refused;
}

void SpeechJobDispatch::acknowledge(SessionManager& manager, std::uint64_t key) {
  sessions_.erase(key);
  (void)manager.apply(key, Event::ReleaseAcknowledged);
}

void SpeechJobDispatch::job_ended(SessionManager& manager, std::uint64_t key, JobEnded& ended) {
  const bool voice_activity = vad_in_flight_.erase(ended.job_id) != 0;
  std::uint64_t partial_of = 0;
  if (!voice_activity) {
    if (!in_flight_ || in_flight_->job_id != ended.job_id) {
      ++stats_.jobs_discarded;
      return;
    }
    partial_of = in_flight_->partial_of;
    // Its deadline no longer applies; its lane stays taken until it is
    // released.
    in_flight_.reset();
  }
  Session* const served = serving_session(manager, key, ended, voice_activity, partial_of);
  if (served == nullptr) {
    return;
  }
  Session& session = *served;
  ++stats_.jobs_completed;
  if (auto* vad = std::get_if<VadResult>(&ended.outcome); vad != nullptr && voice_activity) {
    session.detection->scores.insert(session.detection->scores.end(), vad->probabilities.begin(),
                                     vad->probabilities.end());
    return;
  }
  auto* transcript = std::get_if<TranscriptResult>(&ended.outcome);
  if (transcript != nullptr && partial_of != 0) {
    take_partial(session, partial_of, std::move(transcript->text));
    return;
  }
  if (transcript != nullptr && !voice_activity && !session.ended_utterances.empty()) {
    session.ended_utterances.front().transcript = std::move(*transcript);
    return;
  }
  if (auto* speech = std::get_if<SpeechAudio>(&ended.outcome);
      speech != nullptr && !voice_activity && session.active && !session.active->synthesized) {
    const StreamAudioFormat& format = session.terms.audio_format;
    const std::uint64_t granted =
        static_cast<std::uint64_t>(session.terms.max_synthesis_audio.count()) *
        format.sample_rate_hz / 1000;
    if (session.samples_finished + speech->bytes / format.bytes_per_sample() > granted) {
      fail(manager, key, session,
           Error::make(Error::Code::ResourceExhausted,
                       "segments since the last Finalize yield more audio than was granted",
                       "synthesis_audio_limit_exceeded"));
      return;
    }
    session.active->audio = std::move(*speech);
    session.active->synthesized = true;
    return;
  }
  fail(manager, key, session,
       Error::make(Error::Code::Internal, "a job ended with a result its session did not ask for",
                   "unexpected_job_result"));
}

SpeechJobDispatch::Session* SpeechJobDispatch::serving_session(SessionManager& manager,
                                                               std::uint64_t key,
                                                               const JobEnded& ended,
                                                               bool voice_activity,
                                                               std::uint64_t partial_of) {
  const auto it = sessions_.find(key);
  const auto* error = std::get_if<Error>(&ended.outcome);
  const bool served = it != sessions_.end() && !it->second.ended;
  if (error != nullptr && backend_was_reset(*error)) {
    ++(served ? stats_.jobs_failed : stats_.jobs_discarded);
    reset_every_session(manager, *error);
    return nullptr;
  }
  if (!served) {
    ++stats_.jobs_discarded;
    return nullptr;
  }
  Session& session = it->second;
  (voice_activity ? session.detection->job : session.job) = JobState::None;
  if (error != nullptr) {
    ++stats_.jobs_failed;
    // A refusal would meet the final too: it fails the session now.
    if (partial_of != 0 && !refusal_reason(*error)) {
      drop_failed_partial(session, partial_of, *error);
    } else {
      fail_for_backend(manager, key, session, *error);
    }
    return nullptr;
  }
  return &session;
}

void SpeechJobDispatch::take_partial(Session& session, std::uint64_t utterance_id,
                                     std::string text) {
  OpenPartials& partials = session.detection->partials;
  // An utterance that ended meanwhile has had its endpoint: no partial follows.
  if (utterance_id != session.utterances + 1 || blank(text) || text == partials.text) {
    return;
  }
  partials.ready = std::move(text);
}

// The final is still decoded, and fails typed on its own if it fails too.
void SpeechJobDispatch::drop_failed_partial(Session& session, std::uint64_t utterance_id,
                                            const Error& error) {
  ++stats_.partials_failed;
  Detection& detection = *session.detection;
  if (detection.failure_reported != utterance_id) {
    detection.failure_reported = utterance_id;
    if (options_.on_partial_failed) {
      options_.on_partial_failed(error);
    }
  }
}

void SpeechJobDispatch::check_bounds(SessionManager& manager) {
  const auto now = clock_.now();
  const auto check = [&](InFlight& job) {
    if (job.past_deadline || now < job.deadline) {
      return;
    }
    job.past_deadline = true;
    ++stats_.jobs_past_deadline;
    if (const auto it = sessions_.find(job.session_key);
        it != sessions_.end() && !it->second.ended) {
      fail(manager, job.session_key, it->second,
           Error::make(Error::Code::Timeout, "the session's job did not end within its deadline",
                       "deadline_missed"));
    }
  };
  if (in_flight_) {
    check(*in_flight_);
  }
  for (auto& [job_id, job] : vad_in_flight_) {
    check(job);
  }
  for (auto& [key, session] : sessions_) {
    if (!session.releasing || session.past_bound || now < session.release_bound_at) {
      continue;
    }
    session.past_bound = true;
    ++stats_.releases_past_bound;
    if (!stalled_ && options_.on_stall) {
      stalled_ = true;
      options_.on_stall(Error::make(Error::Code::Timeout,
                                    "a session's release was not acknowledged within its bound",
                                    "release_bound_exceeded"));
    }
  }
}

std::optional<SchedulerClock::TimePoint> SpeechJobDispatch::next_bound() const {
  std::optional<SchedulerClock::TimePoint> next;
  const auto consider = [&next](SchedulerClock::TimePoint at) {
    if (!next || at < *next) {
      next = at;
    }
  };
  if (in_flight_ && !in_flight_->past_deadline) {
    consider(in_flight_->deadline);
  }
  for (const auto& [job_id, job] : vad_in_flight_) {
    if (!job.past_deadline) {
      consider(job.deadline);
    }
  }
  for (const auto& [key, session] : sessions_) {
    if (session.releasing && !session.past_bound) {
      consider(session.release_bound_at);
    }
  }
  return next;
}

SchedulerClock::TimePoint SpeechJobDispatch::release_due(std::uint64_t key,
                                                         const Session& session) const {
  // A release cancels the session's jobs, which end only when their work does.
  SchedulerClock::TimePoint latest = session.release_asked;
  if (in_flight_ && in_flight_->session_key == key) {
    latest = std::max(latest, in_flight_->deadline);
  }
  for (const auto& [job_id, job] : vad_in_flight_) {
    if (job.session_key == key) {
      latest = std::max(latest, job.deadline);
    }
  }
  return latest + options_.release_bound;
}

SchedulerClock::Duration SpeechJobDispatch::job_deadline(const JobRequest& request,
                                                         const Session& session) const {
  // An input job holds internal audio; a synthesis's audio is known only
  // once it has run: its granted maximum.
  const StreamAudioFormat* format = &kInternalFormat;
  std::uint64_t bytes = 0;
  if (const auto* audio = std::get_if<AudioFrames>(&request.payload())) {
    bytes = audio->pcm.byte_size;
  } else if (const auto* frames = std::get_if<VadFrames>(&request.payload())) {
    bytes = frames->pcm.byte_size;
  } else {
    format = &session.terms.audio_format;
    bytes = static_cast<std::uint64_t>(session.terms.max_segment_audio.count()) *
            format->bytes_per_second() / 1000;
  }
  return options_.job_deadline_base +
         per_audio(options_.job_deadline_per_audio_second, bytes, format->bytes_per_second());
}

void SpeechJobDispatch::want_job(std::uint64_t key, Session& session) {
  if (session.job == JobState::None) {
    session.job = JobState::Queued;
    lane_.push_back(key);
  }
}

void SpeechJobDispatch::want_vad(std::uint64_t key, Session& session) {
  if (session.detection->job == JobState::None) {
    session.detection->job = JobState::Queued;
    vad_lane_.push_back(key);
  }
}

void SpeechJobDispatch::want_partial(std::uint64_t key, Session& session) {
  Detection& detection = *session.detection;
  // Asked even while queued, so that its turn compares with the frames scored last.
  if (session.job == JobState::None && partial_window(session) && !detection.partial_queued) {
    detection.partial_queued = true;
    partial_lane_.push_back(key);
  }
}

std::optional<SampleWindow> SpeechJobDispatch::partial_window(Session& session) {
  Detection& detection = *session.detection;
  // An utterance whose end was asked for is decoded whole next; one the
  // session drains without ending is not decoded at all.
  if (session.draining || !detection.pending.empty()) {
    return std::nullopt;
  }
  const std::uint64_t scored = detection.policy.next_frame_start();
  auto window = detection.windows.due(scored);
  if (window && detection.windows.waits(scored, detection.policy.speech_end().value_or(scored))) {
    return std::nullopt;
  }
  return window;
}

bool SpeechJobDispatch::hold_partial(Session& session) {
  OpenPartials& partials = session.detection->partials;
  if (!partials.ready) {
    return false;
  }
  partials.text = std::move(*partials.ready);
  partials.ready.reset();
  session.held.push_back(make_task_output_item(
      PartialTranscriptOutput{session.utterances + 1, ++partials.revision, partials.text}));
  return true;
}

void SpeechJobDispatch::start_next(SessionManager& manager) {
  while (!(backend_lost_ ? in_flight_.has_value() : unreleased_job_.has_value()) &&
         !(lane_.empty() && partial_lane_.empty())) {
    const bool partial = lane_.empty();
    std::deque<std::uint64_t>& queue = partial ? partial_lane_ : lane_;
    const std::uint64_t key = queue.front();
    queue.pop_front();
    const auto it = sessions_.find(key);
    if (it == sessions_.end() || it->second.ended) {
      continue;
    }
    Session& session = it->second;
    HeldBuffer input;
    std::optional<JobRequest> request;
    if (partial) {
      session.detection->partial_queued = false;
      request = next_partial_request(manager, key, session, input);
    } else {
      session.job = JobState::None;
      request = next_request(manager, key, session, input);
    }
    if (!request) {
      continue;
    }
    const std::uint64_t job_id = request->identity().job_id;
    // Recorded before the submit: the job's events may overtake its return.
    core_->hold_input(job_id, std::move(input));
    in_flight_ = InFlight{job_id, key, clock_.now() + job_deadline(*request, session), false,
                          partial ? session.utterances + 1 : 0};
    unreleased_job_ = job_id;
    session.job = JobState::Running;
    const auto submitted = bridge_->submit(std::move(request).value());
    if (!submitted) {
      // No event follows a refused submit, `released` included.
      (void)core_->take_input(job_id);
      in_flight_.reset();
      unreleased_job_.reset();
      session.job = JobState::None;
      fail_for_backend(manager, key, session, submitted.error());
      continue;
    }
    ++stats_.jobs_submitted;
  }
  start_voice_activity(manager);
}

void SpeechJobDispatch::start_voice_activity(SessionManager& manager) {
  while ((backend_lost_ ? vad_in_flight_.size() : unreleased_vad_.size()) < kVadJobsInFlight &&
         !vad_lane_.empty()) {
    const std::uint64_t key = vad_lane_.front();
    vad_lane_.pop_front();
    const auto it = sessions_.find(key);
    if (it == sessions_.end() || it->second.ended) {
      continue;
    }
    Session& session = it->second;
    Detection& detection = *session.detection;
    detection.job = JobState::None;
    HeldBuffer input;
    auto request = next_vad_request(manager, key, session, input);
    if (!request) {
      continue;
    }
    const std::uint64_t job_id = request->identity().job_id;
    core_->hold_input(job_id, std::move(input));
    vad_in_flight_.emplace(job_id,
                           InFlight{job_id, key, clock_.now() + job_deadline(*request, session)});
    unreleased_vad_.insert(job_id);
    detection.job = JobState::Running;
    const auto submitted = bridge_->submit(std::move(request).value());
    if (!submitted) {
      (void)core_->take_input(job_id);
      vad_in_flight_.erase(job_id);
      unreleased_vad_.erase(job_id);
      detection.job = JobState::None;
      fail_for_backend(manager, key, session, submitted.error());
      continue;
    }
    ++stats_.jobs_submitted;
  }
}

std::optional<JobRequest> SpeechJobDispatch::next_request(SessionManager& manager,
                                                          std::uint64_t key, Session& session,
                                                          HeldBuffer& input) {
  if (session.active) {
    auto synthesis =
        JobRequest::create(JobIdentity{next_job_id_++, key, options_.generation},
                           JobClass::TtsSynthesis, TextSegment{session.active->text},
                           JobOptions{session.terms.language, session.terms.voice, kNormalSpeed});
    if (!synthesis) {
      fail(manager, key, session, synthesis.error());
      return std::nullopt;
    }
    return std::move(synthesis).value();
  }
  EndedUtterance& utterance = session.ended_utterances.front();
  auto decode =
      decode_request(manager, key, session, input, utterance.audio, utterance.first_sample);
  // The buffer is what stays readable for the job from here on.
  std::vector<std::byte>().swap(utterance.audio);
  return decode;
}

std::optional<JobRequest> SpeechJobDispatch::next_partial_request(SessionManager& manager,
                                                                  std::uint64_t key,
                                                                  Session& session,
                                                                  HeldBuffer& input) {
  const auto window = partial_window(session);
  if (!window) {
    return std::nullopt;
  }
  session.detection->windows.started(window->end);
  const auto audio =
      std::span{session.open_audio}.subspan((window->first - session.open_start) * kInternalWidth,
                                            (window->end - window->first) * kInternalWidth);
  return decode_request(manager, key, session, input, audio, window->first);
}

std::optional<JobRequest> SpeechJobDispatch::decode_request(SessionManager& manager,
                                                            std::uint64_t key, Session& session,
                                                            HeldBuffer& input,
                                                            std::span<const std::byte> audio,
                                                            std::uint64_t first_sample) {
  const JobIdentity identity{next_job_id_++, key, options_.generation};
  const auto buffer = core_->buffers.allocate(audio.size());
  if (!buffer) {
    fail(manager, key, session, buffer.error());
    return std::nullopt;
  }
  input = HeldBuffer{core_->buffers, *buffer};
  const auto bytes = core_->buffers.data(*buffer);
  if (!bytes) {
    fail(manager, key, session, bytes.error());
    return std::nullopt;
  }
  std::copy(audio.begin(), audio.end(), bytes->begin());
  JobOptions options;
  options.language = session.terms.language;
  auto decode = JobRequest::create(
      identity, JobClass::SttDecode,
      AudioFrames{kJobInputAudioFormat, PcmWindow{*buffer, 0, audio.size()}, first_sample},
      std::move(options));
  if (!decode) {
    fail(manager, key, session, decode.error());
    return std::nullopt;
  }
  return std::move(decode).value();
}

std::optional<JobRequest> SpeechJobDispatch::next_vad_request(SessionManager& manager,
                                                              std::uint64_t key, Session& session,
                                                              HeldBuffer& input) {
  Detection& detection = *session.detection;
  const std::uint64_t frame_samples = detection.policy.profile().frame_samples;
  const std::uint64_t until =
      detection.pending.empty() ? session.samples_accepted : detection.pending.front().end_sample;
  const auto frames = static_cast<std::uint32_t>(std::min<std::uint64_t>(
      (until - detection.submitted_end) / frame_samples, kMaxVadFramesPerJob));
  const std::size_t size = frames * frame_samples * kInternalWidth;
  const auto buffer = core_->buffers.allocate(size);
  if (!buffer) {
    fail(manager, key, session, buffer.error());
    return std::nullopt;
  }
  input = HeldBuffer{core_->buffers, *buffer};
  const auto bytes = core_->buffers.data(*buffer);
  if (!bytes) {
    fail(manager, key, session, bytes.error());
    return std::nullopt;
  }
  const auto from =
      session.open_audio.begin() +
      static_cast<std::ptrdiff_t>((detection.submitted_end - session.open_start) * kInternalWidth);
  std::copy(from, from + static_cast<std::ptrdiff_t>(size), bytes->begin());
  auto request = JobRequest::create(
      JobIdentity{next_job_id_++, key, options_.generation}, JobClass::VadFrames,
      VadFrames{kJobInputAudioFormat, PcmWindow{*buffer, 0, size},
                static_cast<std::uint32_t>(frame_samples), frames, kVoiceActivityUtterance},
      JobOptions{});
  if (!request) {
    fail(manager, key, session, request.error());
    return std::nullopt;
  }
  detection.submitted_end += frames * frame_samples;
  return std::move(request).value();
}

std::span<const std::byte> SpeechJobDispatch::intake(Session& session,
                                                     std::span<const std::byte> frame) {
  session.input_samples += frame.size() / session.terms.audio_format.bytes_per_sample();
  // The one input format served is the internal one.
  return frame;
}

void SpeechJobDispatch::take_audio(SessionManager& manager, std::uint64_t key, Session& session,
                                   std::span<const std::byte> audio) {
  const std::uint64_t longest = longest_utterance(session.terms);
  while (!audio.empty() && !session.ended) {
    const std::uint64_t room = longest - (session.samples_accepted - session.open_start);
    const auto part = audio.first(std::min<std::uint64_t>(audio.size(), room * kInternalWidth));
    if (session.open_audio.empty()) {
      // At its longest from the start, so it never grows past the audio it may hold.
      session.open_audio.reserve(static_cast<std::size_t>(longest * kInternalWidth));
    }
    session.open_audio.insert(session.open_audio.end(), part.begin(), part.end());
    session.samples_accepted += part.size() / kInternalWidth;
    audio = audio.subspan(part.size());
    if (session.samples_accepted - session.open_start < longest) {
      continue;
    }
    if (!end_automatically(manager, key, session, session.open_start, session.samples_accepted,
                           EndpointReason::DurationLimit)) {
      return;
    }
  }
}

void SpeechJobDispatch::keep_audio(Session& session, std::span<const std::byte> audio,
                                   std::uint64_t frame_bytes) {
  if (session.open_audio.capacity() == 0) {
    // The longest utterance, and the unscored audio the input credit allows
    // past its last scored frame.
    session.open_audio.reserve(static_cast<std::size_t>(
        kInternalFormat.bytes_per_second() *
        (static_cast<std::uint64_t>(session.terms.max_utterance.count()) / 1000 +
         static_cast<std::uint64_t>(SessionBudgets::kInputAudioWindow.count()))));
  }
  session.open_audio.insert(session.open_audio.end(), audio.begin(), audio.end());
  session.samples_accepted += audio.size() / kInternalWidth;
  session.detection->held_credit.push_back({session.samples_accepted, frame_bytes});
}

bool SpeechJobDispatch::may_end_utterance(SessionManager& manager, std::uint64_t key,
                                          Session& session) {
  if (session.ended_utterances.size() >= kEndedUtterances) {
    fail(manager, key, session,
         Error::make(Error::Code::ResourceExhausted,
                     "audio arrived faster than its utterances were decoded",
                     "utterance_backlog_exceeded"));
    return false;
  }
  return true;
}

bool SpeechJobDispatch::end_utterance(SessionManager& manager, std::uint64_t key, Session& session,
                                      std::uint64_t first, std::uint64_t end, EndpointReason reason,
                                      std::uint64_t finalize_sequence) {
  const auto wire =
      to_wire(first, end, session.input_samples, session.terms.audio_format.sample_rate_hz);
  if (!wire) {
    fail(manager, key, session, wire.error());
    return false;
  }
  const auto begin = session.open_audio.begin();
  const auto from =
      begin + static_cast<std::ptrdiff_t>((first - session.open_start) * kInternalWidth);
  const auto to = begin + static_cast<std::ptrdiff_t>((end - session.open_start) * kInternalWidth);
  const std::uint64_t utterance_id = ++session.utterances;
  session.ended_utterances.push_back({utterance_id, first, end, *wire,
                                      std::vector<std::byte>(from, to), reason, finalize_sequence,
                                      std::nullopt});
  session.open_audio.erase(begin, to);
  session.open_start = end;
  session.ended_at = end;
  // Detected by the dispatch itself: the client learns of it before the final.
  if (session.detection) {
    session.detection->windows.close();
    session.detection->partials = {};
    session.held.push_back(make_task_output_item(
        EndpointDetectedOutput{utterance_id, wire->end_sample_offset, reason}));
  }
  return true;
}

bool SpeechJobDispatch::end_automatically(SessionManager& manager, std::uint64_t key,
                                          Session& session, std::uint64_t first, std::uint64_t end,
                                          EndpointReason reason) {
  if (!may_end_utterance(manager, key, session)) {
    return false;
  }
  // The sink shows the finalization this starts before the utterance's job
  // can end, so it is recorded from there.
  if (!manager.apply(key, Event::AutomaticEndpoint)) {
    session.ended = true;
    return false;
  }
  return end_utterance(manager, key, session, first, end, reason, 0);
}

void SpeechJobDispatch::complete_drain(SessionManager& manager, std::uint64_t key,
                                       Session& session) {
  // A Finalize whose frames are still to be scored is owed too.
  if (session.owed_finalize != 0 || session.undelivered != 0 ||
      (session.detection && !session.detection->pending.empty())) {
    return;
  }
  if (input_owed(manager, key, session.detection ? session.detection->held_credit.size() : 0)) {
    return;
  }
  // Nothing is accepted after a drain started, so nothing follows this.
  session.ended = true;
  (void)manager.apply(key, Event::DrainCompleted);
}

void SpeechJobDispatch::fail(SessionManager& manager, std::uint64_t key, Session& session,
                             Error cause) {
  session.ended = true;
  (void)manager.apply(key, Event::Fail, std::move(cause));
}

// The backend's reset ends every session it served, not only the job's; a
// session whose cancel still waits for its release ends as failed instead.
void SpeechJobDispatch::reset_every_session(SessionManager& manager, const Error& error) {
  backend_lost_ = true;
  for (auto& [key, session] : sessions_) {
    session.ended = true;
    (void)manager.apply(key, Event::BackendReset, error);
  }
}

void SpeechJobDispatch::fail_for_backend(SessionManager& manager, std::uint64_t key,
                                         Session& session, const Error& error) {
  session.ended = true;
  if (backend_was_reset(error)) {
    (void)manager.apply(key, Event::BackendReset, error);
    return;
  }
  Error cause = error;
  if (auto reason = refusal_reason(error)) {
    cause.context = std::move(reason);
  }
  (void)manager.apply(key, Event::Fail, std::move(cause));
}

void SpeechJobDispatch::pump(SessionManager& manager, std::uint64_t key, Session& session) {
  while (!session.ended) {
    if (!session.held.empty()) {
      if (!offer_held(manager, key, session)) {
        // With voice detection an endpoint waiting for room does not hold its
        // utterance's decode back; the finalization still waits for it.
        if (!session.ended && session.detection && !session.ended_utterances.empty() &&
            session.ended_utterances.front().needs_decode()) {
          want_job(key, session);
        }
        return;
      }
      continue;
    }
    const bool advanced = session.terms.input_kind == SessionInputKind::Audio
                              ? advance_audio(manager, key, session)
                              : advance_text(manager, key, session);
    if (!advanced) {
      return;
    }
  }
}

bool SpeechJobDispatch::offer_held(SessionManager& manager, std::uint64_t key, Session& session) {
  const auto offered = session.output->offer(session.held.front(), clock_.now());
  if (!offered) {
    fail(manager, key, session, offered.error());
    return false;
  }
  if (*offered == OutputOffer::Full) {
    return false;
  }
  const bool awaited = awaits_delivery(session.held.front().kind);
  session.held.pop_front();
  if (*offered == OutputOffer::Suppressed || *offered == OutputOffer::Closed) {
    session.ended = true;
    session.held.clear();
    return false;
  }
  session.undelivered += awaited ? 1 : 0;
  return true;
}

bool SpeechJobDispatch::complete_utterance(SessionManager& manager, std::uint64_t key,
                                           Session& session) {
  EndedUtterance& head = session.ended_utterances.front();
  if (head.needs_decode()) {
    want_job(key, session);
    return false;
  }
  const EndedUtterance done = std::move(head);
  session.ended_utterances.pop_front();
  // One completion answers every finalization the manager has merged, so it
  // waits for a client's Finalize that is still to be handed over, or whose
  // frames are still to be scored.
  if (session.finalizing && session.owed_finalize == 0 &&
      !(session.detection && !session.detection->pending.empty())) {
    const auto completed = manager.apply(key, Event::FinalizeCompleted);
    if (!completed) {
      session.ended = true;
      return false;
    }
    session.finalizing = false;
    if (completed->state == LogicalSessionState::Active) {
      ++session.completions_unseen;
    }
  }
  FinalTranscriptOutput transcript;
  transcript.utterance_id = done.utterance_id;
  transcript.end_sample_offset = done.wire.end_sample_offset;
  transcript.finalize_sequence = done.finalize_sequence;
  if (done.transcript.has_value() && !done.transcript->text.empty()) {
    transcript.segments.push_back(
        {done.wire.time.start_us, done.wire.time.end_us, done.transcript->text});
  }
  if (!session.detection) {
    session.held.push_back(make_task_output_item(
        EndpointDetectedOutput{done.utterance_id, done.wire.end_sample_offset, done.reason}));
  }
  session.held.push_back(make_task_output_item(std::move(transcript)));
  return true;
}

bool SpeechJobDispatch::advance_audio(SessionManager& manager, std::uint64_t key,
                                      Session& session) {
  if (!session.ended_utterances.empty()) {
    return complete_utterance(manager, key, session);
  }
  if (session.detection) {
    if (hold_partial(session) || advance_detection(manager, key, session)) {
      return true;
    }
    want_partial(key, session);
  }
  if (!session.draining || session.owed_finalize != 0) {
    return false;
  }
  if (session.close_open_utterance && session.samples_accepted > session.ended_at) {
    // A frame accepted before the half-close belongs to this utterance.
    if (input_owed(manager, key, session.detection ? session.detection->held_credit.size() : 0)) {
      return false;
    }
    if (!session.detection) {
      return end_utterance(manager, key, session, session.open_start, session.samples_accepted,
                           EndpointReason::HalfClose, 0);
    }
    if (session.detection->pending.empty()) {
      session.detection->pending.push_back(
          PendingEnd{session.samples_accepted, EndpointReason::HalfClose, 0});
      return true;
    }
    return false;
  }
  complete_drain(manager, key, session);
  return false;
}

bool SpeechJobDispatch::advance_detection(SessionManager& manager, std::uint64_t key,
                                          Session& session) {
  Detection& detection = *session.detection;
  // While draining, only what a Finalize or a half-close ends is scored.
  if (session.draining && detection.pending.empty()) {
    return false;
  }
  if (!detection.scores.empty()) {
    return observe_score(manager, key, session);
  }
  const std::uint64_t frame_samples = detection.policy.profile().frame_samples;
  if (!detection.pending.empty() &&
      detection.policy.next_frame_start() + frame_samples > detection.pending.front().end_sample) {
    return end_as_asked(manager, key, session);
  }
  const std::uint64_t until =
      detection.pending.empty() ? session.samples_accepted : detection.pending.front().end_sample;
  if (until - detection.submitted_end >= frame_samples) {
    want_vad(key, session);
  }
  return false;
}

bool SpeechJobDispatch::observe_score(SessionManager& manager, std::uint64_t key,
                                      Session& session) {
  Detection& detection = *session.detection;
  const float probability = detection.scores.front();
  detection.scores.pop_front();
  // Only an utterance in speech is split or ended.
  const std::uint64_t start = detection.policy.utterance_start().value_or(0);
  const auto outcome = detection.policy.observe(probability);
  if (!outcome) {
    fail(manager, key, session, outcome.error());
    return false;
  }
  if (outcome->duration_split) {
    if (!end_automatically(manager, key, session, start, *outcome->duration_split,
                           EndpointReason::DurationLimit)) {
      return false;
    }
    detection.windows.open(*outcome->duration_split, *outcome->duration_split);
  }
  if (outcome->onset) {
    detection.windows.open(*outcome->onset, *detection.policy.speech_start());
  }
  if (outcome->silence_endpoint) {
    const std::uint64_t end = *outcome->silence_endpoint;
    const std::uint64_t first = outcome->duration_split.value_or(start);
    // Only after a split can an utterance lie wholly inside the silence run
    // that ends it: it holds no speech, so it is not decoded.
    const bool silent = end - first <= detection.policy.silence_run_samples();
    if (!end_automatically(manager, key, session, silent ? end : first, end,
                           EndpointReason::Silence)) {
      return false;
    }
  }
  let_go(manager, key, session, detection.policy.next_frame_start());
  return !session.ended;
}

bool SpeechJobDispatch::end_as_asked(SessionManager& manager, std::uint64_t key, Session& session) {
  Detection& detection = *session.detection;
  const PendingEnd asked = detection.pending.front();
  const std::uint64_t longest = longest_utterance(session.terms);
  // In silence the utterance holds nothing: its final has no segments.
  std::uint64_t first = detection.policy.utterance_start().value_or(asked.end_sample);
  // The frame left unscored can take an utterance in speech past the longest.
  if (asked.end_sample - first > longest) {
    if (!end_automatically(manager, key, session, first, first + longest,
                           EndpointReason::DurationLimit)) {
      return false;
    }
    first += longest;
  }
  if (!may_end_utterance(manager, key, session)) {
    return false;
  }
  if (const auto reset = detection.policy.reset(asked.end_sample); !reset) {
    fail(manager, key, session, reset.error());
    return false;
  }
  detection.pending.pop_front();
  detection.submitted_end = asked.end_sample;
  if (!end_utterance(manager, key, session, first, asked.end_sample, asked.reason,
                     asked.finalize_sequence)) {
    return false;
  }
  let_go(manager, key, session, asked.end_sample);
  return !session.ended;
}

void SpeechJobDispatch::let_go(SessionManager& manager, std::uint64_t key, Session& session,
                               std::uint64_t scored) {
  Detection& detection = *session.detection;
  const std::uint64_t keep_from = detection.policy.earliest_utterance_start();
  if (keep_from > session.open_start) {
    session.open_audio.erase(
        session.open_audio.begin(),
        session.open_audio.begin() +
            static_cast<std::ptrdiff_t>((keep_from - session.open_start) * kInternalWidth));
    session.open_start = keep_from;
  }
  std::uint32_t frames = 0;
  std::uint64_t bytes = 0;
  while (!detection.held_credit.empty() && detection.held_credit.front().end_sample <= scored) {
    ++frames;
    bytes += detection.held_credit.front().bytes;
    detection.held_credit.pop_front();
  }
  if (frames != 0 && !manager.release_audio_input(key, frames, bytes)) {
    session.ended = true;
  }
}

bool SpeechJobDispatch::advance_text(SessionManager& manager, std::uint64_t key, Session& session) {
  if (session.active) {
    return advance_segment(manager, key, session, *session.active);
  }
  if (!session.waiting.empty()) {
    if (!manager.start_text_segment(key)) {
      session.ended = true;
      return false;
    }
    ActiveSegment next;
    next.segment_id = session.waiting.front().segment_id;
    next.text = std::move(session.waiting.front().text);
    session.waiting.pop_front();
    session.active = std::move(next);
    return true;
  }
  if (session.finalize_sequence) {
    const std::uint64_t sequence = *session.finalize_sequence;
    session.finalize_sequence.reset();
    if (!manager.apply(key, Event::FinalizeCompleted)) {
      session.ended = true;
      return false;
    }
    session.held.push_back(make_task_output_item(
        SynthesisCompletedOutput{session.segments_finished, session.samples_finished, sequence}));
    session.segments_finished = 0;
    session.samples_finished = 0;
    return true;
  }
  if (session.draining) {
    complete_drain(manager, key, session);
  }
  return false;
}

bool SpeechJobDispatch::advance_segment(SessionManager& manager, std::uint64_t key,
                                        Session& session, ActiveSegment& segment) {
  if (!segment.synthesized) {
    want_job(key, session);
    return false;
  }
  const std::uint64_t width = session.terms.audio_format.bytes_per_sample();
  if (segment.next_byte < segment.audio.bytes) {
    const auto pcm = core_->buffers.data(segment.audio.buffer.ref());
    if (!pcm) {
      fail(manager, key, session, pcm.error());
      return false;
    }
    const std::size_t chunk_bytes = session.terms.audio_format.bytes_per_second() *
                                    static_cast<std::uint64_t>(kAudioChunkDuration.count()) / 1000;
    const std::size_t count = std::min(chunk_bytes, segment.audio.bytes - segment.next_byte);
    const auto part = pcm->subspan(segment.audio.offset + segment.next_byte, count);
    AudioChunkOutput chunk{segment.segment_id, segment.next_chunk, segment.next_byte / width, {}};
    chunk.pcm.assign(part.begin(), part.end());
    segment.next_byte += count;
    ++segment.next_chunk;
    if (segment.next_byte == segment.audio.bytes) {
      segment.audio.buffer.reset();
    }
    session.held.push_back(make_task_output_item(std::move(chunk)));
    return true;
  }
  const std::uint64_t total_samples = segment.audio.bytes / width;
  if (!segment.completion_queued) {
    segment.completion_queued = true;
    session.held.push_back(make_task_output_item(
        SegmentCompletedOutput{segment.segment_id, total_samples, segment.next_chunk}));
    return true;
  }
  if (session.undelivered != 0) {
    return false;
  }
  // The active slot is held until the segment's output was delivered.
  ++session.segments_finished;
  session.samples_finished += total_samples;
  session.active.reset();
  if (!manager.finish_text_segment(key)) {
    session.ended = true;
    return false;
  }
  return true;
}
}  // namespace tensorplate::serving
