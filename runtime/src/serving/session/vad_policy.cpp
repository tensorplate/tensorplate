// SPDX-License-Identifier: Apache-2.0
#include "serving/session/vad_policy.hpp"

#include <array>
#include <cmath>
#include <limits>
#include <string>
#include <utility>

#include "tensorplate/core/job_request.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;

constexpr std::uint64_t kSamplesPerMs = kJobInputSampleRateHz / 1000;
// The longest utterance one AudioFrames window holds: 30 seconds.
constexpr std::chrono::milliseconds kMaxUtterance{kMaxAudioFramesBytes / sizeof(std::int16_t) /
                                                  kSamplesPerMs};
constexpr std::uint32_t kMaxFrameSamples = kMaxVadFramesBytes / sizeof(std::int16_t);
constexpr std::uint64_t kMicrosecondsPerSecond = 1'000'000;
constexpr std::uint32_t kTelephonyInputRateHz = 8'000;

Error invalid_profile() {
  return Error::make(Error::Code::ConfigInvalid, "invalid voice detection profile",
                     "invalid_vad_profile");
}

Error caller_defect(std::string message, std::string context) {
  return Error::make(Error::Code::Internal, std::move(message), std::move(context));
}

Error overflow() {
  return caller_defect("sample offset out of range", "sample_offset_overflow");
}

std::uint64_t samples(std::chrono::milliseconds duration) {
  return static_cast<std::uint64_t>(duration.count()) * kSamplesPerMs;
}

std::uint32_t whole_frames(std::chrono::milliseconds duration, std::uint32_t frame_samples) {
  return static_cast<std::uint32_t>((samples(duration) + frame_samples - 1) / frame_samples);
}

/// ceil(value * numerator / denominator), refusing overflow.
Result<std::uint64_t> scale_up(std::uint64_t value, std::uint64_t numerator,
                               std::uint64_t denominator) {
  if (value > std::numeric_limits<std::uint64_t>::max() / numerator) {
    return unexpected(overflow());
  }
  const std::uint64_t product = value * numerator;
  return product / denominator + (product % denominator != 0 ? 1 : 0);
}
}  // namespace

Result<VadPolicy> VadPolicy::create(VadProfile profile) {
  if (!std::isfinite(profile.threshold) || profile.threshold <= 0.0F || profile.threshold > 1.0F) {
    return unexpected(invalid_profile());
  }
  if (profile.frame_samples == 0 || profile.frame_samples > kMaxFrameSamples) {
    return unexpected(invalid_profile());
  }
  if (profile.commit_agreement < 2) {
    return unexpected(invalid_profile());
  }
  // Compared in milliseconds, before any conversion can wrap. A max_utterance
  // of zero or less leaves every duration below out of range.
  if (profile.max_utterance > kMaxUtterance) {
    return unexpected(invalid_profile());
  }
  const std::array durations{profile.min_speech,      profile.silence_endpoint,
                             profile.leading_padding, profile.initial_accumulation,
                             profile.decode_stride,   profile.endpoint_retention};
  for (const auto duration : durations) {
    if (duration <= 0ms || duration > profile.max_utterance) {
      return unexpected(invalid_profile());
    }
  }
  const std::uint32_t min_speech_frames = whole_frames(profile.min_speech, profile.frame_samples);
  // An onset and a split never share a frame, and a frame splits at most once.
  if (samples(profile.leading_padding) + std::uint64_t{min_speech_frames} * profile.frame_samples >=
      samples(profile.max_utterance)) {
    return unexpected(invalid_profile());
  }
  return VadPolicy{profile, min_speech_frames,
                   whole_frames(profile.silence_endpoint, profile.frame_samples),
                   samples(profile.leading_padding), samples(profile.max_utterance)};
}

VadPolicy::VadPolicy(VadProfile profile, std::uint32_t min_speech_frames,
                     std::uint32_t silence_frames, std::uint64_t leading_padding,
                     std::uint64_t max_utterance) noexcept
    : profile_(profile),
      min_speech_frames_(min_speech_frames),
      silence_frames_(silence_frames),
      leading_padding_(leading_padding),
      max_utterance_(max_utterance) {}

Result<VadFrameOutcome> VadPolicy::observe(float probability) {
  if (!std::isfinite(probability) || probability < 0.0F || probability > 1.0F) {
    return unexpected(
        caller_defect("voice activity probability out of range", "vad_probability_out_of_range"));
  }
  const std::uint64_t frame_start = next_frame_;
  if (frame_start > std::numeric_limits<std::uint64_t>::max() - profile_.frame_samples) {
    return unexpected(overflow());
  }
  next_frame_ = frame_start + profile_.frame_samples;
  const bool speech = probability >= profile_.threshold;
  if (utterance_start_) {
    return in_speech(speech, *utterance_start_);
  }
  return in_silence(speech, frame_start);
}

VadFrameOutcome VadPolicy::in_silence(bool speech, std::uint64_t frame_start) noexcept {
  if (!speech) {
    run_frames_ = 0;
    return {};
  }
  if (run_frames_ == 0) {
    run_start_ = frame_start;
  }
  if (++run_frames_ < min_speech_frames_) {
    return {};
  }
  const std::uint64_t start = padded(run_start_);
  utterance_start_ = start;
  speech_start_ = run_start_;
  run_frames_ = 0;
  VadFrameOutcome outcome;
  outcome.onset = start;
  return outcome;
}

VadFrameOutcome VadPolicy::in_speech(bool speech, std::uint64_t start) noexcept {
  run_frames_ = speech ? 0 : run_frames_ + 1;
  const bool silence_ended = run_frames_ >= silence_frames_;
  VadFrameOutcome outcome;
  // At exactly max_utterance a silence endpoint ends the utterance instead.
  if (next_frame_ - start > max_utterance_ ||
      (next_frame_ - start == max_utterance_ && !silence_ended)) {
    utterance_start_ = start + max_utterance_;
    speech_start_ = *utterance_start_;
    outcome.duration_split = utterance_start_;
  }
  if (silence_ended) {
    outcome.silence_endpoint = next_frame_;
    utterance_start_.reset();
    floor_ = next_frame_;
    run_frames_ = 0;
  }
  return outcome;
}

Result<void> VadPolicy::reset(std::uint64_t end) {
  if (end < next_frame_) {
    return unexpected(
        caller_defect("utterance end before observed audio", "vad_reset_before_observed_audio"));
  }
  next_frame_ = end;
  floor_ = end;
  utterance_start_.reset();
  run_frames_ = 0;
  return {};
}

std::optional<std::uint64_t> VadPolicy::utterance_start() const noexcept {
  return utterance_start_;
}

std::optional<std::uint64_t> VadPolicy::speech_start() const noexcept {
  if (utterance_start_) {
    return speech_start_;
  }
  return std::nullopt;
}

std::uint64_t VadPolicy::earliest_utterance_start() const noexcept {
  if (utterance_start_) {
    return *utterance_start_;
  }
  // A speech run under way starts the onset; otherwise the next frame does.
  return padded(run_frames_ > 0 ? run_start_ : next_frame_);
}

std::uint64_t VadPolicy::padded(std::uint64_t run_start) const noexcept {
  return run_start - floor_ > leading_padding_ ? run_start - leading_padding_ : floor_;
}

Result<std::uint64_t> to_input_samples(std::uint64_t internal_end, std::uint32_t input_rate_hz) {
  if (input_rate_hz != kTelephonyInputRateHz && input_rate_hz != kJobInputSampleRateHz) {
    return unexpected(
        caller_defect("input rate has no internal mapping", "unsupported_input_rate"));
  }
  return scale_up(internal_end, input_rate_hz, kJobInputSampleRateHz);
}

Result<MicrosecondInterval> to_microseconds(std::uint64_t internal_start,
                                            std::uint64_t internal_end) {
  if (internal_start > internal_end) {
    return unexpected(caller_defect("interval ends before it starts", "interval_out_of_order"));
  }
  const auto end = scale_up(internal_end, kMicrosecondsPerSecond, kJobInputSampleRateHz);
  if (!end) {
    return unexpected(end.error());
  }
  // The start is not past the end, so its product fits too.
  return MicrosecondInterval{internal_start * kMicrosecondsPerSecond / kJobInputSampleRateHz, *end};
}
}  // namespace tensorplate::serving
