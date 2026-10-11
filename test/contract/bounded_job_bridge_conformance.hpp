// SPDX-License-Identifier: Apache-2.0
//
// What include/tensorplate/backend/bounded_job_bridge.hpp promises the caller
// of a bridge whose backend runs `stt_decode` and `tts_synthesis`, as cases
// that hold for every such bridge. The comment above a case quotes what it
// holds, from that header unless it names another, and names what the case
// relies on of its binding: the backend's choices (BridgeUnderTest::Choices)
// and what the binding's unload() and lose_backend() promise.
//
// A binding supplies the bridge with a backend behind it, the hold a case
// needs on that backend and what its backend does where the header leaves a
// choice (BridgeUnderTest). The cases reach the bridge through
// BoundedJobBridge only, record with a RecordingJobEventSink, which checks
// every job's event order again, and never wait on the clock: an order a case
// needs is forced by holding the backend's jobs. Where the header allows two
// outcomes a case accepts both; a binding's own tests hold which one its
// backend produces.
//
// Not held here, because a binding has no hook for them: `job_not_permitted`
// and its place in the order of submit's refusals, `health_timeout`, the
// `failed` that replaces a backend event a job's order refuses, a late or
// foreign event being dropped, bridge calls beside a running lifecycle call,
// and calls from several threads at once. The destruction of the session is
// for a binding's own tests.
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
#include <set>
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
class BridgeUnderTest {
 public:
  /// What the header leaves to a backend, as this one does it. A case names
  /// the members it needs; unstated, it is skipped or holds the header alone.
  struct Choices {
    /// hold_jobs() holds. Without it every case that needs an open job is
    /// skipped.
    bool holds_started_jobs = false;
    /// The backend does not run `vad_frames`, the class the cases submit as
    /// one it refuses.
    bool refuses_vad_frames = false;
    /// It runs one job at a time: a job submitted while a started one is held
    /// has not started.
    bool runs_one_job_at_a_time = false;
    /// A job that has not started ends as soon as it is cancelled, while a
    /// started one is held.
    bool ends_a_waiting_job_at_its_cancel = false;
    /// A job cancelled while it is held ends `failed` with
    /// Error::Code::Cancelled, not `completed`.
    bool fails_a_held_job_that_was_cancelled = false;
    /// Jobs it has open at once while started ones are held, running and
    /// waiting together; one more is refused by submit or ended with `failed`.
    /// Zero states no bound.
    std::size_t open_jobs = 0;
  };

  BridgeUnderTest() = default;
  BridgeUnderTest(const BridgeUnderTest&) = delete;
  BridgeUnderTest& operator=(const BridgeUnderTest&) = delete;
  virtual ~BridgeUnderTest() = default;

  [[nodiscard]] virtual Choices choices() const = 0;
  [[nodiscard]] virtual BoundedJobBridge& bridge() = 0;
  /// Where a request's PCM is read and a result's PCM is published.
  [[nodiscard]] virtual BufferManager& buffers() = 0;

  /// The session's prime: the backend runs jobs from here on.
  virtual void prime() = 0;
  /// The session's unload, primed or not; a second one does nothing. When it
  /// returns every job has its terminal event and `released`, every pending
  /// session is acknowledged and no callback is still owed.
  virtual void unload() = 0;
  /// The backend is gone and its session, primed or not, is not told. Its
  /// jobs fail, which shows the bridge knows, and none is released before
  /// unload().
  virtual void lose_backend() = 0;

  /// From now until release_jobs(), a job that has started gets no terminal
  /// event. unload() and lose_backend() end it all the same.
  virtual void hold_jobs() = 0;
  virtual void release_jobs() = 0;
  /// True once the backend has taken in everything submitted so far and every
  /// callback owed for it has returned, on any thread, as far as held jobs allow.
  [[nodiscard]] virtual bool settled(std::chrono::milliseconds patience) = 0;

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
  using enum JobEventKind;
  using Kinds = std::vector<JobEventKind>;
  using Sessions = std::vector<std::pair<std::uint64_t, std::uint64_t>>;

  /// What one callback's calls into the bridge answered.
  struct CalledBack {
    /// The callback: an event kind's name, or "session_released".
    std::string from;
    std::uint64_t job_id = 0;
    std::uint64_t session_key = 0;
    Result<void> submitted;
    Result<void> cancelled;
    Result<void> released;
    /// An event of the job or the session's acknowledgement arrived before
    /// the callback returned.
    bool answered_inside = false;
  };

  /// How long a wait lasts before the case fails.
  static constexpr std::chrono::seconds kPatience{30};
  static constexpr std::uint64_t kGeneration = 3;

  void SetUp() override {
    under = GetParam().make();
    choices = under->choices();
    sink = std::make_shared<RecordingJobEventSink>();
    sink->set_callback_hook([this](const JobEvent& event) {
      note_callback();
      if (on_event) {
        on_event(event);
      }
    });
    sink->set_session_release_hook([this](std::uint64_t, std::uint64_t) {
      note_callback();
      if (on_session_released) {
        on_session_released();
      }
    });
  }

  // "Each event goes to the callback named for its kind." "The events of one
  // job follow the order in tensorplate/core/job_event.hpp." "Callbacks are
  // serialized: the bridge never runs two at once." "A callback never runs on
  // a thread while that thread is inside a method of the bridge." Held for
  // every case.
  void TearDown() override {
    // A hook reaches the bridge through `under`: no callback may be owed
    // when it goes, also after a case that failed half way.
    under->unload();
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

  /// False, with the case skipped, when the binding does not state `choice`.
  [[nodiscard]] static bool stated(bool choice, std::string_view name) {
    if (!choice) {
      skip(name);
    }
    return choice;
  }

  /// ready() with started jobs held; false, with the case skipped, for a
  /// binding without BridgeUnderTest::Choices::holds_started_jobs.
  [[nodiscard]] bool ready_and_holding() {
    if (!stated(choices.holds_started_jobs, "holds_started_jobs")) {
      return false;
    }
    ready();
    under->hold_jobs();
    return !HasFatalFailure();
  }

  /// A job submitted behind a held one waits, and ends at its cancel.
  [[nodiscard]] bool ends_waiting_jobs_at_once() const {
    return choices.runs_one_job_at_a_time && choices.ends_a_waiting_job_at_its_cancel;
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

  /// A job of the class BridgeUnderTest::Choices::refuses_vad_frames names.
  [[nodiscard]] JobRequest vad(std::uint64_t job_id, std::uint64_t session_key) {
    constexpr std::uint32_t kFrameSamples = 512;
    const BufferRef buffer = under->buffers().allocate(2U * kFrameSamples).value();
    return JobRequest::create(
               JobIdentity{job_id, session_key, kGeneration}, JobClass::VadFrames,
               VadFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, 2U * kFrameSamples},
                         kFrameSamples, 1, 1})
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

  /// As many jobs as BridgeUnderTest::Choices::open_jobs says the held backend
  /// has open at once, with ids from 1.
  void fill_the_backend() {
    ASSERT_TRUE(submit(decode(1, 5)).has_value());
    ASSERT_TRUE(settled());
    for (std::uint64_t job_id = 2; job_id <= choices.open_jobs; ++job_id) {
      ASSERT_TRUE(submit(decode(job_id, 5)).has_value()) << job_id;
    }
    ASSERT_TRUE(settled());
  }

  static void expect_refused(const Result<void>& result, Error::Code code,
                             std::string_view reason) {
    ASSERT_FALSE(result.has_value()) << reason;
    EXPECT_EQ(result.error().code, code) << reason;
    EXPECT_EQ(result.error().context.value_or(""), reason);
  }

  /// `backend_not_ready` or `backend_unavailable`, each under its code.
  static void expect_not_ready_or_unavailable(const Result<void>& result) {
    ASSERT_FALSE(result.has_value());
    if (result.error().code == Error::Code::NotReady) {
      expect_refused(result, Error::Code::NotReady, "backend_not_ready");
    } else {
      expect_refused(result, Error::Code::Unavailable, "backend_unavailable");
    }
  }

  // -- Calls from callbacks. Their answers live in the fixture, which outlives
  // every callback. ----------------------------------------------------------------

  /// Records what a bridge call made in a callback answered.
  void answered(Result<void> answer) {
    const std::lock_guard<std::mutex> lock(noted_);
    answers_.push_back(std::move(answer));
  }

  [[nodiscard]] std::vector<Result<void>> answers() {
    const std::lock_guard<std::mutex> lock(noted_);
    return answers_;
  }

  /// The first callback of each kind, and the first session acknowledgement,
  /// submits a job, cancels it and releases a session that holds nothing.
  void call_back_from_every_kind_of_callback() {
    on_event = [this](const JobEvent& event) { call_back(std::string(to_string(event.kind()))); };
    on_session_released = [this] { call_back("session_released"); };
  }

  [[nodiscard]] std::vector<CalledBack> called_back() {
    const std::lock_guard<std::mutex> lock(noted_);
    return called_back_;
  }

  /// The job a callback submitted was released and the session it released
  /// was acknowledged, as far as the bridge took them.
  [[nodiscard]] bool finished(const CalledBack& made) const {
    return (!made.submitted.has_value() || released(made.job_id)) &&
           (!made.released.has_value() || acknowledged(made.session_key));
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
    return wait([&] { return released(job_id); });
  }

  [[nodiscard]] bool wait_acknowledged(std::uint64_t session_key) {
    return wait([&] { return acknowledged(session_key); });
  }

  [[nodiscard]] bool settled() { return under->settled(kPatience); }

  [[nodiscard]] bool released(std::uint64_t job_id) const {
    return sink->sequence(job_id).value().released();
  }

  [[nodiscard]] bool acknowledged(std::uint64_t session_key) const {
    const Sessions sessions = sink->session_releases();
    return std::any_of(sessions.begin(), sessions.end(),
                       [&](const auto& session) { return session.first == session_key; });
  }

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

  [[nodiscard]] std::size_t terminal_events(std::uint64_t job_id) const {
    return count(job_id, Failed) + count(job_id, Completed);
  }

  /// The job's events without a leading `accepted`, which job_event.hpp lets
  /// a job that fails end without.
  [[nodiscard]] Kinds after_acceptance(std::uint64_t job_id) const {
    Kinds kinds = sink->kinds_for(job_id);
    if (!kinds.empty() && kinds.front() == Accepted) {
      kinds.erase(kinds.begin());
    }
    return kinds;
  }

  /// A released job that ended with `failed` carrying `code` and `reason`
  /// and never completed.
  void expect_failed(std::uint64_t job_id, Error::Code code, std::string_view reason = "") {
    const Kinds kinds = sink->kinds_for(job_id);
    ASSERT_GE(kinds.size(), 2U) << job_id;
    EXPECT_EQ(kinds[kinds.size() - 2], Failed) << job_id;
    EXPECT_EQ(kinds.back(), Released) << job_id;
    ASSERT_EQ(count(job_id, Failed), 1U) << job_id;
    EXPECT_EQ(count(job_id, Completed), 0U) << job_id;
    const Error error = event_of(job_id, Failed).error().value();
    EXPECT_EQ(error.code, code) << job_id;
    if (!reason.empty()) {
      EXPECT_EQ(error.context.value_or(""), reason) << job_id;
    }
  }

  /// A released job that was cancelled: `failed` with Error::Code::Cancelled,
  /// or `completed`, as job_event.hpp allows.
  void expect_cancelled_or_completed(std::uint64_t job_id) {
    if (count(job_id, Completed) == 0) {
      expect_failed(job_id, Error::Code::Cancelled);
      return;
    }
    EXPECT_EQ(terminal_events(job_id), 1U) << job_id;
    EXPECT_TRUE(released(job_id)) << job_id;
  }

  std::unique_ptr<BridgeUnderTest> under;
  BridgeUnderTest::Choices choices;
  std::shared_ptr<RecordingJobEventSink> sink;
  /// Runs inside every job callback. Set it before the first submit.
  std::function<void(const JobEvent&)> on_event;
  /// Runs inside every session acknowledgement. Set it before the first
  /// submit.
  std::function<void()> on_session_released;
  std::atomic<int> callbacks_inside_a_call{0};

 private:
  static void skip(std::string_view choice) {
    GTEST_SKIP() << "the binding does not state BridgeUnderTest::Choices::" << choice;
  }

  void note_callback() {
    if (InsideABridgeCall::depth() != 0) {
      ++callbacks_inside_a_call;
    }
    const std::lock_guard<std::mutex> lock(mutex_);
    changed_.notify_all();
  }

  // Callbacks are serialized, so the first of a kind is still the first when
  // its calls have returned.
  void call_back(const std::string& from) {
    std::uint64_t nth = 0;
    {
      const std::lock_guard<std::mutex> lock(noted_);
      if (std::any_of(called_back_.begin(), called_back_.end(),
                      [&](const CalledBack& made) { return made.from == from; })) {
        return;
      }
      nth = called_back_.size();
    }
    CalledBack made{.from = from, .job_id = 100 + nth, .session_key = 200 + nth};
    made.submitted = submit(decode(made.job_id, 100 + nth));
    made.cancelled = cancel(made.job_id);
    made.released = release(made.session_key);
    made.answered_inside = !sink->kinds_for(made.job_id).empty() || acknowledged(made.session_key);
    const std::lock_guard<std::mutex> lock(noted_);
    called_back_.push_back(std::move(made));
  }

  std::mutex mutex_;
  std::condition_variable changed_;
  /// Guards what callbacks note for the case to read.
  std::mutex noted_;
  std::vector<Result<void>> answers_;
  std::vector<CalledBack> called_back_;
};

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

// set_event_sink: "Registers the sink that receives every event of this
// bridge." "After a method fails, no event is delivered because of that call."
TEST_P(BoundedJobBridgeConformance, ARefusedSecondSinkDoesNotReplaceTheFirst) {
  ASSERT_NO_FATAL_FAILURE(ready());
  const auto second = std::make_shared<RecordingJobEventSink>();
  expect_refused(set_sink(second), Error::Code::ConfigInvalid, "event_sink_already_set");
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(wait_acknowledged(5));
  EXPECT_TRUE(released(1));
  ASSERT_TRUE(settled());
  EXPECT_TRUE(second->events().empty());
  EXPECT_TRUE(second->session_releases().empty());
}

// set_event_sink: "The bridge keeps the sink until the bridge is destroyed, so
// the sink must not own the bridge."
TEST_P(BoundedJobBridgeConformance, TheBridgeKeepsItsSinkOnceTheCallerHasDroppedIt) {
  ASSERT_NO_FATAL_FAILURE(ready());
  const std::weak_ptr<RecordingJobEventSink> registered = sink;
  sink.reset();
  const bool kept = !registered.expired();
  // Taken back to read it; a new one for TearDown when the bridge let go.
  sink = kept ? registered.lock() : std::make_shared<RecordingJobEventSink>();
  ASSERT_TRUE(kept);
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
}

// submit: "Error::Code::NotReady, "event_sink_missing": no sink registered".
// release_session: "or Error::Code::NotReady, "event_sink_missing"".
TEST_P(BoundedJobBridgeConformance, WithoutASinkSubmitAndReleaseAreRefused) {
  ASSERT_FALSE(set_sink(nullptr).has_value());
  under->prime();
  expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "event_sink_missing");
  expect_refused(release(5), Error::Code::NotReady, "event_sink_missing");
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

// release_session: "@return success, or Error::Code::ConfigInvalid,
// "session_key_zero" or "generation_zero"; or Error::Code::NotReady,
// "event_sink_missing"; or Error::Code::Unavailable, "backend_unavailable"":
// submit's `backend_not_ready` is not among them, before the prime either.
// A release that succeeds is acknowledged "also when the backend held nothing
// for the session".
TEST_P(BoundedJobBridgeConformance, BeforeThePrimeAReleaseSucceedsOrIsUnavailable) {
  ASSERT_TRUE(set_sink(sink).has_value());
  const Result<void> taken = release(5);
  under->prime();
  if (taken.has_value()) {
    ASSERT_TRUE(wait_acknowledged(5));
    expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "session_releasing");
  } else {
    expect_refused(taken, Error::Code::Unavailable, "backend_unavailable");
    ASSERT_TRUE(settled());
    EXPECT_TRUE(sink->session_releases().empty());
  }
}

// health: "Reports whether the backend can run jobs now, within the
// implementation's bounded health deadline and without entering the session
// lifecycle or waiting for a job." BridgeUnderTest::Choices::holds_started_jobs.
TEST_P(BoundedJobBridgeConformance, APrimedBackendIsHealthyAlsoWhileAJobRuns) {
  if (!ready_and_holding()) {
    return;
  }
  EXPECT_TRUE(health().has_value());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  EXPECT_TRUE(health().has_value());
  under->release_jobs();
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
}

// -- The order of submit's refusals. ---------------------------------------------
//
// submit: "@return success, or the first of these that applies:". Each case
// makes two entries of that list apply at once and quotes both, the earlier
// one first. `job_not_permitted` has no case: no binding can make a profile
// refuse a request.

// "Error::Code::NotReady, "event_sink_missing": no sink registered", then
// "Error::Code::NotReady, "backend_not_ready": the backend cannot run jobs
// yet, for example before its session is primed".
TEST_P(BoundedJobBridgeConformance, AMissingSinkIsRefusedAheadOfABackendThatIsNotReady) {
  ASSERT_FALSE(set_sink(nullptr).has_value());
  expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "event_sink_missing");
}

// "Error::Code::Unavailable, "backend_unavailable": the backend was reset, its
// session was unloaded or destroyed, or it is shutting down". The entry names
// an unloaded session, primed or not, so a session unloaded before its prime
// answers it, and nothing later in the list: the class of
// BridgeUnderTest::Choices::refuses_vad_frames is refused the same way.
// BridgeUnderTest::unload() before the prime.
TEST_P(BoundedJobBridgeConformance, ASessionUnloadedBeforeItsPrimeIsUnavailable) {
  ASSERT_TRUE(set_sink(sink).has_value());
  under->unload();
  expect_refused(resubmit(decode(1, 5)), Error::Code::Unavailable, "backend_unavailable");
  if (choices.refuses_vad_frames) {
    expect_refused(resubmit(vad(2, 5)), Error::Code::Unavailable, "backend_unavailable");
  }
}

// "Error::Code::NotReady, "backend_not_ready": the backend cannot run jobs
// yet, for example before its session is primed", then
// "Error::Code::Unavailable, "backend_unavailable": the backend was reset, its
// session was unloaded or destroyed, or it is shutting down", for a backend
// that BridgeUnderTest::lose_backend() took away before the prime. The header
// does not say whether the first still applies to a backend that is gone, so
// either is accepted, and nothing later in the list: the class is that of
// Choices::refuses_vad_frames.
TEST_P(BoundedJobBridgeConformance, ABackendLostBeforeThePrimeIsNotReadyOrUnavailable) {
  ASSERT_TRUE(set_sink(sink).has_value());
  under->lose_backend();
  expect_not_ready_or_unavailable(resubmit(decode(1, 5)));
  if (choices.refuses_vad_frames) {
    expect_not_ready_or_unavailable(resubmit(vad(2, 5)));
  }
}

// "Error::Code::NotReady, "backend_not_ready": the backend cannot run jobs
// yet, for example before its session is primed", then
// "Error::Code::Unsupported, "job_class_unsupported": the loaded backend does
// not run the request's job class"; `backend_unavailable`, between them, does
// not apply before the prime. BridgeUnderTest::Choices::refuses_vad_frames.
TEST_P(BoundedJobBridgeConformance, ABackendThatIsNotReadyIsRefusedAheadOfAClassItDoesNotRun) {
  if (!stated(choices.refuses_vad_frames, "refuses_vad_frames")) {
    return;
  }
  ASSERT_TRUE(set_sink(sink).has_value());
  expect_refused(resubmit(vad(1, 5)), Error::Code::NotReady, "backend_not_ready");
}

// "Error::Code::Unavailable, "backend_unavailable": the backend was reset, its
// session was unloaded or destroyed, or it is shutting down", then
// "Error::Code::Unsupported, "job_class_unsupported": the loaded backend does
// not run the request's job class". BridgeUnderTest::Choices::
// refuses_vad_frames and unload().
TEST_P(BoundedJobBridgeConformance, AnUnavailableBackendIsRefusedAheadOfAClassItDoesNotRun) {
  if (!stated(choices.refuses_vad_frames, "refuses_vad_frames")) {
    return;
  }
  ASSERT_NO_FATAL_FAILURE(ready());
  under->unload();
  expect_refused(resubmit(vad(1, 5)), Error::Code::Unavailable, "backend_unavailable");
}

// "Error::Code::Unsupported, "job_class_unsupported": the loaded backend does
// not run the request's job class", then "Error::Code::NotReady,
// "session_releasing": release_session() was called for the request's session
// key"; `job_not_permitted`, between them, no binding can reach.
// BridgeUnderTest::Choices::refuses_vad_frames.
TEST_P(BoundedJobBridgeConformance, AClassTheBackendDoesNotRunIsRefusedAheadOfAReleasingSession) {
  if (!stated(choices.refuses_vad_frames, "refuses_vad_frames")) {
    return;
  }
  ASSERT_NO_FATAL_FAILURE(ready());
  ASSERT_TRUE(release(5).has_value());
  expect_refused(resubmit(vad(1, 5)), Error::Code::Unsupported, "job_class_unsupported");
  expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "session_releasing");
}

// "Error::Code::NotReady, "session_releasing": release_session() was called
// for the request's session key", then "Error::Code::ConfigInvalid,
// "duplicate_job_id": a job with the same job_id has not been released yet".
// BridgeUnderTest::Choices::holds_started_jobs keeps job 1 unreleased.
TEST_P(BoundedJobBridgeConformance, AReleasingSessionIsRefusedAheadOfADuplicateJobId) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 6)).has_value());
  ASSERT_TRUE(release(5).has_value());
  expect_refused(resubmit(decode(1, 5)), Error::Code::NotReady, "session_releasing");
  expect_refused(resubmit(decode(1, 7)), Error::Code::ConfigInvalid, "duplicate_job_id");
  under->release_jobs();
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
}

// "Error::Code::ConfigInvalid, "duplicate_job_id": a job with the same job_id
// has not been released yet", then "Error::Code::ResourceExhausted,
// "job_capacity_exhausted": the backend's bounded admission is full". A
// backend that ends the job beyond its bound with `failed` refuses the
// duplicate all the same. BridgeUnderTest::Choices::open_jobs and
// holds_started_jobs.
TEST_P(BoundedJobBridgeConformance, ADuplicateJobIdIsRefusedAheadOfExhaustedCapacity) {
  if (!stated(choices.open_jobs != 0, "open_jobs") || !ready_and_holding()) {
    return;
  }
  ASSERT_NO_FATAL_FAILURE(fill_the_backend());
  expect_refused(resubmit(decode(1, 5)), Error::Code::ConfigInvalid, "duplicate_job_id");
  under->release_jobs();
  for (std::uint64_t job_id = 1; job_id <= choices.open_jobs; ++job_id) {
    EXPECT_TRUE(wait_kinds(job_id, {Accepted, Completed, Released})) << job_id;
  }
}

// -- Jobs. -----------------------------------------------------------------------

// submit: "Success means the bridge took the job and its events follow through
// the sink." job_event.hpp: "Exactly one of `completed` (requires `accepted`)
// or `failed` ends the job's output. Nothing but `released` follows it."
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
// BridgeUnderTest::Choices::refuses_vad_frames.
TEST_P(BoundedJobBridgeConformance, AClassTheBackendDoesNotRunIsRefused) {
  if (!stated(choices.refuses_vad_frames, "refuses_vad_frames")) {
    return;
  }
  ASSERT_NO_FATAL_FAILURE(ready());
  expect_refused(resubmit(vad(1, 5)), Error::Code::Unsupported, "job_class_unsupported");
}

// submit: "Error::Code::ConfigInvalid, "duplicate_job_id": a job with the same
// job_id has not been released yet".
// BridgeUnderTest::Choices::holds_started_jobs.
TEST_P(BoundedJobBridgeConformance, AJobIdIsRefusedUntilItsJobWasReleased) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  expect_refused(resubmit(decode(1, 5)), Error::Code::ConfigInvalid, "duplicate_job_id");
  expect_refused(resubmit(synthesis(1, 6)), Error::Code::ConfigInvalid, "duplicate_job_id");
  under->release_jobs();
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released, Accepted, Completed, Released}));
}

// "an implementation that admits fewer refuses the rest with
// Error::Code::ResourceExhausted"; submit: "Error::Code::ResourceExhausted,
// "job_capacity_exhausted": the backend's bounded admission is full".
// job_event.hpp: "A job the backend refuses, or that is cancelled before
// admission, ends with `failed` without it."
// BridgeUnderTest::Choices::open_jobs and holds_started_jobs.
TEST_P(BoundedJobBridgeConformance, OneJobBeyondTheBackendsBoundIsTurnedAway) {
  if (!stated(choices.open_jobs != 0, "open_jobs") || !ready_and_holding()) {
    return;
  }
  ASSERT_NO_FATAL_FAILURE(fill_the_backend());
  const std::uint64_t beyond = choices.open_jobs + 1;
  for (std::uint64_t job_id = 1; job_id < beyond; ++job_id) {
    EXPECT_EQ(terminal_events(job_id), 0U) << job_id;
  }
  const Result<void> taken = submit(decode(beyond, 5));
  ASSERT_TRUE(settled());
  if (taken.has_value()) {
    EXPECT_EQ(sink->kinds_for(beyond), (Kinds{Failed, Released}));
    expect_failed(beyond, Error::Code::ResourceExhausted, "job_capacity_exhausted");
  } else {
    expect_refused(taken, Error::Code::ResourceExhausted, "job_capacity_exhausted");
    EXPECT_TRUE(sink->kinds_for(beyond).empty());
  }
  under->release_jobs();
  for (std::uint64_t job_id = 1; job_id < beyond; ++job_id) {
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
  ASSERT_TRUE(settled());
  expect_refused(cancel(1), Error::Code::NotReady, "unknown_job");
}

// cancel: "The job still ends with exactly one terminal event and then, once
// its release is confirmed, `released`". job_event.hpp: "`failed` with
// Error::Code::Cancelled if the backend stopped the job, `completed` if its
// bounded work had already finished." BridgeUnderTest::Choices::
// holds_started_jobs; with runs_one_job_at_a_time and
// ends_a_waiting_job_at_its_cancel the job ends, as `failed`, while the first
// one is held.
TEST_P(BoundedJobBridgeConformance, ACancelledWaitingJobEndsOnceAndIsReleased) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(cancel(2).has_value());
  if (ends_waiting_jobs_at_once()) {
    ASSERT_TRUE(wait_released(2));
    expect_failed(2, Error::Code::Cancelled);
    EXPECT_EQ(terminal_events(1), 0U);
  }
  under->release_jobs();
  ASSERT_TRUE(wait_released(2));
  expect_cancelled_or_completed(2);
  EXPECT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
}

// cancel: "Idempotent: repeating it, or cancelling a job whose terminal event
// was already delivered, succeeds and changes nothing." job_event.hpp:
// "`cancel_acknowledged` (at most once)"; "The terminal event still follows".
// BridgeUnderTest::Choices::holds_started_jobs keeps the job open; with
// fails_a_held_job_that_was_cancelled it ends `failed`.
TEST_P(BoundedJobBridgeConformance, ACancelledRunningJobEndsOnceWhenItsWorkReturns) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(cancel(1).has_value());
  ASSERT_TRUE(cancel(1).has_value());
  ASSERT_TRUE(settled());
  EXPECT_EQ(terminal_events(1), 0U);
  under->release_jobs();
  ASSERT_TRUE(wait_released(1));
  EXPECT_LE(count(1, CancelAcknowledged), 1U);
  if (choices.fails_a_held_job_that_was_cancelled) {
    expect_failed(1, Error::Code::Cancelled);
  } else {
    expect_cancelled_or_completed(1);
  }
}

// cancel: "cancelling a job whose terminal event was already delivered,
// succeeds and changes nothing". job_event.hpp: "never after the terminal
// event". The cancel is made from the job's `completed` callback, before its
// `released`.
TEST_P(BoundedJobBridgeConformance, ACancelAfterTheTerminalEventSucceedsAndChangesNothing) {
  on_event = [this](const JobEvent& event) {
    if (event.kind() == Completed) {
      answered(cancel(event.identity().job_id));
    }
  };
  ASSERT_NO_FATAL_FAILURE(ready());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(settled());
  ASSERT_EQ(answers().size(), 1U);
  EXPECT_TRUE(answers()[0].has_value());
  EXPECT_EQ(sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
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
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(wait_acknowledged(5));
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(settled());
  EXPECT_EQ(sink->session_releases(), (Sessions{{5, kGeneration}}));
}

// release_session: "cancels its unfinished jobs as cancel() does (they still
// end and are released as usual)". JobEventSink: "on_session_released follows
// the `released` event of every job of that session submitted before the
// release_session() call". BridgeUnderTest::Choices::holds_started_jobs; with
// runs_one_job_at_a_time and ends_a_waiting_job_at_its_cancel the second job
// ends while the first is held, and with fails_a_held_job_that_was_cancelled
// the first ends `failed`.
TEST_P(BoundedJobBridgeConformance, AReleaseEndsTheSessionsJobsAndIsAcknowledgedAfterThem) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 5)).has_value());
  ASSERT_TRUE(submit(decode(3, 6)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(release(5).has_value());
  if (ends_waiting_jobs_at_once()) {
    ASSERT_TRUE(wait_released(2));
    expect_failed(2, Error::Code::Cancelled);
  }
  ASSERT_TRUE(settled());
  EXPECT_TRUE(sink->session_releases().empty());
  EXPECT_EQ(terminal_events(1) + terminal_events(3), 0U);
  under->release_jobs();
  ASSERT_TRUE(wait_acknowledged(5));
  EXPECT_EQ(sink->session_releases(), (Sessions{{5, kGeneration}}));
  if (choices.fails_a_held_job_that_was_cancelled) {
    expect_failed(1, Error::Code::Cancelled);
  } else {
    expect_cancelled_or_completed(1);
  }
  expect_cancelled_or_completed(2);
  EXPECT_TRUE(wait_kinds(3, {Accepted, Completed, Released}));
}

// submit: "Error::Code::NotReady, "session_releasing": release_session() was
// called for the request's session key". release_session: "The session key is
// never submitted again." BridgeUnderTest::Choices::holds_started_jobs.
TEST_P(BoundedJobBridgeConformance, AReleasedSessionTakesNoJobBeforeOrAfterItsAcknowledgement) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(release(5).has_value());
  expect_refused(resubmit(decode(2, 5)), Error::Code::NotReady, "session_releasing");
  under->release_jobs();
  ASSERT_TRUE(wait_acknowledged(5));
  expect_refused(resubmit(decode(3, 5)), Error::Code::NotReady, "session_releasing");
  ASSERT_TRUE(submit(decode(4, 6)).has_value());
  EXPECT_TRUE(wait_kinds(4, {Accepted, Completed, Released}));
}

// -- Callbacks. ------------------------------------------------------------------

// JobEventSink: "It may call submit(), cancel() and release_session() on the
// bridge, whose events then arrive after it returns".
TEST_P(BoundedJobBridgeConformance, ACallbackOfAnyKindMaySubmitCancelAndRelease) {
  call_back_from_every_kind_of_callback();
  ASSERT_NO_FATAL_FAILURE(ready());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(wait([&] {
    const std::vector<CalledBack> made = called_back();
    return std::all_of(made.begin(), made.end(),
                       [&](const CalledBack& one) { return finished(one); });
  }));
  ASSERT_TRUE(settled());

  std::set<std::string> called_from;
  for (const CalledBack& made : called_back()) {
    called_from.insert(made.from);
    EXPECT_TRUE(made.submitted.has_value()) << made.from;
    EXPECT_TRUE(made.cancelled.has_value()) << made.from;
    EXPECT_TRUE(made.released.has_value()) << made.from;
    EXPECT_FALSE(made.answered_inside) << made.from;
    ASSERT_TRUE(finished(made)) << made.from;
    EXPECT_EQ(terminal_events(made.job_id), 1U) << made.from;
  }
  std::set<std::string> arrived{"session_released"};
  for (const JobEvent& event : sink->events()) {
    arrived.insert(std::string(to_string(event.kind())));
  }
  EXPECT_EQ(called_from, arrived);
  for (const char* kind : {"accepted", "completed", "released", "session_released"}) {
    EXPECT_TRUE(called_from.contains(kind)) << kind;
  }
}

// -- The session's end. ----------------------------------------------------------

// JobEventSink: "Every job whose submit() succeeded receives exactly one
// `completed` or `failed`, also when the backend resets or the session unloads
// (then `failed` with Error::Code::Unavailable and reason "backend_reset" or
// "session_unloaded")." That the unload ends a held job, and that all of it
// has happened when the unload returns, are BridgeUnderTest::hold_jobs() and
// unload(). BridgeUnderTest::Choices::holds_started_jobs; with
// runs_one_job_at_a_time the second job has not started, so the unload is
// what ends it.
TEST_P(BoundedJobBridgeConformance, AnUnloadEndsEveryJobAndAcknowledgesEverySession) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 6)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(settled());
  EXPECT_TRUE(sink->session_releases().empty());

  under->unload();

  EXPECT_TRUE(released(1));
  EXPECT_EQ(terminal_events(1), 1U);
  if (choices.runs_one_job_at_a_time) {
    expect_failed(2, Error::Code::Unavailable, "session_unloaded");
  } else {
    EXPECT_TRUE(released(2));
    EXPECT_EQ(terminal_events(2), 1U);
  }
  EXPECT_EQ(sink->session_releases(), (Sessions{{5, kGeneration}}));
}

// submit: "Error::Code::Unavailable, "backend_unavailable": the backend was
// reset, its session was unloaded or destroyed, or it is shutting down".
// release_session: "or Error::Code::Unavailable, "backend_unavailable"".
// health: "Error::Code::Unavailable ("backend_unavailable")". "After a method
// fails, no event is delivered because of that call." BridgeUnderTest::unload().
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

// cancel: "@return success, or Error::Code::NotReady, "unknown_job": no job
// with this id was submitted, or it was already released." When the unload
// has returned every job was released: BridgeUnderTest::unload().
TEST_P(BoundedJobBridgeConformance, AfterAnUnloadACancelIsRefusedAsUnknownJob) {
  ASSERT_NO_FATAL_FAILURE(ready());
  ASSERT_TRUE(submit(decode(1, 5)).has_value());

  under->unload();

  expect_refused(cancel(1), Error::Code::NotReady, "unknown_job");
  expect_refused(cancel(2), Error::Code::NotReady, "unknown_job");
}

// JobEventSink: "(then `failed` with Error::Code::Unavailable and reason
// "backend_reset" or "session_unloaded")"; "`released` follows only once
// release actually happened, including after a confirmed process reap".
// BridgeUnderTest::Choices::holds_started_jobs, lose_backend() and unload().
TEST_P(BoundedJobBridgeConformance, ALostBackendFailsItsJobsAndReleasesThemOnlyAtTheUnload) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(submit(decode(2, 5)).has_value());
  ASSERT_TRUE(settled());

  under->lose_backend();

  for (const std::uint64_t job_id : {1U, 2U}) {
    ASSERT_TRUE(wait([&] { return after_acceptance(job_id) == Kinds{Failed}; })) << job_id;
    const Error lost = event_of(job_id, Failed).error().value();
    EXPECT_EQ(lost.code, Error::Code::Unavailable);
    EXPECT_EQ(lost.context.value_or(""), "backend_reset");
  }
  ASSERT_TRUE(settled());
  EXPECT_EQ(after_acceptance(1), Kinds{Failed});
  EXPECT_EQ(after_acceptance(2), Kinds{Failed});

  under->unload();

  EXPECT_EQ(after_acceptance(1), (Kinds{Failed, Released}));
  EXPECT_EQ(after_acceptance(2), (Kinds{Failed, Released}));
}

// cancel: "cancelling a job whose terminal event was already delivered,
// succeeds and changes nothing". submit, release_session and health:
// "Error::Code::Unavailable", "backend_unavailable".
// BridgeUnderTest::Choices::holds_started_jobs and lose_backend().
TEST_P(BoundedJobBridgeConformance, AfterItsBackendWasLostTheBridgeIsUnavailable) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  under->lose_backend();
  ASSERT_TRUE(wait([&] { return after_acceptance(1) == Kinds{Failed}; }));

  EXPECT_TRUE(cancel(1).has_value());
  expect_refused(resubmit(decode(2, 5)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(release(6), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(health(), Error::Code::Unavailable, "backend_unavailable");
  ASSERT_TRUE(settled());
  EXPECT_EQ(after_acceptance(1), Kinds{Failed});
  EXPECT_TRUE(sink->session_releases().empty());
}

// JobEventSink: "a job whose release cannot be confirmed is never released and
// its reservations stay held." A cancel that was pending when the backend
// went does not release it either. BridgeUnderTest::Choices::
// holds_started_jobs, lose_backend() and unload().
TEST_P(BoundedJobBridgeConformance, ALostBackendLeavesACancelledJobUnreleasedUntilTheUnload) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(cancel(1).has_value());
  ASSERT_TRUE(settled());
  EXPECT_EQ(terminal_events(1), 0U);

  under->lose_backend();

  ASSERT_TRUE(wait([&] { return terminal_events(1) == 1; }));
  ASSERT_TRUE(settled());
  EXPECT_FALSE(released(1));

  under->unload();

  EXPECT_TRUE(released(1));
  EXPECT_EQ(terminal_events(1), 1U);
}

// JobEventSink: "a session with a job whose release cannot be confirmed is
// never acknowledged, and its reservations stay held." BridgeUnderTest::
// Choices::holds_started_jobs, lose_backend() and unload().
TEST_P(BoundedJobBridgeConformance, ALostBackendAcknowledgesAReleasingSessionOnlyAtTheUnload) {
  if (!ready_and_holding()) {
    return;
  }
  ASSERT_TRUE(submit(decode(1, 5)).has_value());
  ASSERT_TRUE(settled());
  ASSERT_TRUE(release(5).has_value());
  ASSERT_TRUE(settled());

  under->lose_backend();

  ASSERT_TRUE(wait([&] { return terminal_events(1) == 1; }));
  ASSERT_TRUE(settled());
  EXPECT_FALSE(released(1));
  EXPECT_TRUE(sink->session_releases().empty());

  under->unload();

  EXPECT_TRUE(released(1));
  EXPECT_EQ(sink->session_releases(), (Sessions{{5, kGeneration}}));
}

}  // namespace tensorplate::testing
