// SPDX-License-Identifier: Apache-2.0
//
// Optional job control and event bridge of an execution backend.
//
// A backend that runs typed jobs (tensorplate/core/job_request.hpp) beside
// its ExecutionSession lifecycle exposes one BoundedJobBridge per session.
// The registry hands out the session and its bridge from a single factory
// call (BackendEntry::session_with_bridge_factory), so the bridge is bound to
// its session at construction; nothing looks a bridge up or downcasts a
// session to find one. A backend entry without that factory reports
// Error::Code::Unsupported for jobs.
//
// Bridge calls never enter the ExecutionSession lifecycle: they neither call
// nor wait for load, prime, infer, infer_async or unload, and they may run
// while one of those runs on another thread. An implementation serializes
// its bridge work against its own lifecycle changes internally.
//
// The bridge fixes no executor width, queue depth, fairness policy or
// deadline. Several jobs may be outstanding at once; an implementation that
// admits fewer refuses the rest with Error::Code::ResourceExhausted, and the
// dispatch strategy above it decides how many to submit. The bridge is a
// submit/cancel/release seam, not a scheduler: nothing here is called per
// token or per decode step.
//
// No vendor SDK type appears in this header.

#pragma once

#include <cstdint>
#include <memory>

#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Receiver of one bridge's job events.
///
/// Delivery guarantees, for the sink registered with a bridge:
///   - Each event goes to the callback named for its kind.
///   - The events of one job follow the order in
///     tensorplate/core/job_event.hpp. The bridge checks each one with a
///     JobEventSequence built from the submitted request, noting every
///     successful cancel() and release_session(), and never delivers an
///     event that check refuses. It treats a refusal as a backend fault:
///     when the job has no terminal event yet, it delivers `failed` carrying
///     the refusal's error in its place, and a refused `released` is still
///     delivered after that `failed`. A refused event that follows the
///     terminal event, or that names another session or generation, is
///     dropped. Events of different jobs may interleave in any order.
///   - Every job whose submit() succeeded receives exactly one `completed`
///     or `failed`, also when the backend resets or the session unloads
///     (then `failed` with Error::Code::Unavailable and reason
///     "backend_reset" or "session_unloaded"). `released` follows
///     only once release actually happened, including after a confirmed
///     process reap; a job whose release cannot be confirmed is never
///     released and its reservations stay held.
///   - Callbacks are serialized: the bridge never runs two at once.
///   - A callback never runs on a thread while that thread is inside a
///     method of the bridge. It may run on another thread before the bridge
///     method that caused it returns: the submit() of its job, or the
///     cancel() or release_session() it acknowledges. So the submitter
///     records a job before submitting it, and a receiver that checks events
///     again notes a cancellation before requesting it.
///   - on_session_released follows the `released` event of every job of
///     that session submitted before the release_session() call, so it is
///     never delivered while one of those jobs is unreleased: a session with
///     a job whose release cannot be confirmed is never acknowledged, and
///     its reservations stay held.
///
/// Obligations of the sink: a callback returns promptly without blocking or
/// throwing. It may call submit(), cancel() and release_session() on the
/// bridge, whose events then arrive after it returns; it never calls
/// health() or set_event_sink(). A receiver releases the buffer of every
/// AudioChunkResult it receives exactly once, including when it discards the
/// event, for example a late event of a cancelled job or a stale generation.
class JobEventSink {
 public:
  virtual ~JobEventSink() = default;

  /// A JobEventKind::Accepted event.
  virtual void on_accepted(const JobEvent& event) noexcept = 0;
  /// A JobEventKind::Progress event.
  virtual void on_progress(const JobEvent& event) noexcept = 0;
  /// A JobEventKind::Completed event.
  virtual void on_completed(const JobEvent& event) noexcept = 0;
  /// A JobEventKind::Failed event.
  virtual void on_failed(const JobEvent& event) noexcept = 0;
  /// A JobEventKind::CancelAcknowledged event.
  virtual void on_cancel_acknowledged(const JobEvent& event) noexcept = 0;
  /// A JobEventKind::Released event.
  virtual void on_released(const JobEvent& event) noexcept = 0;

  /// The backend released every job and all state it held for the session
  /// named in a successful release_session() call. Delivered at most once
  /// per session: once all of the session's jobs were released, and never
  /// when one of them cannot be.
  virtual void on_session_released(std::uint64_t session_key,
                                   std::uint64_t generation) noexcept = 0;
};

/// Job control of one backend session. Thread-safe: every method may be
/// called from any thread.
///
/// Error reasons below are stable snake_case strings in Error::context.
/// After a method fails, no event is delivered because of that call.
class BoundedJobBridge {
 public:
  BoundedJobBridge() noexcept = default;
  virtual ~BoundedJobBridge() = default;

  BoundedJobBridge(const BoundedJobBridge&) = delete;
  BoundedJobBridge& operator=(const BoundedJobBridge&) = delete;
  BoundedJobBridge(BoundedJobBridge&&) = delete;
  BoundedJobBridge& operator=(BoundedJobBridge&&) = delete;

  /// Registers the sink that receives every event of this bridge. Call once,
  /// before the first submit(). The bridge keeps the sink until the bridge is
  /// destroyed, so the sink must not own the bridge.
  ///
  /// @return success, or Error::Code::ConfigInvalid with reason
  ///   "event_sink_null" or "event_sink_already_set".
  [[nodiscard]] virtual Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink) = 0;

  /// Submits one job. Success means the bridge took the job and its events
  /// follow through the sink. The submitter keeps the request's buffers
  /// readable until the job's `released` event.
  ///
  /// @return success, or the first of these that applies:
  ///   - Error::Code::NotReady, "event_sink_missing": no sink registered;
  ///   - Error::Code::NotReady, "backend_not_ready": the backend cannot run
  ///     jobs yet, for example before its session is primed;
  ///   - Error::Code::Unavailable, "backend_unavailable": the backend was
  ///     reset, its session was unloaded or destroyed, or it is shutting
  ///     down;
  ///   - Error::Code::Unsupported, "job_class_unsupported": the loaded
  ///     backend does not run the request's job class;
  ///   - Error::Code::Unsupported, "job_not_permitted": the loaded profile
  ///     does not permit the request's options or sizes, for example a
  ///     language, voice or speed it was not qualified for, or a window
  ///     longer than it admits. A backend that can tell only after admission
  ///     ends the job with `failed` carrying the same code and reason;
  ///   - Error::Code::NotReady, "session_releasing": release_session() was
  ///     called for the request's session key;
  ///   - Error::Code::ConfigInvalid, "duplicate_job_id": a job with the same
  ///     job_id has not been released yet;
  ///   - Error::Code::ResourceExhausted, "job_capacity_exhausted": the
  ///     backend's bounded admission is full.
  [[nodiscard]] virtual Result<void> submit(JobRequest request) = 0;

  /// Requests cancellation of a submitted job. Idempotent: repeating it, or
  /// cancelling a job whose terminal event was already delivered, succeeds
  /// and changes nothing. The job still ends with exactly one terminal event
  /// and then, once its release is confirmed, `released`;
  /// `cancel_acknowledged` may precede the terminal event (see
  /// tensorplate/core/job_event.hpp).
  ///
  /// @return success, or Error::Code::NotReady, "unknown_job": no job with
  ///   this id was submitted, or it was already released.
  [[nodiscard]] virtual Result<void> cancel(std::uint64_t job_id) = 0;

  /// Releases everything the backend holds for one logical session: cancels
  /// its unfinished jobs as cancel() does (they still end and are released
  /// as usual) and discards its session state, such as voice-activity state.
  /// Once every job of the session was released, the sink receives
  /// on_session_released(session_key, generation) once, also when the
  /// backend held nothing for the session; a job whose release cannot be
  /// confirmed withholds it (see JobEventSink). Repeating the call, before
  /// or after that acknowledgement, succeeds and changes nothing. The
  /// session key is never submitted again.
  ///
  /// @return success, or Error::Code::ConfigInvalid, "session_key_zero" or
  ///   "generation_zero"; or Error::Code::NotReady, "event_sink_missing"; or
  ///   Error::Code::Unavailable, "backend_unavailable" (the backend holds no
  ///   state any more; the caller treats the session as released).
  [[nodiscard]] virtual Result<void> release_session(std::uint64_t session_key,
                                                     std::uint64_t generation) = 0;

  /// Reports whether the backend can run jobs now, within the
  /// implementation's bounded health deadline and without entering the
  /// session lifecycle or waiting for a job.
  ///
  /// @return success, or Error::Code::NotReady ("backend_not_ready"),
  ///   Error::Code::Unavailable ("backend_unavailable") or
  ///   Error::Code::Timeout ("health_timeout").
  [[nodiscard]] virtual Result<void> health() = 0;
};

}  // namespace tensorplate
