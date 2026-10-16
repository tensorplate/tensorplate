// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <vector>

#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

#include "serving/session/vad_policy.hpp"

namespace tensorplate::serving {

/// One aligned word of a decode.
struct AlignedWord {
  std::string text;
  std::vector<std::uint32_t> tokens;
  /// Audio [start_sample, end_sample) on the session's internal timeline.
  std::uint64_t start_sample = 0;
  std::uint64_t end_sample = 0;

  friend bool operator==(const AlignedWord& lhs, const AlignedWord& rhs) noexcept = default;
};

/// Bounds of the commitment, in internal samples at 16 kHz.
inline constexpr std::uint64_t kCommitTolerance = 2'560;     // 160 ms
inline constexpr std::uint64_t kCommitGuard = 5'120;         // 320 ms
inline constexpr std::uint64_t kRetainedOverlap = 16'000;    // 1 s
inline constexpr std::uint64_t kTranscriptWindow = 128'000;  // 8 s

/// What one decode did to the utterance's transcript.
struct CommitStep {
  /// False when the decode does not repeat the committed words its window
  /// holds: it commits nothing and leaves the suffix as it was.
  bool reconciled = false;
  /// Words this snapshot moved from the suffix into the committed prefix.
  std::size_t newly_committed = 0;

  friend bool operator==(const CommitStep& lhs, const CommitStep& rhs) noexcept = default;
};

/// Stable-prefix commitment of one utterance's transcript over successive
/// decodes of a window that starts at ring_start(); the rules are in
/// docs/architecture/serving-worker.md ("Transcript commitment"). Pure: no
/// clock, no thread, no job.
class TranscriptCommitment {
 public:
  /// An utterance whose first sample is `utterance_start`, committing under
  /// a profile VadPolicy::create accepted.
  TranscriptCommitment(const VadProfile& profile, std::uint64_t utterance_start);

  /// Takes the decode of [window_start, window_end).
  /// @return Internal with context "commitment_window_mismatch" unless the
  ///   window starts at ring_start() and ends after the last observed one,
  ///   or "invalid_transcript_snapshot" for a word outside the window, out of
  ///   audio order or with tokens outside the result's; the commitment is
  ///   unchanged either way.
  [[nodiscard]] Result<CommitStep> observe(std::uint64_t window_start, std::uint64_t window_end,
                                           const TranscriptResult& result);

  /// Whether the ring may take audio up to `end` (exclusive).
  /// @return ResourceExhausted with context "unstable_transcript_window" when
  ///   [ring_start(), end) is longer than kTranscriptWindow: nothing committed
  ///   lets the ring advance far enough.
  [[nodiscard]] Result<void> admit(std::uint64_t end) const;

  /// The utterance's final words from the decode of its retained tail
  /// [ring_start(), window_end), which may end where the last decode ended:
  /// the committed prefix followed by the tail's words after the committed
  /// words it repeats. The commitment is unchanged.
  /// @return InferenceFailed with context "ambiguous_final_overlap" when the
  ///   tail does not repeat them as observe() requires; the observe() errors
  ///   otherwise.
  [[nodiscard]] Result<std::vector<AlignedWord>> finalize(std::uint64_t window_start,
                                                          std::uint64_t window_end,
                                                          const TranscriptResult& tail) const;

  /// Starts the utterance whose first sample is `utterance_start` with
  /// nothing committed.
  void reset(std::uint64_t utterance_start);

  [[nodiscard]] const std::vector<AlignedWord>& committed() const noexcept { return committed_; }
  /// The uncommitted words of the last reconciled decode; a partial is
  /// committed() followed by them.
  [[nodiscard]] const std::vector<AlignedWord>& suffix() const noexcept {
    return history_.back().words;
  }
  [[nodiscard]] std::uint64_t ring_start() const noexcept { return ring_start_; }

 private:
  /// The decode's words, if it covers the ring's window ending at or after
  /// `min_end`.
  [[nodiscard]] Result<std::vector<AlignedWord>> words_of(std::uint64_t window_start,
                                                          std::uint64_t window_end,
                                                          std::uint64_t min_end,
                                                          const TranscriptResult& result) const;
  /// How many of `words` repeat committed words: none, or the word cut at
  /// the ring start, followed by every committed word after it, one to one.
  /// Nothing when they do not.
  [[nodiscard]] std::optional<std::size_t> anchored(const std::vector<AlignedWord>& words) const;
  /// Whether `word`, uncommitted word `index` of a decode, may commit.
  [[nodiscard]] bool settled(std::size_t index, const AlignedWord& word) const;
  void advance_ring() noexcept;

  /// The uncommitted words of a reconciled decode of [ring_start(), end).
  struct Snapshot {
    std::vector<AlignedWord> words;
    std::uint64_t end = 0;
  };

  std::uint32_t agreement_;
  std::vector<AlignedWord> committed_;
  /// Committed words before this index lie before ring_start_.
  std::size_t ring_index_ = 0;
  std::uint64_t ring_start_;
  /// The last agreement_ - 1 reconciled decodes, oldest first, which the next
  /// is compared with; before the first, one empty snapshot.
  std::vector<Snapshot> history_;
  std::uint64_t observed_end_ = 0;
};

}  // namespace tensorplate::serving
