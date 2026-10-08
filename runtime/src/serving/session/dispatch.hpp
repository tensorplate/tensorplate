// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <functional>
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
  /// Text input: the largest segment and the longest audio a segment yields.
  std::uint32_t max_segment_text_bytes = 0;
  std::chrono::milliseconds max_segment_audio{0};
};

struct TranscriptSegment {
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

using TaskOutput = std::variant<FinalTranscriptOutput, AudioChunkOutput, SegmentCompletedOutput>;

/// Body of every task-output item a dispatch queues.
struct TaskOutputBody final : OutputBody {
  explicit TaskOutputBody(TaskOutput output_in) : output(std::move(output_in)) {}
  TaskOutput output;
};

/// What a Result item is charged besides its text: per message and per
/// transcript segment, bounds on what the fields around the text encode to.
inline constexpr std::uint64_t kTaskOutputMessageBytes = 48;
inline constexpr std::uint64_t kTaskOutputSegmentBytes = 32;

/// The queue item that carries `output`: Audio charged its PCM bytes for a
/// chunk, Result for anything else.
[[nodiscard]] OutputItem make_task_output_item(TaskOutput output);

/// The task side of the streams of one deployment: it takes each session's
/// accepted input, runs the deployment's work on it and queues typed output.
/// The transport binding drives the SessionManager, writes the lifecycle
/// messages and calls this interface; docs/architecture/serving-worker.md
/// ("Session dispatch") has the division of work and of ownership.
///
/// Except attach() and stop(), every method is thread-safe and returns
/// without waiting on a job, the output queue or the manager.
class SessionDispatch {
 public:
  SessionDispatch() = default;
  SessionDispatch(const SessionDispatch&) = delete;
  SessionDispatch& operator=(const SessionDispatch&) = delete;
  virtual ~SessionDispatch() = default;

  /// Called on the dispatch's thread, with the status the manager returned,
  /// after each report that returned input credit or completed a
  /// finalization: the manager's sink is not called for those. It may call
  /// the manager and the output queue, and must not block.
  using StatusSink =
      std::function<void(std::uint64_t session_key, const LogicalSessionStatus& status)>;

  /// The manager this dispatch reports to and where it announces what the
  /// manager's sink does not. Once, before the first session.
  virtual void attach(SessionManager& manager, StatusSink on_status) = 0;

  /// Ends the dispatch and waits for the report in progress; none follows.
  /// Call it before the manager is destroyed, never from the manager's sink
  /// or initialize hook. Sessions still open are not released. It may be
  /// repeated, also concurrently.
  virtual void stop() noexcept = 0;

  /// What a session asking `request` would be granted. Keeps no state, so a
  /// refusal precedes admission.
  /// @return Unsupported with context "unsupported_audio_format" or
  ///   "unsupported_input_kind" when the deployment does not serve it.
  [[nodiscard]] virtual Result<SessionTerms> negotiate(const SessionRequest& request) const = 0;

  /// Binds `session_key` to its terms. Call it from the initialize hook of
  /// SessionManager::open, where it only records. A failure fails the
  /// session; its cleanup request is still acknowledged here.
  [[nodiscard]] virtual Result<void> open_session(std::uint64_t session_key,
                                                  const SessionTerms& terms) = 0;

  /// Every transition the manager's sink receives, in that order, from the
  /// sink, where it only records. Each `accept_input`, and each
  /// `start_finalize` a client Finalize caused, is a hand-over the binding
  /// owes below; a drain does not complete while one is owed. `start_drain`
  /// completes accepted work, an open utterance included, and reports
  /// `drain_completed` once its output was delivered. Every
  /// `request_cleanup` is answered by exactly one `release_acknowledged`,
  /// whether or not the session was opened here, and the session is
  /// forgotten with what it held.
  virtual void on_transition(const ManagedSessionTransition& transition) = 0;

  /// One audio frame, after SessionManager::accept_input returned
  /// `accept_input` for it. The binding has checked it against the terms.
  /// The frame is the dispatch's from here: its credit returns when it joins
  /// the utterance being collected, which the dispatch keeps within
  /// SessionTerms::max_utterance, and it is dropped with that utterance's
  /// result.
  virtual void audio(std::uint64_t session_key, std::vector<std::byte> frame) = 0;

  /// One text segment, after SessionManager::accept_input returned
  /// `accept_input` for it. The binding has checked it against the terms.
  /// Its waiting credit returns when its work starts, and its active slot
  /// when the last item of its output was delivered.
  virtual void text_segment(std::uint64_t session_key, std::uint64_t segment_id,
                            std::string text) = 0;

  /// A client Finalize, after SessionManager::apply returned `start_finalize`
  /// for it. Audio input: the utterance ends here and its final carries
  /// `client_sequence`. Text input: completes when every accepted segment's
  /// output was delivered. `finalize_completed` is reported before the
  /// answering output is offered.
  virtual void finalize(std::uint64_t session_key, std::uint64_t client_sequence) = 0;

  /// The transport delivered an item of `kind`, task output or not: call it
  /// after each successful BoundedOutputQueue::delivered. A dispatch that
  /// was answered `full` offers again from here.
  virtual void output_delivered(std::uint64_t session_key, OutputKind kind) = 0;
};
}  // namespace tensorplate::serving
