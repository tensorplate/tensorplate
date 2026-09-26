// SPDX-License-Identifier: Apache-2.0
//
// JobResult validation and the result-kind names.

#include "tensorplate/core/job_result.hpp"

#include <array>
#include <cmath>
#include <cstddef>
#include <type_traits>
#include <utility>
#include <variant>

#include "core/job_validation.hpp"
#include "core/utf8.hpp"

namespace tensorplate {

namespace {

constexpr std::string_view name_of(JobResultKind kind) noexcept {
  switch (kind) {
    case JobResultKind::Transcript:
      return "transcript";
    case JobResultKind::AudioChunk:
      return "audio_chunk";
    case JobResultKind::Vad:
      return "vad";
  }
  return {};
}

constexpr std::array<JobResultKind, 3> kJobResultKinds{
    JobResultKind::Transcript, JobResultKind::AudioChunk, JobResultKind::Vad};

// JobResultPayload alternatives are in JobResultKind order.
static_assert(std::is_same_v<std::variant_alternative_t<0, JobResultPayload>, TranscriptResult>);
static_assert(std::is_same_v<std::variant_alternative_t<1, JobResultPayload>, AudioChunkResult>);
static_assert(std::is_same_v<std::variant_alternative_t<2, JobResultPayload>, VadResult>);

Result<void> validate(const TranscriptResult& transcript) {
  if (transcript.text.size() > kMaxTranscriptTextBytes) {
    return internal::invalid("text_too_large", "TranscriptResult text exceeds 8192 bytes");
  }
  if (!internal::is_well_formed_utf8(transcript.text)) {
    return internal::invalid("text_invalid_utf8", "TranscriptResult text is not well-formed UTF-8");
  }
  if (transcript.tokens.size() > kMaxTranscriptTokens) {
    return internal::invalid("token_count_exceeded", "TranscriptResult has more than 448 tokens");
  }
  std::size_t word_text_bytes = 0;
  std::uint32_t previous_token_end = 0;
  std::uint64_t previous_start = 0;
  for (const WordUnit& word : transcript.words) {
    if (word.text.empty()) {
      return internal::invalid("word_text_empty", "WordUnit text must not be empty");
    }
    if (!internal::is_well_formed_utf8(word.text)) {
      return internal::invalid("text_invalid_utf8", "WordUnit text is not well-formed UTF-8");
    }
    word_text_bytes += word.text.size();
    if (word_text_bytes > kMaxTranscriptTextBytes) {
      return internal::invalid("text_too_large", "WordUnit texts exceed 8192 bytes");
    }
    if (word.token_begin >= word.token_end) {
      return internal::invalid("word_token_interval_empty", "WordUnit token interval is empty");
    }
    if (word.token_end > transcript.tokens.size()) {
      return internal::invalid("word_token_interval_out_of_range",
                               "WordUnit token interval exceeds the tokens");
    }
    if (word.token_begin < previous_token_end) {
      return internal::invalid("word_token_interval_overlap",
                               "WordUnit token intervals overlap or are out of order");
    }
    if (word.start_sample > word.end_sample) {
      return internal::invalid("word_sample_interval_reversed",
                               "WordUnit start_sample exceeds end_sample");
    }
    if (word.start_sample < previous_start) {
      return internal::invalid("word_sample_order",
                               "WordUnit starts before the previous word's start");
    }
    previous_token_end = word.token_end;
    previous_start = word.start_sample;
  }
  return {};
}

Result<void> validate(const AudioChunkResult& chunk) {
  if (auto ok = internal::validate_pcm(chunk.format, kJobOutputAudioFormat, chunk.pcm,
                                       "AudioChunkResult");
      !ok) {
    return ok;
  }
  if (chunk.pcm.byte_size > kMaxAudioChunkBytes) {
    return internal::invalid("pcm_too_large", "AudioChunkResult window exceeds 1440000 bytes");
  }
  if (chunk.clipped_samples > chunk.pcm.byte_size / 2) {
    return internal::invalid("clipped_samples_out_of_range",
                             "AudioChunkResult clipped_samples exceeds its samples");
  }
  return {};
}

Result<void> validate(const VadResult& vad) {
  if (vad.probabilities.empty() || vad.probabilities.size() > kMaxVadProbabilities) {
    return internal::invalid("vad_probability_count_out_of_range",
                             "VadResult must hold 1..32 probabilities");
  }
  for (const float p : vad.probabilities) {
    if (!std::isfinite(p) || p < 0.0F || p > 1.0F) {
      return internal::invalid("vad_probability_out_of_range",
                               "VadResult probabilities must be finite and within [0, 1]");
    }
  }
  return {};
}

}  // namespace

std::string_view to_string(JobResultKind kind) noexcept {
  const std::string_view name = name_of(kind);
  return name.empty() ? std::string_view{"unknown"} : name;
}

std::optional<JobResultKind> job_result_kind_from_string(std::string_view name) noexcept {
  for (const JobResultKind kind : kJobResultKinds) {
    if (name_of(kind) == name) {
      return kind;
    }
  }
  return std::nullopt;
}

JobResultKind result_kind_for(JobClass job_class) noexcept {
  switch (job_class) {
    case JobClass::SttDecode:
      return JobResultKind::Transcript;
    case JobClass::TtsSynthesis:
      return JobResultKind::AudioChunk;
    case JobClass::VadFrames:
      return JobResultKind::Vad;
  }
  return JobResultKind::Transcript;
}

Result<JobResult> JobResult::create(JobResultPayload payload) {
  auto ok = std::visit([](const auto& p) { return validate(p); }, payload);
  if (!ok) {
    return unexpected(std::move(ok).error());
  }
  return JobResult(std::move(payload));
}

}  // namespace tensorplate
