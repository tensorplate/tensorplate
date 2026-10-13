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

class RefusedCalls;

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
/// time, each answered by CallOutput::write_done(), and no write after
/// finish().
class CallWriter {
 public:
  CallWriter() = default;
  CallWriter(const CallWriter&) = delete;
  CallWriter& operator=(const CallWriter&) = delete;
  virtual ~CallWriter() = default;

  /// `event` stays unchanged until the write is answered. A writer that
  /// cannot send it may answer before this returns: CallOutput::finish(), then
  /// CallOutput::write_done(false).
  virtual void start_write(const ::tensorplate::stream::v1::ServerEvent& event) = 0;
  virtual void finish(const grpc::Status& status) = 0;
  /// Ends the call without waiting for the peer: a write in flight is
  /// answered as failed. It may follow finish(), and may run while another
  /// thread starts a write or finishes.
  virtual void cancel() = 0;
};

/// The write path of one call. It sends a session's queued output in sequence
/// with one write in flight, confirms each delivery to the queue before the
/// next write starts, and ends the call once. Task output is encoded when it
/// is taken, and SessionClosed leaves with the totals of what was taken
/// before it.
///
/// A reader that stops would hold its call for as long as it likes: a write
/// in flight completes only when the peer takes it, and nothing ends the call
/// before that. So once the session has ended the call is given the session's
/// no-progress limit, from the end and again from each write that completes,
/// and cancel_if_stalled() cancels one that outruns it.
///
/// Thread-safe. The writer is called with no lock a reaction takes. This
/// object outlives the writer, which detach() takes away.
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

  /// Ends a call that was never admitted. With a place among `refused` it
  /// ends with `status` and keeps the place until its writer is gone; with
  /// none it is cancelled, so its peer reads no status of this server's
  /// choosing. Nothing for a call that is ending already. `refused` outlives
  /// the writer.
  void refuse(RefusedCalls& refused, grpc::Status status);

  /// The writer is gone; nothing is written, finished or cancelled from now
  /// on. Waits for a cancel in progress.
  void detach();

  /// The session's terminal event was queued at `now`.
  void session_ended(SchedulerClock::TimePoint now);

  /// Cancels the call, once, if its session ended and it has neither
  /// completed a write nor lost its writer for the no-progress limit since.
  /// @return when to ask again; none once nothing is left to watch.
  [[nodiscard]] std::optional<SchedulerClock::TimePoint> cancel_if_stalled(
      SchedulerClock::TimePoint now);

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
  /// Held through a cancel so that detach() cannot take the writer from under
  /// it. Taken before mu_.
  std::mutex cancel_mu_;
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
  /// Since the session ended: when it did, or when a later write completed.
  std::optional<SchedulerClock::TimePoint> ended_progress_at_;
  bool cancelled_ = false;
  /// Where this call holds a place as a refused call, until detach().
  RefusedCalls* refused_ = nullptr;
};

}  // namespace tensorplate::serving::stream
