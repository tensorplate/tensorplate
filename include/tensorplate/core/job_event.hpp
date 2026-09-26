// SPDX-License-Identifier: Apache-2.0
//
// Typed backend job events and the per-job event order.
//
// Every job whose submission succeeded delivers its events in this order:
//
//   [accepted]  (progress | cancel_acknowledged)*  (completed | failed)  released
//
//   - `accepted` (at most once) is the first event when present: the backend
//     admitted the job. A job the backend refuses, or that is cancelled before
//     admission, ends with `failed` without it.
//   - `progress` requires `accepted`. It carries a JobResult fragment of the
//     job's result kind and a progress_sequence that starts at 1 and grows by
//     exactly one. A job delivers at most JobRequest::progress_limit() of
//     them; with the default limit of zero it delivers none.
//   - `cancel_acknowledged` (at most once) reports that the backend received
//     the job's cancellation and will deliver no further progress. It comes
//     only after a successful cancellation (BoundedJobBridge::cancel or
//     release_session) and never after the terminal event; a job whose
//     terminal event overtakes the cancellation ends without it. The
//     terminal event still follows: `failed` with Error::Code::Cancelled if
//     the backend stopped the job, `completed` if its bounded work had
//     already finished.
//   - Exactly one of `completed` (requires `accepted`) or `failed` ends the
//     job's output. Nothing but `released` follows it.
//   - `released` is last: the backend released everything it held for the
//     job, and the submitter may reuse the job's input buffers and
//     reservations. It is delivered only once release actually happened;
//     a job whose release cannot be confirmed never receives it.
//
// Progress fragments are increments: a job's result is its progress fragments
// in progress_sequence order followed by the `completed` payload. Transcript
// texts and tokens concatenate and word units append, each fragment's token
// intervals indexing that fragment's tokens; audio samples concatenate;
// probabilities concatenate. A job's whole result stays within the one-result
// ceilings of tensorplate/core/job_result.hpp, and a VAD job's probabilities
// number exactly its frames.
//
// JobEventSequence checks one job's events against these rules. A bridge
// checks every event before delivering it; receivers and tests may check
// again. A refusal is Error::Code::InferenceFailed, the code a backend
// protocol violation already carries, with a stable snake_case reason in
// Error::context, so a bridge can end the offending job with it unchanged.
//
// Enum values only ever append; names never change. No vendor SDK type
// appears in this header.

#pragma once

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string_view>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Largest Error::message, and largest Error::context, a `failed` event
/// carries, in bytes; the bound of a failure detail in failure_reason.json.
inline constexpr std::size_t kMaxJobErrorTextBytes = 512;

/// Kind of a job event. Stable snake_case names in parentheses.
enum class JobEventKind : std::uint8_t {
  /// (`accepted`) The backend admitted the job.
  Accepted = 0,
  /// (`progress`) A result fragment before the job ends.
  Progress = 1,
  /// (`completed`) The job succeeded; carries the rest of its result.
  Completed = 2,
  /// (`failed`) The job ended without a result; carries the error.
  Failed = 3,
  /// (`cancel_acknowledged`) The backend received the job's cancellation.
  CancelAcknowledged = 4,
  /// (`released`) The backend released everything it held for the job.
  Released = 5,
};

/// Stable snake_case name of `kind`; "unknown" for a value that is not an
/// enumerator.
[[nodiscard]] std::string_view to_string(JobEventKind kind) noexcept;

/// Inverse of to_string(JobEventKind): std::nullopt for any other text.
[[nodiscard]] std::optional<JobEventKind> job_event_kind_from_string(
    std::string_view name) noexcept;

/// Validated, immutable event of one job. Copyable.
///
/// Every factory returns Error::Code::ConfigInvalid with reason
/// "job_id_zero", "session_key_zero" or "generation_zero" when an identity
/// field is zero. Additional reasons are listed per factory.
class JobEvent {
 public:
  [[nodiscard]] static Result<JobEvent> accepted(JobIdentity identity);

  /// Additional reasons: "progress_sequence_zero", and
  /// "progress_sequence_too_large" when progress_sequence >
  /// kMaxJobProgressEvents.
  [[nodiscard]] static Result<JobEvent> progress(JobIdentity identity,
                                                 std::uint32_t progress_sequence,
                                                 JobResult fragment);

  [[nodiscard]] static Result<JobEvent> completed(JobIdentity identity, JobResult result);

  /// Additional reason: "error_text_too_large" when the error's message or
  /// context exceeds kMaxJobErrorTextBytes. Callers relaying a backend's
  /// text shorten it first; a transcript never rides in an error.
  [[nodiscard]] static Result<JobEvent> failed(JobIdentity identity, Error error);

  [[nodiscard]] static Result<JobEvent> cancel_acknowledged(JobIdentity identity);

  [[nodiscard]] static Result<JobEvent> released(JobIdentity identity);

  [[nodiscard]] JobEventKind kind() const noexcept { return kind_; }
  [[nodiscard]] const JobIdentity& identity() const noexcept { return identity_; }

  /// 1..kMaxJobProgressEvents for `progress`; 0 for every other kind.
  [[nodiscard]] std::uint32_t progress_sequence() const noexcept { return progress_sequence_; }

  /// The fragment of a `progress` event or the result of a `completed`
  /// event; empty for every other kind.
  [[nodiscard]] const std::optional<JobResult>& result() const noexcept { return result_; }

  /// The error of a `failed` event; empty for every other kind.
  [[nodiscard]] const std::optional<Error>& error() const noexcept { return error_; }

  friend bool operator==(const JobEvent& lhs, const JobEvent& rhs) noexcept = default;

 private:
  JobEvent(JobEventKind kind, JobIdentity identity) noexcept : kind_(kind), identity_(identity) {}

  JobEventKind kind_ = JobEventKind::Accepted;
  JobIdentity identity_;
  std::uint32_t progress_sequence_ = 0;
  std::optional<JobResult> result_;
  std::optional<Error> error_;
};

/// Checks the events of one job, in delivery order, against the event order
/// in this header.
///
/// A value type: copyable, comparable and holding no reference to the
/// request it was built from. Not thread-safe; its owner feeds one job's
/// events from one thread at a time.
class JobEventSequence {
 public:
  /// Starts checking the job `request` describes: its identity, result kind,
  /// progress limit and, for a VAD job, its frame count.
  explicit JobEventSequence(const JobRequest& request) noexcept;

  /// Records that cancellation of the job was requested, which permits one
  /// `cancel_acknowledged` before the terminal event. Idempotent. A bridge
  /// calls it once its cancel() or release_session() for the job succeeded.
  /// A receiver that checks events again calls it before requesting the
  /// cancellation, because the acknowledgement may arrive on another thread
  /// before that request returns.
  void note_cancel_requested() noexcept { cancel_requested_ = true; }

  /// Accepts the next event of the job, or refuses it and changes nothing.
  ///
  /// @return success, or Error::Code::InferenceFailed with the first of these
  ///   reasons that applies, checked in this order:
  ///   - "identity_mismatch": the event's identity differs from the job's;
  ///   - "event_after_released": the job was already released;
  ///   - "event_after_terminal": anything but `released` after `completed`
  ///     or `failed`;
  ///   - accepted: "accepted_out_of_order" (after any event, itself
  ///     included);
  ///   - progress and completed: "not_accepted"; for progress only
  ///     "progress_after_cancel_acknowledged"; "result_kind_mismatch" (not
  ///     result_kind_for(job class)); for progress only
  ///     "progress_sequence_gap" (not one more than the previous, the first
  ///     being 1) and "progress_limit_exceeded" (more than
  ///     JobRequest::progress_limit()); "result_total_exceeded" (the job's
  ///     summed transcript text, word texts or tokens, or summed audio,
  ///     exceed a one-result ceiling); "vad_result_count_mismatch" (a VAD
  ///     job's probabilities exceed its frames, or number fewer at
  ///     `completed`);
  ///   - cancel_acknowledged: "cancel_acknowledged_without_cancel" (before
  ///     note_cancel_requested), "duplicate_cancel_acknowledged";
  ///   - released: "released_before_terminal".
  [[nodiscard]] Result<void> observe(const JobEvent& event);

  /// True once `accepted` was observed.
  [[nodiscard]] bool accepted() const noexcept { return accepted_; }
  /// True once note_cancel_requested() was called.
  [[nodiscard]] bool cancel_requested() const noexcept { return cancel_requested_; }
  /// True once `cancel_acknowledged` was observed.
  [[nodiscard]] bool cancel_acknowledged() const noexcept { return cancel_acknowledged_; }
  /// True once `completed` or `failed` was observed.
  [[nodiscard]] bool terminal() const noexcept { return terminal_; }
  /// True once `released` was observed; the job accepts no further event.
  [[nodiscard]] bool released() const noexcept { return released_; }
  /// Number of `progress` events observed.
  [[nodiscard]] std::uint32_t progress_count() const noexcept { return progress_count_; }

  friend bool operator==(const JobEventSequence& lhs,
                         const JobEventSequence& rhs) noexcept = default;

 private:
  // The job's summed result sizes, checked against the one-result ceilings.
  struct Totals {
    std::size_t text_bytes = 0;
    std::size_t word_text_bytes = 0;
    std::size_t tokens = 0;
    std::size_t pcm_bytes = 0;
    std::size_t probabilities = 0;

    friend bool operator==(const Totals& lhs, const Totals& rhs) noexcept = default;
  };

  // The observe() checks for a progress or completed event. On success
  // `totals` is the job's totals with the event's result added.
  [[nodiscard]] Result<void> check_result(const JobEvent& event, Totals& totals) const;

  JobIdentity identity_;
  JobResultKind result_kind_ = JobResultKind::Transcript;
  std::uint32_t progress_limit_ = 0;
  std::uint32_t vad_frames_ = 0;

  bool observed_any_ = false;
  bool accepted_ = false;
  bool cancel_requested_ = false;
  bool cancel_acknowledged_ = false;
  bool terminal_ = false;
  bool released_ = false;
  std::uint32_t progress_count_ = 0;
  Totals totals_;
};

}  // namespace tensorplate
