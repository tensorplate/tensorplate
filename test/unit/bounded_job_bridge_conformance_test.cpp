// SPDX-License-Identifier: Apache-2.0
//
// The bridge conformance cases against the in-process fixture bridge, and the
// fixture's own hold on its backend, which those cases do not reach.

#include "bounded_job_bridge_conformance.hpp"

#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <future>
#include <memory>
#include <string>
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

#include "fixture_job_bridge.hpp"
#include "recording_job_event_sink.hpp"

namespace tensorplate::testing {
namespace {

std::unique_ptr<BufferManager> make_buffers(std::size_t capacity_bytes = 16U << 20U) {
  BufferManagerConfig config;
  config.pool_name = "job_bridge_test";
  config.capacity_bytes = capacity_bytes;
  config.max_buffer_bytes = std::min<std::size_t>(2U << 20U, capacity_bytes);
  return BufferManager::create(config).value();
}

class FixtureBridgeUnderTest final : public BridgeUnderTest {
 public:
  BoundedJobBridge& bridge() override { return bridge_; }
  BufferManager& buffers() override { return *buffers_; }
  void prime() override { bridge_.prime(); }
  void unload() override { bridge_.unload(); }
  void lose_backend() override { bridge_.lose_backend(); }
  void hold_jobs() override { bridge_.hold_jobs(); }
  void release_jobs() override { bridge_.release_jobs(); }
  bool settled(std::chrono::milliseconds patience) override { return bridge_.settled(patience); }
  std::size_t waiting_jobs() const override { return FixtureJobBridge::kWaitingJobs; }
  TranscriptResult transcript(const JobRequest& request) const override {
    return fixture_transcript(std::get<AudioFrames>(request.payload()));
  }
  std::vector<std::byte> speech(const JobRequest& request) const override {
    const std::size_t text_bytes = std::get<TextSegment>(request.payload()).text.size();
    return std::vector<std::byte>(kFixtureSpeechBytesPerTextByte * text_bytes, std::byte{0});
  }

 private:
  std::unique_ptr<BufferManager> buffers_ = make_buffers();
  FixtureJobBridge bridge_{*buffers_};
};

// -- The fixture's own hold on its backend. ---------------------------------------

constexpr std::chrono::seconds kPatience{30};

JobRequest synthesis(std::uint64_t job_id) {
  return JobRequest::create(
             JobIdentity{job_id, 5, 3}, JobClass::TtsSynthesis, TextSegment{"hello there."},
             JobOptions{.language = "en-US", .voice = "af_heart", .speed_milli = 1000})
      .value();
}

JobRequest decode(std::uint64_t job_id, BufferManager& buffers) {
  const BufferRef buffer = buffers.allocate(3'200).value();
  return JobRequest::create(JobIdentity{job_id, 5, 3}, JobClass::SttDecode,
                            AudioFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, 3'200}, 0},
                            JobOptions{.language = "en"})
      .value();
}

struct Bench {
  explicit Bench(std::unique_ptr<BufferManager> pool = make_buffers(),
                 FixtureJobBridge::ResultFunction result = fixture_job_result)
      : buffers(std::move(pool)), bridge(*buffers, std::move(result)) {
    EXPECT_TRUE(bridge.set_event_sink(sink).has_value());
    bridge.prime();
  }

  [[nodiscard]] Result<void> submit(const JobRequest& request) {
    sink->expect_job(request);
    return bridge.submit(request);
  }

  /// The job's event of `kind`, once the bridge has settled.
  [[nodiscard]] JobEvent event_of(std::uint64_t job_id, JobEventKind kind) {
    EXPECT_TRUE(bridge.settled(kPatience));
    for (const JobEvent& event : sink->events()) {
      if (event.identity().job_id == job_id && event.kind() == kind) {
        return event;
      }
    }
    ADD_FAILURE() << "job " << job_id << " has no " << to_string(kind);
    return JobEvent::released(JobIdentity{job_id, 1, 1}).value();
  }

  std::unique_ptr<BufferManager> buffers;
  std::shared_ptr<RecordingJobEventSink> sink = std::make_shared<RecordingJobEventSink>();
  FixtureJobBridge bridge;
};

using Kinds = std::vector<JobEventKind>;
using enum JobEventKind;

// backends/fixture.py: "hello" with one word unit that ends a quarter of the
// payload's bytes after its start; 160 bytes of silence for each byte of text.
TEST(FixtureJobBridge, ItsResultsAreTheRunnersFixtureResults) {
  Bench bench;
  const JobRequest audio = decode(1, *bench.buffers);
  AudioFrames shifted = std::get<AudioFrames>(audio.payload());
  shifted.start_sample = 480;
  EXPECT_EQ(fixture_transcript(shifted),
            (TranscriptResult{"hello", {1}, {WordUnit{"hello", 0, 1, 480, 1'280}}}));
  ASSERT_TRUE(bench.submit(synthesis(2)).has_value());
  const JobResult speech = bench.event_of(2, Completed).result().value();
  const PcmWindow pcm = std::get<AudioChunkResult>(speech.payload()).pcm;
  EXPECT_EQ(pcm.byte_size, 1'920U);
  const auto bytes = bench.buffers->data(pcm.buffer).value();
  EXPECT_EQ(std::count(bytes.begin(), bytes.end(), std::byte{0}), 1'920);
}

// jobs.py: a cancellation is acknowledged when it arrives; a waiting job ends
// then, the running one when its work returns, both cancelled.
TEST(FixtureJobBridge, ACancelIsAcknowledgedAtOnceAndEndsTheJobCancelled) {
  Bench bench;
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(2, *bench.buffers)).has_value());
  for (const std::uint64_t job_id : {1U, 2U}) {
    bench.sink->expect_cancel(job_id);
    ASSERT_TRUE(bench.bridge.cancel(job_id).has_value());
  }
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged}));
  EXPECT_EQ(bench.sink->kinds_for(2), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(bench.event_of(2, Failed).error().value().code, Error::Code::Cancelled);

  bench.bridge.release_jobs();
  EXPECT_EQ(bench.event_of(1, Failed).error().value().code, Error::Code::Cancelled);
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_TRUE(bench.sink->violations().empty());
}

TEST(FixtureJobBridge, TheNextJobFailsWithTheErrorItWasGivenAndTheOneAfterCompletes) {
  Bench bench;
  bench.bridge.fail_next_job(Error::make(Error::Code::OOMError, "no room", "oom_error"));
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(2, *bench.buffers)).has_value());
  const Error failed = bench.event_of(1, Failed).error().value();
  EXPECT_EQ(failed.code, Error::Code::OOMError);
  EXPECT_EQ(failed.context.value_or(""), "oom_error");
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(bench.sink->kinds_for(2), (Kinds{Accepted, Completed, Released}));
  EXPECT_TRUE(bench.sink->violations().empty());
}

TEST(FixtureJobBridge, AJobCompletesWithWhatItsResultFunctionReturns) {
  // A transcript that names its request, so a result cannot come from
  // another job's input.
  Bench bench(make_buffers(), [](const JobRequest& request, BufferManager&) {
    const std::string heard = std::to_string(request.identity().job_id);
    return JobResult::create(TranscriptResult{heard, {}, {}});
  });
  ASSERT_TRUE(bench.submit(decode(7, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(8, *bench.buffers)).has_value());
  EXPECT_EQ(bench.event_of(7, Completed).result().value(),
            JobResult::create(TranscriptResult{"7", {}, {}}).value());
  EXPECT_EQ(bench.event_of(8, Completed).result().value(),
            JobResult::create(TranscriptResult{"8", {}, {}}).value());
}

TEST(FixtureJobBridge, ASynthesisItsBuffersCannotHoldFailsWithTheirError) {
  // Twelve bytes of text need 1,920 bytes of PCM.
  Bench bench(make_buffers(/*capacity_bytes=*/1'024));
  ASSERT_TRUE(bench.submit(synthesis(1)).has_value());
  const Error failed = bench.event_of(1, Failed).error().value();
  EXPECT_EQ(failed.code, bench.buffers->allocate(1'920).error().code);
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
}

TEST(FixtureJobBridge, HealthAnswersWhatItWasSetToWhileTheBackendRunsJobs) {
  Bench bench;
  bench.bridge.set_health(unexpected(Error::make(Error::Code::Timeout, "slow", "health_timeout")));
  const Result<void> slow = bench.bridge.health();
  ASSERT_FALSE(slow.has_value());
  EXPECT_EQ(slow.error().context.value_or(""), "health_timeout");
  bench.bridge.unload();
  EXPECT_EQ(bench.bridge.health().error().context.value_or(""), "backend_unavailable");
}

TEST(FixtureJobBridge, APrimeDoesNotReopenALostOrAnUnloadedBackend) {
  Bench lost;
  lost.bridge.lose_backend();
  lost.bridge.prime();
  EXPECT_EQ(lost.bridge.health().error().context.value_or(""), "backend_unavailable");
  Bench unloaded;
  unloaded.bridge.unload();
  unloaded.bridge.prime();
  EXPECT_EQ(unloaded.bridge.health().error().context.value_or(""), "backend_unavailable");
  EXPECT_FALSE(unloaded.submit(synthesis(1)).has_value());
}

TEST(FixtureJobBridge, ItsDestructionUnloads) {
  std::shared_ptr<RecordingJobEventSink> sink;
  {
    Bench bench;
    sink = bench.sink;
    bench.bridge.hold_jobs();
    ASSERT_TRUE(bench.submit(synthesis(1)).has_value());
    ASSERT_TRUE(bench.bridge.settled(kPatience));
  }
  ASSERT_EQ(sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(sink->events()[1].error().value().context.value_or(""), "session_unloaded");
}

TEST(FixtureJobBridge, IsNotSettledWhileACallbackRuns) {
  std::promise<void> inside;
  std::promise<void> leave;
  Bench bench;
  bench.sink->set_callback_hook([&, waiting = leave.get_future().share()](const JobEvent& event) {
    if (event.kind() == Completed) {
      inside.set_value();
      waiting.wait();
    }
  });
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), Kinds{Accepted});

  bench.bridge.release_jobs();
  inside.get_future().wait();
  EXPECT_FALSE(bench.bridge.settled(std::chrono::milliseconds{0}));
  leave.set_value();
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
}

}  // namespace

INSTANTIATE_TEST_SUITE_P(Bridges, BoundedJobBridgeConformance,
                         ::testing::Values(BridgeCase{
                             "fixture", [] { return std::make_unique<FixtureBridgeUnderTest>(); }}),
                         [](const auto& bridge_case) { return bridge_case.param.name; });

}  // namespace tensorplate::testing
