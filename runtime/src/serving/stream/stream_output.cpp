// SPDX-License-Identifier: Apache-2.0
#include "serving/stream/stream_output.hpp"

namespace tensorplate::serving::stream {

CallOutput::CallOutput(CallWriter& writer, const SchedulerClock& clock)
    : clock_(clock), writer_(&writer) {}

void CallOutput::bind(std::shared_ptr<BoundedOutputQueue> output, Delivered delivered) {
  const std::lock_guard guard(mu_);
  if (output_) {
    return;
  }
  output_ = std::move(output);
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
  const std::lock_guard guard(mu_);
  writer_ = nullptr;
}

CallOutput::Step CallOutput::next_locked() {
  if (writer_ == nullptr || writing_ || finished_) {
    return {};
  }
  if (!finish_wanted_ && output_) {
    if (auto next = output_->take()) {
      auto* body = dynamic_cast<WireEvent*>(next->item.body.get());
      if (body == nullptr) {
        finish_wanted_ = grpc::Status(grpc::StatusCode::INTERNAL, "output this server cannot send");
      } else {
        buffer_ = std::move(body->event);
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
