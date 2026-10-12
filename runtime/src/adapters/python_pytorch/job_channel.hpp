// SPDX-License-Identifier: Apache-2.0
//
// The adapter's side of the `speech_jobs_v1` messages on one sidecar
// connection (protocol/schemas/python_pytorch_ipc.json): a reader thread that
// owns the socket's read side, a writer thread that owns its write side, the
// table of unreleased jobs, the slot a unary exchange waits in and the health
// probe that is answered beside it.
//
// Threads. The reader reads one frame at a time, routes it and calls the
// event sink; while the connection lives nothing else calls the sink. The
// writer sends whole frames in the order they were queued, and is the only
// thread that touches the socket's write side: not a caller's thread, and not
// the reader. submit(), cancel(), release_session(), exchange() and
// probe_health() only queue, so a sink callback may call the first three: the
// reader keeps reading whatever the adapter has to send. The sidecar requires
// that of its peer, because it writes a job message while holding the lock
// its own reader needs, and gives up a peer that stops reading.
//
// The queue the writer drains is exact. A frame is queued for a record (a job
// in the table, a pending session release, the exchange that waits, the
// health probe that is unanswered), at most once per kind of frame, and
// leaves the queue with its record. So the queue never holds more than
// kMaxQueuedFrames frames and nobody is refused or waits for room in it: only
// the job table and the pending releases refuse a caller, with
// Error::Code::ResourceExhausted. The writer takes a frame out before its
// first byte, so a frame it has begun is written whole. Frames leave in the
// order they were queued: a cancel waits behind earlier submits.
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
// acknowledged once the process is reaped, a waiting exchange gets the error
// the connection ended with, and a health probe ends as unavailable.
//
// One mutex guards all state. It is never held across socket I/O or a sink
// callback. submit() copies the request's PCM window twice while holding it,
// out of the buffer manager and then into the encoded frame: the channel's
// mutex, then the buffer manager's, which never calls back.
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
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

#include "job_wire.hpp"

namespace tensorplate::adapters::python_pytorch {

/// Most jobs the channel holds unreleased; the sidecar runs one and lets
/// eight wait. It bounds the message-id correlation as well.
inline constexpr std::size_t kMaxOpenJobs = 16;

/// Most sessions whose release the sidecar has not acknowledged yet: the
/// protocol's ceiling on logical sessions. A session manager keeps a
/// session's slot until its release is acknowledged, so it never has more
/// pending; only a caller that is not bounded that way finds this full.
inline constexpr std::size_t kMaxPendingSessionReleases = 2'048;

/// Most frames of each kind that wait for the writer: a `job_submit` and a
/// `job_cancel` for each job in the table, a `session_release` for each
/// pending release, the request of the one exchange that waits and that of
/// the one health probe that is unanswered.
inline constexpr std::size_t kMaxQueuedSubmits = kMaxOpenJobs;
inline constexpr std::size_t kMaxQueuedCancels = kMaxOpenJobs;
inline constexpr std::size_t kMaxQueuedReleases = kMaxPendingSessionReleases;
inline constexpr std::size_t kMaxQueuedRequests = 2;

/// Most frames that wait for the writer.
inline constexpr std::size_t kMaxQueuedFrames =
    kMaxQueuedSubmits + kMaxQueuedCancels + kMaxQueuedReleases + kMaxQueuedRequests;
static_assert(kMaxQueuedFrames == 2 * kMaxOpenJobs + kMaxPendingSessionReleases + 2,
              "the queue has a place for every frame the job table, the pending releases, the "
              "waiting exchange and the unanswered health probe can own, and no other");

/// Every session key that counts as released, as runs of consecutive keys: one
/// whose release was acknowledged, and one whose release was refused because
/// the connection had gone. The sidecar forgets a session it released, so
/// this record is what keeps a repeated release from being sent and a job
/// from being taken for a released session.
///
/// Its size is its number of runs: at most the keys below the highest
/// recorded one that are not in it, plus one. With keys issued in increasing
/// order those are the sessions that still hold a slot and the keys never
/// released through a channel that had been started; the adapter's page says
/// which.
class ReleasedSessionKeys {
 public:
  void insert(std::uint64_t key);
  [[nodiscard]] bool contains(std::uint64_t key) const noexcept;
  [[nodiscard]] std::size_t runs() const noexcept { return runs_.size(); }

 private:
  /// A run's first key to its last.
  std::map<std::uint64_t, std::uint64_t> runs_;
};

/// Largest payload of a frame the reader takes: the longest synthesized
/// segment a job returns (kMaxAudioChunkBytes), which no frame the sidecar
/// sends a started channel exceeds. A frame that declares more ends the
/// connection, as an unreadable header does; its body is neither allocated
/// nor read.
inline constexpr std::size_t kMaxJobFramePayloadBytes = kMaxAudioChunkBytes;

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
  /// for, the answer to a health probe after its last caller gave up.
  std::uint64_t dropped_other_messages = 0;

  friend bool operator==(const JobChannelCounters&, const JobChannelCounters&) noexcept = default;
};

/// What the channel still owes its sink, for tests and diagnostics: nothing
/// in the adapter decides anything from it.
struct JobChannelBacklog {
  /// Jobs without `released`.
  std::size_t open_jobs = 0;
  /// Session releases without their acknowledgement.
  std::size_t pending_releases = 0;
  /// Sink calls decided whose batch has not returned: those of one frame, of
  /// the connection's end, or of a stop.
  std::size_t owed_callbacks = 0;

  friend bool operator==(const JobChannelBacklog&, const JobChannelBacklog&) noexcept = default;
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

  // -- The session's side: never from a sink callback. One call at a time,
  // except that stop() may run on another thread while an exchange waits;
  // that exchange returns the error the connection ended with.

  /// Takes over `socket` after a load that enabled kSpeechJobsCapability and
  /// declared `job_classes`. The socket stays the caller's and must outlive
  /// stop(). Jobs are refused until admit_jobs().
  ///
  /// @return Error::Code::Internal when the channel is already started, with
  ///   "job_channel_retired" after retire(), with "socket_closed" when
  ///   `socket` is not open, or with
  ///   "job_channel_thread_unavailable" when a thread cannot be created;
  ///   Error::Code::Unsupported, "job_class_unsupported" when `job_classes`
  ///   holds neither class a lane runs (`stt_decode`, `tts_synthesis`).
  [[nodiscard]] Result<void> start(ipc::UnixSocket& socket,
                                   const std::vector<JobClass>& job_classes);

  /// The backend can run jobs: submit() takes them from here on.
  void admit_jobs() noexcept;

  /// The backend is about to unload: submit() refuses from here on and a
  /// probe_health() that waits returns. cancel() and release_session() still
  /// reach the sidecar, which acts on them while an unreleased job remains. A
  /// job the sidecar fails as unavailable ("backend_unavailable") from here
  /// on, which is how it ends a job that waits when the unload arrives,
  /// reaches the sink with kSessionUnloaded.
  void close_admission() noexcept;

  /// Sends a unary request and waits for its response: the next frame that is
  /// neither a job message nor the health probe's answer, or the `error_event`
  /// that names `message_id`. One exchange at a time. A response is not
  /// matched to its request: after Error::Code::Timeout a late one would
  /// answer the next exchange, so the caller stops the channel before it
  /// exchanges again. A request the writer had not begun when its exchange
  /// ended is not sent.
  ///
  /// @return the response frame, or Error::Code::Timeout at `deadline`, the
  ///   error the connection ended with, Error::Code::NotReady when the
  ///   channel is not started, Error::Code::Internal when another exchange
  ///   is waiting.
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

  /// The session is gone. One-way, whatever the channel's state: from here on
  /// start() is refused, and submit(), release_session() and probe_health()
  /// answer "backend_unavailable" ahead of every other check of that state,
  /// the buffer manager's included; a probe_health() that waits returns. Ends
  /// nothing: the session calls stop() first, and a channel retired while it
  /// runs still delivers its jobs' events until stop().
  void retire() noexcept;

  // -- The job side: any thread, a sink callback included. -------------------

  /// @return success, or Error::Code::ConfigInvalid with reason
  ///   "event_sink_null" or "event_sink_already_set".
  [[nodiscard]] Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink);

  /// Queues a `job_submit`. The job's events follow through the sink.
  ///
  /// @return success, or the first of these that applies:
  ///   Error::Code::NotReady, "event_sink_missing";
  ///   Error::Code::Unavailable, "backend_unavailable" (after retire());
  ///   Error::Code::NotReady, "backend_not_ready" (before admit_jobs(), or
  ///   without a buffer manager);
  ///   Error::Code::Unavailable, "backend_unavailable" (admission closed, the
  ///   connection ended or the channel stopped);
  ///   Error::Code::Unsupported, "job_class_unsupported" (not `stt_decode`
  ///   or `tts_synthesis`, or not among the load's job classes);
  ///   Error::Code::NotReady, "session_releasing";
  ///   Error::Code::ConfigInvalid, "duplicate_job_id";
  ///   Error::Code::ResourceExhausted, "job_capacity_exhausted" (kMaxOpenJobs
  ///   are unreleased; a job in the table has its place in the queue);
  ///   encode_job_submit()'s refusal;
  ///   Error::Code::ConfigInvalid, "pcm_buffer_invalid" (the buffer manager
  ///   does not hold the request's window); ipc::encode_frame()'s refusal.
  [[nodiscard]] Result<void> submit(const JobRequest& request);

  /// Queues a `job_cancel`, once per job; a repeat, or a job that already has
  /// its terminal event, changes nothing. The sidecar answers nothing for a
  /// job it no longer knows, so nothing waits for an acknowledgement. Never
  /// refused for room: every job in the table has a place for its cancel. The
  /// cancel is sent in its turn, behind the frames queued before it.
  ///
  /// @return success, or Error::Code::NotReady, "unknown_job".
  [[nodiscard]] Result<void> cancel(std::uint64_t job_id);

  /// Queues a `session_release`, once per session key for as long as the
  /// channel lives: a repeat changes nothing, while the release is pending and
  /// after it was acknowledged (ReleasedSessionKeys). The sink's
  /// on_session_released follows the `released` of the session's last job. A
  /// sidecar whose jobs an unload has closed refuses the release; that refusal
  /// is counted and the release stays pending until stop() acknowledges it.
  /// Never refused for room in the queue: every pending release has a place.
  ///
  /// @return success, or Error::Code::NotReady, "event_sink_missing";
  ///   Error::Code::Unavailable, "backend_unavailable" (retired, not
  ///   started, or the connection ended: the caller takes the session as
  ///   released, and a channel that was ever started and is not retired
  ///   records it so, unless the key or the generation is one
  ///   encode_session_release() refuses);
  ///   Error::Code::ResourceExhausted,
  ///   "session_release_capacity_exhausted" (kMaxPendingSessionReleases are
  ///   unacknowledged); or encode_session_release()'s refusal of a zero key
  ///   or generation.
  [[nodiscard]] Result<void> release_session(std::uint64_t session_key, std::uint64_t generation);

  // -- Health: any thread but one inside a sink callback. ----------------------

  /// Reports whether the backend can run jobs now. While the channel admits
  /// jobs it asks the sidecar with a `health_check` of its own, answered by
  /// `message_id` beside whatever exchange waits. At most one such probe is
  /// unanswered: a call made meanwhile waits for that one's answer, and a
  /// caller that gives up at `deadline` leaves it standing. Its answer,
  /// however late, is then dropped by its id and never taken for an
  /// exchange's, and until it comes or the connection ends no other probe is
  /// sent.
  ///
  /// @return success, or Error::Code::NotReady, "backend_not_ready" (as
  ///   submit() answers it, or the sidecar reports that it is not ready);
  ///   Error::Code::Unavailable, "backend_unavailable" (as submit() answers
  ///   it, also when that became true while the call waited; or the answer
  ///   cannot be read as one, or is an `error_event`);
  ///   Error::Code::Timeout, "health_timeout" at `deadline`;
  ///   ipc::encode_frame()'s refusal.
  [[nodiscard]] Result<void> probe_health(TimePoint deadline);

  /// Callers that wait in probe_health().
  [[nodiscard]] std::size_t health_waiters() const;

  [[nodiscard]] JobChannelCounters counters() const;

  /// A reading under the channel's lock that changes nothing.
  [[nodiscard]] JobChannelBacklog backlog() const;

  /// Runs of consecutive keys in the record of released sessions.
  [[nodiscard]] std::size_t released_session_runs() const;

  /// Frames that wait for the writer; the one it is writing is not among them.
  [[nodiscard]] std::size_t queued_frames() const;

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

  // What a queued frame was queued for; `key` is the job's id, the session's
  // key, or zero for the exchange and for the probe.
  enum class QueuedFor : std::uint8_t { Submit, Cancel, Release, Exchange, Probe };
  struct QueuedFrame {
    QueuedFor owner;
    std::uint64_t key;
    std::vector<std::byte> bytes;
  };

  // One sink call: a job event, or a session release.
  using Delivery = std::variant<JobEvent, SessionReleased>;

  // An exchange in its wait. It lives on the waiting thread's stack, so
  // nothing that ends or restarts the connection can take its answer away.
  struct UnaryWaiter {
    std::string_view message_id;
    std::optional<Result<ipc::SidecarFrame>> outcome;
  };

  // The `health_check` that is unanswered. Its callers share it, and it stands
  // until a frame carries its id or the connection ends, whoever still waits.
  struct HealthProbe {
    std::string message_id;
    std::size_t waiting = 0;
    std::optional<Result<void>> outcome;
  };

  void read_loop();
  void write_loop();
  // Routes one frame; false when it ends the connection.
  [[nodiscard]] bool route(ipc::SidecarFrame frame);
  void end_connection(const Error& error);
  void deliver(const std::vector<Delivery>& deliveries);

  [[nodiscard]] bool live_locked() const noexcept;
  // Why no job can run now, in the order submit() refuses; none when one can.
  [[nodiscard]] std::optional<Error> not_running_locked() const;
  [[nodiscard]] bool names_probe_locked(std::string_view message_id) const noexcept;
  // Ends the probe that stands with `outcome` and wakes its callers.
  void end_probe_locked(Result<void> outcome);
  // As end_probe_locked(), for a frame that carried the probe's id.
  void answer_probe_locked(Result<void> outcome);
  // The `failed` the sink gets in `event`'s place when the sidecar failed a job
  // because an unload was closing its lane; none for any other event.
  [[nodiscard]] std::optional<JobEvent> unloaded_locked(const JobEvent& event) const;
  // Counts the batch a caller is about to hand to deliver().
  void owe_locked(const std::vector<Delivery>& out) noexcept;
  // The `*_locked` members below need mutex_ held and append what the sink is
  // owed to `out`.
  // True when `received`, or the failure that stands in for it, was appended.
  bool admit_locked(const JobEvent& received, std::vector<Delivery>& out);
  static void fail_locked(Job& job, const Error& error, std::vector<Delivery>& out);
  void release_locked(std::map<std::uint64_t, Job>::iterator job, std::vector<Delivery>& out);
  // Drops a released job and what named it, and settles its session.
  void forget_locked(std::map<std::uint64_t, Job>::iterator job, std::vector<Delivery>& out);
  void settle_locked(std::uint64_t session_key, std::vector<Delivery>& out);
  void acknowledge_locked(std::map<std::uint64_t, PendingRelease>::iterator pending,
                          std::vector<Delivery>& out);
  void refuse_locked(const MessageRefused& refused, ipc::SidecarFrame&& frame,
                     std::vector<Delivery>& out);
  // A frame that is not a job message: the probe's answer when it carries the
  // probe's id, else the waiting exchange's.
  void offer_other_locked(const std::string& message_id, ipc::SidecarFrame&& frame);
  // Answers the waiting exchange with `frame`; `message_id` is the request an
  // `error_event` names, null for any other frame.
  void offer_unary_locked(ipc::SidecarFrame&& frame, const std::string* message_id);
  void queue_locked(QueuedFor owner, std::uint64_t key, std::vector<std::byte> frame);
  // Takes out of the queue the frame of a record that is gone. Reached from
  // the reader as it routes or ends the connection, from stop() and from an
  // exchange that gives up: from no call a sink callback may make.
  void unqueue_locked(QueuedFor owner, std::uint64_t key);

  BufferManager* const manager_;
  const std::chrono::milliseconds write_timeout_;

  mutable std::mutex mutex_;
  std::condition_variable outbox_cv_;
  std::condition_variable unary_cv_;
  std::condition_variable probe_cv_;

  Phase phase_ = Phase::Idle;
  bool retired_ = false;
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
  ReleasedSessionKeys released_sessions_;

  std::deque<QueuedFrame> outbox_;

  /// The exchange that waits for its answer; null once it has one.
  UnaryWaiter* unary_ = nullptr;
  std::shared_ptr<HealthProbe> probe_;

  JobChannelCounters counters_;
  std::size_t owed_callbacks_ = 0;

  std::thread reader_;
  std::thread writer_;
};

}  // namespace tensorplate::adapters::python_pytorch
