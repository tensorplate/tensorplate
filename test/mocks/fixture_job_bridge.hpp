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
//   - it runs `stt_decode` and `tts_synthesis` on one lane and `vad_frames`
//     on another; in each lane one job runs and kWaitingJobs wait in
//     submission order; one more ends with `failed`
//     (`job_capacity_exhausted`) and is never `accepted`;
//   - a `vad_frames` job whose frames are not kFixtureVadFrameSamples samples
//     ends with `failed` (`job_not_permitted`) and is never `accepted`; no
//     voice-activity state is kept from one job to the next;
//   - a job is `accepted` when the backend takes it in, not when it starts;
//   - a cancellation is acknowledged at once; a waiting job ends then, the
//     running one when its work returns, each `failed` with
//     Error::Code::Cancelled;
//   - a session release cancels the session's unfinished jobs, the running
//     one included;
//   - a lost backend fails its jobs (`backend_reset`) and releases nothing
//     until the session unloads.
//
// Two answers are its own, where the bridge header leaves them open: before
// the prime a session release is taken and acknowledged, and a backend lost
// or a session unloaded before the prime answers `backend_unavailable`, not
// `backend_not_ready`. FixtureJobBridgeOptions turns two of the rules above
// into the other outcome the bridge header allows, and adds the one event the
// runner's backends never deliver: `progress`, for a job that allows it.
//
// It differs from the adapter with a real sidecar in six ways:
//
//   - a job starts at its submit when nothing runs in its lane, so the eight
//     that wait are exact; the runner counts a job its lane's thread has not
//     taken yet among them;
//   - a job's work runs in one step with the bridge's lock held: a
//     cancellation finds a started job before its work, which then does not
//     run, or after its end, never in between. It comes before the work for
//     certain while a test holds the jobs, and for a job cancelled by the
//     callback that submitted it;
//   - an unload fails every unfinished job with `session_unloaded` at once,
//     where the runner fails the waiting ones itself and lets the running
//     ones end;
//   - the wrapped fake refuses a submit while 64 jobs are unreleased;
//   - a prime after an unload or a loss does not reopen it: there is no
//     second load of one session;
//   - release_session on a lost or unloaded backend answers
//     `backend_unavailable` before it looks at the sink or the arguments.
//
// One thread runs the jobs and makes the callbacks while the backend lives.
// unload() returns once every callback owed has run, on its caller's thread
// or on that one. prime(), lose_backend(), unload() and the destructor stand
// for the session's side: one at a time, never from a sink callback or while
// holding a lock one takes.

#pragma once

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <deque>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
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

/// Samples in each frame of a `vad_frames` job the fixture backend runs.
inline constexpr std::uint32_t kFixtureVadFrameSamples = 512;

/// The fixture backend's transcript of `audio`: one word, "hello", that ends
/// half way through the window.
[[nodiscard]] TranscriptResult fixture_transcript(const AudioFrames& audio);

/// The fixture backend's voice activity of `pcm`, frames of `frame_samples`
/// samples back to back in kJobInputAudioFormat: of each frame, its largest
/// absolute sample over 32768.
[[nodiscard]] inline std::vector<float> fixture_voice_activity(std::span<const std::byte> pcm,
                                                               std::uint32_t frame_samples) {
  std::vector<float> scored;
  for (std::size_t frame = 0; 2 * (frame + 1) * frame_samples <= pcm.size(); ++frame) {
    int peak = 0;
    for (std::size_t sample = 0; sample < frame_samples; ++sample) {
      const std::size_t at = 2 * (frame * frame_samples + sample);
      const auto bits = static_cast<std::uint16_t>(std::to_integer<unsigned>(pcm[at]) |
                                                   (std::to_integer<unsigned>(pcm[at + 1]) << 8U));
      peak = std::max(peak, std::abs(static_cast<int>(static_cast<std::int16_t>(bits))));
    }
    scored.push_back(static_cast<float>(peak) / 32768.0F);
  }
  return scored;
}

/// The fixture backend's result for `request`: fixture_transcript() of a
/// decode; for a synthesis, silence of kFixtureSpeechBytesPerTextByte bytes
/// for each byte of text, in a buffer of `buffers`; fixture_voice_activity()
/// of a `vad_frames` job's window.
[[nodiscard]] Result<JobResult> fixture_job_result(const JobRequest& request,
                                                   BufferManager& buffers);

/// Where the bridge header allows a backend two outcomes: which one the
/// fixture produces. The defaults are the runner's.
struct FixtureJobBridgeOptions {
  /// submit() refuses a job (`job_capacity_exhausted`) while 1 + kWaitingJobs,
  /// the count of one of the runner's lanes, are unreleased in both lanes
  /// together, as the adapter's job table refuses at its own count of 16; none
  /// is failed for it. A shared count, which the header allows: the cases
  /// that use it submit decodes only.
  bool submit_refuses_at_capacity = false;
  /// A cancellation comes too late for its job, started or not: the job runs
  /// to its end and completes, with no `cancel_acknowledged`.
  bool cancels_arrive_late = false;
  /// A job whose request allows progress delivers one fragment before it
  /// completes: what its result function returns, called once more. For a
  /// receiver that has to settle a fragment; the two together must stay
  /// within one result's ceilings, or the job's event order fails the job.
  bool progress_when_allowed = false;
};

class FixtureJobBridge final : public BoundedJobBridge {
 public:
  /// Jobs that wait behind the running one, in each lane.
  static constexpr std::size_t kWaitingJobs = 8;

  /// What a job that ran returns; an error fails the job with it. Called
  /// with the bridge's lock held: it neither blocks nor calls the bridge.
  using ResultFunction = std::function<Result<JobResult>(const JobRequest&, BufferManager&)>;

  /// @param buffers  where a synthesis result is published; outlives the
  ///                 bridge.
  explicit FixtureJobBridge(BufferManager& buffers, ResultFunction result = fixture_job_result,
                            FixtureJobBridgeOptions options = {});
  /// Unloads first, as a session's destructor does.
  ~FixtureJobBridge() override;

  FixtureJobBridge(const FixtureJobBridge&) = delete;
  FixtureJobBridge& operator=(const FixtureJobBridge&) = delete;
  FixtureJobBridge(FixtureJobBridge&&) = delete;
  FixtureJobBridge& operator=(FixtureJobBridge&&) = delete;

  // -- BoundedJobBridge. -------------------------------------------------------

  [[nodiscard]] Result<void> set_event_sink(std::shared_ptr<JobEventSink> sink) override;
  [[nodiscard]] Result<void> submit(JobRequest request) override;
  [[nodiscard]] bool runs(JobClass job_class) const noexcept override;
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

  /// From now until release_jobs(), the running job of each lane does not end.
  void hold_jobs();
  /// The same for the one lane that runs `job_class`.
  void hold_jobs(JobClass job_class);
  void release_jobs();
  /// The next job whose work returns fails with `error`.
  void fail_next_job(Error error);
  /// What health() answers while the backend runs jobs.
  void set_health(Result<void> health);
  /// True once everything submitted so far was worked through and every
  /// callback owed for it has returned, an unload's on another thread too, as
  /// far as a held job allows; false when that takes longer than `patience`.
  [[nodiscard]] bool settled(std::chrono::milliseconds patience);
  /// Events of its own backend that a job's event order refused. Empty unless
  /// the fixture is wrong or the test's result function returns a wrong result.
  [[nodiscard]] std::vector<Error> refusals() const;

 private:
  enum class Phase : std::uint8_t { Loaded, Ready, Lost, Unloaded };

  struct OpenJob {
    JobRequest request;
    bool cancelled = false;
  };

  struct Lane {
    /// Zero when no job runs.
    std::uint64_t running = 0;
    std::deque<std::uint64_t> waiting;
    bool held = false;
  };

  Lane& lane_of(JobClass job_class) noexcept;
  void run();
  // The `*_locked` members need mutex_ held.
  /// The lane whose running job may end now, if any.
  Lane* due_lane_locked() noexcept;
  void empty_lanes_locked() noexcept;
  void end_running_locked(Lane& lane);
  void cancel_locked(std::map<std::uint64_t, OpenJob>::iterator job);
  void wake_locked();

  BufferManager& buffers_;
  const ResultFunction result_;
  const FixtureJobBridgeOptions options_;
  FakeBoundedJobBridge rules_;

  std::mutex mutex_;
  std::condition_variable wake_;
  std::condition_variable settled_;
  Phase phase_ = Phase::Loaded;
  /// Read by runs() without the lock, which a result function holds.
  std::atomic<bool> unloaded_{false};
  bool sink_set_ = false;
  bool stopping_ = false;
  /// Events wait for the thread to deliver them.
  bool pending_ = false;
  bool delivering_ = false;
  /// An unload is delivering on its caller's thread.
  bool unloading_ = false;
  /// Jobs without a terminal event: those of both lanes.
  std::map<std::uint64_t, OpenJob> open_;
  Lane speech_;
  Lane voice_activity_;
  /// Jobs a lost backend failed; released by unload().
  std::vector<std::uint64_t> unreleased_;
  std::optional<Error> next_failure_;
  Result<void> health_;

  std::thread thread_;
};

}  // namespace tensorplate::testing
