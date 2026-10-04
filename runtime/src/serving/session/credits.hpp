// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <chrono>
#include <cstdint>
#include <deque>

#include "tensorplate/core/result.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {

/// What a session's client sends as input.
enum class SessionInputKind : std::uint8_t {
  Audio = 0,
  Text = 1,
};

/// Byte budgets of one logical session, derived from the byte rate of the
/// audio format negotiated at open.
class SessionBudgets {
 public:
  /// Seconds of accepted, not yet consumed audio a session may hold.
  static constexpr std::chrono::seconds kInputAudioWindow{1};
  /// Text segments that may wait behind the active one, and their combined
  /// size in bytes.
  static constexpr std::uint32_t kWaitingTextSegments = 2;
  static constexpr std::uint64_t kWaitingTextBytes = 8192;
  /// Text segments being executed or delivered at once.
  static constexpr std::uint32_t kActiveTextSegments = 1;
  /// Seconds of undelivered PCM output, and bytes of undelivered control and
  /// transcript metadata.
  static constexpr std::chrono::seconds kOutputPcmWindow{2};
  static constexpr std::uint64_t kOutputMetadataBytes = 16384;
  /// The part of the metadata budget only lifecycle messages may use, so
  /// that queued task output cannot keep them out.
  static constexpr std::uint64_t kOutputControlReserveBytes = 1024;
  /// How long output may await delivery without progress.
  static constexpr std::chrono::seconds kOutputNoProgressTimeout{5};

  /// Audio in, transcripts out: one second of input at
  /// `input_bytes_per_second` and no PCM output.
  /// @return ConfigInvalid with context "invalid_session_budgets" if the rate
  ///   is zero or one second of it exceeds kMaxAudioFramesBytes.
  [[nodiscard]] static Result<SessionBudgets> for_audio_input(std::uint64_t input_bytes_per_second);

  /// Text in, audio out: the waiting and active text segments, and two
  /// seconds of PCM output at `output_bytes_per_second`.
  /// @return ConfigInvalid with context "invalid_session_budgets" if the rate
  ///   is zero or two seconds of it exceed kMaxAudioChunkBytes.
  [[nodiscard]] static Result<SessionBudgets> for_text_input(std::uint64_t output_bytes_per_second);

  [[nodiscard]] SessionInputKind input_kind() const noexcept { return input_kind_; }
  [[nodiscard]] std::uint64_t input_credit_bytes() const noexcept { return input_credit_bytes_; }
  /// Zero when the count of waiting items is not limited (audio).
  [[nodiscard]] std::uint32_t waiting_items() const noexcept { return waiting_items_; }
  /// Zero when input has no active stage (audio).
  [[nodiscard]] std::uint32_t active_items() const noexcept { return active_items_; }
  [[nodiscard]] std::uint64_t output_pcm_bytes() const noexcept { return output_pcm_bytes_; }
  [[nodiscard]] std::uint64_t output_metadata_bytes() const noexcept {
    return output_metadata_bytes_;
  }
  [[nodiscard]] std::uint64_t output_control_reserve_bytes() const noexcept {
    return output_control_reserve_bytes_;
  }

  friend constexpr bool operator==(const SessionBudgets& lhs,
                                   const SessionBudgets& rhs) noexcept = default;

 private:
  constexpr SessionBudgets(SessionInputKind input_kind, std::uint64_t input_credit_bytes,
                           std::uint32_t waiting_items, std::uint32_t active_items,
                           std::uint64_t output_pcm_bytes) noexcept
      : input_kind_(input_kind),
        input_credit_bytes_(input_credit_bytes),
        waiting_items_(waiting_items),
        active_items_(active_items),
        output_pcm_bytes_(output_pcm_bytes) {}

  SessionInputKind input_kind_;
  std::uint64_t input_credit_bytes_;
  std::uint32_t waiting_items_;
  std::uint32_t active_items_;
  std::uint64_t output_pcm_bytes_;
  std::uint64_t output_metadata_bytes_ = kOutputMetadataBytes;
  std::uint64_t output_control_reserve_bytes_ = kOutputControlReserveBytes;
};

/// Input credit of one logical session: what the session has accepted and
/// still owns. Not thread-safe; its owner serializes access.
///
/// Audio is held until consumed. A text segment waits, then occupies the
/// single active slot from the start of execution until its output has been
/// delivered; its waiting credit returns when it becomes active.
///
/// A refused call changes nothing. Refusals other than the two charge() names
/// are defects in the caller: Error::Code::Internal with context
/// "wrong_input_kind", "input_release_mismatch" or "segment_stage_violation".
class InputCredit {
 public:
  explicit InputCredit(const SessionBudgets& budgets) noexcept;

  /// Accepts one audio chunk or text segment of `bytes` bytes.
  /// @return ConfigInvalid with context "empty_input" if bytes is zero, or
  ///   ResourceExhausted with context "input_credit_exceeded".
  [[nodiscard]] Result<void> charge(std::uint64_t bytes);

  /// Audio: returns `bytes` consumed bytes, `chunks` of the accepted chunks
  /// being consumed in full by them. Chunks and bytes run out together.
  [[nodiscard]] Result<void> release_audio(std::uint32_t chunks, std::uint64_t bytes);

  /// Text: the oldest waiting segment takes the free active slot.
  [[nodiscard]] Result<void> start_segment();

  /// Text: the active segment's output was delivered or discarded.
  [[nodiscard]] Result<void> finish_segment();

  /// Accepted items still owned, the active one included.
  [[nodiscard]] std::uint32_t depth() const noexcept;

  /// Bytes held against the input credit: unconsumed audio, or waiting text.
  [[nodiscard]] LogicalSessionUsage usage() const;

 private:
  SessionBudgets budgets_;
  std::uint64_t held_bytes_ = 0;
  std::uint32_t audio_chunks_ = 0;
  std::deque<std::uint64_t> waiting_segments_;
  std::uint32_t active_segments_ = 0;
};
}  // namespace tensorplate::serving
