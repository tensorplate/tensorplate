// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <grpcpp/support/status.h>

#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <utility>

#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/stream/v1/session.pb.h"

#include "serving/session/dispatch.hpp"
#include "serving/session/output_queue.hpp"

namespace tensorplate::serving::stream {

/// A server event queued without its sequence, which the stream assigns.
struct WireEvent final : OutputBody {
  explicit WireEvent(::tensorplate::stream::v1::ServerEvent event_in)
      : event(std::move(event_in)) {}
  ::tensorplate::stream::v1::ServerEvent event;
};

/// The queue item of a lifecycle event: Terminal for SessionClosed, else
/// Control. Pong, Accepted and SessionStatus carry a replace key each: the
/// newest unsent one says everything, so an unread stream holds at most one
/// of each and they never use up the budget kept for lifecycle messages.
[[nodiscard]] OutputItem lifecycle_item(::tensorplate::stream::v1::ServerEvent event);

/// The transport of one call, as its write path drives it: one write at a
/// time, each answered by CallOutput::write_done(), and nothing after finish().
class CallWriter {
 public:
  CallWriter() = default;
  CallWriter(const CallWriter&) = delete;
  CallWriter& operator=(const CallWriter&) = delete;
  virtual ~CallWriter() = default;

  /// `event` stays unchanged until the write is answered.
  virtual void start_write(const ::tensorplate::stream::v1::ServerEvent& event) = 0;
  virtual void finish(const grpc::Status& status) = 0;
};

/// The write path of one call. It sends a session's queued output in sequence
/// with one write in flight, confirms each delivery to the queue before the
/// next write starts, and ends the call once. Task output is encoded when it
/// is taken, and SessionClosed leaves with the totals of what was taken
/// before it.
///
/// Thread-safe. The writer is called with no lock held. This object outlives
/// the writer, which detach() takes away.
class CallOutput {
 public:
  /// Hears of each confirmed delivery, in the order written, before the next
  /// write starts. Called with no lock held.
  using Delivered = std::function<void(OutputKind)>;

  CallOutput(CallWriter& writer, const SchedulerClock& clock);

  /// The queue to send from, and the session's audio format, which its
  /// budgets were derived from: its byte rate is not zero. The first call
  /// wins.
  void bind(std::shared_ptr<BoundedOutputQueue> output, Delivered delivered,
            const StreamAudioFormat& format);
  [[nodiscard]] bool bound() const;

  /// The status the call ends with once its terminal event is written.
  void set_end_status(grpc::Status status);

  /// Starts the next write if none is in flight: the queue's consumer.
  void pump();

  /// Answers the write in flight; `ok` is false when the peer is gone, which
  /// ends the call as cancelled.
  void write_done(bool ok);

  /// Ends the call once no write is in flight, sending nothing more. The
  /// first status wins.
  void finish(grpc::Status status);

  /// The writer is gone; nothing is written or finished from now on.
  void detach();

 private:
  struct Step {
    enum class Kind : std::uint8_t { None, Write, Finish };
    Kind kind = Kind::None;
    CallWriter* writer = nullptr;
    grpc::Status status;
  };

  /// What to do next. Caller holds mu_ and runs the step unlocked.
  [[nodiscard]] Step next_locked();
  /// Fills buffer_ from a taken item. False for a body this server cannot
  /// send. Caller holds mu_.
  [[nodiscard]] bool encode_locked(OutputBody* body);
  void run(const Step& step);

  const SchedulerClock& clock_;
  mutable std::mutex mu_;
  CallWriter* writer_;
  std::shared_ptr<BoundedOutputQueue> output_;
  std::shared_ptr<const Delivered> delivered_;
  /// Held from a write's start until its delivery was confirmed and told.
  bool writing_ = false;
  OutputKind writing_kind_ = OutputKind::Control;
  std::uint64_t writing_sequence_ = 0;
  ::tensorplate::stream::v1::ServerEvent buffer_;
  StreamAudioFormat format_;
  /// Chunk samples, finals and segment completions taken for writing.
  std::uint64_t audio_samples_ = 0;
  std::uint32_t utterances_ = 0;
  std::uint32_t segments_ = 0;
  grpc::Status end_status_;
  std::optional<grpc::Status> finish_wanted_;
  bool finished_ = false;
};

}  // namespace tensorplate::serving::stream
