// SPDX-License-Identifier: Apache-2.0
//
// What include/tensorplate/backend/bounded_job_bridge.hpp promises the caller
// of a bridge whose backend runs `stt_decode` and `tts_synthesis`, as cases
// that hold for every such bridge. The comment above a case quotes the
// sentence it holds, from that header unless it names another.
//
// A binding supplies the bridge with a backend behind it and the hold a case
// needs on that backend (BridgeUnderTest). The cases reach the bridge through
// BoundedJobBridge only, record with a RecordingJobEventSink, which checks
// every job's event order again, and never wait on the clock: an order a case
// needs is forced by holding the backend's jobs.
//
// Not held here: `job_not_permitted` (the bindings permit every option),
// `health_timeout`, a backend event the job's order refuses being replaced by
// `failed`, calls from several threads at once, and the destruction of the
// session, which a binding tests for itself.
//
// Include it in one source file of a test binary and instantiate the suite
// there with that binary's bindings.

#pragma once

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <ostream>
#include <string>
#include <string_view>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

#include "recording_job_event_sink.hpp"

namespace tensorplate::testing {

/// A bridge with its backend, and what the cases must know of that backend.
///
/// Where the header leaves a backend a choice, the cases rely on what a
/// binding promises here: its backend runs `stt_decode` and `tts_synthesis`
/// and no other class; it admits a job (`accepted`) when it takes it in, also
/// behind a held one; and a job that has not started ends as soon as it is
/// cancelled, without waiting for the running one.
class BridgeUnderTest {
 public:
  BridgeUnderTest() = default;
  BridgeUnderTest(const BridgeUnderTest&) = delete;
  BridgeUnderTest& operator=(const BridgeUnderTest&) = delete;
  virtual ~BridgeUnderTest() = default;

  [[nodiscard]] virtual BoundedJobBridge& bridge() = 0;
  /// Where a request's PCM is read and a result's PCM is published.
  [[nodiscard]] virtual BufferManager& buffers() = 0;

  /// The session's prime: the backend runs jobs from here on.
  virtual void prime() = 0;
  /// The session's unload. When it returns every job has its terminal event
  /// and `released`, every pending session is acknowledged and no callback
  /// is still owed.
  virtual void unload() = 0;
  /// The backend is gone under a primed session, which is not told: its jobs
  /// fail and none is released before unload().
  virtual void lose_backend() = 0;

  /// From now until release_jobs(), a job that has started does not end.
  virtual void hold_jobs() = 0;
  virtual void release_jobs() = 0;
  /// True once the backend has taken in everything submitted so far and every
  /// callback owed for it has run, as far as held jobs allow.
  [[nodiscard]] virtual bool settled(std::chrono::milliseconds patience) = 0;

  /// Jobs the backend admits behind the running one. It turns one more away,
  /// by refusing the submit or by ending the job with `failed`.
  [[nodiscard]] virtual std::size_t waiting_jobs() const = 0;
  /// The backend's result for a decode `request`.
  [[nodiscard]] virtual TranscriptResult transcript(const JobRequest& request) const = 0;
  /// The PCM the backend synthesizes for `request`.
  [[nodiscard]] virtual std::vector<std::byte> speech(const JobRequest& request) const = 0;
};

/// Marks the calling thread as inside a bridge method while it lives.
class InsideABridgeCall {
 public:
  InsideABridgeCall() { ++calls(); }
  ~InsideABridgeCall() { --calls(); }
  InsideABridgeCall(const InsideABridgeCall&) = delete;
  InsideABridgeCall& operator=(const InsideABridgeCall&) = delete;
  InsideABridgeCall(InsideABridgeCall&&) = delete;
  InsideABridgeCall& operator=(InsideABridgeCall&&) = delete;

  /// Bridge calls the calling thread is inside.
  [[nodiscard]] static int depth() { return calls(); }

 private:
  static int& calls() {
    thread_local int count = 0;
    return count;
  }
};

struct BridgeCase {
  std::string name;
  std::function<std::unique_ptr<BridgeUnderTest>()> make;
};

inline std::ostream& operator<<(std::ostream& out, const BridgeCase& bridge_case) {
  return out << bridge_case.name;
}

class BoundedJobBridgeConformance : public ::testing::TestWithParam<BridgeCase> {
 protected:
  using Kinds = std::vector<JobEventKind>;

  /// How long a wait lasts before the case fails.
  static constexpr std::chrono::seconds kPatience{30};
  static constexpr std::uint64_t kGeneration = 3;

  void SetUp() override {
    under = GetParam().make();
    sink = std::make_shared<RecordingJobEventSink>();
    sink->set_callback_hook([this](const JobEvent& event) {
      note_callback();
      if (on_event) {
        on_event(event);
      }
    });
    sink->set_session_release_hook([this](std::uint64_t, std::uint64_t) { note_callback(); });
  }

  // "Callbacks are serialized: the bridge never runs two at once." "A callback
  // never runs on a thread while that thread is inside a method of the
  // bridge." "The events of one job follow the order in
  // tensorplate/core/job_event.hpp." Held for every case.
  void TearDown() override {
    under.reset();
    EXPECT_EQ(sink->violations(), std::vector<std::string>{});
    EXPECT_EQ(callbacks_inside_a_call.load(), 0);
  }

  [[nodiscard]] BoundedJobBridge& bridge() { return under->bridge(); }

  /// Registers the sink and primes: the backend runs jobs.
  void ready() {
    ASSERT_TRUE(set_sink(sink).has_value());
    under->prime();
  }

  // -- Requests. ---------------------------------------------------------------

  [[nodiscard]] JobRequest decode(std::uint64_t job_id, std::uint64_t session_key,
                                  std::uint64_t start_sample = 0) {
    constexpr std::size_t kBytes = 3'200;
    const BufferRef buffer = under->buffers().allocate(kBytes).value();
    return JobRequest::create(
               JobIdentity{job_id, session_key, kGeneration}, JobClass::SttDecode,
               AudioFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, kBytes}, start_sample},
               JobOptions{.language = "en"})
        .value();
  }

  [[nodiscard]] static JobRequest synthesis(std::uint64_t job_id, std::uint64_t session_key) {
    return JobRequest::create(
               JobIdentity{job_id, session_key, kGeneration}, JobClass::TtsSynthesis,
               TextSegment{"hello there."},
               JobOptions{.language = "en-US", .voice = "af_heart", .speed_milli = 1000})
        .value();
  }

  // -- Bridge calls, noted with the sink first as a receiver notes them. --------

  [[nodiscard]] Result<void> set_sink(std::shared_ptr<JobEventSink> registered) {
    const InsideABridgeCall inside;
    return bridge().set_event_sink(std::move(registered));
  }

  [[nodiscard]] Result<void> health() {
    const InsideABridgeCall inside;
    return bridge().health();
  }

  [[nodiscard]] Result<void> submit(const JobRequest& request) {
    sink->expect_job(request);
    return resubmit(request);
  }

  /// A submit the sink is not told of: one the case expects to be refused.
  [[nodiscard]] Result<void> resubmit(const JobRequest& request) {
    const InsideABridgeCall inside;
    return bridge().submit(request);
  }

  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) {
    sink->expect_cancel(job_id);
    const InsideABridgeCall inside;
    return bridge().cancel(job_id);
  }

  [[nodiscard]] Result<void> release(std::uint64_t session_key,
                                     std::uint64_t generation = kGeneration) {
    sink->expect_session_release(session_key);
    const InsideABridgeCall inside;
    return bridge().release_session(session_key, generation);
  }

  static void expect_refused(const Result<void>& result, Error::Code code,
                             std::string_view reason) {
    ASSERT_FALSE(result.has_value()) << reason;
    EXPECT_EQ(result.error().code, code) << reason;
    EXPECT_EQ(result.error().context.value_or(""), reason);
  }

  // -- Waits and readings. -----------------------------------------------------

  /// Waits until `reached` holds; false when it never did.
  [[nodiscard]] bool wait(const std::function<bool()>& reached) {
    const auto limit = std::chrono::steady_clock::now() + kPatience;
    std::unique_lock<std::mutex> lock(mutex_);
    while (!reached()) {
      if (std::chrono::steady_clock::now() >= limit) {
        return false;
      }
      changed_.wait_for(lock, std::chrono::milliseconds{2});
    }
    return true;
  }

  [[nodiscard]] bool wait_kinds(std::uint64_t job_id, const Kinds& kinds) {
    return wait([&] { return sink->kinds_for(job_id) == kinds; });
  }

  [[nodiscard]] bool wait_released(std::uint64_t job_id) {
    return wait([&] { return sink->sequence(job_id).value().released(); });
  }

  [[nodiscard]] bool settled() { return under->settled(kPatience); }

  /// The job's event of `kind`; the case fails when it has none.
  [[nodiscard]] JobEvent event_of(std::uint64_t job_id, JobEventKind kind) const {
    for (const JobEvent& event : sink->events()) {
      if (event.identity().job_id == job_id && event.kind() == kind) {
        return event;
      }
    }
    ADD_FAILURE() << "job " << job_id << " has no " << to_string(kind);
    return JobEvent::released(JobIdentity{job_id, 1, 1}).value();
  }

  [[nodiscard]] std::size_t count(std::uint64_t job_id, JobEventKind kind) const {
    const Kinds kinds = sink->kinds_for(job_id);
    return static_cast<std::size_t>(std::count(kinds.begin(), kinds.end(), kind));
  }

  /// A released job that ended with `failed` carrying `code` and `reason`
  /// and never completed.
  void expect_failed(std::uint64_t job_id, Error::Code code, std::string_view reason = "") {
    const Kinds kinds = sink->kinds_for(job_id);
    ASSERT_GE(kinds.size(), 2U);
    EXPECT_EQ(kinds[kinds.size() - 2], JobEventKind::Failed);
    EXPECT_EQ(kinds.back(), JobEventKind::Released);
    EXPECT_EQ(count(job_id, JobEventKind::Failed), 1U);
    EXPECT_EQ(count(job_id, JobEventKind::Completed), 0U);
    const Error error = event_of(job_id, JobEventKind::Failed).error().value();
    EXPECT_EQ(error.code, code);
    if (!reason.empty()) {
      EXPECT_EQ(error.context.value_or(""), reason);
    }
  }

  std::unique_ptr<BridgeUnderTest> under;
  std::shared_ptr<RecordingJobEventSink> sink;
  /// Runs inside every job callback. Set it before the first submit.
  std::function<void(const JobEvent&)> on_event;
  std::atomic<int> callbacks_inside_a_call{0};

 private:
  void note_callback() {
    if (InsideABridgeCall::depth() != 0) {
      ++callbacks_inside_a_call;
    }
    const std::lock_guard<std::mutex> lock(mutex_);
    changed_.notify_all();
  }

  std::mutex mutex_;
  std::condition_variable changed_;
};

using enum JobEventKind;

// -- The sink. -------------------------------------------------------------------

// set_event_sink: "@return success, or Error::Code::ConfigInvalid with reason
// "event_sink_null" or "event_sink_already_set"."
TEST_P(BoundedJobBridgeConformance, ANullSinkAndASecondSinkAreRefused) {
  expect_refused(set_sink(nullptr), Error::Code::ConfigInvalid, "event_sink_null");
  ASSERT_TRUE(set_sink(sink).has_value());
  expect_refused(set_sink(sink), Error::Code::ConfigInvalid, "event_sink_already_set");
  expect_refused(set_sink(std::make_shared<RecordingJobEventSink>()), Error::Code::ConfigInvalid,
                 "event_sink_already_set");
}

// submit: "the first of these that applies: Error::Code::NotReady,
// "event_sink_missing": no sink registered", ahead of "backend_not_ready".
// release_session: "or Error::Code::NotReady, "event_sink_missing"".
TEST_P(BoundedJobBridgeConformance, WithoutASinkSubmitAndReleaseAreRefused) {
  ASSERT_FALSE(set_sink(nullptr).has_value());
  expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "event_sink_missing");
  expect_refused(release(5), Error::Code::NotReady, "event_sink_missing");
  under->prime();
  expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "event_sink_missing");
}

// -- Readiness. ------------------------------------------------------------------

// submit: "Error::Code::NotReady, "backend_not_ready": the backend cannot run
// jobs yet, for example before its session is primed". health: "@return
// success, or Error::Code::NotReady ("backend_not_ready")". "After a method
// fails, no event is delivered because of that call."
TEST_P(BoundedJobBridgeConformance, BeforeThePrimeNoJobIsTakenAndHealthIsNotReady) {
  ASSERT_TRUE(set_sink(sink).has_value());
  expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "backend_not_ready");
  expect_refused(health(), Error::Code::NotReady, "backend_not_ready");
  ASSERT_TRUE(settled());
  EXPECT_TRUE(sink->events().empty());
}

// health: "Reports whether the backend can run jobs now ... without entering
// the session lifecycle or waiting for a job."
TEST_P(BoundedJobBridgeConformance, APrimedBackendIsHealthyAlsoWhileAJobRuns) {
  ASSERT_NO_FATAL_FAILURE(ready());
  EXPECT_TRUE(health().has_value());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  EXPECT_TRUE(health().has_value());
  under->release_jobs();
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
}

// -- Jobs. -----------------------------------------------------------------------

// submit: "Success means the bridge took the job and its events follow through
// the sink." job_event.hpp: "[accepted] ... (completed | failed) released".
TEST_P(BoundedJobBridgeConformance, ADecodeIsAcceptedCompletedWithItsTranscriptAndReleased) {
  ASSERT_NO_FATAL_FAILURE(ready());
  const JobRequest request = decode(1, 5, /*start_sample=*/480);
  ASSERT_TRUE(submit(request).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  const JobEvent completed = event_of(1, Completed);
  EXPECT_EQ(completed.identity(), request.identity());
  EXPECT_EQ(completed.result().value(), JobResult::create(under->transcript(request)).value());
}

// JobEventSink: "A receiver releases the buffer of every AudioChunkResult it
// receives exactly once". job_result.hpp: "Synthesized speech in
// kJobOutputAudioFormat".
TEST_P(BoundedJobBridgeConformance, ASynthesisCompletesWithOneChunkTheReceiverReleases) {
  ASSERT_NO_FATAL_FAILURE(ready());
  const JobRequest request = synthesis(1, 5);
  ASSERT_TRUE(submit(request).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  const JobResult result = event_of(1, Completed).result().value();
  const auto* chunk = std::get_if<AudioChunkResult>(&result.payload());
  ASSERT_NE(chunk, nullptr);
  EXPECT_EQ(chunk->format, kJobOutputAudioFormat);
  const auto held = under->buffers().data(chunk->pcm.buffer);
  ASSERT_TRUE(held.has_value());
  const auto window = held.value().subspan(chunk->pcm.byte_offset, chunk->pcm.byte_size);
  EXPECT_EQ(std::vector<std::byte>(window.begin(), window.end()), under->speech(request));
  EXPECT_TRUE(under->buffers().release(chunk->pcm.buffer).has_value());
}

// job_event.hpp: "A job delivers at most JobRequest::progress_limit() of them;
// with the default limit of zero it delivers none."
TEST_P(BoundedJobBridgeConformance, NeitherSpeechClassDeliversProgress) {
  ASSERT_NO_FATAL_FAILURE(ready());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(submit(synthesis(2, 5)).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(wait_kinds(2, {Accepted, Completed, Released}));
  EXPECT_EQ(count(1, Progress) + count(2, Progress), 0U);
  const JobResult speech = event_of(2, Completed).result().value();
  EXPECT_TRUE(under->buffers()
                  .release(std::get<AudioChunkResult>(speech.payload()).pcm.buffer)
                  .has_value());
}

// submit: "Error::Code::Unsupported, "job_class_unsupported": the loaded
// backend does not run the request's job class".
TEST_P(BoundedJobBridgeConformance, AClassTheBackendDoesNotRunIsRefused) {
  ASSERT_NO_FATAL_FAILURE(ready());
  constexpr std::uint32_t kFrameSamples = 512;
  const BufferRef buffer = under->buffers().allocate(2U * kFrameSamples).value();
  const JobRequest vad =
      JobRequest::create(JobIdentity{1, 5, kGeneration}, JobClass::VadFrames,
                         VadFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, 2U * kFrameSamples},
                                   kFrameSamples, 1, 1})
          .value();
  expect_refused(resubmit(vad), Error::Code::Unsupported, "job_class_unsupported");
}

// submit: "Error::Code::ConfigInvalid, "duplicate_job_id": a job with the same
// job_id has not been released yet".
TEST_P(BoundedJobBridgeConformance, AJobIdIsRefusedUntilItsJobWasReleased) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  expect_refused(resubmit(decode(1, 5)), Error::Code::ConfigInvalid, "duplicate_job_id");
  expect_refused(resubmit(synthesis(1, 6)), Error::Code::ConfigInvalid, "duplicate_job_id");
  under->release_jobs();
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released, Accepted, Completed, Released}));
}

// "an implementation that admits fewer refuses the rest with
// Error::Code::ResourceExhausted"; submit: ""job_capacity_exhausted": the
// backend's bounded admission is full". BridgeUnderTest::waiting_jobs().
TEST_P(BoundedJobBridgeConformance, OneJobBeyondTheBackendsBoundIsTurnedAway) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  const std::uint64_t last = 1 + under->waiting_jobs();
  for (std::uint64_t job_id = 2; job_id <= last; ++job_id) {
    ASSERT_TRUE(submit(decode(job_id, 5)).has_value()) << job_id;
  }
  ASSERT_TRUE(settled());
  for (std::uint64_t job_id = 1; job_id <= last; ++job_id) {
    EXPECT_EQ(sink->kinds_for(job_id), Kinds{Accepted}) << job_id;
  }
  const Result<void> beyond = submit(decode(last + 1, 5));
  if (beyond.has_value()) {
    ASSERT_TRUE(wait_kinds(last + 1, {Failed, Released}));
    expect_failed(last + 1, Error::Code::ResourceExhausted, "job_capacity_exhausted");
  } else {
    expect_refused(beyond, Error::Code::ResourceExhausted, "job_capacity_exhausted");
  }
  under->release_jobs();
  for (std::uint64_t job_id = 1; job_id <= last; ++job_id) {
    EXPECT_TRUE(wait_kinds(job_id, {Accepted, Completed, Released})) << job_id;
  }
}

// -- Cancel. ---------------------------------------------------------------------

// cancel: "@return success, or Error::Code::NotReady, "unknown_job": no job
// with this id was submitted, or it was already released."
TEST_P(BoundedJobBridgeConformance, ACancelOfAJobNeverSubmittedOrAlreadyReleasedIsRefused) {
  ASSERT_NO_FATAL_FAILURE(ready());
  expect_refused(cancel(1), Error::Code::NotReady, "unknown_job");
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  expect_refused(cancel(1), Error::Code::NotReady, "unknown_job");
}

// cancel: "The job still ends with exactly one terminal event and then, once
// its release is confirmed, `released`". job_event.hpp: "`failed` with
// Error::Code::Cancelled if the backend stopped the job". That it ends while
// another job is held is BridgeUnderTest's promise.
TEST_P(BoundedJobBridgeConformance, ACancelledWaitingJobFailsCancelledAndIsReleased) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(cancel(2).has_value());
  ASSERT_TRUE(wait_released(2));
  expect_failed(2, Error::Code::Cancelled);
  EXPECT_EQ(sink->kinds_for(2).front(), Accepted);
  EXPECT_EQ(sink->kinds_for(1), Kinds{Accepted});
  under->release_jobs();
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
}

// cancel: "Idempotent: repeating it ... succeeds and changes nothing."
// job_event.hpp: "`cancel_acknowledged` (at most once)"; "The terminal event
// still follows".
TEST_P(BoundedJobBridgeConformance, ACancelledRunningJobEndsOnceWhenItsWorkReturns) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(synthesis(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(cancel(1).has_value());
  ASSERT_TRUE(cancel(1).has_value());
  ASSERT_TRUE(settled());
  EXPECT_EQ(count(1, Failed) + count(1, Completed), 0U);
  under->release_jobs();
  ASSERT_TRUE(wait_released(1));
  EXPECT_LE(count(1, CancelAcknowledged), 1U);
  EXPECT_EQ(count(1, Failed) + count(1, Completed), 1U);
  if (count(1, Failed) == 1) {
    expect_failed(1, Error::Code::Cancelled);
  } else {
    const JobResult speech = event_of(1, Completed).result().value();
    EXPECT_TRUE(under->buffers()
                    .release(std::get<AudioChunkResult>(speech.payload()).pcm.buffer)
                    .has_value());
  }
}

// -- Session release. ------------------------------------------------------------

// release_session: "@return success, or Error::Code::ConfigInvalid,
// "session_key_zero" or "generation_zero"".
TEST_P(BoundedJobBridgeConformance, AReleaseWithAZeroKeyOrGenerationIsRefused) {
  ASSERT_NO_FATAL_FAILURE(ready());
  expect_refused(release(0), Error::Code::ConfigInvalid, "session_key_zero");
  expect_refused(release(5, 0), Error::Code::ConfigInvalid, "generation_zero");
  ASSERT_TRUE(settled());
  EXPECT_TRUE(sink->session_releases().empty());
}

// release_session: "the sink receives on_session_released(session_key,
// generation) once, also when the backend held nothing for the session";
// "Repeating the call, before or after that acknowledgement, succeeds and
// changes nothing."
TEST_P(BoundedJobBridgeConformance, ASessionThatHeldNothingIsAcknowledgedOnce) {
  ASSERT_NO_FATAL_FAILURE(ready());
  const std::vector<std::pair<std::uint64_t, std::uint64_t>> once{{5, kGeneration}};
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(wait([&] { return sink->session_releases() == once; }));
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(settled());
  EXPECT_EQ(sink->session_releases(), once);
}

// release_session: "cancels its unfinished jobs as cancel() does (they still
// end and are released as usual)". JobEventSink: "on_session_released follows
// the `released` event of every job of that session submitted before the
// release_session() call".
TEST_P(BoundedJobBridgeConformance, AReleaseEndsTheSessionsJobsAndIsAcknowledgedAfterThem) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 5)).has_value());
  ASSERT_TRUE(submit(decode(3, 6)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(wait_released(2));
  ASSERT_TRUE(settled());
  expect_failed(2, Error::Code::Cancelled);
  EXPECT_TRUE(sink->session_releases().empty());
  EXPECT_EQ(sink->kinds_for(3), Kinds{Accepted});
  under->release_jobs();
  ASSERT_TRUE(wait([&] { return !sink->session_releases().empty(); }));
  EXPECT_EQ(sink->session_releases(),
            (std::vector<std::pair<std::uint64_t, std::uint64_t>>{{5, kGeneration}}));
  EXPECT_TRUE(sink->sequence(1).value().released());
  EXPECT_EQ(count(1, Failed) + count(1, Completed), 1U);
  if (count(1, Failed) == 1) {
    expect_failed(1, Error::Code::Cancelled);
  }
  EXPECT_TRUE(wait_kinds(3, {Accepted, Completed, Released}));
}

// submit: "Error::Code::NotReady, "session_releasing": release_session() was
// called for the request's session key". release_session: "The session key is
// never submitted again."
TEST_P(BoundedJobBridgeConformance, AReleasedSessionTakesNoJobBeforeOrAfterItsAcknowledgement) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(release(5).has_value());
  expect_refused(resubmit(decode(2, 5)), Error::Code::NotReady, "session_releasing");
  under->release_jobs();
  ASSERT_TRUE(wait([&] { return !sink->session_releases().empty(); }));
  expect_refused(resubmit(decode(3, 5)), Error::Code::NotReady, "session_releasing");
  ASSERT_TRUE(submit(decode(4, 6)).has_value());
  EXPECT_TRUE(wait_kinds(4, {Accepted, Completed, Released}));
}

// -- Callbacks. ------------------------------------------------------------------

// JobEventSink: "It may call submit(), cancel() and release_session() on the
// bridge, whose events then arrive after it returns".
TEST_P(BoundedJobBridgeConformance, ACallbackMaySubmitCancelAndRelease) {
  std::vector<Result<void>> answers;
  bool arrived_inside = false;
  on_event = [&](const JobEvent& event) {
    if (event.identity().job_id != 1 || event.kind() != Completed) {
      return;
    }
    answers.push_back(submit(decode(2, 6)));
    answers.push_back(cancel(2));
    answers.push_back(release(7));
    arrived_inside = !sink->kinds_for(2).empty() || !sink->session_releases().empty();
  };
  ASSERT_NO_FATAL_FAILURE(ready());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(wait_released(2));
  ASSERT_TRUE(wait([&] { return !sink->session_releases().empty(); }));
  ASSERT_EQ(answers.size(), 3U);
  for (const Result<void>& answer : answers) {
    EXPECT_TRUE(answer.has_value());
  }
  EXPECT_FALSE(arrived_inside);
  EXPECT_EQ(count(2, Failed) + count(2, Completed), 1U);
  EXPECT_EQ(sink->session_releases(),
            (std::vector<std::pair<std::uint64_t, std::uint64_t>>{{7, kGeneration}}));
}

// -- The session's end. ----------------------------------------------------------

// JobEventSink: "Every job whose submit() succeeded receives exactly one
// `completed` or `failed`, also when ... the session unloads (then `failed`
// with Error::Code::Unavailable and reason ... "session_unloaded")". That all
// of it has happened when the unload returns is BridgeUnderTest::unload().
TEST_P(BoundedJobBridgeConformance, AnUnloadEndsEveryJobAndAcknowledgesEverySession) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 6)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(settled());
  EXPECT_TRUE(sink->session_releases().empty());

  under->unload();

  EXPECT_TRUE(sink->sequence(1).value().released());
  EXPECT_EQ(count(1, Failed) + count(1, Completed), 1U);
  expect_failed(2, Error::Code::Unavailable, "session_unloaded");
  EXPECT_EQ(sink->session_releases(),
            (std::vector<std::pair<std::uint64_t, std::uint64_t>>{{5, kGeneration}}));
}

// submit: "Error::Code::Unavailable, "backend_unavailable": the backend was
// reset, its session was unloaded or destroyed, or it is shutting down".
// release_session: "or Error::Code::Unavailable, "backend_unavailable"".
// health: "Error::Code::Unavailable ("backend_unavailable")".
TEST_P(BoundedJobBridgeConformance, AfterAnUnloadTheBridgeIsUnavailableAndSilent) {
  ASSERT_NO_FATAL_FAILURE(ready());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));

  under->unload();

  const std::size_t delivered = sink->events().size();
  expect_refused(resubmit(decode(2, 6)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(release(6), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(health(), Error::Code::Unavailable, "backend_unavailable");
  ASSERT_TRUE(settled());
  EXPECT_EQ(sink->events().size(), delivered);
  EXPECT_TRUE(sink->session_releases().empty());
}

// JobEventSink: "also when the backend resets ... (then `failed` with
// Error::Code::Unavailable and reason "backend_reset" ...)"; "`released`
// follows only once release actually happened". BridgeUnderTest::lose_backend().
TEST_P(BoundedJobBridgeConformance, ALostBackendFailsItsJobsAndReleasesThemOnlyAtTheUnload) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 5)).has_value());
  ASSERT_TRUE(settled());

  under->lose_backend();

  for (const std::uint64_t job_id : {1U, 2U}) {
    ASSERT_TRUE(wait_kinds(job_id, {Accepted, Failed})) << job_id;
    const Error lost = event_of(job_id, Failed).error().value();
    EXPECT_EQ(lost.code, Error::Code::Unavailable);
    EXPECT_EQ(lost.context.value_or(""), "backend_reset");
  }
  ASSERT_TRUE(settled());
  EXPECT_EQ(sink->kinds_for(1), (Kinds{Accepted, Failed}));
  EXPECT_EQ(sink->kinds_for(2), (Kinds{Accepted, Failed}));

  under->unload();

  EXPECT_EQ(sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(sink->kinds_for(2), (Kinds{Accepted, Failed, Released}));
}

// cancel: "cancelling a job whose terminal event was already delivered,
// succeeds and changes nothing". submit, release_session and health:
// "Error::Code::Unavailable", "backend_unavailable".
TEST_P(BoundedJobBridgeConformance, AfterItsBackendWasLostTheBridgeIsUnavailable) {
  ASSERT_NO_FATAL_FAILURE(ready());
  under->hold_jobs();
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  under->lose_backend();
  ASSERT_TRUE(wait_kinds(1, {Accepted, Failed}));

  EXPECT_TRUE(cancel(1).has_value());
  expect_refused(resubmit(decode(2, 5)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(release(6), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(health(), Error::Code::Unavailable, "backend_unavailable");
  ASSERT_TRUE(settled());
  EXPECT_EQ(sink->kinds_for(1), (Kinds{Accepted, Failed}));
  EXPECT_TRUE(sink->session_releases().empty());
}

}  // namespace tensorplate::testing
