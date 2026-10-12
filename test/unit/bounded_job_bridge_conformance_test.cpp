// SPDX-License-Identifier: Apache-2.0
//
// The bridge conformance cases against the in-process fixture bridge and
// against the Python/PyTorch adapter's bridge over a scripted sidecar, and the
// fixture's own hold on its backend, which those cases do not reach.

#include "bounded_job_bridge_conformance.hpp"

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <future>
#include <memory>
#include <mutex>
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

#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"

#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "job_bridge_harness.hpp"
#include "scripted_sidecar.hpp"
#endif

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
  /// @param holds  false for a binding that cannot hold its backend's jobs.
  explicit FixtureBridgeUnderTest(FixtureJobBridgeOptions options, bool holds = true)
      : options_(options), holds_(holds) {}
  ~FixtureBridgeUnderTest() override {
    bridge_.unload();
    EXPECT_TRUE(bridge_.refusals().empty());
  }

  // With late cancels it states nothing more than its hold and the class it
  // refuses: one binding that the cases must pass on the header alone.
  Choices choices() const override {
    Choices stated{.holds_started_jobs = holds_, .refuses_vad_frames = true};
    if (!options_.cancels_arrive_late) {
      stated.runs_one_job_at_a_time = true;
      stated.ends_a_waiting_job_at_its_cancel = true;
      stated.fails_a_held_job_that_was_cancelled = true;
      stated.open_jobs = 1 + FixtureJobBridge::kWaitingJobs;
    }
    return stated;
  }
  BoundedJobBridge& bridge() override { return bridge_; }
  BufferManager& buffers() override { return *buffers_; }
  void prime() override { bridge_.prime(); }
  void unload() override { bridge_.unload(); }
  void lose_backend() override { bridge_.lose_backend(); }
  void hold_jobs() override {
    if (holds_) {
      bridge_.hold_jobs();
    }
  }
  void release_jobs() override { bridge_.release_jobs(); }
  bool settled(std::chrono::milliseconds patience) override { return bridge_.settled(patience); }
  TranscriptResult transcript(const JobRequest& request) const override {
    return fixture_transcript(std::get<AudioFrames>(request.payload()));
  }
  std::vector<std::byte> speech(const JobRequest& request) const override {
    const std::size_t text_bytes = std::get<TextSegment>(request.payload()).text.size();
    return std::vector<std::byte>(kFixtureSpeechBytesPerTextByte * text_bytes, std::byte{0});
  }

 private:
  const FixtureJobBridgeOptions options_;
  const bool holds_;
  std::unique_ptr<BufferManager> buffers_ = make_buffers();
  FixtureJobBridge bridge_{*buffers_, fixture_job_result, options_};
};

BridgeCase fixture_case(std::string name, FixtureJobBridgeOptions options, bool holds = true) {
  return {std::move(name),
          [options, holds] { return std::make_unique<FixtureBridgeUnderTest>(options, holds); }};
}

// The depth of the runner's job queue (GPU_LANE_QUEUE_DEPTH in jobs.py).
static_assert(FixtureJobBridge::kWaitingJobs == 8);

#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR

/// The Python/PyTorch adapter's bridge with its session, over a scripted
/// sidecar whose backend is a thread with the fixture's results. Built
/// loaded, as a session is before its prime: a release is then taken, and
/// the backend can be lost.
class ScriptedSidecarBridgeUnderTest final : public BridgeUnderTest {
 public:
  ScriptedSidecarBridgeUnderTest() {
    ExecutionSessionRuntimeHooks hooks{};
    hooks.buffer_manager = buffers_.get();
    adapters::python_pytorch::PythonPytorchConfig config;
    config.startup_timeout = ScriptedSidecar::kPatience;
    config.health_timeout = ScriptedSidecar::kPatience;
    built_ = adapters::python_pytorch::make_python_pytorch_session_with_bridge(
        hooks, std::move(config), sidecar_.launcher());
    const auto loaded = built_.session->load(
        ModelSpec::create("speech", ModelClass::Speech, "/dev/null", "python_pytorch").value());
    EXPECT_TRUE(loaded.has_value()) << loaded.error().message;
  }
  ~ScriptedSidecarBridgeUnderTest() override { unload(); }

  // What the runner does, which the scripted sidecar keeps: one job runs,
  // eight wait, a waiting job ends at its cancel and a running one that was
  // cancelled ends `failed`; the job beyond them is failed, not refused.
  Choices choices() const override {
    return {.holds_started_jobs = true,
            .refuses_vad_frames = true,
            .runs_one_job_at_a_time = true,
            .ends_a_waiting_job_at_its_cancel = true,
            .fails_a_held_job_that_was_cancelled = true,
            .open_jobs = 1 + FixtureJobBridge::kWaitingJobs};
  }
  BoundedJobBridge& bridge() override { return *built_.bridge; }
  BufferManager& buffers() override { return *buffers_; }
  void prime() override {
    const auto primed = built_.session->prime();
    EXPECT_TRUE(primed.has_value()) << primed.error().message;
  }
  // The session's unload waits behind the job the sidecar runs: a held one is
  // let go only once the sidecar has the unload, so the jobs behind it are
  // still waiting when it arrives. After a lost backend the unload fails, and
  // releases all the same.
  void unload() override {
    backend_.end_a_held_job_at_the_unload();
    (void)built_.session->unload();
  }
  void lose_backend() override {
    backend_.lose();
    sidecar_.die();
  }
  void hold_jobs() override { backend_.hold(); }
  void release_jobs() override { backend_.release(); }
  bool settled(std::chrono::milliseconds patience) override {
    return settled_over(*built_.channel, sidecar_, backend_, patience);
  }
  TranscriptResult transcript(const JobRequest& request) const override {
    return fixture_transcript(std::get<AudioFrames>(request.payload()));
  }
  std::vector<std::byte> speech(const JobRequest& request) const override {
    const std::size_t text_bytes = std::get<TextSegment>(request.payload()).text.size();
    return std::vector<std::byte>(kFixtureSpeechBytesPerTextByte * text_bytes, std::byte{0});
  }

 private:
  // In this order: the session goes first, which reaps the sidecar, then the
  // backend's thread, then the sidecar it reads.
  std::unique_ptr<BufferManager> buffers_ = make_buffers();
  ScriptedSidecar sidecar_;
  ScriptedBackend backend_{sidecar_};
  adapters::python_pytorch::SessionWithJobBridge built_;
};

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR

// -- The fixture's own rules, which the cases leave open. -------------------------

constexpr std::chrono::seconds kPatience{30};
constexpr std::uint64_t kGeneration = 3;

JobRequest synthesis(std::uint64_t job_id, std::string text = "hello there.",
                     std::uint32_t progress_limit = 0) {
  return JobRequest::create(
             JobIdentity{job_id, 5, kGeneration}, JobClass::TtsSynthesis,
             TextSegment{std::move(text)},
             JobOptions{.language = "en-US", .voice = "af_heart", .speed_milli = 1000},
             progress_limit)
      .value();
}

JobRequest decode(std::uint64_t job_id, BufferManager& buffers, std::uint32_t progress_limit = 0,
                  std::uint64_t session_key = 5) {
  const BufferRef buffer = buffers.allocate(3'200).value();
  return JobRequest::create(JobIdentity{job_id, session_key, kGeneration}, JobClass::SttDecode,
                            AudioFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, 3'200}, 0},
                            JobOptions{.language = "en"}, progress_limit)
      .value();
}

/// Opens once, for a callback and the test body to wait on each other. A wait
/// is bounded: a fixture that never gets there fails the test.
class Gate {
 public:
  void open() {
    {
      const std::lock_guard<std::mutex> lock(mutex_);
      open_ = true;
    }
    opened_.notify_all();
  }

  /// False when the gate did not open in time.
  [[nodiscard]] bool wait() {
    std::unique_lock<std::mutex> lock(mutex_);
    return opened_.wait_for(lock, kPatience, [this] { return open_; });
  }

 private:
  std::mutex mutex_;
  std::condition_variable opened_;
  bool open_ = false;
};

/// Opens a gate when the test body ends, however it ends, so that the bench
/// declared before it does not unload around a callback that still waits.
struct OpensAtExit {
  Gate& gate;
  ~OpensAtExit() { gate.open(); }
};

struct Bench {
  explicit Bench(std::unique_ptr<BufferManager> pool = make_buffers(),
                 FixtureJobBridge::ResultFunction result = fixture_job_result,
                 FixtureJobBridgeOptions options = {}, bool primed = true)
      : buffers(std::move(pool)), bridge(*buffers, std::move(result), options) {
    EXPECT_TRUE(bridge.set_event_sink(sink).has_value());
    if (primed) {
      bridge.prime();
    }
  }
  Bench(const Bench&) = delete;
  Bench& operator=(const Bench&) = delete;
  // After the unload: what it delivers is checked too.
  ~Bench() {
    bridge.unload();
    EXPECT_EQ(bridge.refusals().size(), refusals_expected);
  }

  [[nodiscard]] Result<void> submit(const JobRequest& request) {
    sink->expect_job(request);
    return bridge.submit(request);
  }

  [[nodiscard]] Result<void> cancel(std::uint64_t job_id) {
    sink->expect_cancel(job_id);
    return bridge.cancel(job_id);
  }

  [[nodiscard]] Result<void> release(std::uint64_t session_key) {
    sink->expect_session_release(session_key);
    return bridge.release_session(session_key, kGeneration);
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
using Sessions = std::vector<std::pair<std::uint64_t, std::uint64_t>>;
using enum JobEventKind;

std::string reason_of(const Result<void>& refused) {
  return refused.has_value() ? "" : refused.error().context.value_or("");
}

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
  // Declared before the bench: the result function writes it until the unload.
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

// FixtureJobBridgeOptions::submit_refuses_at_capacity: the other way the
// header lets a full backend turn a job away.
TEST(FixtureJobBridge, SubmitRefusesTheTenthUnreleasedJobWhenToldTo) {
  Bench bench(make_buffers(), fixture_job_result, {.submit_refuses_at_capacity = true});
  bench.bridge.hold_jobs();
  for (std::uint64_t job_id = 1; job_id <= 9; ++job_id) {
    ASSERT_TRUE(bench.submit(decode(job_id, *bench.buffers)).has_value()) << job_id;
  }
  const Result<void> tenth = bench.bridge.submit(decode(10, *bench.buffers));
  ASSERT_FALSE(tenth.has_value());
  EXPECT_EQ(tenth.error().code, Error::Code::ResourceExhausted);
  EXPECT_EQ(reason_of(tenth), "job_capacity_exhausted");
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_TRUE(bench.sink->kinds_for(10).empty());

  bench.bridge.release_jobs();
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(9), (Kinds{Accepted, Completed, Released}));
  ASSERT_TRUE(bench.submit(decode(10, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(10), (Kinds{Accepted, Completed, Released}));
}

// jobs.py: a cancellation is acknowledged when it arrives; a waiting job ends
// then, the running one when its work returns, both cancelled and no reason.
TEST(FixtureJobBridge, ACancelIsAcknowledgedAtOnceAndEndsTheJobCancelled) {
  Bench bench;
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(2, *bench.buffers)).has_value());
  for (const std::uint64_t job_id : {1U, 2U}) {
    ASSERT_TRUE(bench.cancel(job_id).has_value());
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

// FixtureJobBridgeOptions::cancels_arrive_late: the other end job_event.hpp
// gives a cancelled job, for a running job and for a waiting one.
TEST(FixtureJobBridge, ACancelledJobCompletesUnacknowledgedWhenCancelsArriveLate) {
  Bench bench(make_buffers(), fixture_job_result, {.cancels_arrive_late = true});
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(2, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(3, *bench.buffers, 0, /*session_key=*/6)).has_value());
  ASSERT_TRUE(bench.cancel(1).has_value());
  ASSERT_TRUE(bench.cancel(2).has_value());
  ASSERT_TRUE(bench.release(6).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  for (const std::uint64_t job_id : {1U, 2U, 3U}) {
    EXPECT_EQ(bench.sink->kinds_for(job_id), Kinds{Accepted}) << job_id;
  }
  EXPECT_TRUE(bench.sink->session_releases().empty());

  bench.bridge.release_jobs();
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  for (const std::uint64_t job_id : {1U, 2U, 3U}) {
    EXPECT_EQ(bench.sink->kinds_for(job_id), (Kinds{Accepted, Completed, Released})) << job_id;
  }
  EXPECT_EQ(bench.sink->session_releases(), (Sessions{{6, kGeneration}}));
  EXPECT_TRUE(bench.sink->violations().empty());
}

TEST(FixtureJobBridge, ASessionReleaseCancelsItsRunningJobAndItsWaitingOne) {
  Bench bench;
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(2, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.submit(decode(3, *bench.buffers, 0, /*session_key=*/6)).has_value());
  ASSERT_TRUE(bench.release(5).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged}));
  EXPECT_EQ(bench.sink->kinds_for(2), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(bench.sink->kinds_for(3), Kinds{Accepted});
  EXPECT_TRUE(bench.sink->session_releases().empty());

  bench.bridge.release_jobs();
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(bench.event_of(1, Failed).error().value().code, Error::Code::Cancelled);
  EXPECT_EQ(bench.sink->kinds_for(3), (Kinds{Accepted, Completed, Released}));
  EXPECT_EQ(bench.sink->session_releases(), (Sessions{{5, kGeneration}}));
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

// A backend lost before its prime is unavailable, not "not ready", where the
// header's order of refusals leaves that open; a session unloaded before its
// prime is unavailable by the header's own entry.
TEST(FixtureJobBridge, BeforeItsPrimeALostOrAnUnloadedBackendIsUnavailable) {
  Bench lost(make_buffers(), fixture_job_result, {}, /*primed=*/false);
  lost.bridge.lose_backend();
  EXPECT_EQ(reason_of(lost.bridge.submit(synthesis(1))), "backend_unavailable");
  EXPECT_EQ(reason_of(lost.bridge.health()), "backend_unavailable");
  Bench unloaded(make_buffers(), fixture_job_result, {}, /*primed=*/false);
  unloaded.bridge.unload();
  EXPECT_EQ(reason_of(unloaded.bridge.submit(synthesis(1))), "backend_unavailable");
  EXPECT_EQ(reason_of(unloaded.bridge.health()), "backend_unavailable");
}

TEST(FixtureJobBridge, BeforeItsPrimeASessionReleaseIsTakenAndAcknowledged) {
  Bench bench(make_buffers(), fixture_job_result, {}, /*primed=*/false);
  ASSERT_TRUE(bench.release(5).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->session_releases(), (Sessions{{5, kGeneration}}));
  bench.bridge.prime();
  EXPECT_EQ(reason_of(bench.bridge.submit(synthesis(1))), "session_releasing");
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

// The cancellation was acknowledged before the backend went: the job still
// fails as reset. It is the bench's unload that releases it.
TEST(FixtureJobBridge, ALostBackendFailsACancelledRunningJobAsReset) {
  Bench bench;
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.cancel(1).has_value());
  bench.bridge.lose_backend();
  EXPECT_EQ(bench.event_of(1, Failed).error().value().context.value_or(""), "backend_reset");
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged, Failed}));
}

// Without a bench, whose own unload would come first.
TEST(FixtureJobBridge, ItsDestructionUnloads) {
  const std::unique_ptr<BufferManager> buffers = make_buffers();
  const auto sink = std::make_shared<RecordingJobEventSink>();
  {
    FixtureJobBridge bridge(*buffers);
    ASSERT_TRUE(bridge.set_event_sink(sink).has_value());
    bridge.prime();
    bridge.hold_jobs();
    const JobRequest request = synthesis(1);
    sink->expect_job(request);
    ASSERT_TRUE(bridge.submit(request).has_value());
    ASSERT_TRUE(bridge.settled(kPatience));
  }
  ASSERT_EQ(sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(sink->events()[1].error().value().context.value_or(""), "session_unloaded");
  EXPECT_TRUE(sink->violations().empty());
}

TEST(FixtureJobBridge, IsNotSettledWhileACallbackRuns) {
  Gate inside;
  Gate leave;
  Bench bench;
  const OpensAtExit opens{leave};
  bench.sink->set_callback_hook([&inside, &leave](const JobEvent& event) {
    if (event.kind() == Completed) {
      inside.open();
      EXPECT_TRUE(leave.wait());
    }
  });
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), Kinds{Accepted});

  bench.bridge.release_jobs();
  ASSERT_TRUE(inside.wait());
  EXPECT_FALSE(bench.bridge.settled(std::chrono::milliseconds{0}));
  leave.open();
  ASSERT_TRUE(bench.bridge.settled(kPatience));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
}

// The unload makes its callbacks on its caller's thread: the thread that runs
// the jobs is idle while this one is inside `failed`.
TEST(FixtureJobBridge, IsNotSettledWhileAnUnloadOnAnotherThreadIsInACallback) {
  Gate inside;
  Gate leave;
  Bench bench;
  std::future<void> unloaded;
  const OpensAtExit opens{leave};
  bench.sink->set_callback_hook([&inside, &leave](const JobEvent& event) {
    if (event.kind() == Failed) {
      inside.open();
      EXPECT_TRUE(leave.wait());
    }
  });
  bench.bridge.hold_jobs();
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(bench.bridge.settled(kPatience));

  unloaded = std::async(std::launch::async, [&bench] { bench.bridge.unload(); });
  ASSERT_TRUE(inside.wait());
  EXPECT_FALSE(bench.bridge.settled(std::chrono::milliseconds{0}));
  leave.open();
  ASSERT_EQ(unloaded.wait_for(kPatience), std::future_status::ready);
  EXPECT_TRUE(bench.bridge.settled(std::chrono::milliseconds{0}));
  EXPECT_EQ(bench.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
}

// The callback of job 1 starts job 2, which is held, and then blocks: the
// unload finds a job to fail while a callback is running on the other thread.
TEST(FixtureJobBridge, AnUnloadWaitsForARunningCallbackAndLeavesNothingOwed) {
  // What the callback and the unloading thread write outlives the bench.
  Gate inside;
  Gate leave;
  std::atomic<bool> left{false};
  bool left_at_return = false;
  Kinds second_at_return;
  Bench bench;
  std::future<void> unloaded;
  const OpensAtExit opens{leave};
  bench.sink->set_callback_hook([&, second = decode(2, *bench.buffers)](const JobEvent& event) {
    if (event.identity().job_id == 1 && event.kind() == Completed) {
      bench.bridge.hold_jobs();
      EXPECT_TRUE(bench.submit(second).has_value());
      inside.open();
      EXPECT_TRUE(leave.wait());
      left = true;
    }
  });
  ASSERT_TRUE(bench.submit(decode(1, *bench.buffers)).has_value());
  ASSERT_TRUE(inside.wait());

  unloaded = std::async(std::launch::async, [&] {
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
  leave.open();
  ASSERT_EQ(unloaded.wait_for(kPatience), std::future_status::ready);

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

// The fixture with each of its options, so that both outcomes the header
// allows pass the same cases, and once without its hold.
INSTANTIATE_TEST_SUITE_P(
    Bridges, BoundedJobBridgeConformance,
    ::testing::Values(fixture_case("fixture", {}),
                      fixture_case("fixture_refusing_at_capacity",
                                   {.submit_refuses_at_capacity = true}),
                      fixture_case("fixture_with_late_cancels", {.cancels_arrive_late = true}),
                      fixture_case("fixture_without_a_hold", {}, /*holds=*/false)),
    [](const auto& bridge_case) { return bridge_case.param.name; });

#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR
INSTANTIATE_TEST_SUITE_P(PythonPytorchAdapter, BoundedJobBridgeConformance,
                         ::testing::Values(BridgeCase{
                             "scripted_sidecar",
                             [] { return std::make_unique<ScriptedSidecarBridgeUnderTest>(); }}),
                         [](const auto& bridge_case) { return bridge_case.param.name; });
#endif

}  // namespace tensorplate::testing
