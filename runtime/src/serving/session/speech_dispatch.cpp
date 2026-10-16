// SPDX-License-Identifier: Apache-2.0
#include "serving/session/speech_dispatch.hpp"

#include <algorithm>
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

constexpr StreamAudioFormat kInputFormat{StreamAudioEncoding::PcmS16Le, kJobInputSampleRateHz, 1};
constexpr StreamAudioFormat kOutputFormat{StreamAudioEncoding::PcmS16Le, kJobOutputSampleRateHz, 1};
/// The longest utterance and the longest audio of a segment: what one job's
/// input and one job's result may hold.
constexpr std::uint64_t kLongestSeconds = 30;
static_assert(kInputFormat.bytes_per_second() * kLongestSeconds == kMaxAudioFramesBytes);
static_assert(kOutputFormat.bytes_per_second() * kLongestSeconds == kMaxAudioChunkBytes);
/// The waiting segments and the one being delivered.
constexpr std::uint32_t kSynthesisSegments = SessionBudgets::kWaitingTextSegments + 1;
constexpr std::uint16_t kNormalSpeed = 1000;
/// The utterances of a session that ended and have no final yet: the one
/// being decoded and those behind it. One more fails the session.
constexpr std::size_t kEndedUtterances = 3;
constexpr std::uint64_t kMicrosPerSecond = 1'000'000;
/// The shortest timed wait for a bound not yet due: a clock moved by hand can
/// stand just short of one for as long as a test likes.
constexpr SchedulerClock::Duration kShortestWait = std::chrono::milliseconds{1};

/// A segment's interval is rounded outward: its start down, its end up.
std::uint64_t start_micros(std::uint64_t sample, std::uint64_t rate) {
  return sample * kMicrosPerSecond / rate;
}

std::uint64_t end_micros(std::uint64_t sample, std::uint64_t rate) {
  return (sample * kMicrosPerSecond + rate - 1) / rate;
}

bool task_output(OutputKind kind) {
  return kind == OutputKind::Audio || kind == OutputKind::Partial || kind == OutputKind::Result;
}

Error not_served(const char* message, const char* context) {
  return Error::make(Error::Code::Unsupported, message, context);
}

/// Input the manager accepted that was not handed over yet, or is still
/// being worked on, keeps its depth above zero.
bool input_owed(const SessionManager& manager, std::uint64_t key) {
  const auto status = manager.status(key);
  return !status || status->input_queue_depth != 0;
}

Error wrong_input_kind() {
  return Error::make(Error::Code::Internal, "input of a kind the session does not take",
                     "wrong_input_kind");
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
  std::variant<TranscriptResult, SpeechAudio, Error> outcome;
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
    bool task_output = false;
  };
  struct SessionReleased {};

  std::uint64_t session_key = 0;
  std::variant<Opened, Moved, AudioIn, WaitingSegment, FinalizeIn, Delivered, JobEnded,
               SessionReleased>
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
  void on_released(const JobEvent& event) noexcept override {
    (void)take_input(event.identity().job_id);
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
    if (transcript != nullptr) {
      ended.outcome = *transcript;
    } else if (chunk != nullptr) {
      ended.outcome = SpeechAudio{HeldBuffer{buffers, chunk->pcm.buffer}, chunk->pcm.byte_offset,
                                  chunk->pcm.byte_size};
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
  auto core = std::make_shared<Core>(buffers);
  std::unique_ptr<SpeechJobDispatch> dispatch;
  try {
    dispatch = std::unique_ptr<SpeechJobDispatch>(
        new SpeechJobDispatch(std::move(bridge), core, clock, std::move(options)));
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
                                     SpeechDispatchOptions options)
    : bridge_(std::move(bridge)),
      core_(std::move(core)),
      clock_(clock),
      options_(std::move(options)),
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
    if (in_flight_) {
      const auto owner = sessions_.find(in_flight_->session_key);
      if (owner != sessions_.end() && owner->second.release_refused) {
        (void)bridge_->cancel(in_flight_->job_id);
      }
    }
    sessions_.clear();
    lane_.clear();
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
  core_->post({session_key, Command::Delivered{task_output(kind)}});
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
    sessions_[key].terms = std::move(opened->terms);
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
    // On a session that takes text the credit refuses this as wrong_input_kind.
    if (manager.release_audio_input(key, 1, input->frame.size())) {
      take_audio(manager, key, session, input->frame);
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
    } else if (may_end_utterance(manager, key, session)) {
      end_utterance(session, EndpointReason::ClientFinalize, asked->client_sequence);
    }
  } else if (const auto* delivered = std::get_if<Command::Delivered>(&command.body)) {
    // Any delivery can make room for a held item; only task output counts.
    session.undelivered -= delivered->task_output ? 1 : 0;
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
  session.open_audio = {};
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
  if (!in_flight_ || in_flight_->job_id != ended.job_id) {
    ++stats_.jobs_discarded;
    return;
  }
  // The lane is free whatever became of the job's session.
  in_flight_.reset();
  const auto it = sessions_.find(key);
  const auto* error = std::get_if<Error>(&ended.outcome);
  const bool served = it != sessions_.end() && !it->second.ended;
  if (error != nullptr && backend_was_reset(*error)) {
    ++(served ? stats_.jobs_failed : stats_.jobs_discarded);
    reset_every_session(manager, *error);
    return;
  }
  if (!served) {
    ++stats_.jobs_discarded;
    return;
  }
  Session& session = it->second;
  session.job = JobState::None;
  if (error != nullptr) {
    ++stats_.jobs_failed;
    fail_for_backend(manager, key, session, *error);
    return;
  }
  ++stats_.jobs_completed;
  if (auto* transcript = std::get_if<TranscriptResult>(&ended.outcome);
      transcript != nullptr && !session.ended_utterances.empty()) {
    session.ended_utterances.front().transcript = std::move(*transcript);
    return;
  }
  if (auto* speech = std::get_if<SpeechAudio>(&ended.outcome);
      speech != nullptr && session.active && !session.active->synthesized) {
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

void SpeechJobDispatch::check_bounds(SessionManager& manager) {
  const auto now = clock_.now();
  if (in_flight_ && !in_flight_->past_deadline && now >= in_flight_->deadline) {
    in_flight_->past_deadline = true;
    ++stats_.jobs_past_deadline;
    const std::uint64_t key = in_flight_->session_key;
    if (const auto it = sessions_.find(key); it != sessions_.end() && !it->second.ended) {
      fail(manager, key, it->second,
           Error::make(Error::Code::Timeout, "the session's job did not end within its deadline",
                       "deadline_missed"));
    }
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
  for (const auto& [key, session] : sessions_) {
    if (session.releasing && !session.past_bound) {
      consider(session.release_bound_at);
    }
  }
  return next;
}

SchedulerClock::TimePoint SpeechJobDispatch::release_due(std::uint64_t key,
                                                         const Session& session) const {
  // A release cancels the session's job, which ends only when its work does.
  const bool job_in_flight = in_flight_ && in_flight_->session_key == key;
  return std::max(session.release_asked,
                  job_in_flight ? in_flight_->deadline : SchedulerClock::TimePoint{}) +
         options_.release_bound;
}

SchedulerClock::Duration SpeechJobDispatch::job_deadline(const JobRequest& request,
                                                         const Session& session) const {
  // A synthesis's audio is known only once it has run: its granted maximum.
  const StreamAudioFormat& format = session.terms.audio_format;
  const auto* audio = std::get_if<AudioFrames>(&request.payload());
  const std::uint64_t bytes =
      audio != nullptr ? audio->pcm.byte_size
                       : static_cast<std::uint64_t>(session.terms.max_segment_audio.count()) *
                             format.bytes_per_second() / 1000;
  return options_.job_deadline_base +
         per_audio(options_.job_deadline_per_audio_second, bytes, format.bytes_per_second());
}

void SpeechJobDispatch::want_job(std::uint64_t key, Session& session) {
  if (session.job == JobState::None) {
    session.job = JobState::Queued;
    lane_.push_back(key);
  }
}

void SpeechJobDispatch::start_next(SessionManager& manager) {
  while (!in_flight_ && !lane_.empty()) {
    const std::uint64_t key = lane_.front();
    lane_.pop_front();
    const auto it = sessions_.find(key);
    if (it == sessions_.end() || it->second.ended) {
      continue;
    }
    Session& session = it->second;
    session.job = JobState::None;
    HeldBuffer input;
    auto request = next_request(manager, key, session, input);
    if (!request) {
      continue;
    }
    const std::uint64_t job_id = request->identity().job_id;
    // Recorded before the submit: the job's events may overtake its return.
    core_->hold_input(job_id, std::move(input));
    in_flight_ = InFlight{job_id, key, clock_.now() + job_deadline(*request, session)};
    session.job = JobState::Running;
    const auto submitted = bridge_->submit(std::move(request).value());
    if (!submitted) {
      // No event follows a refused submit, `released` included.
      (void)core_->take_input(job_id);
      in_flight_.reset();
      session.job = JobState::None;
      fail_for_backend(manager, key, session, submitted.error());
      continue;
    }
    ++stats_.jobs_submitted;
  }
}

std::optional<JobRequest> SpeechJobDispatch::next_request(SessionManager& manager,
                                                          std::uint64_t key, Session& session,
                                                          HeldBuffer& input) {
  const JobIdentity identity{next_job_id_++, key, options_.generation};
  if (session.active) {
    auto synthesis =
        JobRequest::create(identity, JobClass::TtsSynthesis, TextSegment{session.active->text},
                           JobOptions{session.terms.language, session.terms.voice, kNormalSpeed});
    if (!synthesis) {
      fail(manager, key, session, synthesis.error());
      return std::nullopt;
    }
    return std::move(synthesis).value();
  }
  EndedUtterance& utterance = session.ended_utterances.front();
  const auto buffer = core_->buffers.allocate(utterance.audio.size());
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
  std::copy(utterance.audio.begin(), utterance.audio.end(), bytes->begin());
  const std::size_t size = utterance.audio.size();
  // The buffer is what stays readable for the job from here on.
  utterance.audio = {};
  JobOptions options;
  options.language = session.terms.language;
  auto decode = JobRequest::create(
      identity, JobClass::SttDecode,
      AudioFrames{kJobInputAudioFormat, PcmWindow{*buffer, 0, size}, utterance.first_sample},
      std::move(options));
  if (!decode) {
    fail(manager, key, session, decode.error());
    return std::nullopt;
  }
  return std::move(decode).value();
}

void SpeechJobDispatch::take_audio(SessionManager& manager, std::uint64_t key, Session& session,
                                   std::span<const std::byte> frame) {
  const std::uint64_t width = session.terms.audio_format.bytes_per_sample();
  const std::uint64_t longest = static_cast<std::uint64_t>(session.terms.max_utterance.count()) *
                                session.terms.audio_format.sample_rate_hz / 1000;
  while (!frame.empty() && !session.ended) {
    const std::uint64_t room = longest - (session.samples_accepted - session.utterance_start);
    const auto part = frame.first(std::min<std::uint64_t>(frame.size(), room * width));
    if (session.open_audio.empty()) {
      // At its longest from the start, so it never grows past the audio it may hold.
      session.open_audio.reserve(static_cast<std::size_t>(longest * width));
    }
    session.open_audio.insert(session.open_audio.end(), part.begin(), part.end());
    session.samples_accepted += part.size() / width;
    frame = frame.subspan(part.size());
    if (session.samples_accepted - session.utterance_start < longest) {
      continue;
    }
    if (!may_end_utterance(manager, key, session)) {
      return;
    }
    // The sink shows the finalization this starts before the utterance's job
    // can end, so it is recorded from there.
    if (!manager.apply(key, Event::AutomaticEndpoint)) {
      session.ended = true;
      return;
    }
    end_utterance(session, EndpointReason::DurationLimit, 0);
  }
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

void SpeechJobDispatch::end_utterance(Session& session, EndpointReason reason,
                                      std::uint64_t finalize_sequence) {
  session.ended_utterances.push_back({session.utterance_start, session.samples_accepted,
                                      std::move(session.open_audio), reason, finalize_sequence,
                                      std::nullopt});
  session.utterance_start = session.samples_accepted;
  session.open_audio = {};
}

void SpeechJobDispatch::complete_drain(SessionManager& manager, std::uint64_t key,
                                       Session& session) {
  if (session.owed_finalize != 0 || session.undelivered != 0) {
    return;
  }
  if (input_owed(manager, key)) {
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
  for (auto& [key, session] : sessions_) {
    session.ended = true;
    (void)manager.apply(key, Event::BackendReset, error);
  }
}

void SpeechJobDispatch::fail_for_backend(SessionManager& manager, std::uint64_t key,
                                         Session& session, const Error& error) {
  const std::string_view reason = error.context ? std::string_view{*error.context} : "";
  session.ended = true;
  if (backend_was_reset(error)) {
    (void)manager.apply(key, Event::BackendReset, error);
    return;
  }
  Error cause = error;
  if (reason == "session_unloaded") {
    cause.context = "backend_unavailable";
  } else if (reason == "job_class_unsupported" || reason == "job_not_permitted") {
    cause.context = "backend_unsupported_capability";
  }
  (void)manager.apply(key, Event::Fail, std::move(cause));
}

void SpeechJobDispatch::pump(SessionManager& manager, std::uint64_t key, Session& session) {
  while (!session.ended) {
    if (!session.held.empty()) {
      if (!offer_held(manager, key, session)) {
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
  session.held.pop_front();
  if (*offered == OutputOffer::Suppressed || *offered == OutputOffer::Closed) {
    session.ended = true;
    session.held.clear();
    return false;
  }
  ++session.undelivered;
  return true;
}

bool SpeechJobDispatch::complete_utterance(SessionManager& manager, std::uint64_t key,
                                           Session& session) {
  EndedUtterance& head = session.ended_utterances.front();
  // An utterance without audio runs no job: a request could not carry it.
  if (head.end_sample > head.first_sample && !head.transcript.has_value()) {
    want_job(key, session);
    return false;
  }
  const EndedUtterance done = std::move(head);
  session.ended_utterances.pop_front();
  // One completion answers every finalization the manager has merged, so it
  // waits for a client's Finalize that is still to be handed over.
  if (session.finalizing && session.owed_finalize == 0) {
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
  transcript.utterance_id = ++session.utterances;
  transcript.end_sample_offset = done.end_sample;
  transcript.finalize_sequence = done.finalize_sequence;
  if (done.transcript.has_value() && !done.transcript->text.empty()) {
    const std::uint64_t rate = session.terms.audio_format.sample_rate_hz;
    transcript.segments.push_back({start_micros(done.first_sample, rate),
                                   end_micros(done.end_sample, rate), done.transcript->text});
  }
  session.held.push_back(make_task_output_item(
      EndpointDetectedOutput{transcript.utterance_id, done.end_sample, done.reason}));
  session.held.push_back(make_task_output_item(std::move(transcript)));
  return true;
}

bool SpeechJobDispatch::advance_audio(SessionManager& manager, std::uint64_t key,
                                      Session& session) {
  if (!session.ended_utterances.empty()) {
    return complete_utterance(manager, key, session);
  }
  if (!session.draining || session.owed_finalize != 0) {
    return false;
  }
  if (session.close_open_utterance && session.samples_accepted > session.utterance_start) {
    // A frame accepted before the half-close belongs to this utterance.
    if (input_owed(manager, key)) {
      return false;
    }
    end_utterance(session, EndpointReason::HalfClose, 0);
    return true;
  }
  complete_drain(manager, key, session);
  return false;
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
