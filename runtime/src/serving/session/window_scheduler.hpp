// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <cstdint>
#include <optional>

#include "serving/session/vad_policy.hpp"

namespace tensorplate::serving {

/// Samples [first, end) on the 16 kHz internal timeline.
struct SampleWindow {
  std::uint64_t first = 0;
  std::uint64_t end = 0;

  friend bool operator==(const SampleWindow& lhs, const SampleWindow& rhs) noexcept = default;
};

/// When the open utterance of a voice-detected session is due a partial
/// decode, and over which samples. Pure: no clock and no thread; it counts
/// scored source audio, never wall time.
///
/// A window is the utterance so far, from its first sample to the end of its
/// scored audio. The first is due once that end is initial_accumulation past
/// the utterance's first speech sample, and each later one once it is
/// decode_stride past the end of the last window started. The caller asks
/// only while no decode of the session is in flight, so the strides that
/// pass meanwhile give one window over the latest audio, never a backlog.
///
/// A due window waits while the utterance is in a silence run that could end
/// it, so that its final does not queue behind a decode started on trailing
/// silence: it starts only if the last frame scored is speech, or a frame
/// scored since it was last asked was (silence may follow that frame). Only
/// asks since the utterance opened or its last window started count.
class WindowScheduler {
 public:
  /// `profile` is one VadPolicy::create() accepted.
  explicit WindowScheduler(const VadProfile& profile) noexcept;

  /// An utterance opened at `first`; its speech starts at `speech_start`,
  /// after any leading padding.
  void open(std::uint64_t first, std::uint64_t speech_start) noexcept;
  /// The utterance ended: nothing is due until the next one opens.
  void close() noexcept;
  /// The window due over audio scored up to `scored_end`, if one is.
  [[nodiscard]] std::optional<SampleWindow> due(std::uint64_t scored_end) const noexcept;
  /// A decode of the open utterance up to `end` was started.
  void started(std::uint64_t end) noexcept;
  /// Whether the window due over audio scored up to `scored_end` waits; the
  /// last frame scored as speech ends at `speech_end`. Asked only while a
  /// window is due; asking again at the same `scored_end` gives the same answer.
  [[nodiscard]] bool waits(std::uint64_t scored_end, std::uint64_t speech_end) noexcept;

 private:
  std::uint64_t accumulation_;
  std::uint64_t stride_;
  bool open_ = false;
  std::uint64_t first_ = 0;
  std::uint64_t speech_start_ = 0;
  /// The end of the open utterance's last window started.
  std::optional<std::uint64_t> last_end_;
  /// The scored ends at which waits() was last asked, and the one before.
  std::optional<std::uint64_t> asked_at_;
  std::optional<std::uint64_t> asked_before_;
};
}  // namespace tensorplate::serving
