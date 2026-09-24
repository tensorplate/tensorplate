// SPDX-License-Identifier: Apache-2.0
//
// JobRequest validation and the job-class and audio-encoding names.

#include "tensorplate/core/job_request.hpp"

#include <algorithm>
#include <array>
#include <cstddef>
#include <string>
#include <type_traits>
#include <utility>
#include <variant>

#include "tensorplate/core/error.hpp"

#include "core/job_validation.hpp"
#include "core/utf8.hpp"

namespace tensorplate {

namespace {

// No default: -Wswitch makes an appended enumerator without a name a build
// error. An empty result means the value is not an enumerator.
constexpr std::string_view name_of(JobClass job_class) noexcept {
  switch (job_class) {
    case JobClass::SttDecode:
      return "stt_decode";
    case JobClass::TtsSynthesis:
      return "tts_synthesis";
    case JobClass::VadFrames:
      return "vad_frames";
  }
  return {};
}

constexpr std::string_view name_of(AudioEncoding encoding) noexcept {
  switch (encoding) {
    case AudioEncoding::PcmS16Le:
      return "pcm_s16le";
  }
  return {};
}

constexpr std::array<JobClass, 3> kJobClasses{JobClass::SttDecode, JobClass::TtsSynthesis,
                                              JobClass::VadFrames};

// JobPayload alternatives are in JobClass order.
static_assert(std::is_same_v<std::variant_alternative_t<0, JobPayload>, AudioFrames>);
static_assert(std::is_same_v<std::variant_alternative_t<1, JobPayload>, TextSegment>);
static_assert(std::is_same_v<std::variant_alternative_t<2, JobPayload>, VadFrames>);

constexpr bool is_ascii_alpha(char c) noexcept {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
}
constexpr bool is_ascii_digit(char c) noexcept {
  return c >= '0' && c <= '9';
}

// 2 or 3 letters, then zero or more "-" followed by 1..8 letters or digits.
constexpr bool is_language_tag(std::string_view tag) noexcept {
  if (tag.size() > kMaxLanguageTagBytes) {
    return false;
  }
  std::size_t i = 0;
  while (i < tag.size() && is_ascii_alpha(tag[i])) {
    ++i;
  }
  if (i < 2 || i > 3) {
    return false;
  }
  while (i < tag.size()) {
    if (tag[i] != '-') {
      return false;
    }
    const std::size_t start = ++i;
    while (i < tag.size() && (is_ascii_alpha(tag[i]) || is_ascii_digit(tag[i]))) {
      ++i;
    }
    if (i - start < 1 || i - start > 8) {
      return false;
    }
  }
  return true;
}

constexpr bool is_voice_id(std::string_view voice) noexcept {
  if (voice.empty() || voice.size() > kMaxVoiceIdBytes) {
    return false;
  }
  return std::ranges::all_of(voice, [](char c) {
    return is_ascii_alpha(c) || is_ascii_digit(c) || c == '_' || c == '-' || c == '.';
  });
}

Result<void> validate_options(JobClass job_class, const JobOptions& options) {
  const bool takes_language = job_class != JobClass::VadFrames;
  const bool takes_voice = job_class == JobClass::TtsSynthesis;
  if ((!takes_language && !options.language.empty()) ||
      (!takes_voice && (!options.voice.empty() || options.speed_milli != 0))) {
    return internal::invalid(
        "option_not_applicable",
        "an option is set that job class " + std::string(to_string(job_class)) + " does not take");
  }
  if (takes_language && !is_language_tag(options.language)) {
    return internal::invalid("language_invalid", "language is missing or not a language tag");
  }
  if (takes_voice && !is_voice_id(options.voice)) {
    return internal::invalid("voice_invalid", "voice is missing or not a voice identifier");
  }
  if (takes_voice && options.speed_milli == 0) {
    return internal::invalid("speed_invalid", "speed_milli must be nonzero");
  }
  return {};
}

Result<void> validate(const AudioFrames& frames) {
  if (auto ok =
          internal::validate_pcm(frames.format, kJobInputAudioFormat, frames.pcm, "AudioFrames");
      !ok) {
    return ok;
  }
  if (frames.pcm.byte_size > kMaxAudioFramesBytes) {
    return internal::invalid("pcm_too_large", "AudioFrames window exceeds 960000 bytes");
  }
  return {};
}

Result<void> validate(const TextSegment& segment) {
  if (segment.text.empty()) {
    return internal::invalid("text_empty", "TextSegment text must not be empty");
  }
  if (segment.text.size() > kMaxTextSegmentBytes) {
    return internal::invalid("text_too_large", "TextSegment text exceeds 4096 bytes");
  }
  if (!internal::is_well_formed_utf8(segment.text)) {
    return internal::invalid("text_invalid_utf8", "TextSegment text is not well-formed UTF-8");
  }
  return {};
}

Result<void> validate(const VadFrames& frames) {
  if (auto ok =
          internal::validate_pcm(frames.format, kJobInputAudioFormat, frames.pcm, "VadFrames");
      !ok) {
    return ok;
  }
  if (frames.frame_count == 0 || frames.frame_count > kMaxVadFramesPerJob) {
    return internal::invalid("vad_frame_count_out_of_range", "VadFrames frame_count must be 1..32");
  }
  if (frames.pcm.byte_size > kMaxVadFramesBytes) {
    return internal::invalid("pcm_too_large", "VadFrames window exceeds 32768 bytes");
  }
  // frame_count <= 32, so the product cannot overflow 64 bits.
  const std::uint64_t expected =
      std::uint64_t{frames.frame_count} * std::uint64_t{frames.frame_samples} * 2U;
  if (frames.pcm.byte_size != expected) {
    return internal::invalid("vad_frame_bytes_mismatch",
                             "VadFrames window must hold frame_count * frame_samples samples");
  }
  if (frames.utterance_id == 0) {
    return internal::invalid("utterance_id_zero", "VadFrames utterance_id must be nonzero");
  }
  return {};
}

}  // namespace

std::string_view to_string(JobClass job_class) noexcept {
  const std::string_view name = name_of(job_class);
  return name.empty() ? std::string_view{"unknown"} : name;
}

std::optional<JobClass> job_class_from_string(std::string_view name) noexcept {
  for (const JobClass job_class : kJobClasses) {
    if (name_of(job_class) == name) {
      return job_class;
    }
  }
  return std::nullopt;
}

std::string_view to_string(AudioEncoding encoding) noexcept {
  const std::string_view name = name_of(encoding);
  return name.empty() ? std::string_view{"unknown"} : name;
}

std::optional<AudioEncoding> audio_encoding_from_string(std::string_view name) noexcept {
  if (name == name_of(AudioEncoding::PcmS16Le)) {
    return AudioEncoding::PcmS16Le;
  }
  return std::nullopt;
}

namespace internal {

Result<void> validate_job_identity(const JobIdentity& identity) {
  if (identity.job_id == 0) {
    return invalid("job_id_zero", "job_id must be nonzero");
  }
  if (identity.session_key == 0) {
    return invalid("session_key_zero", "session_key must be nonzero");
  }
  if (identity.generation == 0) {
    return invalid("generation_zero", "generation must be nonzero");
  }
  return {};
}

Result<void> validate_pcm(const AudioFormat& format, const AudioFormat& required,
                          const PcmWindow& pcm, std::string_view payload) {
  const std::string name(payload);
  if (format != required) {
    return invalid("audio_format_unsupported", name + " format is not mono pcm_s16le at " +
                                                   std::to_string(required.sample_rate_hz) + " Hz");
  }
  if (!pcm.buffer.is_valid()) {
    return invalid("pcm_buffer_invalid", name + " buffer is released or missing");
  }
  if (pcm.byte_size == 0) {
    return invalid("pcm_window_empty", name + " window is empty");
  }
  const std::size_t buffer_bytes = pcm.buffer.size_bytes();
  if (pcm.byte_offset > buffer_bytes || pcm.byte_size > buffer_bytes - pcm.byte_offset) {
    return invalid("pcm_window_out_of_bounds", name + " window leaves its buffer");
  }
  if (pcm.byte_offset % 2 != 0 || pcm.byte_size % 2 != 0) {
    return invalid("pcm_misaligned", name + " window must hold whole 16-bit samples");
  }
  return {};
}

}  // namespace internal

Result<JobRequest> JobRequest::create(JobIdentity identity, JobClass job_class, JobPayload payload,
                                      JobOptions options, std::uint32_t progress_limit) {
  if (auto ok = internal::validate_job_identity(identity); !ok) {
    return unexpected(std::move(ok).error());
  }
  if (name_of(job_class).empty()) {
    return unexpected(
        internal::invalid_error("job_class_unknown", "job_class is not a known class"));
  }
  if (payload.index() != static_cast<std::size_t>(job_class)) {
    return unexpected(internal::invalid_error(
        "payload_class_mismatch",
        "payload type does not match job_class " + std::string(to_string(job_class))));
  }
  if (progress_limit > kMaxJobProgressEvents) {
    return unexpected(
        internal::invalid_error("progress_limit_too_large", "progress_limit exceeds 1500"));
  }
  if (auto ok = validate_options(job_class, options); !ok) {
    return unexpected(std::move(ok).error());
  }
  auto payload_ok = std::visit([](const auto& p) { return validate(p); }, payload);
  if (!payload_ok) {
    return unexpected(std::move(payload_ok).error());
  }
  return JobRequest(identity, job_class, std::move(payload), std::move(options), progress_limit);
}

}  // namespace tensorplate
