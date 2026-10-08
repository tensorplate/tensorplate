// SPDX-License-Identifier: Apache-2.0
#include "serving/session/synthetic_dispatch.hpp"

#include <algorithm>
#include <chrono>
#include <cstddef>
#include <utility>

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;

constexpr StreamAudioFormat kInputFormat{StreamAudioEncoding::PcmS16Le, 16'000, 1};
constexpr StreamAudioFormat kOutputFormat{StreamAudioEncoding::PcmS16Le, 24'000, 1};
constexpr std::uint32_t kMaxSegmentTextBytes = 4'096;
/// Two waiting segments and the one being delivered.
constexpr std::uint32_t kSynthesisSegments = 3;
static_assert(std::chrono::milliseconds{1000} * SyntheticSessionDispatch::kChunkSamples ==
              kAudioChunkDuration * kOutputFormat.sample_rate_hz);
constexpr std::uint64_t kMicrosPerSecond = 1'000'000;

/// A segment's interval is rounded outward: its start down, its end up.
std::uint64_t start_micros(std::uint64_t input_sample) {
  return input_sample * kMicrosPerSecond / kInputFormat.sample_rate_hz;
}

std::uint64_t end_micros(std::uint64_t input_sample) {
  const std::uint64_t rate = kInputFormat.sample_rate_hz;
  return (input_sample * kMicrosPerSecond + rate - 1) / rate;
}

bool task_output(OutputKind kind) {
  return kind == OutputKind::Audio || kind == OutputKind::Partial || kind == OutputKind::Result;
}

Error wrong_input_kind() {
  return Error::make(Error::Code::Internal, "input of a kind the session does not take",
                     "wrong_input_kind");
}
}  // namespace

SyntheticSessionDispatch::SyntheticSessionDispatch(const SchedulerClock& clock)
    : clock_(clock), worker_([this] { run(); }) {}

SyntheticSessionDispatch::~SyntheticSessionDispatch() {
  stop();
}

void SyntheticSessionDispatch::attach(SessionManager& manager) {
  {
    const std::lock_guard guard(mu_);
    manager_ = &manager;
  }
  cv_.notify_all();
}

void SyntheticSessionDispatch::stop() noexcept {
  {
    const std::lock_guard guard(mu_);
    stopping_ = true;
    commands_.clear();
  }
  cv_.notify_all();
  std::call_once(joined_, [this] { worker_.join(); });
}

Result<SessionTerms> SyntheticSessionDispatch::negotiate(const SessionRequest& request) const {
  SessionTerms terms;
  terms.input_kind = request.input_kind;
  terms.language = request.language;
  if (request.input_kind == SessionInputKind::Audio) {
    if (request.input_format != kInputFormat) {
      return unexpected(Error::make(Error::Code::Unsupported, "audio input format is not served",
                                    "unsupported_audio_format"));
    }
    terms.audio_format = kInputFormat;
    terms.min_frame = 20ms;
    terms.max_frame = 320ms;
    terms.max_utterance = 30s;
    return terms;
  }
  terms.voice = request.voice;
  terms.audio_format = kOutputFormat;
  terms.max_segment_text_bytes = kMaxSegmentTextBytes;
  terms.max_segment_audio = 30s;
  terms.max_synthesis_text_bytes = kSynthesisSegments * kMaxSegmentTextBytes;
  terms.max_synthesis_audio = kSynthesisSegments * terms.max_segment_audio;
  return terms;
}

Result<void> SyntheticSessionDispatch::open_session(std::uint64_t session_key,
                                                    const SessionTerms& terms) {
  post({session_key, Opened{terms}});
  return {};
}

void SyntheticSessionDispatch::on_transition(const ManagedSessionTransition& transition) {
  post({transition.session_key, Moved{transition.event, transition.effects, transition.output}});
}

void SyntheticSessionDispatch::audio(std::uint64_t session_key, std::span<const std::byte> frame) {
  post({session_key, AudioIn{frame.size()}});
}

void SyntheticSessionDispatch::text_segment(std::uint64_t session_key, std::uint64_t segment_id,
                                            std::string text) {
  post({session_key, TextIn{segment_id, text.size()}});
}

void SyntheticSessionDispatch::finalize(std::uint64_t session_key, std::uint64_t client_sequence) {
  post({session_key, FinalizeIn{client_sequence}});
}

void SyntheticSessionDispatch::output_delivered(std::uint64_t session_key, OutputKind kind) {
  post({session_key, Delivered{task_output(kind)}});
}

void SyntheticSessionDispatch::post(Command command) {
  {
    const std::lock_guard guard(mu_);
    if (stopping_) {
      return;
    }
    commands_.push_back(std::move(command));
  }
  cv_.notify_one();
}

void SyntheticSessionDispatch::run() {
  std::unique_lock lock(mu_);
  for (;;) {
    cv_.wait(lock, [&] { return stopping_ || (manager_ != nullptr && !commands_.empty()); });
    if (stopping_) {
      return;
    }
    Command command = std::move(commands_.front());
    commands_.pop_front();
    SessionManager& manager = *manager_;
    // The manager is called unlocked: its sink posts back here.
    lock.unlock();
    handle(manager, command);
    held_sessions_.store(sessions_.size());
    lock.lock();
  }
}

void SyntheticSessionDispatch::handle(SessionManager& manager, Command& command) {
  const std::uint64_t key = command.session_key;
  if (auto* opened = std::get_if<Opened>(&command.body)) {
    sessions_[key].terms = std::move(opened->terms);
    return;
  }
  if (const auto* transition = std::get_if<Moved>(&command.body)) {
    moved(manager, key, *transition);
  }
  const auto it = sessions_.find(key);
  if (it == sessions_.end()) {
    return;
  }
  Session& session = it->second;
  const bool takes_audio = session.terms.input_kind == SessionInputKind::Audio;
  if (const auto* frame = std::get_if<AudioIn>(&command.body)) {
    // On a session that takes text the credit refuses this as wrong_input_kind.
    if (!manager.release_audio_input(key, 1, frame->bytes)) {
      session.ended = true;
      return;
    }
    session.samples_accepted += frame->bytes / session.terms.audio_format.bytes_per_sample();
  } else if (const auto* segment = std::get_if<TextIn>(&command.body)) {
    if (takes_audio) {
      fail(manager, key, session, wrong_input_kind());
      return;
    }
    session.waiting.push_back(*segment);
  } else if (const auto* asked = std::get_if<FinalizeIn>(&command.body)) {
    session.owed_finalize -= std::min(session.owed_finalize, 1U);
    session.finalize_sequence = asked->client_sequence;
  } else if (const auto* delivered = std::get_if<Delivered>(&command.body)) {
    // Any delivery can make room for a held item; only task output counts.
    if (delivered->task_output && session.undelivered > 0) {
      --session.undelivered;
    }
  }
  pump(manager, key, session);
}

void SyntheticSessionDispatch::moved(SessionManager& manager, std::uint64_t key,
                                     const Moved& moved) {
  if (moved.effects.contains(Effect::RequestCleanup)) {
    sessions_.erase(key);
    (void)manager.apply(key, Event::ReleaseAcknowledged);
    return;
  }
  const auto it = sessions_.find(key);
  if (it == sessions_.end()) {
    return;
  }
  Session& session = it->second;
  session.output = moved.output;
  // An automatic endpoint starts a finalization too; only a client's
  // Finalize is handed over.
  if (moved.effects.contains(Effect::StartFinalize) && moved.event == Event::Finalize) {
    ++session.owed_finalize;
  }
  if (moved.effects.contains(Effect::StartDrain)) {
    session.draining = true;
    session.close_open_utterance = moved.event == Event::HalfClose;
  }
}

void SyntheticSessionDispatch::complete_drain(SessionManager& manager, std::uint64_t key,
                                              Session& session) {
  if (session.owed_finalize != 0 || session.undelivered != 0) {
    return;
  }
  // Accepted input that was not handed over yet, or is still being worked
  // on, keeps the manager's depth above zero.
  const auto status = manager.status(key);
  if (!status || status->input_queue_depth != 0) {
    return;
  }
  // Nothing is accepted after a drain started, so nothing follows this.
  session.ended = true;
  (void)manager.apply(key, Event::DrainCompleted);
}

void SyntheticSessionDispatch::fail(SessionManager& manager, std::uint64_t key, Session& session,
                                    Error cause) {
  session.ended = true;
  (void)manager.apply(key, Event::Fail, std::move(cause));
}

void SyntheticSessionDispatch::pump(SessionManager& manager, std::uint64_t key, Session& session) {
  // Nothing is produced before the first transition names the queue.
  while (session.output && !session.ended) {
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

bool SyntheticSessionDispatch::offer_held(SessionManager& manager, std::uint64_t key,
                                          Session& session) {
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

void SyntheticSessionDispatch::hold_final(Session& session, EndpointReason reason,
                                          std::uint64_t finalize_sequence) {
  FinalTranscriptOutput transcript;
  transcript.utterance_id = ++session.utterances;
  session.held.push_back(make_task_output_item(
      EndpointDetectedOutput{transcript.utterance_id, session.samples_accepted, reason}));
  transcript.end_sample_offset = session.samples_accepted;
  transcript.finalize_sequence = finalize_sequence;
  if (session.samples_accepted > session.utterance_start) {
    transcript.segments.push_back({start_micros(session.utterance_start),
                                   end_micros(session.samples_accepted), std::string{kTranscript}});
  }
  session.utterance_start = session.samples_accepted;
  session.held.push_back(make_task_output_item(std::move(transcript)));
}

bool SyntheticSessionDispatch::advance_audio(SessionManager& manager, std::uint64_t key,
                                             Session& session) {
  if (session.finalize_sequence) {
    const std::uint64_t sequence = *session.finalize_sequence;
    session.finalize_sequence.reset();
    if (!manager.apply(key, Event::FinalizeCompleted)) {
      session.ended = true;
      return false;
    }
    hold_final(session, EndpointReason::ClientFinalize, sequence);
    return true;
  }
  if (!session.draining || session.owed_finalize != 0) {
    return false;
  }
  if (session.close_open_utterance && session.samples_accepted > session.utterance_start) {
    hold_final(session, EndpointReason::HalfClose, 0);
    return true;
  }
  complete_drain(manager, key, session);
  return false;
}

bool SyntheticSessionDispatch::advance_text(SessionManager& manager, std::uint64_t key,
                                            Session& session) {
  if (session.active) {
    return advance_segment(manager, key, session);
  }
  if (!session.waiting.empty()) {
    if (!manager.start_text_segment(key)) {
      session.ended = true;
      return false;
    }
    const TextIn next = session.waiting.front();
    session.waiting.pop_front();
    session.active = ActiveSegment{next.segment_id, next.bytes * kSamplesPerTextByte};
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

bool SyntheticSessionDispatch::advance_segment(SessionManager& manager, std::uint64_t key,
                                               Session& session) {
  if (!session.active) {
    return false;
  }
  ActiveSegment& segment = *session.active;
  if (segment.next_sample < segment.total_samples) {
    const std::uint64_t count =
        std::min<std::uint64_t>(kChunkSamples, segment.total_samples - segment.next_sample);
    AudioChunkOutput chunk{segment.segment_id, segment.next_chunk, segment.next_sample, {}};
    chunk.pcm.reserve(count * kOutputFormat.bytes_per_sample());
    for (std::uint64_t index = segment.next_sample; index < segment.next_sample + count; ++index) {
      const auto sample = static_cast<std::uint16_t>(sample_at(index));
      chunk.pcm.push_back(static_cast<std::byte>(sample & 0xffU));
      chunk.pcm.push_back(static_cast<std::byte>(sample >> 8U));
    }
    segment.next_sample += count;
    ++segment.next_chunk;
    session.held.push_back(make_task_output_item(std::move(chunk)));
    return true;
  }
  if (!segment.completion_queued) {
    segment.completion_queued = true;
    session.held.push_back(make_task_output_item(
        SegmentCompletedOutput{segment.segment_id, segment.total_samples, segment.next_chunk}));
    return true;
  }
  if (session.undelivered != 0) {
    return false;
  }
  // The active slot is held until the segment's output was delivered.
  ++session.segments_finished;
  session.samples_finished += segment.total_samples;
  session.active.reset();
  if (!manager.finish_text_segment(key)) {
    session.ended = true;
    return false;
  }
  return true;
}
}  // namespace tensorplate::serving
