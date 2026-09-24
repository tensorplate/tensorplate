// SPDX-License-Identifier: Apache-2.0
//
// Typed backend job results.
//
// A JobResult holds exactly one typed payload: a TranscriptResult, an
// AudioChunkResult or a VadResult. The job class fixes which one
// (result_kind_for). A `completed` event carries a JobResult, and so does
// every `progress` event, as a fragment of the same kind
// (tensorplate/core/job_event.hpp).
//
// The ceilings below bound one JobResult, and the events of one job together
// never exceed them either (JobEventSequence). They are repeated, with the
// validation reasons, in protocol/fixtures/job_seam.json (see
// job_request.hpp).
//
// An AudioChunkResult's buffer is published by the backend adapter. The
// receiver of the event that carries it releases it exactly once, whether it
// publishes the audio or discards the event.
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
#include <vector>

#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

// -- Frozen ceilings. --------------------------------------------------------

/// Largest TranscriptResult text in UTF-8 bytes; the texts of its word units
/// together obey the same ceiling.
inline constexpr std::size_t kMaxTranscriptTextBytes = 8'192;

/// Most output tokens of one decode job. Exceeding it is a typed job failure,
/// never truncation. Word units cover disjoint, non-empty token intervals, so
/// they never outnumber the tokens.
inline constexpr std::size_t kMaxTranscriptTokens = 448;

/// Largest AudioChunkResult window in bytes: the 30-second limit on one
/// synthesized segment at kJobOutputSampleRateHz, 16-bit mono
/// (30 * 24,000 * 2).
inline constexpr std::size_t kMaxAudioChunkBytes = 1'440'000;

/// Most probabilities in one VadResult: one per frame of the largest
/// VadFrames payload.
inline constexpr std::size_t kMaxVadProbabilities = kMaxVadFramesPerJob;

// -- Payloads. ---------------------------------------------------------------

/// One aligned word of a transcript.
struct WordUnit {
  /// Non-empty, well-formed UTF-8 text of the word, including any attached
  /// punctuation.
  std::string text;
  /// Tokens [token_begin, token_end) of the enclosing TranscriptResult;
  /// non-empty and inside TranscriptResult::tokens.
  std::uint32_t token_begin = 0;
  std::uint32_t token_end = 0;
  /// Audio [start_sample, end_sample) on the session's internal timeline,
  /// the timeline of AudioFrames::start_sample; start_sample <= end_sample.
  std::uint64_t start_sample = 0;
  std::uint64_t end_sample = 0;

  friend bool operator==(const WordUnit& lhs, const WordUnit& rhs) noexcept = default;
};

/// Decoded text, tokens and word units of one decode job. Any of them may be
/// empty; an entirely empty result is valid (a window without speech).
///
/// Word units appear in audio order: their token intervals increase without
/// overlapping and their start samples never decrease.
struct TranscriptResult {
  std::string text;
  std::vector<std::uint32_t> tokens;
  std::vector<WordUnit> words;

  friend bool operator==(const TranscriptResult& lhs,
                         const TranscriptResult& rhs) noexcept = default;
};

/// Synthesized speech in kJobOutputAudioFormat, at most kMaxAudioChunkBytes.
struct AudioChunkResult {
  AudioFormat format;
  PcmWindow pcm;
  /// Samples the backend's conversion to 16-bit PCM clamped; at most the
  /// window's sample count. Reported, never hidden.
  std::uint32_t clipped_samples = 0;

  friend bool operator==(const AudioChunkResult& lhs,
                         const AudioChunkResult& rhs) noexcept = default;
};

/// Voice-activity probabilities, one per VAD frame in frame order, each
/// finite and within [0, 1]; 1..kMaxVadProbabilities of them.
struct VadResult {
  std::vector<float> probabilities;

  friend bool operator==(const VadResult& lhs, const VadResult& rhs) noexcept = default;
};

/// Exactly one typed payload, in JobResultKind order.
using JobResultPayload = std::variant<TranscriptResult, AudioChunkResult, VadResult>;

/// Kind of a JobResult payload. Stable names in parentheses.
enum class JobResultKind : std::uint8_t {
  /// (`transcript`) TranscriptResult.
  Transcript = 0,
  /// (`audio_chunk`) AudioChunkResult.
  AudioChunk = 1,
  /// (`vad`) VadResult.
  Vad = 2,
};

/// Stable snake_case name of `kind`; "unknown" for a value that is not an
/// enumerator.
[[nodiscard]] std::string_view to_string(JobResultKind kind) noexcept;

/// Inverse of to_string(JobResultKind): std::nullopt for any other text.
[[nodiscard]] std::optional<JobResultKind> job_result_kind_from_string(
    std::string_view name) noexcept;

/// The kind of every result and progress fragment of a `job_class` job.
/// JobRequest::create admits no other job class value.
[[nodiscard]] JobResultKind result_kind_for(JobClass job_class) noexcept;

// -- Result. -----------------------------------------------------------------

/// Validated, immutable job result or progress fragment. Copyable; copies
/// name the same buffers.
class JobResult {
 public:
  /// Validating factory.
  ///
  /// @return the result, or Error::Code::ConfigInvalid with the first of
  ///   these reasons that applies, checked in this order:
  ///   - TranscriptResult: "text_too_large" (> kMaxTranscriptTextBytes),
  ///     "text_invalid_utf8", "token_count_exceeded"
  ///     (> kMaxTranscriptTokens); then word by word: "word_text_empty",
  ///     "text_invalid_utf8", "text_too_large" (the word texts so far exceed
  ///     kMaxTranscriptTextBytes), "word_token_interval_empty",
  ///     "word_token_interval_out_of_range", "word_token_interval_overlap"
  ///     (begins before the previous word's end),
  ///     "word_sample_interval_reversed", "word_sample_order" (starts before
  ///     the previous word's start);
  ///   - AudioChunkResult: "audio_format_unsupported" (not
  ///     kJobOutputAudioFormat), "pcm_buffer_invalid", "pcm_window_empty",
  ///     "pcm_window_out_of_bounds", "pcm_misaligned", "pcm_too_large"
  ///     (> kMaxAudioChunkBytes), "clipped_samples_out_of_range";
  ///   - VadResult: "vad_probability_count_out_of_range" (outside
  ///     1..kMaxVadProbabilities), "vad_probability_out_of_range" (NaN,
  ///     infinite, below 0 or above 1).
  [[nodiscard]] static Result<JobResult> create(JobResultPayload payload);

  [[nodiscard]] JobResultKind kind() const noexcept {
    return static_cast<JobResultKind>(payload_.index());
  }
  [[nodiscard]] const JobResultPayload& payload() const noexcept { return payload_; }

  friend bool operator==(const JobResult& lhs, const JobResult& rhs) noexcept = default;

 private:
  explicit JobResult(JobResultPayload payload) noexcept : payload_(std::move(payload)) {}

  JobResultPayload payload_;
};

}  // namespace tensorplate
