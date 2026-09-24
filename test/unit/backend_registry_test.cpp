// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01-T02 / T03: Unit tests for `BackendRegistry`.

#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/model_spec.hpp"

#include "fake_bounded_job_bridge.hpp"
#include "mock_execution_session.hpp"
#include "recording_job_event_sink.hpp"

namespace tensorplate {
namespace {

using testing::FakeBoundedJobBridge;
using testing::MockSession;
using testing::RecordingJobEventSink;

BackendCapability simple_cap(const std::string& name,
                             std::vector<PrecisionHint> precision = {PrecisionHint::Fp32}) {
  return BackendCapability::create(name, std::move(precision)).value();
}

ExecutionSessionFactory mock_factory(const std::string& name) {
  return [name](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
    return std::unique_ptr<ExecutionSession>(
        new MockSession(name, hooks.event_sink, hooks.buffer_manager));
  };
}

TEST(BackendRegistry, RegisterAndLookup) {
  BackendRegistry reg;
  auto entry = BackendEntry{"mock", simple_cap("mock"), mock_factory("mock")};
  ASSERT_TRUE(reg.register_backend(std::move(entry)).has_value());

  EXPECT_TRUE(reg.is_registered("mock"));
  EXPECT_FALSE(reg.is_registered("absent"));

  auto cap = reg.capability("mock");
  ASSERT_TRUE(cap.has_value());
  EXPECT_EQ(cap.value().backend_name(), "mock");

  auto session = reg.create_session("mock");
  ASSERT_TRUE(session.has_value());
  EXPECT_EQ(std::string(session.value()->backend_name()), "mock");
}

TEST(BackendRegistry, DuplicateRegistrationRejected) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"mock", simple_cap("mock"), mock_factory("mock")}).has_value());

  auto r = reg.register_backend({"mock", simple_cap("mock"), mock_factory("mock")});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Internal);
}

TEST(BackendRegistry, EmptyNameRejected) {
  BackendRegistry reg;
  // capability with empty name fails to construct; build it manually
  // through a non-empty name then mismatch the registry entry.
  auto cap = simple_cap("real");
  auto r = reg.register_backend({"", std::move(cap), mock_factory("real")});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendRegistry, NullFactoryRejected) {
  BackendRegistry reg;
  auto r = reg.register_backend({"mock", simple_cap("mock"), nullptr});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendRegistry, NameMismatchRejected) {
  BackendRegistry reg;
  auto r = reg.register_backend({"mock", simple_cap("other"), mock_factory("mock")});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendRegistry, LookupUnknownReturnsUnsupported) {
  BackendRegistry reg;
  auto cap = reg.capability("absent");
  ASSERT_FALSE(cap.has_value());
  EXPECT_EQ(cap.error().code, Error::Code::Unsupported);

  auto session = reg.create_session("absent");
  ASSERT_FALSE(session.has_value());
  EXPECT_EQ(session.error().code, Error::Code::Unsupported);
}

TEST(BackendRegistry, RegisteredBackendsSortedAndStable) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"libtorch", simple_cap("libtorch"), mock_factory("libtorch")})
                  .has_value());
  ASSERT_TRUE(reg.register_backend({"tensorrt", simple_cap("tensorrt"), mock_factory("tensorrt")})
                  .has_value());
  ASSERT_TRUE(reg.register_backend({"python_pytorch", simple_cap("python_pytorch"),
                                    mock_factory("python_pytorch")})
                  .has_value());

  auto names = reg.registered_backends();
  ASSERT_EQ(names.size(), 3u);
  EXPECT_EQ(names[0], "libtorch");
  EXPECT_EQ(names[1], "python_pytorch");
  EXPECT_EQ(names[2], "tensorrt");
}

TEST(BackendRegistry, DeregisterReturnsTrueOnHit) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"mock", simple_cap("mock"), mock_factory("mock")}).has_value());
  EXPECT_TRUE(reg.deregister_backend("mock"));
  EXPECT_FALSE(reg.is_registered("mock"));
  EXPECT_FALSE(reg.deregister_backend("mock"));
}

TEST(BackendRegistry, ValidateBackendHintRejectsUnknown) {
  BackendRegistry reg;
  auto spec = ModelSpec::create("m", ModelClass::Vision, "/dev/null", "missing").value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

TEST(BackendRegistry, ValidateBackendHintAcceptsAutoPrecision) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"tensorrt", simple_cap("tensorrt", {PrecisionHint::Fp16}),
                                    mock_factory("tensorrt")})
                  .has_value());
  auto spec = ModelSpec::create("vision-m", ModelClass::Vision, "/dev/null", "tensorrt",
                                PrecisionHint::Auto)
                  .value();
  ASSERT_TRUE(reg.validate_backend_hint(spec).has_value());
}

TEST(BackendRegistry, ValidateBackendHintRejectsUnsupportedPrecision) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"tensorrt", simple_cap("tensorrt", {PrecisionHint::Fp16}),
                                    mock_factory("tensorrt")})
                  .has_value());
  auto spec = ModelSpec::create("vision-m", ModelClass::Vision, "/dev/null", "tensorrt",
                                PrecisionHint::Fp32)
                  .value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
  EXPECT_TRUE(r.error().context.has_value());
}

TEST(BackendRegistry, PythonPytorchDoesNotFallBackToLibtorch) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"libtorch", simple_cap("libtorch"), mock_factory("libtorch")})
                  .has_value());
  auto spec = ModelSpec::create("smolvla", ModelClass::Vla, "/dev/null", "python_pytorch").value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value()) << "python_pytorch must not silently redirect to libtorch";
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);

  auto session = reg.create_session("python_pytorch");
  ASSERT_FALSE(session.has_value());
  EXPECT_EQ(session.error().code, Error::Code::Unsupported);
}

struct NoopSink final : SessionEventSink {
  void on_event(const SessionEvent& /*event*/) override {}
};

TEST(BackendRegistry, FactoryReceivesHooks) {
  BackendRegistry reg;
  bool seen_sink = false;
  bool seen_manager = false;

  ExecutionSessionFactory f =
      [&](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
    seen_sink = (hooks.event_sink != nullptr);
    seen_manager = (hooks.buffer_manager != nullptr);
    return std::unique_ptr<ExecutionSession>(new MockSession("mock"));
  };
  ASSERT_TRUE(reg.register_backend({"mock", simple_cap("mock"), std::move(f)}).has_value());

  NoopSink sink;
  BufferManagerConfig bm_cfg;
  bm_cfg.pool_name = "registry-test";
  bm_cfg.capacity_bytes = 1024;
  bm_cfg.max_buffer_bytes = 1024;
  auto bm = BufferManager::create(bm_cfg);
  ASSERT_TRUE(bm.has_value());

  ExecutionSessionRuntimeHooks hooks{};
  hooks.event_sink = &sink;
  hooks.buffer_manager = bm.value().get();
  auto s = reg.create_session("mock", hooks);
  ASSERT_TRUE(s.has_value());
  EXPECT_TRUE(seen_sink);
  EXPECT_TRUE(seen_manager);
}

// -- Sessions with a job bridge. ----------------------------------------------

// Counts calls and remembers the bridge each call created.
struct BridgeFactoryProbe {
  int calls = 0;
  std::vector<BoundedJobBridge*> bridges;
};

SessionWithBridgeFactory bridge_factory(const std::string& name, BridgeFactoryProbe& probe) {
  return [name, &probe](ExecutionSessionRuntimeHooks hooks) -> Result<SessionWithBridge> {
    ++probe.calls;
    auto bridge = std::make_shared<FakeBoundedJobBridge>();
    probe.bridges.push_back(bridge.get());
    return SessionWithBridge{std::unique_ptr<ExecutionSession>(
                                 new MockSession(name, hooks.event_sink, hooks.buffer_manager)),
                             std::move(bridge)};
  };
}

ExecutionSessionFactory counting_factory(const std::string& name, int& calls) {
  return [name, &calls](ExecutionSessionRuntimeHooks) -> Result<std::unique_ptr<ExecutionSession>> {
    ++calls;
    return std::unique_ptr<ExecutionSession>(new MockSession(name));
  };
}

TEST(BackendRegistryJobBridge, OneFactoryCallYieldsTheSessionAndItsBridge) {
  BackendRegistry reg;
  BridgeFactoryProbe probe;
  int plain_calls = 0;
  ASSERT_TRUE(
      reg.register_backend({"jobs", simple_cap("jobs"), counting_factory("jobs", plain_calls),
                            bridge_factory("jobs", probe)})
          .has_value());

  auto pair = reg.create_session_with_bridge("jobs");
  ASSERT_TRUE(pair.has_value()) << pair.error().message;
  EXPECT_EQ(probe.calls, 1);
  EXPECT_EQ(plain_calls, 0) << "the plain factory must not run for a bridged session";
  ASSERT_NE(pair.value().session, nullptr);
  EXPECT_EQ(std::string(pair.value().session->backend_name()), "jobs");
  ASSERT_EQ(probe.bridges.size(), 1U);
  EXPECT_EQ(pair.value().bridge.get(), probe.bridges[0]) << "the bridge is the factory's own";
}

TEST(BackendRegistryJobBridge, EveryCallCreatesItsOwnPair) {
  BackendRegistry reg;
  BridgeFactoryProbe probe;
  int plain_calls = 0;
  ASSERT_TRUE(
      reg.register_backend({"jobs", simple_cap("jobs"), counting_factory("jobs", plain_calls),
                            bridge_factory("jobs", probe)})
          .has_value());
  auto first = reg.create_session_with_bridge("jobs");
  auto second = reg.create_session_with_bridge("jobs");
  ASSERT_TRUE(first.has_value());
  ASSERT_TRUE(second.has_value());
  EXPECT_EQ(probe.calls, 2);
  EXPECT_NE(first.value().bridge, second.value().bridge);
  EXPECT_NE(first.value().session.get(), second.value().session.get());
}

TEST(BackendRegistryJobBridge, EntryWithoutBridgeFactoryReportsUnsupportedForJobs) {
  BackendRegistry reg;
  int plain_calls = 0;
  ASSERT_TRUE(
      reg.register_backend({"plain", simple_cap("plain"), counting_factory("plain", plain_calls)})
          .has_value());

  auto pair = reg.create_session_with_bridge("plain");
  ASSERT_FALSE(pair.has_value());
  EXPECT_EQ(pair.error().code, Error::Code::Unsupported);
  EXPECT_EQ(pair.error().context.value_or(""), "job_bridge_unsupported");
  EXPECT_EQ(plain_calls, 0) << "no session is created when jobs are unsupported";

  auto session = reg.create_session("plain");
  ASSERT_TRUE(session.has_value()) << "the plain path is unchanged";
  EXPECT_EQ(plain_calls, 1);
}

TEST(BackendRegistryJobBridge, UnknownBackendIsUnsupportedWithoutTheBridgeReason) {
  BackendRegistry reg;
  auto pair = reg.create_session_with_bridge("absent");
  ASSERT_FALSE(pair.has_value());
  EXPECT_EQ(pair.error().code, Error::Code::Unsupported);
  EXPECT_FALSE(pair.error().context.has_value());
}

TEST(BackendRegistryJobBridge, CreateSessionStillUsesThePlainFactory) {
  BackendRegistry reg;
  BridgeFactoryProbe probe;
  int plain_calls = 0;
  ASSERT_TRUE(
      reg.register_backend({"jobs", simple_cap("jobs"), counting_factory("jobs", plain_calls),
                            bridge_factory("jobs", probe)})
          .has_value());
  ASSERT_TRUE(reg.create_session("jobs").has_value());
  EXPECT_EQ(plain_calls, 1);
  EXPECT_EQ(probe.calls, 0);
}

TEST(BackendRegistryJobBridge, PlainFactoryIsStillRequired) {
  BackendRegistry reg;
  BridgeFactoryProbe probe;
  auto r =
      reg.register_backend({"jobs", simple_cap("jobs"), nullptr, bridge_factory("jobs", probe)});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
  EXPECT_FALSE(reg.is_registered("jobs"));
}

TEST(BackendRegistryJobBridge, FactoryErrorsPassThroughUnchanged) {
  BackendRegistry reg;
  const Error failure = Error::make(Error::Code::LoadFailed, "sidecar missing", "python_exe");
  int plain_calls = 0;
  ASSERT_TRUE(
      reg.register_backend({"jobs", simple_cap("jobs"), counting_factory("jobs", plain_calls),
                            [&](ExecutionSessionRuntimeHooks) -> Result<SessionWithBridge> {
                              return unexpected(failure);
                            }})
          .has_value());
  auto pair = reg.create_session_with_bridge("jobs");
  ASSERT_FALSE(pair.has_value());
  EXPECT_EQ(pair.error(), failure);
}

TEST(BackendRegistryJobBridge, AMissingHalfOfThePairIsInternalAndTheOtherHalfIsDestroyed) {
  BackendRegistry reg;
  int plain_calls = 0;
  std::weak_ptr<BoundedJobBridge> orphan;
  ASSERT_TRUE(reg.register_backend({"no_session", simple_cap("no_session"),
                                    counting_factory("no_session", plain_calls),
                                    [&](ExecutionSessionRuntimeHooks) -> Result<SessionWithBridge> {
                                      auto bridge = std::make_shared<FakeBoundedJobBridge>();
                                      orphan = bridge;
                                      return SessionWithBridge{nullptr, std::move(bridge)};
                                    }})
                  .has_value());
  ASSERT_TRUE(reg.register_backend({"no_bridge", simple_cap("no_bridge"),
                                    counting_factory("no_bridge", plain_calls),
                                    [](ExecutionSessionRuntimeHooks) -> Result<SessionWithBridge> {
                                      return SessionWithBridge{
                                          std::unique_ptr<ExecutionSession>(new MockSession()),
                                          nullptr};
                                    }})
                  .has_value());
  auto a = reg.create_session_with_bridge("no_session");
  ASSERT_FALSE(a.has_value());
  EXPECT_EQ(a.error().code, Error::Code::Internal);
  EXPECT_EQ(a.error().context.value_or(""), "null_session");
  EXPECT_TRUE(orphan.expired()) << "the registry must not leak the half it was given";
  auto b = reg.create_session_with_bridge("no_bridge");
  ASSERT_FALSE(b.has_value());
  EXPECT_EQ(b.error().code, Error::Code::Internal);
  EXPECT_EQ(b.error().context.value_or(""), "null_bridge");
}

TEST(BackendRegistryJobBridge, BridgeFactoryReceivesHooks) {
  BackendRegistry reg;
  NoopSink sink;
  bool seen_sink = false;
  int plain_calls = 0;
  ASSERT_TRUE(
      reg.register_backend({"jobs", simple_cap("jobs"), counting_factory("jobs", plain_calls),
                            [&](ExecutionSessionRuntimeHooks hooks) -> Result<SessionWithBridge> {
                              seen_sink = hooks.event_sink == &sink;
                              return SessionWithBridge{
                                  std::unique_ptr<ExecutionSession>(new MockSession()),
                                  std::make_shared<FakeBoundedJobBridge>()};
                            }})
          .has_value());
  ExecutionSessionRuntimeHooks hooks{};
  hooks.event_sink = &sink;
  ASSERT_TRUE(reg.create_session_with_bridge("jobs", hooks).has_value());
  EXPECT_TRUE(seen_sink);
}

// -- The fixture bridge: one job without progress, and interleaved jobs. ------

const JobOptions kSttOptions{.language = "en"};
const JobOptions kTtsOptions{.language = "en-US", .voice = "af_heart", .speed_milli = 1000};

BufferRef pcm_buffer(std::uint64_t id, std::size_t bytes, BufferOwnership ownership) {
  return BufferRef::create(id, bytes, ownership).value();
}

JobRequest speech_request(std::uint64_t job_id, JobClass job_class, std::uint64_t session_key = 0) {
  const JobIdentity identity{job_id, session_key == 0 ? 10 + job_id : session_key, 1};
  const BufferRef pcm = pcm_buffer(100 + job_id, 2048, BufferOwnership::Borrowed);
  switch (job_class) {
    case JobClass::SttDecode:
      return JobRequest::create(identity, job_class,
                                AudioFrames{kJobInputAudioFormat, PcmWindow{pcm, 0, 2048}, 0},
                                kSttOptions)
          .value();
    case JobClass::TtsSynthesis:
      return JobRequest::create(identity, job_class, TextSegment{"Hello there."}, kTtsOptions)
          .value();
    case JobClass::VadFrames:
      break;
  }
  return JobRequest::create(identity, job_class,
                            VadFrames{kJobInputAudioFormat, PcmWindow{pcm, 0, 2048}, 512, 2, 1})
      .value();
}

JobResult speech_result(JobClass job_class) {
  switch (job_class) {
    case JobClass::SttDecode:
      return JobResult::create(TranscriptResult{}).value();
    case JobClass::TtsSynthesis:
      return JobResult::create(
                 AudioChunkResult{kJobOutputAudioFormat,
                                  PcmWindow{pcm_buffer(900, 960, BufferOwnership::Owned), 0, 960}})
          .value();
    case JobClass::VadFrames:
      break;
  }
  return JobResult::create(VadResult{{0.25F, 0.75F}}).value();
}

JobResult tokens(std::uint32_t first, std::uint32_t count) {
  TranscriptResult t{"t", {}, {}};
  for (std::uint32_t i = 0; i < count; ++i) {
    t.tokens.push_back(first + i);
  }
  return JobResult::create(std::move(t)).value();
}

struct BridgeUnderTest {
  std::unique_ptr<ExecutionSession> session;  // Kept alive: the bridge serves it.
  std::shared_ptr<FakeBoundedJobBridge> fake;
  std::shared_ptr<RecordingJobEventSink> sink = std::make_shared<RecordingJobEventSink>();

  // Records the job with the sink, then submits it, as a receiver must.
  void submit(const JobRequest& request) {
    sink->expect_job(request);
    ASSERT_TRUE(fake->submit(request).has_value());
  }
  // Notes the cancellation, then requests it: the acknowledgement may arrive
  // before cancel() returns.
  void cancel(std::uint64_t job_id) {
    sink->expect_cancel(job_id);
    ASSERT_TRUE(fake->cancel(job_id).has_value());
  }
};

// The fixture bridge obtained the way serving obtains any bridge: from the
// registry, bound to its session by one factory call. The session stays
// alive for the whole test, as serving keeps it.
BridgeUnderTest bridge_from_registry() {
  BackendRegistry reg;
  BridgeFactoryProbe probe;
  int plain_calls = 0;
  EXPECT_TRUE(
      reg.register_backend({"speech", simple_cap("speech"), counting_factory("speech", plain_calls),
                            bridge_factory("speech", probe)})
          .has_value());
  auto pair = reg.create_session_with_bridge("speech");
  EXPECT_TRUE(pair.has_value());
  BridgeUnderTest out;
  out.session = std::move(pair.value().session);
  out.fake = std::static_pointer_cast<FakeBoundedJobBridge>(pair.value().bridge);
  EXPECT_TRUE(out.fake->set_event_sink(out.sink).has_value());
  return out;
}

TEST(JobBridgeFixture, BoundedJobDeliversNoProgress) {
  auto b = bridge_from_registry();
  std::uint64_t job_id = 1;
  for (const JobClass job_class :
       {JobClass::SttDecode, JobClass::TtsSynthesis, JobClass::VadFrames}) {
    b.submit(speech_request(job_id, job_class));
    b.fake->run_bounded_job(job_id, speech_result(job_class));
    ++job_id;
  }
  EXPECT_TRUE(b.sink->events().empty()) << "nothing is delivered inside submit";
  EXPECT_EQ(b.fake->deliver(), 9U);
  const std::vector<JobEventKind> bounded{JobEventKind::Accepted, JobEventKind::Completed,
                                          JobEventKind::Released};
  for (std::uint64_t id = 1; id < job_id; ++id) {
    EXPECT_EQ(b.sink->kinds_for(id), bounded) << "job " << id;
    ASSERT_TRUE(b.sink->sequence(id).has_value());
    EXPECT_EQ(b.sink->sequence(id)->progress_count(), 0U) << "job " << id;
    EXPECT_TRUE(b.sink->sequence(id)->released()) << "job " << id;
  }
  EXPECT_TRUE(b.sink->violations().empty());
  EXPECT_TRUE(b.fake->refusals().empty());
  EXPECT_EQ(b.fake->unreleased_jobs(), 0U);
}

TEST(JobBridgeFixture, ProgressFromASpeechJobEndsTheJobInsteadOfReachingTheSink) {
  auto b = bridge_from_registry();
  b.submit(speech_request(1, JobClass::SttDecode));
  b.fake->queue_accepted(1);
  b.fake->queue_progress(1, 1, speech_result(JobClass::SttDecode));
  b.fake->queue_completed(1, speech_result(JobClass::SttDecode));
  b.fake->queue_released(1);
  EXPECT_EQ(b.fake->deliver(), 3U);
  EXPECT_EQ(b.sink->kinds_for(1),
            (std::vector<JobEventKind>{JobEventKind::Accepted, JobEventKind::Failed,
                                       JobEventKind::Released}));
  const auto events = b.sink->events();
  ASSERT_EQ(events.size(), 3U);
  ASSERT_TRUE(events[1].error().has_value());
  EXPECT_EQ(events[1].error()->code, Error::Code::InferenceFailed);
  EXPECT_EQ(events[1].error()->context.value_or(""), "progress_limit_exceeded");
  const auto refusals = b.fake->refusals();
  ASSERT_EQ(refusals.size(), 2U) << "the progress, then the late completion";
  EXPECT_EQ(refusals[1].context.value_or(""), "event_after_terminal");
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, ReleasedBeforeTerminalEndsTheJobThenReleasesIt) {
  auto b = bridge_from_registry();
  b.submit(speech_request(1, JobClass::VadFrames));
  b.fake->queue_accepted(1);
  b.fake->queue_released(1);
  EXPECT_EQ(b.fake->deliver(), 3U);
  EXPECT_EQ(b.sink->kinds_for(1),
            (std::vector<JobEventKind>{JobEventKind::Accepted, JobEventKind::Failed,
                                       JobEventKind::Released}));
  EXPECT_EQ(b.fake->unreleased_jobs(), 0U) << "the backend's release is not lost";
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, InterleavedProgressOfTwoJobsReachesOneSinkInOrder) {
  auto b = bridge_from_registry();
  const BufferRef pcm = pcm_buffer(5, 3200, BufferOwnership::Borrowed);
  const JobRequest ra =
      JobRequest::create(JobIdentity{1, 1, 1}, JobClass::SttDecode,
                         AudioFrames{kJobInputAudioFormat, PcmWindow{pcm, 0, 3200}, 0}, kSttOptions,
                         3)
          .value();
  const JobRequest rb =
      JobRequest::create(JobIdentity{2, 2, 1}, JobClass::SttDecode,
                         AudioFrames{kJobInputAudioFormat, PcmWindow{pcm, 0, 3200}, 0}, kSttOptions,
                         3)
          .value();
  b.submit(ra);
  b.submit(rb);
  b.fake->queue_accepted(1);
  b.fake->queue_accepted(2);
  b.fake->queue_progress(1, 1, tokens(11, 1));
  b.fake->queue_progress(2, 1, tokens(21, 1));
  b.fake->queue_progress(1, 2, tokens(12, 1));
  b.fake->queue_progress(2, 2, tokens(22, 1));
  b.fake->queue_completed(2, tokens(23, 1));
  b.fake->queue_progress(1, 3, tokens(13, 1));
  b.fake->queue_released(2);
  b.fake->queue_completed(1, tokens(14, 1));
  b.fake->queue_released(1);
  EXPECT_EQ(b.fake->deliver(), 11U);
  EXPECT_TRUE(b.fake->refusals().empty());
  EXPECT_TRUE(b.sink->violations().empty());
  const auto events = b.sink->events();
  ASSERT_EQ(events.size(), 11U);
  EXPECT_EQ(events[6].identity(), rb.identity()) << "job 2 completes first";
  EXPECT_EQ(events[6].kind(), JobEventKind::Completed);
  EXPECT_EQ(b.sink->sequence(1)->progress_count(), 3U);
  EXPECT_EQ(b.sink->sequence(2)->progress_count(), 2U);
}

TEST(JobBridgeFixture, CancellingOneJobLeavesTheOtherAdvancing) {
  auto b = bridge_from_registry();
  const BufferRef pcm = pcm_buffer(5, 3200, BufferOwnership::Borrowed);
  for (std::uint64_t id : {1U, 2U}) {
    b.submit(JobRequest::create(JobIdentity{id, id, 1}, JobClass::SttDecode,
                                AudioFrames{kJobInputAudioFormat, PcmWindow{pcm, 0, 3200}, 0},
                                kSttOptions, 4)
                 .value());
    b.fake->queue_accepted(id);
    b.fake->queue_progress(id, 1, tokens(10 * id, 1));
  }
  EXPECT_EQ(b.fake->deliver(), 4U);
  b.cancel(1);
  b.fake->queue_progress(1, 2, tokens(12, 1));  // In flight when the cancel arrived.
  b.fake->queue_cancel_acknowledged(1);
  b.fake->queue_progress(2, 2, tokens(22, 1));
  b.fake->queue_failed(1, Error::make(Error::Code::Cancelled, "cancelled", "cancelled"));
  b.fake->queue_progress(2, 3, tokens(23, 1));
  b.fake->queue_released(1);
  b.fake->queue_completed(2, tokens(24, 1));
  b.fake->queue_released(2);
  EXPECT_EQ(b.fake->deliver(), 8U);
  EXPECT_TRUE(b.fake->refusals().empty());
  EXPECT_TRUE(b.sink->violations().empty());
  EXPECT_EQ(b.sink->kinds_for(1).back(), JobEventKind::Released);
  EXPECT_EQ(b.sink->sequence(2)->progress_count(), 3U);
  EXPECT_EQ(b.fake->cancelled(), (std::vector<std::uint64_t>{1}));
}

TEST(JobBridgeFixture, AcknowledgementWithoutCancelIsAFault) {
  auto b = bridge_from_registry();
  b.submit(speech_request(1, JobClass::TtsSynthesis));
  b.fake->queue_accepted(1);
  b.fake->queue_cancel_acknowledged(1);
  b.fake->queue_released(1);
  EXPECT_EQ(b.fake->deliver(), 3U);
  const auto events = b.sink->events();
  ASSERT_EQ(events.size(), 3U);
  EXPECT_EQ(events[1].kind(), JobEventKind::Failed);
  EXPECT_EQ(events[1].error()->context.value_or(""), "cancel_acknowledged_without_cancel");
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, SessionReleaseCancelsItsJobsAndFollowsTheirRelease) {
  auto b = bridge_from_registry();
  const JobRequest vad = speech_request(1, JobClass::VadFrames, 7);
  const JobRequest other = speech_request(2, JobClass::VadFrames, 8);
  b.submit(vad);
  b.submit(other);
  b.fake->queue_accepted(1);
  EXPECT_EQ(b.fake->deliver(), 1U);

  b.sink->expect_session_release(7);
  ASSERT_TRUE(b.fake->release_session(7, 1).has_value());
  EXPECT_TRUE(b.fake->release_session(7, 1).has_value()) << "idempotent while pending";
  auto late = b.fake->submit(speech_request(3, JobClass::VadFrames, 7));
  ASSERT_FALSE(late.has_value());
  EXPECT_EQ(late.error().code, Error::Code::NotReady);
  EXPECT_EQ(late.error().context.value_or(""), "session_releasing");

  b.fake->queue_cancel_acknowledged(1);
  b.fake->queue_failed(1, Error::make(Error::Code::Cancelled, "session released", "cancelled"));
  EXPECT_EQ(b.fake->deliver(), 2U);
  EXPECT_TRUE(b.sink->session_releases().empty()) << "job 1 is not released yet";
  b.fake->queue_released(1);
  EXPECT_EQ(b.fake->deliver(), 2U) << "released, then the session release";
  EXPECT_EQ(b.sink->session_releases(),
            (std::vector<std::pair<std::uint64_t, std::uint64_t>>{{7, 1}}));
  EXPECT_EQ(b.sink->kinds_for(2), std::vector<JobEventKind>{}) << "session 8 is untouched";
  EXPECT_TRUE(b.sink->violations().empty());
  EXPECT_TRUE(b.fake->refusals().empty());

  ASSERT_TRUE(b.fake->release_session(9, 1).has_value()) << "a session with no jobs";
  EXPECT_EQ(b.fake->deliver(), 1U);
  EXPECT_EQ(b.sink->session_releases().back(), (std::pair<std::uint64_t, std::uint64_t>{9, 1}));
}

TEST(JobBridgeFixture, CallbackMaySubmitAndCancel) {
  auto b = bridge_from_registry();
  b.submit(speech_request(1, JobClass::SttDecode));
  const JobRequest next = speech_request(2, JobClass::SttDecode);
  b.sink->expect_job(next);
  bool submitted_from_callback = false;
  std::size_t job2_events_inside_callback = 99;
  b.sink->set_callback_hook([&](const JobEvent& event) {
    if (event.kind() == JobEventKind::Released && event.identity().job_id == 1) {
      submitted_from_callback = b.fake->submit(next).has_value();
      b.sink->expect_cancel(2);
      EXPECT_TRUE(b.fake->cancel(2).has_value());
      // The backend answers at once; its events still wait for this callback.
      b.fake->queue_cancel_acknowledged(2);
      b.fake->queue_failed(2, Error::make(Error::Code::Cancelled, "cancelled", "cancelled"));
      b.fake->queue_released(2);
      job2_events_inside_callback = b.sink->kinds_for(2).size();
    }
  });
  b.fake->run_bounded_job(1, speech_result(JobClass::SttDecode));
  EXPECT_EQ(b.fake->deliver(), 6U) << "events caused by a callback follow in the same delivery";
  EXPECT_TRUE(submitted_from_callback);
  EXPECT_EQ(job2_events_inside_callback, 0U) << "nothing for job 2 runs inside the callback";
  EXPECT_EQ(b.sink->kinds_for(2),
            (std::vector<JobEventKind>{JobEventKind::CancelAcknowledged, JobEventKind::Failed,
                                       JobEventKind::Released}));
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, ConcurrentDeliveriesTakeTurns) {
  auto b = bridge_from_registry();
  b.submit(speech_request(1, JobClass::SttDecode));
  b.submit(speech_request(2, JobClass::SttDecode));
  b.fake->run_bounded_job(1, speech_result(JobClass::SttDecode));
  b.fake->run_bounded_job(2, speech_result(JobClass::SttDecode));
  // A slow callback widens the window in which a second deliver() would
  // overlap it if the fake did not serialize them.
  b.sink->set_callback_hook(
      [](const JobEvent&) { std::this_thread::sleep_for(std::chrono::milliseconds(5)); });
  std::size_t first = 0;
  std::size_t second = 0;
  std::thread other([&] { second = b.fake->deliver(); });
  first = b.fake->deliver();
  other.join();
  EXPECT_EQ(first + second, 6U);
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, StaleGenerationEventIsDroppedAndTheJobStillCompletes) {
  auto b = bridge_from_registry();
  const JobRequest request = speech_request(1, JobClass::SttDecode);
  b.submit(request);
  JobIdentity stale = request.identity();
  stale.generation += 1;
  b.fake->queue(JobEvent::accepted(stale));
  EXPECT_EQ(b.fake->deliver(), 0U) << "an event naming another generation is dropped";
  ASSERT_EQ(b.fake->refusals().size(), 1U);
  EXPECT_EQ(b.fake->refusals()[0].context.value_or(""), "identity_mismatch");
  b.fake->run_bounded_job(1, speech_result(JobClass::SttDecode));
  EXPECT_EQ(b.fake->deliver(), 3U);
  EXPECT_EQ(b.sink->kinds_for(1),
            (std::vector<JobEventKind>{JobEventKind::Accepted, JobEventKind::Completed,
                                       JobEventKind::Released}));
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, ReleasingOneSessionDoesNotCancelAnothersJobs) {
  auto b = bridge_from_registry();
  b.submit(speech_request(1, JobClass::SttDecode, 7));
  b.submit(speech_request(2, JobClass::SttDecode, 8));
  b.sink->expect_session_release(7);
  ASSERT_TRUE(b.fake->release_session(7, 1).has_value());
  b.fake->queue_accepted(2);
  b.fake->queue_cancel_acknowledged(2);
  EXPECT_EQ(b.fake->deliver(), 2U);
  EXPECT_EQ(b.sink->kinds_for(2),
            (std::vector<JobEventKind>{JobEventKind::Accepted, JobEventKind::Failed}))
      << "session 8 was not cancelled, so its acknowledgement is a fault";
  ASSERT_EQ(b.fake->refusals().size(), 1U);
  EXPECT_EQ(b.fake->refusals()[0].context.value_or(""), "cancel_acknowledged_without_cancel");
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, RepeatedSessionReleaseIsAcknowledgedOnce) {
  auto b = bridge_from_registry();
  b.sink->expect_session_release(9);
  ASSERT_TRUE(b.fake->release_session(9, 1).has_value());
  EXPECT_EQ(b.fake->deliver(), 1U);
  EXPECT_TRUE(b.fake->release_session(9, 1).has_value()) << "a repeat after the acknowledgement";
  EXPECT_EQ(b.fake->deliver(), 0U) << "and no second acknowledgement";
  EXPECT_EQ(b.sink->session_releases(),
            (std::vector<std::pair<std::uint64_t, std::uint64_t>>{{9, 1}}));
  auto late = b.fake->submit(speech_request(1, JobClass::SttDecode, 9));
  ASSERT_FALSE(late.has_value());
  EXPECT_EQ(late.error().context.value_or(""), "session_releasing");
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, SessionReleaseRepeatedDuringItsAcknowledgementIsNotAcknowledgedAgain) {
  // The fake drops its lock to run on_session_released; a repeat made in that
  // window must not re-arm the release.
  auto b = bridge_from_registry();
  int repeats = 0;
  b.sink->set_session_release_hook([&](std::uint64_t key, std::uint64_t generation) {
    if (repeats++ == 0) {  // Once, so a re-armed release cannot loop.
      EXPECT_TRUE(b.fake->release_session(key, generation).has_value());
    }
  });
  ASSERT_TRUE(b.fake->release_session(9, 1).has_value());
  EXPECT_EQ(b.fake->deliver(), 1U);
  EXPECT_EQ(repeats, 1);
  EXPECT_EQ(b.fake->deliver(), 0U);
  EXPECT_EQ(b.sink->session_releases().size(), 1U);
  EXPECT_TRUE(b.sink->violations().empty());
}

TEST(JobBridgeFixture, RecordingSinkFlagsADuplicateSessionRelease) {
  // Control case: a second acknowledgement of one session is reported.
  auto sink = std::make_shared<RecordingJobEventSink>();
  sink->on_session_released(7, 1);
  EXPECT_TRUE(sink->violations().empty());
  sink->on_session_released(7, 1);
  EXPECT_EQ(sink->violations(), std::vector<std::string>{"duplicate_session_released 7"});
}

TEST(JobBridgeFixture, RecordingSinkFlagsEachViolation) {
  // Control cases for the sink's other guards, so the empty-violation checks
  // above can fail.
  auto sink = std::make_shared<RecordingJobEventSink>();
  const JobRequest request = speech_request(1, JobClass::SttDecode, 7);
  sink->expect_job(request);
  const JobIdentity identity = request.identity();
  sink->on_completed(JobEvent::accepted(identity).value());
  sink->on_accepted(JobEvent::accepted(JobIdentity{5, 5, 1}).value());
  sink->on_released(JobEvent::released(identity).value());
  sink->on_session_released(7, 1);
  EXPECT_EQ(
      sink->violations(),
      (std::vector<std::string>{"callback_kind_mismatch 1", "unexpected_job 5",
                                "released_before_terminal 1", "session_released_before_job 1"}));
}

TEST(JobBridgeFixture, RecordingSinkFlagsOverlappingCallbacks) {
  // Control case for the sink's own guard: a callback that starts while
  // another runs must be reported, or the fixture tests above prove nothing
  // about serialization.
  auto sink = std::make_shared<RecordingJobEventSink>();
  const JobRequest request = speech_request(1, JobClass::SttDecode);
  sink->expect_job(request);
  bool nested = false;
  sink->set_callback_hook([&](const JobEvent& event) {
    if (!nested) {
      nested = true;
      sink->on_accepted(event);
    }
  });
  sink->on_accepted(JobEvent::accepted(request.identity()).value());
  const auto violations = sink->violations();
  EXPECT_NE(std::find(violations.begin(), violations.end(), "overlapping_callbacks"),
            violations.end());
}

TEST(JobBridgeFixture, RefusalsAreTypedAndAFailedSubmitDeliversNothing) {
  auto fake =
      std::make_shared<FakeBoundedJobBridge>(std::vector<JobClass>{JobClass::TtsSynthesis}, 1);
  const JobRequest stt = speech_request(1, JobClass::SttDecode);
  const JobRequest tts = speech_request(2, JobClass::TtsSynthesis);
  auto expect_refusal = [](const Result<void>& r, Error::Code code, const char* reason) {
    ASSERT_FALSE(r.has_value()) << reason;
    EXPECT_EQ(r.error().code, code) << reason;
    EXPECT_EQ(r.error().context.value_or(""), reason);
  };

  expect_refusal(fake->submit(tts), Error::Code::NotReady, "event_sink_missing");
  expect_refusal(fake->release_session(1, 1), Error::Code::NotReady, "event_sink_missing");
  expect_refusal(fake->set_event_sink(nullptr), Error::Code::ConfigInvalid, "event_sink_null");
  auto sink = std::make_shared<RecordingJobEventSink>();
  ASSERT_TRUE(fake->set_event_sink(sink).has_value());
  expect_refusal(fake->set_event_sink(sink), Error::Code::ConfigInvalid, "event_sink_already_set");
  expect_refusal(fake->submit(stt), Error::Code::Unsupported, "job_class_unsupported");

  fake->fail_next_submit(
      Error::make(Error::Code::Unsupported, "voice not qualified", "job_not_permitted"));
  expect_refusal(fake->submit(tts), Error::Code::Unsupported, "job_not_permitted");
  EXPECT_EQ(fake->deliver(), 0U);
  EXPECT_TRUE(sink->events().empty()) << "a failed submit delivers nothing";

  ASSERT_TRUE(fake->submit(tts).has_value()) << "the job id is free again";
  expect_refusal(fake->submit(tts), Error::Code::ConfigInvalid, "duplicate_job_id");
  expect_refusal(fake->submit(speech_request(3, JobClass::TtsSynthesis)),
                 Error::Code::ResourceExhausted, "job_capacity_exhausted");
  expect_refusal(fake->cancel(99), Error::Code::NotReady, "unknown_job");
  EXPECT_TRUE(fake->cancel(2).has_value());
  EXPECT_TRUE(fake->cancel(2).has_value()) << "cancel is idempotent";
  expect_refusal(fake->release_session(0, 1), Error::Code::ConfigInvalid, "session_key_zero");
  expect_refusal(fake->release_session(1, 0), Error::Code::ConfigInvalid, "generation_zero");

  fake->set_health(unexpected(Error::make(Error::Code::Timeout, "no answer", "health_timeout")));
  expect_refusal(fake->health(), Error::Code::Timeout, "health_timeout");
}

}  // namespace
}  // namespace tensorplate
