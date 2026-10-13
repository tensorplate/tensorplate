// SPDX-License-Identifier: Apache-2.0
//
// The job bridge harness itself (test/mocks/job_bridge_harness.hpp): what its
// settle waits take for settled and when its scripted backend lets a held job
// go, each in a state the test holds still. And what the scripted sidecar
// declares at a load.

#include <gtest/gtest.h>

#if !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobBridgeHarness, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <mutex>
#include <nlohmann/json.hpp>
#include <thread>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

#include "adapters/python_pytorch/job_channel.hpp"
#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "job_bridge_harness.hpp"
#include "job_channel_harness.hpp"
#include "scripted_sidecar.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using testing::failure_of;
using testing::JobBridgeHarness;
using testing::JobChannelHarness;
using testing::ScriptedBackend;
using testing::ScriptedSidecar;
using testing::settled_over;
using Json = nlohmann::json;
using Kinds = JobBridgeHarness::Kinds;
using enum JobEventKind;

constexpr std::chrono::milliseconds kPatience{JobBridgeHarness::kPatience};
// A deadline that has passed when the wait begins.
constexpr std::chrono::milliseconds kNoPatience{0};

PythonPytorchConfig patient() {
  PythonPytorchConfig config;
  config.startup_timeout = JobBridgeHarness::kPatience;
  config.health_timeout = JobBridgeHarness::kPatience;
  return config;
}

Json transcript() {
  return Json{{"type", "transcript"}, {"text", "hello"}, {"tokens", {1}}, {"words", Json::array()}};
}

/// A sink callback that waits for the test.
class ParkedCallback {
 public:
  /// Parks the calling callback until let_go(); it gives up after kPatience.
  void park() {
    std::unique_lock<std::mutex> lock(mutex_);
    entered_ = true;
    (void)changed_.wait_for(lock, kPatience, [this] { return let_go_; });
  }

  [[nodiscard]] bool entered() {
    const std::lock_guard<std::mutex> lock(mutex_);
    return entered_;
  }

  void let_go() {
    {
      const std::lock_guard<std::mutex> lock(mutex_);
      let_go_ = true;
    }
    changed_.notify_all();
  }

 private:
  std::mutex mutex_;
  std::condition_variable changed_;
  bool entered_ = false;
  bool let_go_ = false;
};

class JobBridgeHarnessTest : public ::testing::Test {
 protected:
  // The harness stops the channel when it goes: no callback may still wait.
  void TearDown() override { parked.let_go(); }

  // Loaded and primed: the bridge takes jobs.
  void up() {
    const auto loaded = h.session->load(
        ModelSpec::create("speech", ModelClass::Speech, "/dev/null", "python_pytorch").value());
    ASSERT_TRUE(loaded.has_value()) << loaded.error().message;
    const auto primed = h.session->prime();
    ASSERT_TRUE(primed.has_value()) << primed.error().message;
  }

  // Jobs 1 and 2 of one session with the backend holding: 1 runs, 2 waits.
  void hold_job_1_ahead_of_job_2() {
    backend.hold();
    ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
    ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
    ASSERT_TRUE(settled(kPatience));
    ASSERT_EQ(h.sink->kinds_for(1), Kinds{Accepted});
    ASSERT_EQ(h.sink->kinds_for(2), Kinds{Accepted});
  }

  [[nodiscard]] bool settled(std::chrono::milliseconds patience) {
    return settled_over(
        *h.channel, peer, [this] { return backend.idle(); }, patience);
  }

  ScriptedSidecar peer;
  // Before the harness: its callbacks use them until it has gone.
  ParkedCallback parked;
  std::atomic<bool> in_callback{false};
  std::atomic<bool> queued_behind{false};
  JobBridgeHarness h{patient(), peer.launcher()};
  ScriptedBackend backend{peer};
};

// -- settled_over(). ---------------------------------------------------------------

TEST_F(JobBridgeHarnessTest, NoRoundTripBeginsUntilTheBackendIsIdle) {
  ASSERT_NO_FATAL_FAILURE(up());
  const std::size_t quiet = peer.written_count();
  std::vector<std::size_t> written_when_asked;
  const auto idle_from_the_third_ask = [&] {
    written_when_asked.push_back(peer.written_count());
    return written_when_asked.size() >= 3;
  };
  ASSERT_TRUE(settled_over(*h.channel, peer, idle_from_the_third_ask, kPatience));
  // Asked ahead of each round trip; the sidecar wrote nothing while it was busy.
  EXPECT_EQ(written_when_asked, (std::vector<std::size_t>{quiet, quiet, quiet, quiet + 1}));
}

TEST_F(JobBridgeHarnessTest, AQuietConnectionIsSettledAfterExactlyTwoRoundTrips) {
  ASSERT_NO_FATAL_FAILURE(up());
  const std::size_t quiet = peer.written_count();
  ASSERT_TRUE(settled(kPatience));
  const std::vector<Json> written = peer.written();
  ASSERT_EQ(written.size(), quiet + 2);
  EXPECT_EQ(written[quiet].at("kind"), "health_check_response");
  EXPECT_EQ(written[quiet + 1].at("kind"), "health_check_response");
}

// What the second round trip is for. Job 1's `accepted` callback outlasts the
// answer to the first, stops the sidecar reading and submits job 2: that
// submit is behind the first request, and unanswered when its answer is read.
TEST_F(JobBridgeHarnessTest, AFrameACallbackQueuedBehindARoundTripIsAnsweredBeforeItIsSettled) {
  h.on_event = [this](const JobEvent& event) {
    if (event.identity().job_id != 1 || event.kind() != Accepted) {
      return;
    }
    const std::size_t written = peer.written_count();
    in_callback = true;
    const bool answered = testing::wait_until(testing::SettleClock::now() + kPatience,
                                              [&] { return peer.written_count() > written; });
    peer.stop_reading();
    queued_behind = answered && h.submit(h.stt(2, 1)).has_value();
  };
  ASSERT_NO_FATAL_FAILURE(up());
  backend.hold();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait([&] { return in_callback.load(); }));
  const std::size_t before = peer.written_count();

  // The sidecar reads again once the first round trip is over.
  const auto idle = [&] {
    if (queued_behind) {
      peer.resume_reading();
    }
    return backend.idle();
  };
  ASSERT_TRUE(settled_over(*h.channel, peer, idle, kPatience));

  EXPECT_TRUE(queued_behind);
  EXPECT_EQ(h.sink->kinds_for(2), Kinds{Accepted});
  // The first answer, job 2's `accepted`, the answer behind it, and two quiet ones.
  EXPECT_EQ(peer.written_count(), before + 5);
}

// -- A round trip that failed. ---------------------------------------------------------

TEST_F(JobBridgeHarnessTest, AfterItsConnectionEndedItIsNotSettledUntilTheFailuresHaveReturned) {
  h.on_event = [this](const JobEvent& event) {
    if (event.kind() == Failed && !parked.entered()) {
      parked.park();
    }
  };
  ASSERT_NO_FATAL_FAILURE(up());
  backend.hold();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(settled(kPatience));
  peer.close();
  ASSERT_TRUE(h.wait([&] { return parked.entered(); }));

  EXPECT_FALSE(settled(kNoPatience));
  parked.let_go();
  EXPECT_TRUE(settled(kPatience));
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed}));
}

// A lifecycle call of the session has the slot. Nothing between the thread's
// start and its join ends the test.
TEST_F(JobBridgeHarnessTest, ItIsNotSettledWhileAnotherExchangeHasTheChannelsSlot) {
  ASSERT_NO_FATAL_FAILURE(up());
  // The prime's.
  ASSERT_EQ(peer.sent("health_check", 1).size(), 1U);
  peer.hold_health();
  Result<Json> exchanged = unexpected(Error::Code::Internal, "not run");
  std::thread lifecycle([&] { exchanged = h.unary("health_check"); });
  const bool waiting = peer.sent("health_check", 2).size() == 2;
  const bool settled_meanwhile = settled(kPatience);
  peer.release_health();
  lifecycle.join();

  ASSERT_TRUE(waiting);
  EXPECT_FALSE(settled_meanwhile);
  EXPECT_TRUE(exchanged.has_value());
  EXPECT_TRUE(settled(kPatience));
}

TEST_F(JobBridgeHarnessTest, ARoundTripTheSidecarDoesNotAnswerIsNotSettled) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  EXPECT_FALSE(settled(kNoPatience));
}

// -- The scripted backend. -------------------------------------------------------------

TEST_F(JobBridgeHarnessTest, AHeldJobEndsOnceAnUnloadWaitsBehindItAndTheJobBehindItFailsUnloaded) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_job_1_ahead_of_job_2());

  const auto unloaded = h.session->unload();

  ASSERT_TRUE(unloaded.has_value()) << unloaded.error().message;
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
  ASSERT_EQ(h.sink->kinds_for(2), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 2)->context.value_or(""), kSessionUnloaded);
}

TEST_F(JobBridgeHarnessTest, ABackendIsIdleWithTheJobItHoldsAndNotWhileOneItHasNotTakenIsDue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_job_1_ahead_of_job_2());
  EXPECT_TRUE(backend.idle());
  // Ended from here, not by the backend: it still holds job 1, and job 2 is due.
  peer.complete(1, transcript());
  EXPECT_FALSE(backend.idle());
  backend.release();
  EXPECT_TRUE(h.wait_kinds(2, {Accepted, Completed, Released}));
}

// -- The scripted sidecar. -------------------------------------------------------------

TEST(JobBridgeScriptedSidecar, DeclaresTheClassesALaneRunsInLaneOrderAndOnceEach) {
  ScriptedSidecar peer(ScriptedSidecar::Options{
      .job_classes = {"tts_synthesis", "vad_frames", "stt_decode", "tts_synthesis"}});
  JobChannelHarness h;
  const auto loaded = h.connect(peer.launcher());
  ASSERT_TRUE(loaded.has_value()) << loaded.error().message;
  EXPECT_EQ(loaded.value().at("job_classes"), Json::array({"stt_decode", "tts_synthesis"}));
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
