// SPDX-License-Identifier: Apache-2.0
//
// A BoundedJobBridge with a backend of its own, for tests that need speech
// jobs to run without a sidecar process.
//
// It keeps the bridge's promises through FakeBoundedJobBridge (the refusals,
// one JobEventSequence per job, a session acknowledged after its jobs) and
// plays the backend as the in-tree sidecar runner does with its fixture
// backend (jobs.py and backends/fixture.py under
// backends/python_pytorch/src/tensorplate_pytorch_backend/):
//
//   - it runs `stt_decode` and `tts_synthesis`; one job runs and
//     kWaitingJobs wait in submission order; one more ends with `failed`
//     (`job_capacity_exhausted`) and is never `accepted`;
//   - a job is `accepted` when the backend takes it in, not when it starts;
//   - a cancellation is acknowledged at once; a waiting job ends then, the
//     running one when its work returns, each `failed` with
//     Error::Code::Cancelled;
//   - a session release cancels the session's unfinished jobs;
//   - a lost backend fails its jobs (`backend_reset`) and releases nothing
//     until the session unloads.
//
// Where it differs from the runner: a job starts at its submit when nothing
// runs, so the eight that wait are exact; a job's work runs with the bridge's
// lock held, so a cancel reaches a running job only while a test holds it;
// an unload fails every unfinished job with `session_unloaded` at once, where
// the runner fails the waiting ones itself and lets the running one end; and
// the wrapped fake refuses a submit while 64 jobs are unreleased.
//
// One thread runs the jobs and makes the callbacks while the backend lives.
// unload() returns once every callback owed has run, on its caller's thread
// or on that one. prime(), lose_backend(), unload() and the destructor stand
// for the session's side: never call them from a sink callback or while
// holding a lock one takes.

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
#include <thread>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

#include "fake_bounded_job_bridge.hpp"

namespace tensorplate::testing {

/// PCM bytes the fixture backend synthesizes for each byte of text.
inline constexpr std::size_t kFixtureSpeechBytesPerTextByte = 160;

/// The fixture backend's transcript of `audio`: one word, "hello", that ends
/// half way through the window.
[[nodiscard]] TranscriptResult fixture_transcript(const AudioFrames& audio);

/// The fixture backend's result for `request`: fixture_transcript() of a
/// decode; for a synthesis, silence of kFixtureSpeechBytesPerTextByte bytes
/// for each byte of text, in a buffer of `buffers`.
[[nodiscard]] Result<JobResult> fixture_job_result(const JobRequest& request,
                                                   BufferManager& buffers);

class FixtureJobBridge final : public BoundedJobBridge {
 public:
  /// Jobs that wait behind the running one.
  static constexpr std::size_t kWaitingJobs = 8;

  /// What a job that ran returns; an error fails the job with it. Called
  /// with the bridge's lock held: it neither blocks nor calls the bridge.
  using ResultFunction = std::function<Result<JobResult>(const JobRequest&, BufferManager&)>;

  /// @param buffers  where a synthesis result is published; outlives the
  ///                 bridge.
  explicit FixtureJobBridge(BufferManager& buffers, ResultFunction result = fixture_job_result);
  /// Unloads first, as a session's destructor does.
  ~FixtureJobBridge() override;

  FixtureJobBridge(const FixtureJobBridge&) = delete;
  FixtureJobBridge& operator=(const FixtureJobBridge&) = delete;
  FixtureJobBridge(FixtureJobBridge&&) = delete;
  FixtureJobBridge& operator=(FixtureJobBridge&&) = delete;

  // -- BoundedJobBridge. -------------------------------------------------------

  [[nodiscard]] Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink) override;
  [[nodiscard]] Result<void> submit(JobRequest request) override;
  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) override;
  [[nodiscard]] Result<void> release_session(std::uint64_t session_key,
                                             std::uint64_t generation) override;
  [[nodiscard]] Result<void> health() override;

  // -- The session's side. -----------------------------------------------------

  /// The backend runs jobs from here on. Does nothing once the backend was
  /// lost or the session unloaded.
  void prime();
  /// The backend is gone and the session does not know: every unfinished job
  /// fails with `backend_reset` and stays unreleased until unload().
  void lose_backend();
  /// Every unfinished job fails with `session_unloaded`, every job is
  /// released and every released session acknowledged, before this returns.
  void unload();

  // -- The test's hold on the backend. -----------------------------------------

  /// From now until release_jobs(), the running job does not end.
  void hold_jobs();
  void release_jobs();
  /// The next job whose work returns fails with `error`.
  void fail_next_job(Error error);
  /// What health() answers while the backend runs jobs.
  void set_health(Result<void> health);
  /// True once everything submitted so far was worked through and every
  /// callback owed for it has run, as far as a held job allows; false when
  /// that takes longer than `patience`.
  [[nodiscard]] bool settled(std::chrono::milliseconds patience);
  /// Events of its own backend that a job's event order refused. Empty unless
  /// the fixture itself is wrong.
  [[nodiscard]] std::vector<Error> refusals() const;

 private:
  enum class Phase : std::uint8_t { Loaded, Ready, Lost, Unloaded };

  struct OpenJob {
    JobRequest request;
    bool cancelled = false;
  };

  void run();
  // The `*_locked` members need mutex_ held.
  void end_running_locked();
  void cancel_locked(std::map<std::uint64_t, OpenJob>::iterator job);
  void wake_locked();

  BufferManager& buffers_;
  const ResultFunction result_;
  FakeBoundedJobBridge rules_;

  std::mutex mutex_;
  std::condition_variable wake_;
  std::condition_variable settled_;
  Phase phase_ = Phase::Loaded;
  bool sink_set_ = false;
  bool held_ = false;
  bool stopping_ = false;
  /// Events wait for the thread to deliver them.
  bool pending_ = false;
  bool delivering_ = false;
  /// Jobs without a terminal event: the running one and those in lane_.
  std::map<std::uint64_t, OpenJob> open_;
  std::deque<std::uint64_t> lane_;
  /// Zero when no job runs.
  std::uint64_t running_ = 0;
  /// Jobs a lost backend failed; released by unload().
  std::vector<std::uint64_t> unreleased_;
  std::optional<Error> next_failure_;
  Result<void> health_;

  std::thread thread_;
};

}  // namespace tensorplate::testing
