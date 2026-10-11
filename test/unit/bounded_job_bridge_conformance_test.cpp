// SPDX-License-Identifier: Apache-2.0
//
// The bridge conformance cases against the in-process fixture bridge, and the
// fixture's own hold on its backend, which those cases do not reach.

#include "bounded_job_bridge_conformance.hpp"

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <future>
#include <memory>
#include <string>
#include <thread>
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
#include "speech_job_golden.hpp"

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
  ~FixtureBridgeUnderTest() override {
    bridge_.unload();
    EXPECT_TRUE(bridge_.refusals().empty());
  }

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

// The lane depth of the runner's job table (GPU_LANE_QUEUE_DEPTH in jobs.py).
static_assert(FixtureJobBridge::kWaitingJobs == 8);

// -- The fixture's own rules, which the cases leave open. -------------------------

constexpr std::chrono::seconds kPatience{30};

JobRequest synthesis(std::uint64_t job_id, std::string text = "hello there.",
                     std::uint32_t progress_limit = 0) {
  return JobRequest::create(
             JobIdentity{job_id, 5, 3}, JobClass::TtsSynthesis, TextSegment{std::move(text)},
             JobOptions{.language = "en-US", .voice = "af_heart", .speed_milli = 1000},
             progress_limit)
      .value();
}

JobRequest decode(std::uint64_t job_id, BufferManager& buffers, std::uint32_t progress_limit = 0) {
  const BufferRef buffer = buffers.allocate(3'200).value();
  return JobRequest::create(JobIdentity{job_id, 5, 3}, JobClass::SttDecode,
                            AudioFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, 3'200}, 0},
                            JobOptions{.language = "en"}, progress_limit)
      .value();
}

struct Bench {
  explicit Bench(std::unique_ptr<BufferManager> pool = make_buffers(),
                 FixtureJobBridge::ResultFunction result = fixture_job_result)
      : buffers(std::move(pool)), bridge(*buffers, std::move(result)) {
    EXPECT_TRUE(bridge.set_event_sink(sink).has_value());
    bridge.prime();
  }
  Bench(const Bench&) = delete;
  Bench& operator=(const Bench&) = delete;
  ~Bench() { EXPECT_EQ(bridge.refusals().size(), refusals_expected); }

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
  /// Events of the fixture's backend that a job's event order refused.
  std::size_t refusals_expected = 0;
};

using Kinds = std::vector<JobEventKind>;
using enum JobEventKind;

// Against the golden frames of the sidecar's job messages
// (protocol/rust/tests/fixtures/python_pytorch_ipc_speech_jobs_*.jsonl): the
// result each records for its submit is the one the fixture gives.
TEST(FixtureJobBridge, ItsResultsAreThoseOfTheGoldenFrames) {
  Bench bench;
  const auto decoded = speech_golden::golden("stt_decode");
  ASSERT_GE(decoded.size(), 3U);
  const auto& window = decoded[0].at("input");
  const auto bytes = window.at("payload_length").get<std::size_t>();
  const AudioFrames audio{kJobInputAudioFormat,
                          PcmWindow{bench.buffers->allocate(bytes).value(), 0, bytes},
                          window.at("start_sample").get<std::uint64_t>()};
  const auto& transcript = decoded[2].at("result");
  const auto& word = transcript.at("words").at(0);
  EXPECT_EQ(fixture_transcript(audio),
            (TranscriptResult{transcript.at("text").get<std::string>(),
                              transcript.at("tokens").get<std::vector<std::uint32_t>>(),
                              {WordUnit{word.at("text").get<std::string>(),
                                        word.at("token_begin").get<std::uint32_t>(),
                                        word.at("token_end").get<std::uint32_t>(),
                                        word.at("start_sample").get<std::uint64_t>(),
                                        word.at("end_sample").get<std::uint64_t>()}}}));

  const auto spoken = speech_golden::golden("tts_synthesis");
  ASSERT_GE(spoken.size(), 3U);
  const auto text_bytes = spoken[0].at("input").at("payload_length").get<std::size_t>();
  ASSERT_TRUE(bench.submit(synthesis(1, std::string(text_bytes, 'a'))).has_value());
  const JobResult speech = bench.event_of(1, Completed).result().value();
  const auto& chunk = std::get<AudioChunkResult>(speech.payload());
  const auto& recorded = spoken[2].at("result");
  EXPECT_EQ(chunk.pcm.byte_size, recorded.at("payload_length").get<std::size_t>());
  EXPECT_EQ(chunk.format.sample_rate_hz,
            recorded.at("format").at("sample_rate_hz").get<std::uint32_t>());
  EXPECT_EQ(chunk.clipped_samples, recorded.at("clipped_samples").get<std::uint32_t>());
  // The frames do not hold the payload; the fixture backend returns silence.
  const auto pcm = bench.buffers->data(chunk.pcm.buffer).value();
  EXPECT_EQ(static_cast<std::size_t>(std::count(pcm.begin(), pcm.end(), std::byte{0})),
            chunk.pcm.byte_size);
}

// The speech backends return a job's whole result with `completed`: a job
// that may deliver progress still delivers none.
TEST(FixtureJobBridge, AJobAllowedProgressDeliversNone) {
  Bench bench;
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers, 4)).has_value());
  ASSERT_TRUE(bench.submit(synthesis(2, "hello there.", 4)).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
  EXPECT_EQ(bench.sink->kinds_for(2), (Kinds{Accepted, Completed, Released}));
}

TEST(FixtureJobBridge, TheTenthJobIsFailedUnacceptedAndTheNineRunInSubmissionOrder) {
  std::vector<std::uint64_t> ran;
  Bench bench(make_buffers(), [&ran](const JobRequest& request, BufferManager& buffers) {
    ran.push_back(request.identity().job_id);
    return fixture_job_result(request, buffers);
  });
  bench.bridge.hold_jobs();
  for (std::uint64_t job_id = 1; job_id <= 10; ++job_id) {
    ASSERT_TRUE(bench.submit(decode(job_id, *bench.buffers)).has_value()) << job_id;
  }
  const Error full = bench.event_of(10, Failed).error().value();
  EXPECT_EQ(full.code, Error::Code::ResourceExhausted);
  EXPECT_EQ(full.context.value_or(""), "job_capacity_exhausted");
  EXPECT_EQ(bench.sink->kinds_for(10), (Kinds{Failed, Released}));
  EXPECT_EQ(bench.sink->kinds_for(9), Kinds{Accepted});

  bench.bridge.release_jobs();
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(ran, (std::vector<std::uint64_t>{1, 2, 3, 4, 5, 6, 7, 8, 9}));
}

// jobs.py: a cancellation is acknowledged when it arrives; a waiting job ends
// then, the running one when its work returns, both cancelled and no reason.
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

  bench.bridge.release_jobs();
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  for (const std::uint64_t job_id : {1U, 2U}) {
    const Error ended = bench.event_of(job_id, Failed).error().value();
    EXPECT_EQ(ended.code, Error::Code::Cancelled) << job_id;
    EXPECT_FALSE(ended.context.has_value()) << job_id;
  }
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

TEST(FixtureJobBridge, AResultOfTheWrongKindFailsItsJobAndIsReported) {
  Bench bench(make_buffers(), [](const JobRequest&, BufferManager&) {
    return JobResult::create(TranscriptResult{"not speech", {}, {}});
  });
  bench.refusals_expected = 1;
  ASSERT_TRUE(bench.submit(synthesis(1)).has_value());
  const Error failed = bench.event_of(1, Failed).error().value();
  EXPECT_EQ(failed.code, Error::Code::InferenceFailed);
  EXPECT_EQ(failed.context.value_or(""), "result_kind_mismatch");
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  ASSERT_EQ(bench.bridge.refusals().size(), 1U);
  EXPECT_EQ(bench.bridge.refusals()[0].context.value_or(""), "result_kind_mismatch");
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

// The callback of job 1 starts job 2, which is held, and then blocks: the
// unload finds a job to fail while a callback is running on the other thread.
TEST(FixtureJobBridge, AnUnloadWaitsForARunningCallbackAndLeavesNothingOwed) {
  std::promise<void> inside;
  std::promise<void> leave;
  std::atomic<bool> left{false};
  Bench bench;
  const JobRequest second = decode(2, *bench.buffers);
  bench.sink->set_callback_hook([&, waiting = leave.get_future().share()](const JobEvent& event) {
    if (event.identity().job_id == 1 && event.kind() == Completed) {
      bench.bridge.hold_jobs();
      EXPECT_TRUE(bench.submit(second).has_value());
      inside.set_value();
      waiting.wait();
      left = true;
    }
  });
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  inside.get_future().wait();

  bool left_at_return = false;
  Kinds second_at_return;
  std::thread unloader([&] {
    bench.bridge.unload();
    left_at_return = left.load();
    second_at_return = bench.sink->kinds_for(2);
  });
  // Once health reports the unload, it is at its wait for the callback.
  const auto limit = std::chrono::steady_clock::now() + kPatience;
  while (bench.bridge.health().has_value() && std::chrono::steady_clock::now() < limit) {
    std::this_thread::yield();
  }
  EXPECT_FALSE(bench.bridge.health().has_value());
  leave.set_value();
  unloader.join();

  EXPECT_TRUE(left_at_return);
  EXPECT_EQ(second_at_return, (Kinds{Accepted, Failed, Released}));
}

TEST(InsideABridgeCall, CountsTheCallsOfItsOwnThreadOnly) {
  EXPECT_EQ(InsideABridgeCall::depth(), 0);
  {
    const InsideABridgeCall outer;
    const InsideABridgeCall inner;
    EXPECT_EQ(InsideABridgeCall::depth(), 2);
    int elsewhere = -1;
    std::thread([&elsewhere] { elsewhere = InsideABridgeCall::depth(); }).join();
    EXPECT_EQ(elsewhere, 0);
  }
  EXPECT_EQ(InsideABridgeCall::depth(), 0);
}

}  // namespace

INSTANTIATE_TEST_SUITE_P(Bridges, BoundedJobBridgeConformance,
                         ::testing::Values(BridgeCase{
                             "fixture", [] { return std::make_unique<FixtureBridgeUnderTest>(); }}),
                         [](const auto& bridge_case) { return bridge_case.param.name; });

}  // namespace tensorplate::testing
