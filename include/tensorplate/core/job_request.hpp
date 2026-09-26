// SPDX-License-Identifier: Apache-2.0
//
// Typed backend job requests.
//
// A job is one bounded unit of backend work submitted through a
// BoundedJobBridge (tensorplate/backend/bounded_job_bridge.hpp) beside, never
// through, ExecutionSession::infer. A JobRequest carries the job's identity,
// its job class, the options the class needs, a per-job progress limit and
// exactly one typed payload. The job class fixes the payload type and the
// result type (tensorplate/core/job_result.hpp):
//
//   job class        payload        result
//   stt_decode       AudioFrames    TranscriptResult
//   tts_synthesis    TextSegment    AudioChunkResult
//   vad_frames       VadFrames      VadResult
//
// PCM rides in a byte window of a BufferRef; the buffer plane keeps owning
// its lifetime. The submitter keeps every buffer a request names readable
// until the job's `released` event (tensorplate/core/job_event.hpp).
//
// The k* limits are frozen ceilings. Each bounds one payload type, so it
// applies to every job of that type whatever profile runs it; a profile may
// admit less (a streaming decode window, a VAD frame count) and refuses the
// rest. protocol/fixtures/job_seam.json repeats every ceiling and stable name
// below together with the validation vectors. The C++ tests replay it, and so
// does every other implementation of these objects; a change edits the header
// and the vectors in the same commit.
//
// Validation failures return Error::Code::ConfigInvalid with a stable
// snake_case reason in Error::context. Enum values only ever append; names
// never change. No vendor SDK type appears in this header.

#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <variant>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

// -- Frozen ceilings. --------------------------------------------------------

/// Sample rate of PCM a job receives: the internal audio timeline that the
/// serving layer decodes and resamples every accepted input onto (mu-law
/// telephony input included) before any job exists.
inline constexpr std::uint32_t kJobInputSampleRateHz = 16'000;

/// Sample rate of PCM a job returns (synthesized speech).
inline constexpr std::uint32_t kJobOutputSampleRateHz = 24'000;

/// Largest AudioFrames window in bytes: the 30-second maximum utterance at
/// kJobInputSampleRateHz, 16-bit mono (30 * 16,000 * 2). A streaming
/// deployment may admit only a shorter rolling window.
inline constexpr std::size_t kMaxAudioFramesBytes = 960'000;

/// Largest TextSegment in UTF-8 bytes: one complete client segment.
inline constexpr std::size_t kMaxTextSegmentBytes = 4'096;

/// Most frames in one VadFrames payload (K). The profile's frame shape bounds
/// the real count through kMaxVadFramesBytes: 32 frames of 512 samples fill
/// it, and one 320 ms network frame, with the samples carried over from the
/// previous one, yields at most 10 such frames.
inline constexpr std::uint32_t kMaxVadFramesPerJob = 32;

/// Largest VadFrames window in bytes: 1.024 seconds at kJobInputSampleRateHz,
/// 16-bit mono. One second of input (32,000 bytes) fits, so a serving layer
/// that holds at most one second of unconsumed input can coalesce all of it
/// into one job.
inline constexpr std::size_t kMaxVadFramesBytes = 32'768;

/// Ceiling of JobRequest::progress_limit(): a 30-second synthesized segment
/// has 1,500 delivery chunks of 20 ms, the last of which rides in
/// `completed`; the ceiling also covers one progress event per transcript
/// token (448) or per VAD frame (32).
inline constexpr std::uint32_t kMaxJobProgressEvents = 1'500;

/// Longest JobOptions::language, in bytes.
inline constexpr std::size_t kMaxLanguageTagBytes = 16;

/// Longest JobOptions::voice, in bytes.
inline constexpr std::size_t kMaxVoiceIdBytes = 64;

// -- Names and formats. ------------------------------------------------------

/// PCM sample encoding. Stable names in parentheses.
enum class AudioEncoding : std::uint8_t {
  /// (`pcm_s16le`) Signed 16-bit little-endian samples.
  PcmS16Le = 0,
};

/// Stable snake_case name of `encoding`; "unknown" for a value that is not an
/// enumerator.
[[nodiscard]] std::string_view to_string(AudioEncoding encoding) noexcept;

/// Inverse of to_string(AudioEncoding): std::nullopt for any other text.
[[nodiscard]] std::optional<AudioEncoding> audio_encoding_from_string(
    std::string_view name) noexcept;

/// Format of a PCM payload.
struct AudioFormat {
  AudioEncoding encoding = AudioEncoding::PcmS16Le;
  std::uint32_t sample_rate_hz = 0;
  std::uint16_t channels = 0;

  friend bool operator==(const AudioFormat& lhs, const AudioFormat& rhs) noexcept = default;
};

/// The only format AudioFrames and VadFrames accept: mono pcm_s16le at
/// kJobInputSampleRateHz.
inline constexpr AudioFormat kJobInputAudioFormat{AudioEncoding::PcmS16Le, kJobInputSampleRateHz,
                                                  1};

/// The only format AudioChunkResult accepts: mono pcm_s16le at
/// kJobOutputSampleRateHz.
inline constexpr AudioFormat kJobOutputAudioFormat{AudioEncoding::PcmS16Le, kJobOutputSampleRateHz,
                                                   1};

/// Class of a job. Fixes the payload type, the options and the result type.
/// Stable names in parentheses.
enum class JobClass : std::uint8_t {
  /// (`stt_decode`) Decode one audio window: AudioFrames -> TranscriptResult.
  SttDecode = 0,
  /// (`tts_synthesis`) Synthesize one text segment: TextSegment ->
  /// AudioChunkResult.
  TtsSynthesis = 1,
  /// (`vad_frames`) Voice-activity probability of each of 1..K consecutive
  /// frames: VadFrames -> VadResult.
  VadFrames = 2,
};

/// Stable snake_case name of `job_class`; "unknown" for a value that is not
/// an enumerator.
[[nodiscard]] std::string_view to_string(JobClass job_class) noexcept;

/// Inverse of to_string(JobClass): std::nullopt for any other text,
/// including differently cased or padded names.
[[nodiscard]] std::optional<JobClass> job_class_from_string(std::string_view name) noexcept;

/// Identity of one job, carried by its request and copied onto every event
/// the job produces. Every field is nonzero.
///
///   - job_id: assigned by the submitter; never given to two unreleased jobs
///     of one bridge.
///   - session_key: the serving layer's opaque key for the logical session
///     that owns the job; it keys session-scoped backend state and is not a
///     client-visible identifier.
///   - generation: the deployment generation that session is bound to.
struct JobIdentity {
  std::uint64_t job_id = 0;
  std::uint64_t session_key = 0;
  std::uint64_t generation = 0;

  friend bool operator==(const JobIdentity& lhs, const JobIdentity& rhs) noexcept = default;
};

/// A byte window of PCM inside one buffer: [byte_offset, byte_offset +
/// byte_size) of `buffer`. Validated by the factory of the object that holds
/// it: the buffer is not released, the window is non-empty, inside the buffer
/// and holds whole 16-bit samples (offset and size even).
struct PcmWindow {
  BufferRef buffer;
  std::size_t byte_offset = 0;
  std::size_t byte_size = 0;

  friend bool operator==(const PcmWindow& lhs, const PcmWindow& rhs) noexcept = default;
};

// -- Payloads. ---------------------------------------------------------------

/// Payload of `stt_decode`: one window of the session's audio in
/// kJobInputAudioFormat, at most kMaxAudioFramesBytes.
struct AudioFrames {
  AudioFormat format;
  PcmWindow pcm;
  /// Index of the window's first sample on the session's internal timeline
  /// (kJobInputSampleRateHz, origin at the first accepted input sample).
  /// Word units in the result use the same timeline.
  std::uint64_t start_sample = 0;

  friend bool operator==(const AudioFrames& lhs, const AudioFrames& rhs) noexcept = default;
};

/// Payload of `tts_synthesis`: one complete text segment of
/// 1..kMaxTextSegmentBytes bytes of well-formed UTF-8.
struct TextSegment {
  std::string text;

  friend bool operator==(const TextSegment& lhs, const TextSegment& rhs) noexcept = default;
};

/// Payload of `vad_frames`: 1..kMaxVadFramesPerJob consecutive frames of one
/// utterance, back to back in kJobInputAudioFormat. The window holds exactly
/// frame_count * frame_samples samples and at most kMaxVadFramesBytes.
///
/// A backend keeps voice-activity state per session key and utterance and
/// runs a session's VadFrames jobs in submission order. The first job naming
/// a new utterance_id for a session starts from reset state and discards the
/// state of that session's previous utterance; later jobs of the utterance
/// continue its state. After a job of an utterance ends without `completed`,
/// the submitter starts a new utterance.
struct VadFrames {
  AudioFormat format;
  PcmWindow pcm;
  /// Samples per frame, fixed by the profile's voice-activity model.
  std::uint32_t frame_samples = 0;
  /// Frames in the window: 1..kMaxVadFramesPerJob.
  std::uint32_t frame_count = 0;
  /// Nonzero utterance key within the session, assigned by the serving
  /// layer.
  std::uint64_t utterance_id = 0;

  friend bool operator==(const VadFrames& lhs, const VadFrames& rhs) noexcept = default;
};

/// Exactly one typed payload, in JobClass order.
using JobPayload = std::variant<AudioFrames, TextSegment, VadFrames>;

/// Selections the logical session made at open that a backend needs to run
/// the job, so that it never detects, guesses or defaults them. A backend
/// refuses a value its loaded profile does not permit. Every field has a
/// default, so designated initializers may name only the fields a class
/// takes, as in JobOptions{.language = "en"}.
///
///   field        stt_decode   tts_synthesis   vad_frames
///   language     required     required        empty
///   voice        empty        required        empty
///   speed_milli  0            required        0
struct JobOptions {
  /// Language tag such as "en", "ar" or "en-US": 2 or 3 ASCII letters, then
  /// zero or more subtags of "-" and 1..8 ASCII letters or digits; at most
  /// kMaxLanguageTagBytes bytes.
  std::string language{};
  /// Voice identifier such as "af_heart": 1..kMaxVoiceIdBytes bytes of ASCII
  /// letters, digits, '_', '-' and '.'.
  std::string voice{};
  /// Speaking rate in thousandths; 1000 is normal speed. Nonzero when
  /// required.
  std::uint16_t speed_milli = 0;

  friend bool operator==(const JobOptions& lhs, const JobOptions& rhs) noexcept = default;
};

// -- Request. ----------------------------------------------------------------

/// Validated, immutable request for one job. Copyable; copies name the same
/// buffers.
class JobRequest {
 public:
  /// Validating factory.
  ///
  /// @param identity       job identity; every field nonzero.
  /// @param job_class      class of the job.
  /// @param payload        the payload type `job_class` requires.
  /// @param options        the options `job_class` requires; see JobOptions.
  /// @param progress_limit most `progress` events the job may deliver,
  ///                       0..kMaxJobProgressEvents. Zero, the default, means
  ///                       the job delivers its whole result with
  ///                       `completed`. The submitter's dispatch strategy
  ///                       chooses it for the profile that runs the job.
  /// @return the request, or Error::Code::ConfigInvalid with the first of
  ///   these reasons that applies, checked in this order:
  ///   - "job_id_zero", "session_key_zero", "generation_zero";
  ///   - "job_class_unknown": `job_class` is not an enumerator;
  ///   - "payload_class_mismatch": the payload type is not the one
  ///     `job_class` requires;
  ///   - "progress_limit_too_large": progress_limit > kMaxJobProgressEvents;
  ///   - "option_not_applicable": an option the class does not take is set;
  ///   - "language_invalid", "voice_invalid", "speed_invalid": a required
  ///     option is missing or malformed;
  ///   - AudioFrames: "audio_format_unsupported" (not kJobInputAudioFormat),
  ///     "pcm_buffer_invalid" (released or null handle), "pcm_window_empty",
  ///     "pcm_window_out_of_bounds" (the window leaves the buffer; computed
  ///     without overflow), "pcm_misaligned" (odd offset or size),
  ///     "pcm_too_large" (> kMaxAudioFramesBytes);
  ///   - TextSegment: "text_empty", "text_too_large" (> kMaxTextSegmentBytes),
  ///     "text_invalid_utf8" (overlong forms, surrogates, code points above
  ///     U+10FFFF and truncated sequences are ill-formed);
  ///   - VadFrames: the AudioFrames reasons up to "pcm_misaligned", then
  ///     "vad_frame_count_out_of_range" (outside 1..kMaxVadFramesPerJob),
  ///     "pcm_too_large" (> kMaxVadFramesBytes), "vad_frame_bytes_mismatch"
  ///     (size differs from frame_count * frame_samples * 2),
  ///     "utterance_id_zero".
  [[nodiscard]] static Result<JobRequest> create(JobIdentity identity, JobClass job_class,
                                                 JobPayload payload, JobOptions options = {},
                                                 std::uint32_t progress_limit = 0);

  [[nodiscard]] const JobIdentity& identity() const noexcept { return identity_; }
  [[nodiscard]] JobClass job_class() const noexcept { return job_class_; }
  [[nodiscard]] const JobPayload& payload() const noexcept { return payload_; }
  [[nodiscard]] const JobOptions& options() const noexcept { return options_; }
  [[nodiscard]] std::uint32_t progress_limit() const noexcept { return progress_limit_; }

  friend bool operator==(const JobRequest& lhs, const JobRequest& rhs) noexcept = default;

 private:
  JobRequest(JobIdentity identity, JobClass job_class, JobPayload payload, JobOptions options,
             std::uint32_t progress_limit) noexcept
      : identity_(identity),
        job_class_(job_class),
        payload_(std::move(payload)),
        options_(std::move(options)),
        progress_limit_(progress_limit) {}

  JobIdentity identity_;
  JobClass job_class_ = JobClass::SttDecode;
  JobPayload payload_;
  JobOptions options_;
  std::uint32_t progress_limit_ = 0;
};

}  // namespace tensorplate
