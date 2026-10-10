// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <span>
#include <string>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/core/result.hpp"

#include "serving/session/credits.hpp"
#include "serving/session/output_queue.hpp"
#include "serving/session/session_manager.hpp"

namespace tensorplate::serving {

enum class StreamAudioEncoding : std::uint8_t {
  PcmS16Le = 0,
  Mulaw = 1,
};

/// Raw audio with no container header, as a stream carries it.
struct StreamAudioFormat {
  StreamAudioEncoding encoding = StreamAudioEncoding::PcmS16Le;
  std::uint32_t sample_rate_hz = 0;
  std::uint32_t channels = 0;

  [[nodiscard]] constexpr std::uint32_t bytes_per_sample() const noexcept {
    return encoding == StreamAudioEncoding::PcmS16Le ? 2U : 1U;
  }
  /// The rate SessionBudgets is derived from.
  [[nodiscard]] constexpr std::uint64_t bytes_per_second() const noexcept {
    return std::uint64_t{sample_rate_hz} * channels * bytes_per_sample();
  }

  friend constexpr bool operator==(const StreamAudioFormat& lhs,
                                   const StreamAudioFormat& rhs) noexcept = default;
};

/// What a client asks of a session, before it is admitted.
struct SessionRequest {
  SessionInputKind input_kind = SessionInputKind::Audio;
  std::string language;
  /// Text input only.
  std::string voice;
  /// Audio input only: the format the client will send.
  StreamAudioFormat input_format;
};

/// What the deployment grants a session: the request as accepted, and the
/// format and limits the stream reports when the session is ready.
struct SessionTerms {
  SessionInputKind input_kind = SessionInputKind::Audio;
  std::string language;
  std::string voice;
  /// The audio the session carries: its input, or for text input its output.
  StreamAudioFormat audio_format;
  /// Audio input: the bounds of one frame and the longest utterance.
  std::chrono::milliseconds min_frame{0};
  std::chrono::milliseconds max_frame{0};
  std::chrono::milliseconds max_utterance{0};
  /// Text input: the largest segment and the longest audio a segment yields,
  /// then the same for the segments between one Finalize and the next.
  std::uint32_t max_segment_text_bytes = 0;
  std::chrono::milliseconds max_segment_audio{0};
  std::uint32_t max_synthesis_text_bytes = 0;
  std::chrono::milliseconds max_synthesis_audio{0};
};

/// The budgets of a session granted `terms`: SessionBudgets for its input
/// kind at the byte rate of its audio format.
[[nodiscard]] Result<SessionBudgets> budgets_for(const SessionTerms& terms);

/// What an audio chunk carries, the last of a segment possibly less.
inline constexpr std::chrono::milliseconds kAudioChunkDuration{20};

/// Why an utterance ended.
enum class EndpointReason : std::uint8_t {
  Silence = 0,
  ClientFinalize = 1,
  DurationLimit = 2,
  HalfClose = 3,
};

/// Precedes its utterance's final, once.
struct EndpointDetectedOutput {
  std::uint64_t utterance_id = 0;
  /// Exclusive end of the utterance's input, as in FinalTranscriptOutput.
  std::uint64_t end_sample_offset = 0;
  EndpointReason reason = EndpointReason::ClientFinalize;
};

struct TranscriptSegment {
  /// The segment's interval, rounded outward: the start down, the end up.
  std::uint64_t start_us = 0;
  std::uint64_t end_us = 0;
  std::string text;
};

/// The one final of an utterance. No segments means silence.
struct FinalTranscriptOutput {
  std::uint64_t utterance_id = 0;
  std::vector<TranscriptSegment> segments;
  /// Exclusive end of the utterance's input, in samples of the input format
  /// counted from the session's first.
  std::uint64_t end_sample_offset = 0;
  /// Client sequence of the Finalize this answers; zero when none asked.
  std::uint64_t finalize_sequence = 0;
};

/// Consecutive PCM of one synthesized segment, in the session's audio format.
struct AudioChunkOutput {
  /// The client's segment id, unchanged.
  std::uint64_t segment_id = 0;
  std::uint32_t chunk_index = 0;
  std::uint64_t segment_sample_offset = 0;
  std::vector<std::byte> pcm;
};

/// Follows a segment's last chunk.
struct SegmentCompletedOutput {
  std::uint64_t segment_id = 0;
  std::uint64_t total_samples = 0;
  std::uint32_t chunk_count = 0;
};

/// Answers a text session's Finalize, after the completion of every segment
/// accepted before it. A half-close without a Finalize gets none.
struct SynthesisCompletedOutput {
  /// Segments completed since the previous one, and their samples together.
  std::uint32_t segment_count = 0;
  std::uint64_t total_samples = 0;
  /// Client sequence of the Finalize this answers.
  std::uint64_t finalize_sequence = 0;
};

/// Will gain alternatives, a partial transcript first: a visitor over it
/// needs a default.
using TaskOutput = std::variant<EndpointDetectedOutput, FinalTranscriptOutput, AudioChunkOutput,
                                SegmentCompletedOutput, SynthesisCompletedOutput>;

/// Body of every task-output item a dispatch queues.
struct TaskOutputBody final : OutputBody {
  explicit TaskOutputBody(TaskOutput output_in) : output(std::move(output_in)) {}
  TaskOutput output;
};

/// What a Result item is charged against the metadata budget besides the
/// bytes of its text, in budget units: per message and per transcript
/// segment.
inline constexpr std::uint64_t kTaskOutputMessageUnits = 48;
inline constexpr std::uint64_t kTaskOutputSegmentUnits = 32;

/// The queue item that carries `output`: Audio charged its PCM bytes for a
/// chunk, Result for anything else.
[[nodiscard]] OutputItem make_task_output_item(TaskOutput output);

/// The task side of the streams of one deployment: it takes each session's
/// accepted input, runs the deployment's work on it and queues typed output.
/// One dispatch serves one worker process and so one deployment generation.
/// The transport binding drives the SessionManager, writes the lifecycle
/// messages and calls this interface; docs/architecture/serving-worker.md
/// ("Session dispatch") states the whole contract. In short:
///
/// - Calls. Every method but attach() is thread-safe, and every one but
///   attach() and stop() returns without waiting on a job, the output queue
///   or the manager; a call for a
///   session that was never opened here, whose cleanup was requested, or
///   after stop(), is ignored. Hand-overs return nothing: a dispatch that
///   cannot use one fails the session through the manager.
/// - Order. One session's hand-overs (audio(), text_segment(), finalize())
///   are made one at a time, in the order the manager accepted them; calls
///   for different sessions, and on_transition() or output_delivered() for
///   the same one, may run concurrently with them.
/// - Reports. A dispatch reports to the manager and offers output from
///   threads of its own, one report or offer at a time per session and in
///   order; nothing is promised across sessions. It applies
///   `finalize_completed`, `drain_completed`, `release_acknowledged`,
///   `automatic_endpoint`, `backend_reset`, `fail` and the credit releases,
///   and no other event.
/// - Output. It offers TaskOutputBody items only, and none after
///   `suppress_output`. An item answered `full` stays with the dispatch
///   until a delivery is reported.
class SessionDispatch {
 public:
  SessionDispatch() = default;
  SessionDispatch(const SessionDispatch&) = delete;
  SessionDispatch& operator=(const SessionDispatch&) = delete;
  virtual ~SessionDispatch() = default;

  /// The manager this dispatch reports to. Once, before the first session.
  virtual void attach(SessionManager& manager) = 0;

  /// Ends the dispatch and waits for the report in progress; none follows,
  /// and sessions still open keep their slots. Safe to repeat and to call
  /// while binding calls run. Never call it from the manager's sink or
  /// initialize hook or from a queue's consumer: they hold the manager's
  /// serialized section, which that report waits for, or run on the
  /// reporting thread itself. Destroy the manager after it and the dispatch
  /// last, since the manager's timer calls the sink until then.
  virtual void stop() noexcept = 0;

  /// What a session asking `request` would be granted. Keeps no state, so a
  /// refusal precedes admission.
  /// @return Unsupported with context "unsupported_input_kind",
  ///   "unsupported_audio_format", "unsupported_language" or
  ///   "unsupported_voice".
  [[nodiscard]] virtual Result<SessionTerms> negotiate(const SessionRequest& request) const = 0;

  /// Binds `session_key` to its terms. Call it from the initialize hook of
  /// SessionManager::open, where it only records: it refuses only what it
  /// can tell without waiting, and anything the backend decides fails the
  /// session later. A refusal fails the session; its cleanup is still
  /// answered here.
  [[nodiscard]] virtual Result<void> open_session(std::uint64_t session_key,
                                                  const SessionTerms& terms) = 0;

  /// Every transition the manager's sink receives, in that order, from the
  /// sink: on a transport thread, the manager's timer thread or a dispatch
  /// thread inside its own report. It only records. The sink runs inside
  /// the manager call that caused the transition, before that call returns.
  /// Credit returns (no event) arrive while the slot is held, in any state:
  /// a binding reports one only in active or finalizing, where no
  /// transition carries a status it must hold back.
  ///
  /// Audio input: a client's half-close finalizes the open utterance, if it
  /// holds audio, with an endpoint of its own reason, once every frame the
  /// manager accepted was handed over: those frames belong to it. A drain
  /// the worker starts completes only finalizations already asked: audio
  /// that was accepted and not finalized is not transcribed. Text input:
  /// every accepted segment is synthesized and delivered in either drain.
  /// Either way `drain_completed` is reported once no accepted input or
  /// Finalize is still to be handed over and the output of accepted work was
  /// delivered.
  ///
  /// A `request_cleanup` is answered by one `release_acknowledged` once the
  /// backend has released what the session held, also for a session never
  /// opened here; a backend that never confirms leaves the slot held.
  virtual void on_transition(const ManagedSessionTransition& transition) = 0;

  /// One audio frame, after SessionManager::accept_input, charged exactly
  /// `frame.size()`, returned `accept_input` for it. The binding has checked
  /// it against the terms; a frame shorter than `min_frame` is accepted, and
  /// more audio before a Finalize or half-close then fails the session, an
  /// automatic endpoint in between or not. The bytes are copied before the
  /// call returns. The frame's credit returns when it joins the utterance
  /// being collected, which the dispatch ends itself (`automatic_endpoint`,
  /// DurationLimit) at SessionTerms::max_utterance; what it collected stays
  /// readable for as long as a backend job may read it.
  virtual void audio(std::uint64_t session_key, std::span<const std::byte> frame) = 0;

  /// One text segment, after SessionManager::accept_input, charged exactly
  /// `text.size()`, returned `accept_input` for it. The binding has checked
  /// it against the terms and that `segment_id` is above every earlier one.
  /// Its waiting credit returns when its work starts, and its active slot
  /// when the last item of its output was delivered.
  virtual void text_segment(std::uint64_t session_key, std::uint64_t segment_id,
                            std::string text) = 0;

  /// A client Finalize, after SessionManager::apply returned `start_finalize`
  /// for it. Audio input: the utterance ends here and its final carries
  /// `client_sequence`. Text input: answered by a SynthesisCompletedOutput
  /// when every accepted segment's output was delivered. `finalize_completed`
  /// is reported before the answer is offered. A Finalize or an automatic
  /// endpoint applied while a finalization runs joins it: the manager counts
  /// one finalization, so exactly one `finalize_completed` is applied for
  /// all of them, and a second fails the session.
  virtual void finalize(std::uint64_t session_key, std::uint64_t client_sequence) = 0;

  /// The transport delivered an item of `kind`, task output or not: call it
  /// after every successful BoundedOutputQueue::delivered. A dispatch that
  /// was answered `full` offers again only from here, and no timer covers a
  /// call that is missed.
  virtual void output_delivered(std::uint64_t session_key, OutputKind kind) = 0;
};
}  // namespace tensorplate::serving
