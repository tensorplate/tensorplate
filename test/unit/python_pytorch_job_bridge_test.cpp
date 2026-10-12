// SPDX-License-Identifier: Apache-2.0
//
// The Python/PyTorch adapter's job bridge over a scripted sidecar: what the
// bridge conformance cases (test/contract/bounded_job_bridge_conformance.hpp,
// bound to this bridge in bounded_job_bridge_conformance_test.cpp) leave to a
// binding's own tests, and what this bridge does where the public header
// leaves a choice.

#include <gtest/gtest.h>

#if !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobBridge, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <latch>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

#include "adapters/python_pytorch/job_bridge.hpp"
#include "adapters/python_pytorch/job_channel.hpp"
#include "adapters/python_pytorch/job_wire.hpp"
#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"
#include "job_bridge_harness.hpp"
#include "job_channel_harness.hpp"
#include "scripted_sidecar.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using testing::failure_of;
using testing::JobBridgeHarness;
using testing::ParkedCallback;
using testing::ScriptedBackend;
using testing::ScriptedSidecar;
using Json = nlohmann::json;
using Kinds = JobBridgeHarness::Kinds;
using Sessions = std::vector<std::pair<std::uint64_t, std::uint64_t>>;
using enum JobEventKind;

// No bound a test that works runs into, and jobs left as a caller that never
// heard of them leaves them: the factory enables them.
PythonPytorchConfig patient() {
  PythonPytorchConfig config;
  config.speech_jobs = false;
  config.startup_timeout = JobBridgeHarness::kPatience;
  config.health_timeout = JobBridgeHarness::kPatience;
  return config;
}

ModelSpec speech_spec() {
  return ModelSpec::create("speech", ModelClass::Speech, "/dev/null", "python_pytorch").value();
}

Json transcript() {
  return Json{{"type", "transcript"}, {"text", "hello"}, {"tokens", {1}}, {"words", Json::array()}};
}

Json audio(std::size_t bytes) {
  return Json{{"type", "audio_chunk"},
              {"format", {{"encoding", "pcm_s16le"}, {"sample_rate_hz", 24000}, {"channels", 1}}},
              {"clipped_samples", 0},
              {"payload_length", bytes}};
}

void expect_refused(const Result<void>& result, Error::Code code, std::string_view reason) {
  ASSERT_FALSE(result.has_value()) << reason;
  EXPECT_EQ(result.error().code, code) << reason;
  EXPECT_EQ(result.error().context.value_or(""), reason);
}

// A buffer manager, a sink and requests, bound to no channel.
struct Bench final : testing::JobBench {};

// The health timeout a bridge of this adapter was built with.
std::chrono::milliseconds health_timeout_of(const BoundedJobBridge& bridge) {
  return dynamic_cast<const JobBridge&>(bridge).health_timeout();
}

// An environment variable set, or unset for null, until the end of the scope.
class ScopedEnvironment {
 public:
  ScopedEnvironment(const char* name, const char* value) : name_(name) {
    if (const char* old = std::getenv(name); old != nullptr) {  // NOLINT(concurrency-mt-unsafe)
      saved_ = old;
    }
    set(value);
  }
  ~ScopedEnvironment() { set(saved_.has_value() ? saved_->c_str() : nullptr); }

  ScopedEnvironment(const ScopedEnvironment&) = delete;
  ScopedEnvironment& operator=(const ScopedEnvironment&) = delete;
  ScopedEnvironment(ScopedEnvironment&&) = delete;
  ScopedEnvironment& operator=(ScopedEnvironment&&) = delete;

 private:
  void set(const char* value) {
    if (value != nullptr) {
      (void)::setenv(name_, value, 1);  // NOLINT(concurrency-mt-unsafe)
    } else {
      (void)::unsetenv(name_);  // NOLINT(concurrency-mt-unsafe)
    }
  }

  const char* name_;
  std::optional<std::string> saved_;
};

class JobBridgeTest : public ::testing::Test {
 protected:
  // Loaded and primed: the bridge takes jobs.
  void up() {
    const auto loaded = h.session->load(speech_spec());
    ASSERT_TRUE(loaded.has_value()) << loaded.error().message;
    const auto primed = h.session->prime();
    ASSERT_TRUE(primed.has_value()) << primed.error().message;
  }

  // Job 1, taken by the sidecar's backend: it ends when the test says, and
  // the sidecar answers an unload only after it.
  void run_job_1() {
    ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
    ASSERT_EQ(peer.take_job(), 1U);
    ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  }

  ScriptedSidecar peer;
  JobBridgeHarness h{patient(), peer.launcher()};
};

// -- The factory. ----------------------------------------------------------------

TEST_F(JobBridgeTest, ASessionBuiltWithABridgeLoadsWithJobsWhateverItsConfigurationSays) {
  ASSERT_NO_FATAL_FAILURE(up());
  const auto loads = peer.sent("load_model", 1);
  ASSERT_EQ(loads.size(), 1U);
  EXPECT_EQ(loads[0].at("capabilities"), Json::array({"speech_jobs_v1"}));
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  EXPECT_TRUE(h.wait_kinds(1, {Accepted}));
}

TEST(JobBridgeRegistration, TheRegisteredBackendBuildsABridgeOnlyWithABufferManager) {
  BackendRegistry registry;
  ASSERT_TRUE(register_python_pytorch_backend(registry).has_value());
  const auto refused = registry.create_session_with_bridge(kBackendName);
  ASSERT_FALSE(refused.has_value());
  EXPECT_EQ(refused.error(), Error::make(Error::Code::ConfigInvalid,
                                         "python_pytorch adapter requires a BufferManager hook"));
  // A session alone needs none until it runs a request.
  EXPECT_TRUE(registry.create_session(kBackendName).has_value());

  Bench bench;
  ExecutionSessionRuntimeHooks hooks{};
  hooks.buffer_manager = bench.manager.get();
  const auto built = registry.create_session_with_bridge(kBackendName, hooks);
  ASSERT_TRUE(built.has_value()) << built.error().message;
  expect_refused(built.value().bridge->health(), Error::Code::NotReady, "backend_not_ready");
}

// -- Refusals ahead of the channel's. ----------------------------------------------

TEST_F(JobBridgeTest, AZeroKeyOrGenerationIsRefusedAheadOfEverythingTheChannelChecks) {
  // On a channel with no sink that was never started.
  JobBridge bare(std::make_shared<JobChannel>(h.manager.get(), std::chrono::seconds{5}),
                 JobBridgeHarness::kPatience);
  expect_refused(bare.release_session(0, 1), Error::Code::ConfigInvalid, "session_key_zero");
  expect_refused(bare.release_session(5, 0), Error::Code::ConfigInvalid, "generation_zero");
  expect_refused(bare.release_session(0, 0), Error::Code::ConfigInvalid, "session_key_zero");
  expect_refused(bare.release_session(5, 1), Error::Code::NotReady, "event_sink_missing");

  // And once the session has unloaded.
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.session->unload().has_value());
  expect_refused(h.release(0), Error::Code::ConfigInvalid, "session_key_zero");
  expect_refused(h.release(5, 0), Error::Code::ConfigInvalid, "generation_zero");
  expect_refused(h.release(5), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_TRUE(h.sink->session_releases().empty());
}

// -- What the conformance cases leave to a binding. --------------------------------

TEST_F(JobBridgeTest, AJobTheBackendDoesNotPermitIsTakenAndEndsFailedWithThatReason) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.refuse_next_submit();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->code, Error::Code::Unsupported);
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), "job_not_permitted");
}

TEST_F(JobBridgeTest, HealthGivesUpAtItsTimeoutAndALaterCallIsAnswered) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  JobBridge impatient(h.channel, std::chrono::milliseconds{0});
  expect_refused(impatient.health(), Error::Code::Timeout, "health_timeout");
  peer.release_health();
  EXPECT_TRUE(h.bridge->health().has_value());
}

// The value itself, read from the bridge the factory built: a health call
// that is answered shows no timeout but zero.
TEST(JobBridgeHealthTimeout, TheFactoryHandsItsBridgeTheConfiguredValue) {
  Bench bench;
  ScriptedSidecar peer;
  ExecutionSessionRuntimeHooks hooks{};
  hooks.buffer_manager = bench.manager.get();
  PythonPytorchConfig config;
  config.health_timeout = std::chrono::milliseconds{4'321};
  const auto built = make_python_pytorch_session_with_bridge(hooks, config, peer.launcher());
  EXPECT_EQ(health_timeout_of(*built.bridge), std::chrono::milliseconds{4'321});
  const auto plain = make_python_pytorch_session_with_bridge(hooks, {}, peer.launcher());
  EXPECT_EQ(health_timeout_of(*plain.bridge), kDefaultHealthTimeout);
}

TEST(JobBridgeHealthTimeout, TheRegisteredEntryHandsItsBridgeTheEnvironmentsValue) {
  Bench bench;
  ExecutionSessionRuntimeHooks hooks{};
  hooks.buffer_manager = bench.manager.get();
  BackendRegistry registry;
  ASSERT_TRUE(register_python_pytorch_backend(registry).has_value());
  {
    const ScopedEnvironment named("TP_PYTHON_PYTORCH_HEALTH_TIMEOUT_MS", "4321");
    const auto built = registry.create_session_with_bridge(kBackendName, hooks);
    ASSERT_TRUE(built.has_value()) << built.error().message;
    EXPECT_EQ(health_timeout_of(*built.value().bridge), std::chrono::milliseconds{4'321});
  }
  const ScopedEnvironment unnamed("TP_PYTHON_PYTORCH_HEALTH_TIMEOUT_MS", nullptr);
  const auto built = registry.create_session_with_bridge(kBackendName, hooks);
  ASSERT_TRUE(built.has_value()) << built.error().message;
  EXPECT_EQ(health_timeout_of(*built.value().bridge), kDefaultHealthTimeout);
}

// The answer is held, so a bridge that waits no time has given up when the
// sidecar lets it go. How long it waits is held by the two tests above.
TEST_F(JobBridgeTest, AHealthCallStillWaitsWhenTheSidecarLetsItsHeldAnswerGo) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  std::atomic<bool> returned{false};
  Result<void> answered = unexpected(Error::Code::Internal, "not run");
  std::thread caller([&] {
    answered = h.bridge->health();
    returned = true;
  });
  const bool waiting =
      h.wait([&] { return h.channel->health_waiters() == 1 || returned.load(); }) && !returned;
  peer.release_health();
  caller.join();
  EXPECT_TRUE(waiting);
  EXPECT_TRUE(answered.has_value());
}

TEST_F(JobBridgeTest, AnEventAJobsOrderRefusesBecomesItsFailureAndLateOrForeignOnesAreDropped) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(run_job_1());
  // Foreign: it names the job with another session.
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", "s90"},
                  {"kind", "job_completed"},
                  {"job_id", 1},
                  {"session_key", 2},
                  {"generation", 1},
                  {"result", transcript()}});
  // Refused: an audio chunk is not what a decode returns.
  peer.emit("job_completed", 1, {{"result", audio(1'920)}}, 1'920);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1)->code, Error::Code::InferenceFailed);
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), "result_kind_mismatch");
  // Late: the job has its terminal event.
  peer.emit("job_completed", 1, {{"result", transcript()}});
  peer.emit("job_released", 1);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed, Released}));
  EXPECT_EQ(h.channel->counters(), (JobChannelCounters{2, 0}));
}

// A lifecycle call that runs: the unload waits for the sidecar's answer, which
// waits behind job 1. Nothing between the thread's start and its join ends
// the test.
TEST_F(JobBridgeTest, BridgeCallsAreAnsweredWhileAnUnloadWaitsBehindARunningJob) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(run_job_1());
  Result<void> unloaded = unexpected(Error::Code::Internal, "not run");
  std::thread unload([&] { unloaded = h.session->unload(); });
  const bool arrived = peer.sent("unload", 1).size() == 1;
  const Result<void> health = h.bridge->health();
  const Result<void> submitted = h.bridge->submit(h.stt(2, 1));
  const Result<void> cancelled = h.cancel(1);
  const Result<void> released = h.release(7);
  const bool cancel_arrived = peer.sent("job_cancel", 1).size() == 1;
  const bool acknowledged = h.wait([&] { return h.sink->session_releases() == Sessions{{7, 1}}; });
  peer.complete(1, transcript());
  unload.join();

  ASSERT_TRUE(arrived);
  expect_refused(health, Error::Code::Unavailable, "backend_unavailable");
  expect_refused(submitted, Error::Code::Unavailable, "backend_unavailable");
  EXPECT_TRUE(cancelled.has_value());
  EXPECT_TRUE(released.has_value());
  EXPECT_TRUE(cancel_arrived);
  EXPECT_TRUE(acknowledged);
  EXPECT_TRUE(unloaded.has_value());
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->code, Error::Code::Cancelled);
}

TEST_F(JobBridgeTest, CallsFromSeveralThreadsAtOnceLeaveEveryJobEndedOnceAndReleased) {
  constexpr std::uint64_t kCallers = 4;
  constexpr std::uint64_t kJobsEach = 2;
  ASSERT_NO_FATAL_FAILURE(up());
  ScriptedBackend backend(peer);
  std::atomic<int> refused{0};
  // No caller makes its first call before every caller is running.
  std::latch all_running{kCallers};
  std::vector<std::thread> callers;
  for (std::uint64_t caller = 0; caller < kCallers; ++caller) {
    callers.emplace_back([&, caller] {
      all_running.arrive_and_wait();
      const std::uint64_t session_key = 10 + caller;
      for (std::uint64_t nth = 0; nth < kJobsEach; ++nth) {
        const std::uint64_t job_id = 1 + caller * kJobsEach + nth;
        refused += h.submit(h.stt(job_id, session_key)).has_value() ? 0 : 1;
        refused += h.bridge->health().has_value() ? 0 : 1;
        // Refused as unknown when the job was released first.
        (void)h.cancel(job_id);
      }
      refused += h.release(session_key).has_value() ? 0 : 1;
    });
  }
  for (std::thread& caller : callers) {
    caller.join();
  }
  EXPECT_EQ(refused, 0);
  ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == kCallers; }));
  for (std::uint64_t job_id = 1; job_id <= kCallers * kJobsEach; ++job_id) {
    const Kinds kinds = h.sink->kinds_for(job_id);
    ASSERT_FALSE(kinds.empty()) << job_id;
    EXPECT_EQ(kinds.back(), Released) << job_id;
    EXPECT_EQ(std::count(kinds.begin(), kinds.end(), Completed) +
                  std::count(kinds.begin(), kinds.end(), Failed),
              1)
        << job_id;
  }
  // The last acknowledgement's callback is recorded before it returns.
  EXPECT_TRUE(h.wait([&] { return h.channel->backlog() == JobChannelBacklog{}; }));
}

// -- An unload with jobs open. -----------------------------------------------------

TEST_F(JobBridgeTest, AnUnloadCancelsNoJobTheWaitingOneEndsUnloadedAndTheRunningOneCompletes) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(run_job_1());
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(2, {Accepted}));
  Result<void> unloaded = unexpected(Error::Code::Internal, "not run");
  std::thread unload([&] { unloaded = h.session->unload(); });
  // The waiting job ends when the unload arrives, while job 1 still runs.
  const bool waiting_ended = h.wait_kinds(2, {Accepted, Failed, Released});
  const Kinds running = h.sink->kinds_for(1);
  peer.complete(1, transcript());
  unload.join();

  ASSERT_TRUE(waiting_ended);
  EXPECT_TRUE(unloaded.has_value());
  EXPECT_EQ(running, Kinds{Accepted});
  EXPECT_EQ(failure_of(h, 2)->code, Error::Code::Unavailable);
  EXPECT_EQ(failure_of(h, 2)->context.value_or(""), kSessionUnloaded);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
  EXPECT_TRUE(peer.sent("job_cancel", 0).empty());
}

// -- The connection ended with no job open. ------------------------------------------

class JobBridgeWithoutAConnection : public JobBridgeTest {
 protected:
  // The sidecar closed its end and the channel's reader has left: an exchange
  // fails only then, and no job was open for the end to fail.
  void SetUp() override {
    ASSERT_NO_FATAL_FAILURE(up());
    peer.close();
    ASSERT_FALSE(h.unary("health_check").has_value());
  }

  void expect_no_callback() {
    EXPECT_TRUE(h.sink->events().empty());
    EXPECT_TRUE(h.sink->session_releases().empty());
    EXPECT_EQ(h.channel->backlog(), JobChannelBacklog{});
  }
};

TEST_F(JobBridgeWithoutAConnection, ASubmitIsUnavailable) {
  expect_refused(h.bridge->submit(h.stt(1, 1)), Error::Code::Unavailable, "backend_unavailable");
  expect_no_callback();
}

TEST_F(JobBridgeWithoutAConnection, AReleaseIsUnavailable) {
  expect_refused(h.bridge->release_session(5, 1), Error::Code::Unavailable, "backend_unavailable");
  expect_no_callback();
}

TEST_F(JobBridgeWithoutAConnection, HealthIsUnavailable) {
  expect_refused(h.bridge->health(), Error::Code::Unavailable, "backend_unavailable");
  expect_no_callback();
}

TEST_F(JobBridgeWithoutAConnection, ACancelIsRefusedAsUnknownJob) {
  expect_refused(h.bridge->cancel(1), Error::Code::NotReady, "unknown_job");
  expect_no_callback();
}

// -- A session that is gone. -------------------------------------------------------

TEST_F(JobBridgeTest, WhenItsSessionIsDestroyedTheBridgeEndsItsJobsAndIsUnavailableAfter) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(run_job_1());
  h.session.reset();
  // All of it by the time the destructor has returned.
  ASSERT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->code, Error::Code::Unavailable);
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kSessionUnloaded);
  expect_refused(h.bridge->submit(h.stt(2, 1)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.bridge->release_session(5, 1), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.bridge->health(), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.bridge->cancel(1), Error::Code::NotReady, "unknown_job");
  EXPECT_EQ(h.sink->events().size(), 3U);
  EXPECT_TRUE(h.sink->session_releases().empty());
}

TEST_F(JobBridgeTest, ABridgeWhoseSessionWasDestroyedBeforeItsFirstLoadIsUnavailable) {
  expect_refused(h.bridge->health(), Error::Code::NotReady, "backend_not_ready");
  h.session.reset();
  expect_refused(h.bridge->submit(h.stt(1, 1)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.bridge->release_session(5, 1), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.bridge->health(), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_TRUE(h.sink->events().empty());
  EXPECT_TRUE(h.sink->session_releases().empty());
}

// The session of a bridge built without a buffer manager can never load, which
// only the factory below the registered one allows.
TEST(JobBridgeAfterItsSession, IsUnavailableAlsoWhenItWasBuiltWithoutABufferManager) {
  Bench bench;
  ScriptedSidecar peer;
  auto built = make_python_pytorch_session_with_bridge({}, patient(), peer.launcher());
  ASSERT_TRUE(built.bridge->set_event_sink(bench.sink).has_value());
  expect_refused(built.bridge->health(), Error::Code::NotReady, "backend_not_ready");
  built.session.reset();
  expect_refused(built.bridge->submit(bench.stt(1, 1)), Error::Code::Unavailable,
                 "backend_unavailable");
  expect_refused(built.bridge->release_session(5, 1), Error::Code::Unavailable,
                 "backend_unavailable");
  expect_refused(built.bridge->health(), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_TRUE(bench.sink->events().empty());
}

// -- A backend lost before the prime. ------------------------------------------------

TEST_F(JobBridgeTest, ABackendLostBeforeThePrimeIsNotReadyAsBeforeItWasLost) {
  ASSERT_TRUE(h.session->load(speech_spec()).has_value());
  peer.close();
  // The loss, noticed: the channel's reader has left once an exchange fails.
  // The answers below are the same before that.
  ASSERT_FALSE(h.unary("health_check").has_value());
  expect_refused(h.bridge->submit(h.stt(1, 1)), Error::Code::NotReady, "backend_not_ready");
  expect_refused(h.bridge->health(), Error::Code::NotReady, "backend_not_ready");
  EXPECT_TRUE(h.sink->events().empty());
}

// -- The bridge's destructor. --------------------------------------------------------

// Job 1 is open and the test, as the sidecar's backend, holds its result.
TEST_F(JobBridgeTest, OnceTheBridgeIsDestroyedAHeldResultReachesNoSinkAndItsChunkIsReleased) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(JobBridgeHarness::tts(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  const std::size_t in_use = h.bytes_in_use();

  h.bridge.reset();
  EXPECT_EQ(h.sink.use_count(), 1) << "the bridge kept the sink";
  peer.complete(1, audio(1'920), 1'920);
  // Answered behind the job's last frame: the reader has routed them all.
  ASSERT_TRUE(h.unary("health_check").has_value());

  EXPECT_EQ(h.sink->kinds_for(1), Kinds{Accepted});
  EXPECT_EQ(h.bytes_in_use(), in_use) << "the chunk published for the dropped result";
  // Taken as before: the job left the table, and no frame counted as stray.
  EXPECT_EQ(h.channel->backlog(), JobChannelBacklog{});
  EXPECT_EQ(h.channel->counters(), JobChannelCounters{});
}

// The failure and the release the session's destructor ends job 1 with, on
// the channel's reader and on this thread.
TEST_F(JobBridgeTest, WhatTheSessionEndsOnceItsBridgeIsDestroyedReachesNoSink) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(run_job_1());
  h.bridge.reset();
  h.session.reset();
  EXPECT_EQ(h.sink->kinds_for(1), Kinds{Accepted});
  EXPECT_TRUE(h.sink->session_releases().empty());
  EXPECT_EQ(h.channel->backlog(), JobChannelBacklog{});
}

// The callback of job 1's `accepted` is parked. The destructor is seen
// waiting before the callback is let go, and reads on its return whether the
// callback had returned.
TEST_F(JobBridgeTest, ADestructorCalledWhileACallbackRunsReturnsOnlyOnceItHasReturned) {
  ParkedCallback parked;
  std::atomic<bool> callback_returned{false};
  h.on_event = [&](const JobEvent& event) {
    if (event.kind() == Accepted) {
      parked.park();
      callback_returned = true;
    }
  };
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait([&] { return parked.entered(); }));

  std::atomic<bool> destroyed{false};
  bool returned_after_the_callback = false;
  std::thread destroying([&] {
    h.bridge.reset();
    returned_after_the_callback = callback_returned.load();
    destroyed = true;
  });
  const bool waiting =
      h.wait([&] { return h.channel->detach_waiters() == 1 || destroyed.load(); }) && !destroyed;
  parked.let_go();
  destroying.join();

  EXPECT_TRUE(waiting);
  EXPECT_TRUE(returned_after_the_callback);
  EXPECT_EQ(h.sink.use_count(), 1) << "the bridge kept the sink";
}

// Everything the statement needs is built inside it: the process that runs it
// starts with one thread.
void destroy_the_bridge_inside_a_callback() {
  ScriptedSidecar peer;
  JobBridgeHarness h(patient(), peer.launcher());
  std::atomic<bool> returned{false};
  h.on_event = [&](const JobEvent&) {
    h.bridge.reset();
    returned = true;
  };
  const bool up = h.session->load(speech_spec()).has_value() && h.session->prime().has_value() &&
                  h.submit(h.stt(1, 1)).has_value();
  (void)h.wait([&] { return !up || returned.load(); });
  // The destructor returned, or never will: the harness cannot be taken down
  // around a reader that waits for itself.
  std::_Exit(EXIT_SUCCESS);
}

TEST(JobBridgeDeathTest, ABridgeDestroyedInsideASinkCallbackEndsTheProcessWithAMessage) {
  GTEST_FLAG_SET(death_test_style, "threadsafe");
  EXPECT_DEATH(destroy_the_bridge_inside_a_callback(),
               "a job bridge was destroyed inside one of its sink callbacks");
}

// -- Lifecycle calls from a sink callback. -------------------------------------------

TEST_F(JobBridgeTest, AnUnloadFromASinkCallbackIsRefusedAndTheSessionsSidecarAndJobsLiveOn) {
  std::atomic<bool> called{false};
  Result<void> unloaded = unexpected(Error::Code::Internal, "not run");
  h.on_event = [&](const JobEvent& event) {
    if (event.kind() == Accepted && event.identity().job_id == 1) {
      unloaded = h.session->unload();
      called = true;
    }
  };
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait([&] { return called.load(); }));
  expect_refused(unloaded, Error::Code::Internal, kLifecycleCallInSinkCallback);
  // The lifecycle's rule for every failed unload: the session is not ready.
  // Nothing else changed: the backend is healthy, admission is open and both
  // jobs run to their end.
  EXPECT_FALSE(h.session->is_ready());
  EXPECT_TRUE(h.bridge->health().has_value());
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  peer.complete(1, transcript());
  ASSERT_EQ(peer.take_job(), 2U);
  peer.complete(2, transcript());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(h.wait_kinds(2, {Accepted, Completed, Released}));
  EXPECT_TRUE(peer.sent("unload", 0).empty());
  // From a thread that may make it, the unload goes through.
  EXPECT_TRUE(h.session->unload().has_value());
  EXPECT_EQ(peer.sent("unload", 1).size(), 1U);
}

// Loaded and not primed, the channel is started: a session release is
// acknowledged on its reader, and a prime made there would wait for an
// answer only that thread can route.
TEST_F(JobBridgeTest, APrimeFromASinkCallbackIsRefusedAndNothingIsSent) {
  std::atomic<bool> called{false};
  Result<void> primed = unexpected(Error::Code::Internal, "not run");
  h.on_session_release = [&](std::uint64_t, std::uint64_t) {
    primed = h.session->prime();
    called = true;
  };
  ASSERT_TRUE(h.session->load(speech_spec()).has_value());
  ASSERT_TRUE(h.release(7).has_value());
  ASSERT_TRUE(h.wait([&] { return called.load(); }));
  expect_refused(primed, Error::Code::Internal, kLifecycleCallInSinkCallback);
  // The connection is up and its exchange slot free.
  EXPECT_TRUE(h.unary("health_check").has_value());
  EXPECT_TRUE(peer.sent("prime", 0).empty());
  EXPECT_TRUE(h.session->unload().has_value());
}

// -- Requests this backend cannot carry. ---------------------------------------------

TEST_F(JobBridgeTest, ARequestTheWireOrTheBufferManagerCannotCarryIsRefusedWithTheAdaptersReason) {
  ASSERT_NO_FATAL_FAILURE(up());
  expect_refused(h.bridge->submit(h.stt(kMaxWireInteger + 1, 1)), Error::Code::ConfigInvalid,
                 "wire_integer_out_of_range");
  expect_refused(h.bridge->submit(h.stt(1, kMaxWireInteger + 1)), Error::Code::ConfigInvalid,
                 "wire_integer_out_of_range");
  const JobRequest gone = h.stt(1, 1);
  ASSERT_TRUE(h.manager->release(std::get<AudioFrames>(gone.payload()).pcm.buffer).has_value());
  expect_refused(h.bridge->submit(gone), Error::Code::ConfigInvalid, "pcm_buffer_invalid");
  expect_refused(h.bridge->release_session(kMaxWireInteger + 1, 1), Error::Code::ConfigInvalid,
                 "wire_integer_out_of_range");
  EXPECT_TRUE(h.sink->events().empty());
  // None of them took its job id or its session key.
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  EXPECT_TRUE(h.wait_kinds(1, {Accepted}));
}

// -- A release the backend could not take. -------------------------------------------

TEST(JobBridgeRelease, ASessionReleasedWhileTheBackendWasUnavailableTakesNoJobOnALaterLoad) {
  ScriptedSidecar first;
  ScriptedSidecar second;
  int launches = 0;
  JobBridgeHarness h(patient(), [&](const SidecarLaunchRequest& request) {
    return (launches++ == 0 ? first : second).launcher()(request);
  });
  // Before the first load there is nothing to record: key 6 stays usable.
  expect_refused(h.release(6), Error::Code::Unavailable, "backend_unavailable");
  ASSERT_TRUE(h.session->load(speech_spec()).has_value());
  ASSERT_TRUE(h.session->prime().has_value());
  ASSERT_TRUE(h.session->unload().has_value());
  expect_refused(h.release(5), Error::Code::Unavailable, "backend_unavailable");

  ASSERT_TRUE(h.session->load(speech_spec()).has_value());
  ASSERT_TRUE(h.session->prime().has_value());
  expect_refused(h.bridge->submit(JobBridgeHarness::tts(1, 5)), Error::Code::NotReady,
                 "session_releasing");
  ASSERT_TRUE(h.submit(JobBridgeHarness::tts(2, 6)).has_value());
  EXPECT_TRUE(h.wait_kinds(2, {Accepted}));
  EXPECT_TRUE(h.sink->session_releases().empty());
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
