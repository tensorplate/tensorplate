// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstdint>
#include <optional>

#include "tensorplate/core/result.hpp"

namespace tensorplate::serving {

/// The voice-detection profile of an audio session. Durations are on the
/// 16 kHz internal timeline (kJobInputSampleRateHz); the speech and silence
/// runs count whole frames, rounding a duration up to the next frame.
struct VadProfile {
  /// A frame whose speech probability is at least this is speech.
  float threshold = 0.5F;
  std::chrono::milliseconds min_speech{64};
  std::chrono::milliseconds silence_endpoint{320};
  /// Audio before the first speech frame that an utterance starts with.
  std::chrono::milliseconds leading_padding{160};
  /// Carried for the rolling decode windows; this unit does not read them.
  std::chrono::milliseconds initial_accumulation{320};
  std::chrono::milliseconds decode_stride{160};
  /// Following audio kept while an endpointed utterance finishes, counted
  /// from its endpoint; carried for the dispatch.
  std::chrono::milliseconds endpoint_retention{1000};
  std::chrono::milliseconds max_utterance{30000};
  /// Samples per frame of the voice-activity model.
  std::uint32_t frame_samples = 512;

  friend bool operator==(const VadProfile& lhs, const VadProfile& rhs) noexcept = default;
};

/// What one frame decided, in sample order: a duration split first, then an
/// onset or a silence endpoint. Offsets are absolute internal samples and
/// never reset across utterances.
struct VadFrameOutcome {
  /// The utterance in speech reached max_utterance: it ends here (duration
  /// limit) and the next utterance starts here, still in speech.
  std::optional<std::uint64_t> duration_split;
  /// Speech began: the utterance's first sample, leading padding included,
  /// never before the previous utterance's end.
  std::optional<std::uint64_t> onset;
  /// Silence ended the utterance: the exclusive end of the silence run that
  /// detected it. Final segment times come from aligned words, never from
  /// this offset.
  std::optional<std::uint64_t> silence_endpoint;

  friend bool operator==(const VadFrameOutcome& lhs, const VadFrameOutcome& rhs) noexcept = default;
};

/// Onset and endpoint detection over one session's frame probabilities. Pure:
/// no clock, no thread and no allocation; the frames are back to back from
/// sample zero, or from the end a reset names.
class VadPolicy {
 public:
  /// @return ConfigInvalid with context "invalid_vad_profile" unless the
  ///   threshold is in (0, 1], frame_samples fits one VadFrames payload,
  ///   max_utterance is in (0, 30 s], every other duration is in
  ///   (0, max_utterance], and leading padding plus the minimum speech run
  ///   is shorter than max_utterance.
  [[nodiscard]] static Result<VadPolicy> create(VadProfile profile);

  /// Takes the next frame's speech probability. The caller observes each
  /// frame once and in order, and drops results for frames before a reset's
  /// end: the policy cannot tell a missing or late frame.
  /// @return Internal with context "vad_probability_out_of_range" for a
  ///   probability that is not finite or not in [0, 1], or
  ///   "sample_offset_overflow"; the policy is unchanged either way.
  [[nodiscard]] Result<VadFrameOutcome> observe(float probability);

  /// The dispatch ended the utterance itself (a client Finalize or a
  /// half-close) at `end`: back to silence, with the next frame and the next
  /// onset's floor at `end`.
  /// @return Internal with context "vad_reset_before_observed_audio" if `end`
  ///   is before the next frame's start; the policy is unchanged.
  [[nodiscard]] Result<void> reset(std::uint64_t end);

  [[nodiscard]] const VadProfile& profile() const noexcept { return profile_; }
  [[nodiscard]] std::uint64_t next_frame_start() const noexcept { return next_frame_; }
  /// The open utterance's first sample, while in speech.
  [[nodiscard]] std::optional<std::uint64_t> utterance_start() const noexcept;
  /// The open utterance's first sample, or in silence the earliest the next
  /// onset can name: no utterance takes audio before it.
  [[nodiscard]] std::uint64_t earliest_utterance_start() const noexcept;

 private:
  VadPolicy(VadProfile profile, std::uint32_t min_speech_frames, std::uint32_t silence_frames,
            std::uint64_t leading_padding, std::uint64_t max_utterance) noexcept;

  [[nodiscard]] VadFrameOutcome in_silence(bool speech, std::uint64_t frame_start) noexcept;
  [[nodiscard]] VadFrameOutcome in_speech(bool speech, std::uint64_t start) noexcept;
  /// An onset's start for a speech run from `run_start`: its leading padding,
  /// clamped at the floor.
  [[nodiscard]] std::uint64_t padded(std::uint64_t run_start) const noexcept;

  VadProfile profile_;
  std::uint32_t min_speech_frames_;
  std::uint32_t silence_frames_;
  std::uint64_t leading_padding_;
  std::uint64_t max_utterance_;
  std::uint64_t next_frame_ = 0;
  /// The earliest sample the next onset may start at.
  std::uint64_t floor_ = 0;
  std::optional<std::uint64_t> utterance_start_;
  /// Consecutive frames against the current state: speech while in silence,
  /// silence while in speech. It carries across a duration split.
  std::uint32_t run_frames_ = 0;
  std::uint64_t run_start_ = 0;
};

/// Microseconds [start_us, end_us) on the session's wire origin.
struct MicrosecondInterval {
  std::uint64_t start_us = 0;
  std::uint64_t end_us = 0;

  friend bool operator==(const MicrosecondInterval& lhs,
                         const MicrosecondInterval& rhs) noexcept = default;
};

/// An internal exclusive end b as an offset at the input rate R:
/// ceil(b * R / 16,000).
/// @return Internal with context "unsupported_input_rate" unless R is 8,000
///   or 16,000, or "sample_offset_overflow".
[[nodiscard]] Result<std::uint64_t> to_input_samples(std::uint64_t internal_end,
                                                     std::uint32_t input_rate_hz);

/// An internal interval [a, b) as [floor(a * 10^6 / 16,000),
/// ceil(b * 10^6 / 16,000)) microseconds.
/// @return Internal with context "interval_out_of_order" if a > b, or
///   "sample_offset_overflow".
[[nodiscard]] Result<MicrosecondInterval> to_microseconds(std::uint64_t internal_start,
                                                          std::uint64_t internal_end);
}  // namespace tensorplate::serving
