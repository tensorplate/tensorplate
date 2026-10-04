// SPDX-License-Identifier: Apache-2.0
#include "serving/session/credits.hpp"

#include <string>
#include <utility>

#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

namespace tensorplate::serving {
namespace {
Error invalid_budgets() {
  return Error::make(Error::Code::ConfigInvalid, "invalid session byte rate",
                     "invalid_session_budgets");
}

Error caller_defect(std::string message, std::string context) {
  return Error::make(Error::Code::Internal, std::move(message), std::move(context));
}
}  // namespace

Result<SessionBudgets> SessionBudgets::for_audio_input(std::uint64_t input_bytes_per_second) {
  const auto seconds = static_cast<std::uint64_t>(kInputAudioWindow.count());
  if (input_bytes_per_second == 0 || input_bytes_per_second > kMaxAudioFramesBytes / seconds) {
    return unexpected(invalid_budgets());
  }
  return SessionBudgets{SessionInputKind::Audio, input_bytes_per_second * seconds, 0, 0, 0};
}

Result<SessionBudgets> SessionBudgets::for_text_input(std::uint64_t output_bytes_per_second) {
  const auto seconds = static_cast<std::uint64_t>(kOutputPcmWindow.count());
  if (output_bytes_per_second == 0 || output_bytes_per_second > kMaxAudioChunkBytes / seconds) {
    return unexpected(invalid_budgets());
  }
  return SessionBudgets{SessionInputKind::Text, kWaitingTextBytes, kWaitingTextSegments,
                        kActiveTextSegments, output_bytes_per_second * seconds};
}

InputCredit::InputCredit(const SessionBudgets& budgets) noexcept : budgets_(budgets) {}

Result<void> InputCredit::charge(std::uint64_t bytes) {
  if (bytes == 0) {
    return unexpected(
        Error::make(Error::Code::ConfigInvalid, "input item carries no bytes", "empty_input"));
  }
  const bool text = budgets_.input_kind() == SessionInputKind::Text;
  if (bytes > budgets_.input_credit_bytes() - held_bytes_ ||
      (text && waiting_segments_.size() >= budgets_.waiting_items())) {
    return unexpected(Error::make(Error::Code::ResourceExhausted,
                                  "input exceeds the session's credit", "input_credit_exceeded"));
  }
  held_bytes_ += bytes;
  if (text) {
    waiting_segments_.push_back(bytes);
  } else {
    ++audio_chunks_;
  }
  return {};
}

Result<void> InputCredit::release_audio(std::uint32_t chunks, std::uint64_t bytes) {
  if (budgets_.input_kind() != SessionInputKind::Audio) {
    return unexpected(caller_defect("audio release on a text session", "wrong_input_kind"));
  }
  if (bytes == 0 || bytes > held_bytes_ || chunks > audio_chunks_ ||
      (bytes == held_bytes_) != (chunks == audio_chunks_)) {
    return unexpected(
        caller_defect("audio release does not match the input held", "input_release_mismatch"));
  }
  held_bytes_ -= bytes;
  audio_chunks_ -= chunks;
  return {};
}

Result<void> InputCredit::start_segment() {
  if (budgets_.input_kind() != SessionInputKind::Text) {
    return unexpected(caller_defect("text segment stage on an audio session", "wrong_input_kind"));
  }
  if (waiting_segments_.empty() || active_segments_ >= budgets_.active_items()) {
    return unexpected(caller_defect("no waiting segment or the active slot is occupied",
                                    "segment_stage_violation"));
  }
  held_bytes_ -= waiting_segments_.front();
  waiting_segments_.pop_front();
  ++active_segments_;
  return {};
}

Result<void> InputCredit::finish_segment() {
  if (budgets_.input_kind() != SessionInputKind::Text) {
    return unexpected(caller_defect("text segment stage on an audio session", "wrong_input_kind"));
  }
  if (active_segments_ == 0) {
    return unexpected(caller_defect("no active segment", "segment_stage_violation"));
  }
  --active_segments_;
  return {};
}

std::uint32_t InputCredit::depth() const noexcept {
  return audio_chunks_ + static_cast<std::uint32_t>(waiting_segments_.size()) + active_segments_;
}

LogicalSessionUsage InputCredit::usage() const {
  return LogicalSessionUsage::create(held_bytes_, budgets_.input_credit_bytes())
      .value_or(LogicalSessionUsage{});
}
}  // namespace tensorplate::serving
