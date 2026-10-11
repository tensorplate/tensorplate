// SPDX-License-Identifier: Apache-2.0
//
// The adapter's job channel against the in-tree sidecar runner and its
// fixture backend, over a real process and socket. The test makes the load
// handshake a session makes, then drives the channel.
//
// Skipped when the Python package is not importable; the interpreter comes
// from TP_TEST_PYTHON, else `python3`.

#include <gtest/gtest.h>

#if !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobChannelRunner, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <unistd.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <nlohmann/json.hpp>
#include <string>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

#include "adapters/python_pytorch/job_channel.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"
#include "job_channel_harness.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using testing::JobChannelHarness;
using Json = nlohmann::json;
using Kinds = JobChannelHarness::Kinds;
using enum JobEventKind;

std::string python() {
  const char* named = std::getenv("TP_TEST_PYTHON");
  return named != nullptr && *named != '\0' ? named : "python3";
}

class JobChannelRunner : public ::testing::Test {
 protected:
  void SetUp() override {
    const std::string probe = python() + " -c 'import tensorplate_pytorch_backend' >/dev/null 2>&1";
    if (std::system(probe.c_str()) != 0) {  // NOLINT(concurrency-mt-unsafe)
      GTEST_SKIP() << "tensorplate_pytorch_backend is not importable by " << python();
    }
  }

  void TearDown() override {
    if (!entry_.empty()) {
      std::filesystem::remove_all(entry_.parent_path());
    }
  }

  // The runner is loaded with jobs enabled and primed, and the channel admits.
  // `job_delay_ms` is how long the fixture holds every job open.
  void up(int job_delay_ms = 0) {
    std::string artifact = "/dev/null";
    if (job_delay_ms != 0) {
      const auto dir =
          std::filesystem::temp_directory_path() / ("tp_job_channel_" + std::to_string(::getpid()));
      std::filesystem::create_directories(dir);
      entry_ = dir / "entry.json";
      std::ofstream(entry_) << Json{{"job_delay_ms", job_delay_ms}}.dump();
      artifact = entry_.string();
    }
    SidecarLaunchRequest request;
    request.python_exe = python();
    const auto loaded = h.connect(default_fork_exec_launcher(), request, artifact);
    ASSERT_TRUE(loaded.has_value()) << loaded.error().message;
    EXPECT_EQ(loaded.value().at("job_classes"), Json::array({"stt_decode", "tts_synthesis"}));
    ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
    const auto primed = h.unary("prime");
    ASSERT_TRUE(primed.has_value()) << primed.error().message;
    ASSERT_EQ(primed.value().value("status", ""), "ok");
    h.channel.admit_jobs();
  }

  std::filesystem::path entry_;
  // Written by a sink callback, which can still run in the harness's stop
  // after an assertion has ended the test's body.
  std::atomic<bool> released_before_the_reap{false};
  JobChannelHarness h;
};

// The fixture's hold on a job. No test below waits for it to pass: each ends
// by stopping the channel, which kills the runner. The first job submitted
// has no terminal event until then, whether the runner has started it or
// not, and every later job waits behind it.
constexpr int kHeld = 60'000;

TEST_F(JobChannelRunner, BothSpeechClassesRunToTheirResultAtTheLargestSizes) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Completed, Released}));
  const std::vector<JobEvent> first = h.sink->events();
  const auto& heard = std::get<TranscriptResult>(first.at(1).result()->payload());
  EXPECT_EQ(heard.text, "hello");
  EXPECT_EQ(heard.words.size(), 1U);

  ASSERT_TRUE(h.submit(JobChannelHarness::tts(2, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(2, {Accepted, Completed, Released}));

  // A window of the largest size out, and the fixture's largest chunk back.
  ASSERT_TRUE(h.submit(h.stt(3, 2, kMaxAudioFramesBytes)).has_value());
  ASSERT_TRUE(
      h.submit(JobChannelHarness::tts(4, 2, std::string(kMaxTextSegmentBytes, 'a'))).has_value());
  ASSERT_TRUE(h.wait_kinds(3, {Accepted, Completed, Released}));
  ASSERT_TRUE(h.wait_kinds(4, {Accepted, Completed, Released}));
  std::vector<std::size_t> chunks;
  for (const JobEvent& event : h.sink->events()) {
    if (event.kind() == Completed && event.result()->kind() == JobResultKind::AudioChunk) {
      const auto& chunk = std::get<AudioChunkResult>(event.result()->payload());
      chunks.push_back(chunk.pcm.byte_size);
      EXPECT_TRUE(h.manager->release(chunk.pcm.buffer).has_value());
    }
  }
  // The fixture returns 80 samples of silence for each byte of text.
  EXPECT_EQ(chunks, (std::vector<std::size_t>{160 * 12, 160 * kMaxTextSegmentBytes}));
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
}

TEST_F(JobChannelRunner, BehindAHeldJobHealthIsAnsweredAndWaitingJobsEndWhenCancelledOrReleased) {
  up(kHeld);
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  ASSERT_TRUE(h.submit(h.stt(3, 9)).has_value());
  ASSERT_TRUE(h.wait_kinds(3, {Accepted}));

  const auto health = h.unary("health_check");
  ASSERT_TRUE(health.has_value()) << health.error().message;
  EXPECT_EQ(health.value().at("health").at("ready"), true);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted})) << "the first job is still held";

  ASSERT_TRUE(h.cancel(2).has_value());
  ASSERT_TRUE(h.wait_kinds(2, {Accepted, CancelAcknowledged, Failed, Released}));
  ASSERT_TRUE(h.release(9).has_value());
  ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == 1; }));
  EXPECT_EQ(h.sink->kinds_for(3), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  for (const JobEvent& event : h.sink->events()) {
    if (event.kind() == Failed) {
      EXPECT_EQ(event.error()->code, Error::Code::Cancelled);
    }
  }

  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted}));
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});

  h.stop(kSessionUnloaded);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(h.sink->events().at(h.sink->events().size() - 2).error()->context.value_or(""),
            kSessionUnloaded);
}

TEST_F(JobChannelRunner, StoppingWithAJobOpenFailsItAndReleasesItOnceTheRunnerIsGone) {
  up(kHeld);
  h.on_event = [this](const JobEvent& event) {
    if (event.kind() == Released && !h.reaped) {
      released_before_the_reap = true;
    }
  };
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));

  h.stop(kBackendReset);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  const Error failure = *h.sink->events().at(1).error();
  EXPECT_EQ(failure.code, Error::Code::Unavailable);
  EXPECT_EQ(failure.context.value_or(""), kBackendReset);
  EXPECT_FALSE(released_before_the_reap);
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
