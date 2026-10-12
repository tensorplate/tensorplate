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
#include <latch>
#include <memory>
#include <nlohmann/json.hpp>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
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

// The answer is held, so only a bridge that has its configured timeout still
// waits when the sidecar lets it go.
TEST_F(JobBridgeTest, TheFactorysBridgeWaitsForAHeldHealthAnswerWithItsConfiguredTimeout) {
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
  // The channel's reader has left once an exchange fails.
  ASSERT_FALSE(h.unary("health_check").has_value());
  expect_refused(h.bridge->submit(h.stt(1, 1)), Error::Code::NotReady, "backend_not_ready");
  expect_refused(h.bridge->health(), Error::Code::NotReady, "backend_not_ready");
  EXPECT_TRUE(h.sink->events().empty());
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
