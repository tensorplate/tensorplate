// SPDX-License-Identifier: Apache-2.0
//
// JobEvent factories, the event-kind names and the per-job event order.

#include "tensorplate/core/job_event.hpp"

#include <array>
#include <cstddef>
#include <optional>
#include <utility>
#include <variant>

#include "core/job_validation.hpp"

namespace tensorplate {

namespace {

constexpr std::string_view name_of(JobEventKind kind) noexcept {
  switch (kind) {
    case JobEventKind::Accepted:
      return "accepted";
    case JobEventKind::Progress:
      return "progress";
    case JobEventKind::Completed:
      return "completed";
    case JobEventKind::Failed:
      return "failed";
    case JobEventKind::CancelAcknowledged:
      return "cancel_acknowledged";
    case JobEventKind::Released:
      return "released";
  }
  return {};
}

constexpr std::array<JobEventKind, 6> kJobEventKinds{
    JobEventKind::Accepted, JobEventKind::Progress,           JobEventKind::Completed,
    JobEventKind::Failed,   JobEventKind::CancelAcknowledged, JobEventKind::Released};

// What one fragment or result adds to the job's totals.
struct ResultSize {
  std::size_t text_bytes = 0;
  std::size_t word_text_bytes = 0;
  std::size_t tokens = 0;
  std::size_t pcm_bytes = 0;
  std::size_t probabilities = 0;
};

ResultSize size_of(const JobResult& result) noexcept {
  ResultSize size;
  if (const auto* t = std::get_if<TranscriptResult>(&result.payload())) {
    size.text_bytes = t->text.size();
    for (const WordUnit& word : t->words) {
      size.word_text_bytes += word.text.size();
    }
    size.tokens = t->tokens.size();
  } else if (const auto* a = std::get_if<AudioChunkResult>(&result.payload())) {
    size.pcm_bytes = a->pcm.byte_size;
  } else if (const auto* v = std::get_if<VadResult>(&result.payload())) {
    size.probabilities = v->probabilities.size();
  }
  return size;
}

}  // namespace

std::string_view to_string(JobEventKind kind) noexcept {
  const std::string_view name = name_of(kind);
  return name.empty() ? std::string_view{"unknown"} : name;
}

std::optional<JobEventKind> job_event_kind_from_string(std::string_view name) noexcept {
  for (const JobEventKind kind : kJobEventKinds) {
    if (name_of(kind) == name) {
      return kind;
    }
  }
  return std::nullopt;
}

// -- JobEvent. ----------------------------------------------------------------

Result<JobEvent> JobEvent::accepted(JobIdentity identity) {
  if (auto ok = internal::validate_job_identity(identity); !ok) {
    return unexpected(std::move(ok).error());
  }
  return JobEvent(JobEventKind::Accepted, identity);
}

Result<JobEvent> JobEvent::progress(JobIdentity identity, std::uint32_t progress_sequence,
                                    JobResult fragment) {
  if (auto ok = internal::validate_job_identity(identity); !ok) {
    return unexpected(std::move(ok).error());
  }
  if (progress_sequence == 0) {
    return internal::invalid("progress_sequence_zero", "progress_sequence starts at 1");
  }
  if (progress_sequence > kMaxJobProgressEvents) {
    return internal::invalid("progress_sequence_too_large", "progress_sequence exceeds 1500");
  }
  JobEvent event(JobEventKind::Progress, identity);
  event.progress_sequence_ = progress_sequence;
  event.result_ = std::move(fragment);
  return event;
}

Result<JobEvent> JobEvent::completed(JobIdentity identity, JobResult result) {
  if (auto ok = internal::validate_job_identity(identity); !ok) {
    return unexpected(std::move(ok).error());
  }
  JobEvent event(JobEventKind::Completed, identity);
  event.result_ = std::move(result);
  return event;
}

Result<JobEvent> JobEvent::failed(JobIdentity identity, Error error) {
  if (auto ok = internal::validate_job_identity(identity); !ok) {
    return unexpected(std::move(ok).error());
  }
  if (error.message.size() > kMaxJobErrorTextBytes ||
      (error.context.has_value() && error.context->size() > kMaxJobErrorTextBytes)) {
    return internal::invalid("error_text_too_large", "failed event error text exceeds 512 bytes");
  }
  JobEvent event(JobEventKind::Failed, identity);
  event.error_ = std::move(error);
  return event;
}

Result<JobEvent> JobEvent::cancel_acknowledged(JobIdentity identity) {
  if (auto ok = internal::validate_job_identity(identity); !ok) {
    return unexpected(std::move(ok).error());
  }
  return JobEvent(JobEventKind::CancelAcknowledged, identity);
}

Result<JobEvent> JobEvent::released(JobIdentity identity) {
  if (auto ok = internal::validate_job_identity(identity); !ok) {
    return unexpected(std::move(ok).error());
  }
  return JobEvent(JobEventKind::Released, identity);
}

// -- JobEventSequence. --------------------------------------------------------

JobEventSequence::JobEventSequence(const JobRequest& request) noexcept
    : identity_(request.identity()),
      result_kind_(result_kind_for(request.job_class())),
      progress_limit_(request.progress_limit()) {
  if (const auto* frames = std::get_if<VadFrames>(&request.payload())) {
    vad_frames_ = frames->frame_count;
  }
}

Result<void> JobEventSequence::check_result(const JobEvent& event, Totals& totals) const {
  if (!accepted_) {
    return internal::refused("not_accepted", "result before accepted");
  }
  const bool progress = event.kind() == JobEventKind::Progress;
  if (progress && cancel_acknowledged_) {
    return internal::refused("progress_after_cancel_acknowledged",
                             "progress after cancel_acknowledged");
  }
  const std::optional<JobResult>& result = event.result();
  if (!result.has_value() || result->kind() != result_kind_) {
    return internal::refused("result_kind_mismatch", "result kind differs from the job's");
  }
  if (progress) {
    if (event.progress_sequence() != progress_count_ + 1) {
      return internal::refused("progress_sequence_gap", "progress_sequence is not consecutive");
    }
    if (progress_count_ + 1 > progress_limit_) {
      return internal::refused("progress_limit_exceeded", "more progress than the job allows");
    }
  }
  const ResultSize added = size_of(*result);
  totals.text_bytes += added.text_bytes;
  totals.word_text_bytes += added.word_text_bytes;
  totals.tokens += added.tokens;
  totals.pcm_bytes += added.pcm_bytes;
  totals.probabilities += added.probabilities;
  if (totals.text_bytes > kMaxTranscriptTextBytes ||
      totals.word_text_bytes > kMaxTranscriptTextBytes || totals.tokens > kMaxTranscriptTokens ||
      totals.pcm_bytes > kMaxAudioChunkBytes) {
    return internal::refused("result_total_exceeded", "job result exceeds a one-result ceiling");
  }
  if (result_kind_ == JobResultKind::Vad &&
      (totals.probabilities > vad_frames_ || (!progress && totals.probabilities != vad_frames_))) {
    return internal::refused("vad_result_count_mismatch",
                             "probabilities do not match the job's frames");
  }
  return {};
}

Result<void> JobEventSequence::observe(const JobEvent& event) {
  if (event.identity() != identity_) {
    return internal::refused("identity_mismatch", "event identity differs from the job's");
  }
  if (released_) {
    return internal::refused("event_after_released", "event after released");
  }
  const JobEventKind kind = event.kind();
  if (terminal_ && kind != JobEventKind::Released) {
    return internal::refused("event_after_terminal", "event after completed or failed");
  }

  // Candidate totals; committed only if the event is accepted.
  Totals totals = totals_;

  switch (kind) {
    case JobEventKind::Accepted:
      if (observed_any_) {
        return internal::refused("accepted_out_of_order", "accepted after another event");
      }
      break;
    case JobEventKind::Progress:
    case JobEventKind::Completed:
      if (auto ok = check_result(event, totals); !ok) {
        return ok;
      }
      break;
    case JobEventKind::Failed:
      break;
    case JobEventKind::CancelAcknowledged:
      if (!cancel_requested_) {
        return internal::refused("cancel_acknowledged_without_cancel",
                                 "cancel_acknowledged without a cancellation");
      }
      if (cancel_acknowledged_) {
        return internal::refused("duplicate_cancel_acknowledged",
                                 "cancel_acknowledged delivered twice");
      }
      break;
    case JobEventKind::Released:
      if (!terminal_) {
        return internal::refused("released_before_terminal", "released before completed or failed");
      }
      break;
  }

  observed_any_ = true;
  switch (kind) {
    case JobEventKind::Accepted:
      accepted_ = true;
      break;
    case JobEventKind::Progress:
      ++progress_count_;
      break;
    case JobEventKind::Completed:
    case JobEventKind::Failed:
      terminal_ = true;
      break;
    case JobEventKind::CancelAcknowledged:
      cancel_acknowledged_ = true;
      break;
    case JobEventKind::Released:
      released_ = true;
      break;
  }
  totals_ = totals;
  return {};
}

}  // namespace tensorplate
