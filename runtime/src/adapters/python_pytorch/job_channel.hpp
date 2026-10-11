// SPDX-License-Identifier: Apache-2.0
//
// The adapter's side of the `speech_jobs_v1` messages on one sidecar
// connection (protocol/schemas/python_pytorch_ipc.json): a reader thread that
// owns the socket's read side, a writer thread that owns its write side, the
// table of unreleased jobs and the slot a unary exchange waits in.
//
// Threads. The reader reads one frame at a time, routes it and calls the
// event sink; while the connection lives nothing else calls the sink. The
// writer sends whole frames in the order they were queued, and is the only
// thread that touches the socket's write side: not a caller's thread, and not
// the reader. submit(), cancel(), release_session() and exchange() only
// queue, so a sink callback may call the first three: the reader keeps
// reading whatever the adapter has to send. The sidecar requires that of its
// peer, because it writes a job message while holding the lock its own reader
// needs, and gives up a peer that stops reading.
//
// The queue the writer drains holds at most kMaxQueuedFrames frames: room for
// one `job_submit` and one `job_cancel` for each of kMaxOpenJobs unreleased
// jobs, one `session_release` for each of kMaxPendingSessionReleases
// sessions, and one unary request. A caller that finds the table, the pending
// releases or the queue full is refused with Error::Code::ResourceExhausted;
// nothing waits for room. The queue is counted by itself because a frame can
// outlive what it was queued for: a cancel whose job the sidecar released
// first, the request of an exchange that timed out.
//
// A write that fails, or a frame the sidecar does not take within the write
// bound, ends the connection: the writer records the error, shuts the socket
// down and leaves, and the reader, woken by that, fails every job without a
// terminal event with kBackendReset. An end the reader decides (end of file, a
// read error, a header it cannot read) shuts the socket down as well: the
// sidecar reads end of file and a writer inside a frame is woken.
//
// stop() ends the connection with shutdown(2) on the socket's descriptor,
// which wakes the reader out of its wait and the writer out of a write the
// sidecar is not taking; a writer that waits for a frame is woken by the
// reader as it leaves. stop() joins the reader, then the writer. What was
// still queued is dropped, never silently: the reader, on its way out, fails
// every job without a terminal event, a queued `session_release` is
// acknowledged once the process is reaped, and a waiting exchange gets the
// error the connection ended with.
//
// One mutex guards all state. It is never held across socket I/O or a sink
// callback. submit() copies the request's PCM window while holding it: the
// channel's mutex, then the buffer manager's, which never calls back.
//
// Internal: nothing outside the adapter includes this header.

#pragma once

#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

#include "job_wire.hpp"

namespace tensorplate::adapters::python_pytorch {

/// Most jobs the channel holds unreleased; the sidecar runs one and lets
/// eight wait. It bounds the message-id correlation as well.
inline constexpr std::size_t kMaxOpenJobs = 16;

/// Most sessions whose release the sidecar has not acknowledged yet.
inline constexpr std::size_t kMaxPendingSessionReleases = 64;

/// Most frames that wait for the writer.
inline constexpr std::size_t kMaxQueuedFrames = 2 * kMaxOpenJobs + kMaxPendingSessionReleases + 1;

/// Sessions whose release the sidecar acknowledged that the channel still
/// remembers, oldest forgotten first. The sidecar forgets a session it
/// released, so a release repeated for a remembered one is not sent again.
///
/// That memory is the whole of the channel's "once per session": after this
/// many later acknowledgements the session is forgotten here too, and the
/// channel takes its key as a new session's. A repeated release is then sent,
/// answered and delivered again, and a job submitted for it is taken. Holding
/// "at most once" beyond the memory is the caller's, which knows its sessions.
inline constexpr std::size_t kMaxRememberedSessions = 1'024;

/// Error::context of the `failed` a job gets when its connection ends.
inline constexpr std::string_view kBackendReset = "backend_reset";
inline constexpr std::string_view kSessionUnloaded = "session_unloaded";

/// Sidecar messages the channel dropped.
struct JobChannelCounters {
  /// About a job or a session, and changed nothing: the channel does not hold
  /// it, the job already had its terminal event, or the message answered a
  /// cancel.
  std::uint64_t dropped_job_messages = 0;
  /// About nothing the channel can name: a job message without a readable
  /// job, a refusal of a message it does not know, a frame no exchange waits
  /// for.
  std::uint64_t dropped_other_messages = 0;

  friend bool operator==(const JobChannelCounters&, const JobChannelCounters&) noexcept = default;
};

class JobChannel {
 public:
  using TimePoint = ipc::UnixSocket::TimePoint;

  /// @param manager        where PCM is read from and published to; null
  ///                       refuses every job.
  /// @param write_timeout  longest one frame may take to write before the
  ///                       connection is given up.
  JobChannel(BufferManager* manager, std::chrono::milliseconds write_timeout);
  ~JobChannel() = default;

  JobChannel(const JobChannel&) = delete;
  JobChannel& operator=(const JobChannel&) = delete;
  JobChannel(JobChannel&&) = delete;
  JobChannel& operator=(JobChannel&&) = delete;

  // -- The session's side: one thread at a time, never a sink callback. ------

  /// Takes over `socket` after a load that enabled kSpeechJobsCapability and
  /// declared `job_classes`. The socket stays the caller's and must outlive
  /// stop(). Jobs are refused until admit_jobs().
  ///
  /// @return Error::Code::Internal when the channel is already started, with
  ///   "socket_closed" when `socket` is not open, or with
  ///   "job_channel_thread_unavailable" when a thread cannot be created.
  [[nodiscard]] Result<void> start(ipc::UnixSocket& socket,
                                   const std::vector<JobClass>& job_classes);

  /// The backend can run jobs: submit() takes them from here on.
  void admit_jobs() noexcept;

  /// The backend is about to unload: submit() refuses from here on. cancel()
  /// and release_session() still reach the sidecar, which acts on them while
  /// an unreleased job remains.
  void close_admission() noexcept;

  /// Sends a unary request and waits for its response: the next frame that is
  /// not a job message, or the `error_event` that names `message_id`. One
  /// exchange at a time. A response is not matched to its request: after
  /// Error::Code::Timeout a late one would answer the next exchange, so the
  /// caller stops the channel before it exchanges again.
  ///
  /// @return the response frame, or Error::Code::Timeout at `deadline`, the
  ///   error the connection ended with, Error::Code::NotReady when the
  ///   channel is not started, Error::Code::Internal when another exchange
  ///   is waiting, Error::Code::ResourceExhausted, "send_queue_full".
  [[nodiscard]] Result<ipc::SidecarFrame> exchange(const ipc::SidecarFrame& request,
                                                   std::string_view message_id, TimePoint deadline);

  /// Ends the connection with shutdown(2) on the socket's descriptor and joins
  /// the reader, then the writer. Every job without a terminal event fails with
  /// Error::Code::Unavailable and `reason`, unless the connection had already
  /// ended (then kBackendReset). `reap` then runs, and once it has returned
  /// (the sidecar process is gone) every job is released and every pending
  /// session release is acknowledged, on the calling thread. Waits for a sink
  /// callback that is running. Does nothing but call `reap` when the channel
  /// is not started.
  ///
  /// A started channel is stopped before it is destroyed: it holds two
  /// joinable threads.
  void stop(std::string_view reason, const std::function<void()>& reap);

  // -- The job side: any thread, a sink callback included. -------------------

  /// @return success, or Error::Code::ConfigInvalid with reason
  ///   "event_sink_null" or "event_sink_already_set".
  [[nodiscard]] Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink);

  /// Queues a `job_submit`. The job's events follow through the sink.
  ///
  /// @return success, or the first of these that applies:
  ///   Error::Code::NotReady, "event_sink_missing";
  ///   Error::Code::NotReady, "backend_not_ready" (before admit_jobs(), or
  ///   without a buffer manager);
  ///   Error::Code::Unavailable, "backend_unavailable" (admission closed, the
  ///   connection ended or the channel stopped);
  ///   Error::Code::Unsupported, "job_class_unsupported" (not `stt_decode`
  ///   or `tts_synthesis`, or not among the load's job classes);
  ///   Error::Code::NotReady, "session_releasing";
  ///   Error::Code::ConfigInvalid, "duplicate_job_id";
  ///   Error::Code::ResourceExhausted, "job_capacity_exhausted" (kMaxOpenJobs
  ///   are unreleased);
  ///   Error::Code::ResourceExhausted, "send_queue_full";
  ///   encode_job_submit()'s refusal;
  ///   Error::Code::ConfigInvalid, "pcm_buffer_invalid" (the buffer manager
  ///   does not hold the request's window); ipc::encode_frame()'s refusal.
  [[nodiscard]] Result<void> submit(const JobRequest& request);

  /// Queues a `job_cancel`, once per job; a repeat, or a job that already has
  /// its terminal event, changes nothing. The sidecar answers nothing for a
  /// job it no longer knows, so nothing waits for an acknowledgement.
  ///
  /// @return success, or Error::Code::NotReady, "unknown_job";
  ///   Error::Code::ResourceExhausted, "send_queue_full".
  [[nodiscard]] Result<void> cancel(std::uint64_t job_id);

  /// Queues a `session_release`, once per session key; a repeat while it is
  /// pending or remembered changes nothing, and one for a session the channel
  /// has forgotten (kMaxRememberedSessions) is a new release. The sink's
  /// on_session_released follows the `released` of the session's last job. A
  /// sidecar whose jobs an unload has closed refuses the release; that refusal
  /// is counted and the release stays pending until stop() acknowledges it.
  ///
  /// @return success, or Error::Code::NotReady, "event_sink_missing";
  ///   Error::Code::Unavailable, "backend_unavailable" (not started, or the
  ///   connection ended); Error::Code::ResourceExhausted,
  ///   "session_release_capacity_exhausted" (kMaxPendingSessionReleases are
  ///   unacknowledged) or "send_queue_full"; or encode_session_release()'s
  ///   refusal of a zero key or generation.
  [[nodiscard]] Result<void> release_session(std::uint64_t session_key, std::uint64_t generation);

  [[nodiscard]] JobChannelCounters counters() const;

 private:
  enum class Phase : std::uint8_t { Idle, Started, Admitting, Closing, Stopped };

  struct Job {
    JobIdentity identity;
    JobEventSequence sequence;
    std::string submit_message_id;
    /// Empty until a `job_cancel` was queued.
    std::string cancel_message_id;
  };

  struct PendingRelease {
    std::uint64_t generation = 0;
    bool acknowledged = false;
  };

  // One sink call: a job event, or a session release.
  using Delivery = std::variant<JobEvent, SessionReleased>;

  // An exchange in its wait. It lives on the waiting thread's stack, so
  // nothing that ends or restarts the connection can take its answer away.
  struct UnaryWaiter {
    std::string_view message_id;
    std::optional<Result<ipc::SidecarFrame>> outcome;
  };

  void read_loop();
  void write_loop();
  // Routes one frame; false when it ends the connection.
  [[nodiscard]] bool route(ipc::SidecarFrame frame);
  void end_connection(const Error& error);
  void deliver(const std::vector<Delivery>& deliveries);

  [[nodiscard]] bool live_locked() const noexcept;
  [[nodiscard]] bool queue_full_locked() const noexcept;
  // The `*_locked` members below need mutex_ held and append what the sink is
  // owed to `out`.
  // True when `event` itself was appended.
  bool admit_locked(const JobEvent& event, std::vector<Delivery>& out);
  static void fail_locked(Job& job, const Error& error, std::vector<Delivery>& out);
  void release_locked(std::map<std::uint64_t, Job>::iterator job, std::vector<Delivery>& out);
  // Drops a released job and what named it, and settles its session.
  void forget_locked(std::map<std::uint64_t, Job>::iterator job, std::vector<Delivery>& out);
  void settle_locked(std::uint64_t session_key, std::vector<Delivery>& out);
  void acknowledge_locked(std::map<std::uint64_t, PendingRelease>::iterator pending,
                          std::vector<Delivery>& out);
  void refuse_locked(const MessageRefused& refused, ipc::SidecarFrame&& frame,
                     std::vector<Delivery>& out);
  // Answers the waiting exchange with `frame`; `message_id` is the request an
  // `error_event` names, null for any other frame.
  void offer_unary_locked(ipc::SidecarFrame&& frame, const std::string* message_id);
  void queue_locked(std::vector<std::byte> frame);

  BufferManager* const manager_;
  const std::chrono::milliseconds write_timeout_;

  mutable std::mutex mutex_;
  std::condition_variable outbox_cv_;
  std::condition_variable unary_cv_;

  Phase phase_ = Phase::Idle;
  /// The reader left its loop; set until stop() has finished.
  bool ended_ = false;
  /// stop() is ending the connection: its reason is the jobs' failure.
  bool stopping_ = false;
  std::string stop_reason_;
  /// What the connection ended with; set while ended_ is.
  std::optional<Error> failure_;
  /// The error of the write that failed, when one did.
  std::optional<Error> write_failure_;

  ipc::UnixSocket* socket_ = nullptr;
  std::vector<JobClass> job_classes_;
  std::shared_ptr<JobEventSink> sink_;

  std::map<std::uint64_t, Job> jobs_;
  /// The adapter's `message_id` of each unreleased job's submit and cancel.
  std::map<std::string, std::uint64_t, std::less<>> message_jobs_;
  std::map<std::uint64_t, PendingRelease> releasing_;
  std::deque<std::uint64_t> remembered_sessions_;

  std::deque<std::vector<std::byte>> outbox_;

  /// The exchange that waits for its answer; null once it has one.
  UnaryWaiter* unary_ = nullptr;

  JobChannelCounters counters_;

  std::thread reader_;
  std::thread writer_;
};

}  // namespace tensorplate::adapters::python_pytorch
