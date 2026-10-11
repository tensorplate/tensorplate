// SPDX-License-Identifier: Apache-2.0
#include "serving/stream/stream_output.hpp"

#include <algorithm>

#include "serving/stream/stream_mapping.hpp"

namespace tensorplate::serving::stream {
namespace {

/// Replace keys of the lifecycle replies whose newest instance says everything.
constexpr std::uint64_t kPongReplaceKey = 1;
constexpr std::uint64_t kAcceptedReplaceKey = 2;
constexpr std::uint64_t kStatusReplaceKey = 3;

}  // namespace

OutputItem lifecycle_item(wire::ServerEvent event) {
  auto kind = OutputKind::Control;
  std::uint64_t replace_key = 0;
  switch (event.body_case()) {
    case wire::ServerEvent::kSessionClosed:
      kind = OutputKind::Terminal;
      break;
    case wire::ServerEvent::kPong:
      replace_key = kPongReplaceKey;
      break;
    case wire::ServerEvent::kAccepted:
      replace_key = kAcceptedReplaceKey;
      break;
    case wire::ServerEvent::kStatus:
      replace_key = kStatusReplaceKey;
      break;
    default:
      break;
  }
  const auto bytes = std::max<std::uint64_t>(1, event.ByteSizeLong());
  return OutputItem{kind, bytes, replace_key, std::make_unique<WireEvent>(std::move(event))};
}

CallOutput::CallOutput(CallWriter& writer, const SchedulerClock& clock)
    : clock_(clock), writer_(&writer) {}

void CallOutput::bind(std::shared_ptr<BoundedOutputQueue> output, Delivered delivered,
                      const StreamAudioFormat& format) {
  const std::lock_guard guard(mu_);
  if (output_) {
    return;
  }
  output_ = std::move(output);
  format_ = format;
  delivered_ = std::make_shared<const Delivered>(std::move(delivered));
}

bool CallOutput::bound() const {
  const std::lock_guard guard(mu_);
  return output_ != nullptr;
}

void CallOutput::set_end_status(grpc::Status status) {
  const std::lock_guard guard(mu_);
  end_status_ = std::move(status);
}

void CallOutput::pump() {
  Step step;
  {
    const std::lock_guard guard(mu_);
    step = next_locked();
  }
  run(step);
}

void CallOutput::write_done(bool ok) {
  // Read first: the clock is not called under the lock.
  const auto now = clock_.now();
  OutputKind kind = OutputKind::Control;
  bool confirmed = false;
  std::shared_ptr<const Delivered> delivered;
  {
    const std::lock_guard guard(mu_);
    kind = writing_kind_;
    if (ok && output_) {
      confirmed = output_->delivered(writing_sequence_, now).has_value();
    }
    delivered = delivered_;
  }
  // writing_ still holds, so no later write can be confirmed or told first.
  if (confirmed && delivered && *delivered) {
    (*delivered)(kind);
  }
  Step step;
  {
    const std::lock_guard guard(mu_);
    writing_ = false;
    if (ok && ended_progress_at_) {
      ended_progress_at_ = now;
    }
    if (!finish_wanted_) {
      if (!ok) {
        finish_wanted_ = grpc::Status::CANCELLED;
      } else if (kind == OutputKind::Terminal) {
        finish_wanted_ = end_status_;
      }
    }
    step = next_locked();
  }
  run(step);
}

void CallOutput::finish(grpc::Status status) {
  Step step;
  {
    const std::lock_guard guard(mu_);
    if (!finish_wanted_) {
      finish_wanted_ = std::move(status);
    }
    step = next_locked();
  }
  run(step);
}

void CallOutput::detach() {
  const std::lock_guard cancelling(cancel_mu_);
  const std::lock_guard guard(mu_);
  writer_ = nullptr;
}

void CallOutput::session_ended(SchedulerClock::TimePoint now) {
  const std::lock_guard guard(mu_);
  if (!ended_progress_at_) {
    ended_progress_at_ = now;
  }
}

std::optional<SchedulerClock::TimePoint> CallOutput::cancel_if_stalled(
    SchedulerClock::TimePoint now) {
  const std::lock_guard cancelling(cancel_mu_);
  CallWriter* writer = nullptr;
  {
    const std::lock_guard guard(mu_);
    if (!ended_progress_at_ || writer_ == nullptr || cancelled_) {
      return std::nullopt;
    }
    const auto due = *ended_progress_at_ + SessionBudgets::kOutputNoProgressTimeout;
    if (now < due) {
      return due;
    }
    cancelled_ = true;
    writer = writer_;
  }
  // Outside mu_, which the reactions take; cancel_mu_ keeps the writer.
  writer->cancel();
  return std::nullopt;
}

CallOutput::Step CallOutput::next_locked() {
  if (writer_ == nullptr || writing_ || finished_) {
    return {};
  }
  if (!finish_wanted_ && output_) {
    if (auto next = output_->take()) {
      if (!encode_locked(next->item.body.get())) {
        finish_wanted_ = grpc::Status(grpc::StatusCode::INTERNAL, "output this server cannot send");
      } else {
        buffer_.set_sequence(next->sequence);
        writing_ = true;
        writing_kind_ = next->item.kind;
        writing_sequence_ = next->sequence;
        return {Step::Kind::Write, writer_, {}};
      }
    }
  }
  if (finish_wanted_) {
    finished_ = true;
    return {Step::Kind::Finish, writer_, *finish_wanted_};
  }
  return {};
}

bool CallOutput::encode_locked(OutputBody* body) {
  if (auto* lifecycle = dynamic_cast<WireEvent*>(body)) {
    buffer_ = std::move(lifecycle->event);
    if (buffer_.has_session_closed()) {
      // Everything taken before it was written: a failed write ends the call
      // without this event.
      auto& totals = *buffer_.mutable_session_closed()->mutable_totals();
      totals.set_output_audio_samples(audio_samples_);
      totals.set_utterances_completed(utterances_);
      totals.set_segments_completed(segments_);
    }
    return true;
  }
  const auto* task = dynamic_cast<const TaskOutputBody*>(body);
  if (task == nullptr) {
    return false;
  }
  auto event = task_event(task->output, format_);
  if (!event) {
    return false;
  }
  buffer_ = std::move(*event);
  if (buffer_.has_audio_chunk()) {
    audio_samples_ += buffer_.audio_chunk().data().size() /
                      (std::uint64_t{format_.bytes_per_sample()} * format_.channels);
  } else if (buffer_.has_final_transcript()) {
    ++utterances_;
  } else if (buffer_.has_segment_completed()) {
    ++segments_;
  }
  return true;
}

void CallOutput::run(const Step& step) {
  switch (step.kind) {
    case Step::Kind::Write:
      step.writer->start_write(buffer_);
      break;
    case Step::Kind::Finish:
      step.writer->finish(step.status);
      break;
    case Step::Kind::None:
      break;
  }
}

}  // namespace tensorplate::serving::stream
