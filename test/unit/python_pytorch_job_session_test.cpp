// SPDX-License-Identifier: Apache-2.0
//
// The Python/PyTorch adapter's session with speech jobs enabled, against a
// scripted sidecar that keeps the in-tree runner's rules: the recorded
// negotiation through the session's load, and what the session does with
// its job channel at prime, at unload, when it is destroyed and when the
// connection fails.

#include <gtest/gtest.h>

#include "adapters/python_pytorch/python_pytorch_session.hpp"

#if !defined(TP_ENABLE_PYTHON_PYTORCH_SIDECAR) || !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobSession, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <atomic>
#include <chrono>
#include <cstdint>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

#include "adapters/python_pytorch/job_channel.hpp"
#include "job_session_harness.hpp"
#include "scripted_sidecar.hpp"
#include "speech_job_golden.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using testing::JobSessionHarness;
using testing::ScriptedSidecar;
using testing::speech_golden::golden;
using Json = nlohmann::json;
using Kinds = JobSessionHarness::Kinds;
using enum JobEventKind;

// A session built to run speech jobs. No bound a test could run into: the
// test of a timeout sets its own.
PythonPytorchConfig with_jobs() {
  PythonPytorchConfig config;
  config.speech_jobs = true;
  config.startup_timeout = JobSessionHarness::kPatience;
  config.health_timeout = JobSessionHarness::kPatience;
  return config;
}

// A session not built for jobs, with the same patience.
PythonPytorchConfig without_jobs() {
  PythonPytorchConfig config = with_jobs();
  config.speech_jobs = false;
  return config;
}

// The model the recorded negotiation loads.
ModelSpec recorded_spec() {
  const Json spec = golden("negotiation").at(1).at("model_spec");
  return ModelSpec::create(spec.at("model_id").get<std::string>(), ModelClass::Speech,
                           spec.at("artifact_path").get<std::string>(),
                           spec.at("backend_hint").get<std::string>())
      .value();
}

Json without_id(Json header) {
  header.erase("message_id");
  return header;
}

Json transcript() {
  return Json{{"type", "transcript"}, {"text", "hello"}, {"tokens", {1}}, {"words", Json::array()}};
}

void expect_refused(const Result<void>& result, Error::Code code, std::string_view reason) {
  ASSERT_FALSE(result.has_value()) << reason;
  EXPECT_EQ(result.error().code, code) << reason;
  EXPECT_EQ(result.error().context.value_or(""), reason);
}

// The error of the job's `failed` event, when it has one.
std::optional<Error> failure_of(const JobSessionHarness& h, std::uint64_t job_id) {
  for (const JobEvent& event : h.sink->events()) {
    if (event.identity().job_id == job_id && event.kind() == Failed) {
      return event.error();
    }
  }
  return std::nullopt;
}

// -- The recorded negotiation. ---------------------------------------------------

TEST(JobSessionNegotiation, ALoadWithJobsWritesAndReadsTheRecordedNegotiation) {
  const std::vector<Json> recorded = golden("negotiation");
  ScriptedSidecar peer(ScriptedSidecar::Options{
      .job_classes = recorded.at(2).at("job_classes").get<std::vector<std::string>>()});
  JobSessionHarness h(with_jobs(), peer.launcher());
  ASSERT_TRUE(h.session->load(recorded_spec()).has_value());

  // The session's own `load_model` carries one field the recording does not.
  Json recorded_load = without_id(recorded.at(1));
  recorded_load["model_spec"]["precision_hint"] = "auto";
  const auto sent = peer.sent("load_model", 1);
  ASSERT_EQ(sent.size(), 1U);
  EXPECT_EQ(without_id(sent.at(0)), recorded_load);
  const auto written = peer.written();
  ASSERT_EQ(written.size(), 2U);
  EXPECT_EQ(written.at(0), recorded.at(0));
  EXPECT_EQ(without_id(written.at(1)), without_id(recorded.at(2)));

  // Of the two classes the load declared, the channel takes the one a lane runs.
  ASSERT_TRUE(h.session->prime().has_value());
  expect_refused(h.channel->submit(JobSessionHarness::tts(1, 1)), Error::Code::Unsupported,
                 "job_class_unsupported");
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  EXPECT_TRUE(h.wait_kinds(2, {Accepted}));
}

TEST(JobSessionNegotiation, TheRecordedRefusalFailsTheLoadWithTheSidecarsError) {
  const std::vector<Json> recorded = golden("negotiation_refused");
  const Json error = recorded.at(2).at("error");
  ScriptedSidecar peer(
      ScriptedSidecar::Options{.job_classes = {},
                               .load_refusal_message = error.at("message").get<std::string>(),
                               .load_refusal_context = error.at("context").get<std::string>()});
  JobSessionHarness h(with_jobs(), peer.launcher());
  const auto loaded = h.session->load(recorded_spec());
  ASSERT_FALSE(loaded.has_value());
  EXPECT_EQ(loaded.error(),
            Error::make(Error::Code::Unsupported, error.at("message").get<std::string>(),
                        error.at("context").get<std::string>()));
  const auto written = peer.written();
  ASSERT_EQ(written.size(), 2U);
  EXPECT_EQ(without_id(written.at(1)), without_id(recorded.at(2)));
  EXPECT_TRUE(peer.reaped());
  expect_refused(h.channel->submit(JobSessionHarness::tts(1, 1)), Error::Code::NotReady,
                 "backend_not_ready");
}

TEST(JobSessionNegotiation, ASessionNotBuiltForJobsEnablesNothingTheSidecarLists) {
  ScriptedSidecar peer;
  JobSessionHarness h(without_jobs(), peer.launcher());
  ASSERT_TRUE(h.session->load(recorded_spec()).has_value());
  const auto sent = peer.sent("load_model", 1);
  ASSERT_EQ(sent.size(), 1U);
  EXPECT_FALSE(sent.at(0).contains("capabilities"));
  ASSERT_TRUE(h.session->prime().has_value());
  expect_refused(h.channel->submit(JobSessionHarness::tts(1, 1)), Error::Code::NotReady,
                 "backend_not_ready");
  EXPECT_TRUE(h.session->unload().has_value());
  EXPECT_TRUE(peer.reaped());
}

TEST(JobSessionNegotiation, ASessionNotBuiltForJobsLoadsOnASidecarThatListsNothing) {
  ScriptedSidecar peer(ScriptedSidecar::Options{.capabilities = {}});
  JobSessionHarness h(without_jobs(), peer.launcher());
  ASSERT_TRUE(h.session->load(recorded_spec()).has_value());
  EXPECT_TRUE(h.session->prime().has_value());
}

TEST(JobSessionNegotiation, ASidecarThatDoesNotListTheCapabilityIsNotAskedToLoad) {
  ScriptedSidecar peer(ScriptedSidecar::Options{.capabilities = {}});
  JobSessionHarness h(with_jobs(), peer.launcher());
  expect_refused(h.session->load(recorded_spec()), Error::Code::Unsupported,
                 "speech_jobs_not_offered");
  EXPECT_TRUE(peer.reaped());
  EXPECT_TRUE(peer.sent("load_model", 0).empty());
}

TEST(JobSessionNegotiation, ACapabilityListThatIsNotOneFailsTheLoad) {
  ScriptedSidecar peer(ScriptedSidecar::Options{.ready_overrides = Json{{"capabilities", 7}}});
  JobSessionHarness h(with_jobs(), peer.launcher());
  expect_refused(h.session->load(recorded_spec()), Error::Code::InferenceFailed,
                 "capabilities_malformed");
  EXPECT_TRUE(peer.reaped());
  EXPECT_TRUE(peer.sent("load_model", 0).empty());
}

TEST(JobSessionNegotiation, ALoadAnswerWithoutReadableJobClassesFailsTheLoad) {
  ScriptedSidecar peer(
      ScriptedSidecar::Options{.load_response_overrides = Json{{"job_classes", 7}}});
  JobSessionHarness h(with_jobs(), peer.launcher());
  expect_refused(h.session->load(recorded_spec()), Error::Code::InferenceFailed,
                 "job_classes_malformed");
  EXPECT_TRUE(peer.reaped());
}

TEST(JobSessionNegotiation, ALoadThatDeclaresNoClassALaneRunsFailsAndTheSidecarIsReaped) {
  // The runner refuses such a load itself; a sidecar that did not is not served.
  ScriptedSidecar peer(ScriptedSidecar::Options{.job_classes = {"vad_frames"}});
  JobSessionHarness h(with_jobs(), peer.launcher());
  expect_refused(h.session->load(recorded_spec()), Error::Code::Unsupported,
                 "job_class_unsupported");
  EXPECT_TRUE(peer.reaped());
}

// -- The session and its channel. ------------------------------------------------

class JobSessionTest : public ::testing::Test {
 protected:
  void loaded() { ASSERT_TRUE(h.session->load(recorded_spec()).has_value()); }

  void up() {
    ASSERT_NO_FATAL_FAILURE(loaded());
    const auto primed = h.session->prime();
    ASSERT_TRUE(primed.has_value()) << primed.error().message;
  }

  // Job 1, taken by the sidecar's backend: unary requests wait behind it.
  void hold_a_job() {
    ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
    ASSERT_EQ(peer.take_job(), 1U);
    ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  }

  // Notes `released_before_the_reap` when a job is released while the
  // sidecar "process" still runs.
  void watch_releases() {
    h.on_event = [this](const JobEvent& event) {
      if (event.kind() == Released && !peer.reaped()) {
        released_before_the_reap = true;
      }
    };
  }

  // Members, declared before the harness: a callback can still run when the
  // harness destroys the session, after an assertion has ended the test's
  // body.
  std::atomic<bool> released_before_the_reap{false};
  std::atomic<bool> failed_at_the_reap{false};
  ScriptedSidecar peer;
  JobSessionHarness h{with_jobs(), peer.launcher()};
};

TEST_F(JobSessionTest, JobsAreAdmittedOnlyOncePrimesHealthCheckHasPassed) {
  ASSERT_NO_FATAL_FAILURE(loaded());
  expect_refused(h.channel->submit(JobSessionHarness::tts(1, 1)), Error::Code::NotReady,
                 "backend_not_ready");
  // The sidecar answers prime and then reads nothing, so the health check
  // waits unread. Nothing between the thread's start and its join ends the
  // test.
  peer.stop_reading_after("prime");
  Result<void> primed = unexpected(Error::Code::Internal, "not run");
  std::thread prime([&] { primed = h.session->prime(); });
  EXPECT_TRUE(peer.parked());
  EXPECT_TRUE(h.wait([&] { return peer.unread_bytes() > 0; })) << "the health check was sent";
  const auto early = h.channel->submit(JobSessionHarness::tts(1, 1));
  EXPECT_TRUE(!early.has_value() && early.error().context.value_or("") == "backend_not_ready");
  peer.resume_reading();
  prime.join();
  ASSERT_TRUE(primed.has_value()) << primed.error().message;
  ASSERT_TRUE(h.submit(JobSessionHarness::tts(1, 1)).has_value());
  EXPECT_TRUE(h.wait_kinds(1, {Accepted}));
}

TEST_F(JobSessionTest, AHealthCheckThatReportsNotReadyAdmitsNoJob) {
  ASSERT_NO_FATAL_FAILURE(loaded());
  peer.report_not_ready();
  expect_refused(h.session->prime(), Error::Code::NotReady, "");
  // An answer, not a failed exchange: the connection stays.
  EXPECT_FALSE(peer.reaped());
  expect_refused(h.channel->submit(JobSessionHarness::tts(1, 1)), Error::Code::NotReady,
                 "backend_not_ready");
}

TEST_F(JobSessionTest, UnloadClosesAdmissionThenExchangesThroughTheChannelThenStops) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_a_job());
  Result<void> unloaded = unexpected(Error::Code::Internal, "not run");
  std::thread unload([&] { unloaded = h.session->unload(); });
  // The sidecar has the unload and answers it once the job has ended.
  EXPECT_EQ(peer.sent("unload", 1).size(), 1U);
  const auto late = h.channel->submit(JobSessionHarness::tts(2, 1));
  EXPECT_TRUE(!late.has_value() && late.error().context.value_or("") == "backend_unavailable");
  // The unload waits in the channel's one slot, not on the socket.
  const auto second = h.unary("health_check", std::chrono::milliseconds{0});
  EXPECT_TRUE(!second.has_value() && second.error().code == Error::Code::Internal);
  EXPECT_FALSE(peer.reaped());
  peer.complete(1, transcript());
  unload.join();
  ASSERT_TRUE(unloaded.has_value()) << unloaded.error().message;
  EXPECT_TRUE(peer.reaped());
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
  expect_refused(h.channel->submit(JobSessionHarness::tts(3, 1)), Error::Code::Unavailable,
                 "backend_unavailable");
}

TEST_F(JobSessionTest, AnUnloadAnsweredWithAJobStillOpenFailsThatJobAsUnloaded) {
  watch_releases();
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_a_job());
  Result<void> unloaded = unexpected(Error::Code::Internal, "not run");
  std::thread unload([&] { unloaded = h.session->unload(); });
  const auto sent = peer.sent("unload", 1);
  EXPECT_EQ(sent.size(), 1U);
  // No runner answers an unload ahead of the job it is running.
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", sent.empty() ? Json("") : sent.at(0).at("message_id")},
                  {"kind", "unload_response"},
                  {"status", "ok"}});
  unload.join();
  ASSERT_TRUE(unloaded.has_value()) << unloaded.error().message;
  EXPECT_FALSE(released_before_the_reap);
  ASSERT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kSessionUnloaded);
}

TEST_F(JobSessionTest, DestructionFailsAnOpenJobAndReleasesItOnlyAfterTheReap) {
  // When the process is ended the channel's reader has left the socket: it
  // failed the job on its way out.
  peer.on_terminate = [this] {
    failed_at_the_reap = h.sink->kinds_for(1) == Kinds{Accepted, Failed};
  };
  watch_releases();
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_a_job());
  h.session.reset();
  EXPECT_TRUE(peer.reaped());
  EXPECT_TRUE(failed_at_the_reap);
  EXPECT_FALSE(released_before_the_reap);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1), Error::make(Error::Code::Unavailable, "the sidecar connection ended",
                                          std::string(kSessionUnloaded)));
  expect_refused(h.channel->submit(JobSessionHarness::tts(2, 1)), Error::Code::Unavailable,
                 "backend_unavailable");
}

TEST_F(JobSessionTest, AProcessThatIsGoneFailsTheNextExchangeBeforeItIsSent) {
  watch_releases();
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_a_job());
  peer.die();
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kBackendReset);
  const auto unloaded = h.session->unload();
  ASSERT_FALSE(unloaded.has_value());
  // Only the check ahead of the send answers with this text.
  EXPECT_EQ(unloaded.error().message, "sidecar process exited");
  EXPECT_TRUE(peer.reaped());
  EXPECT_FALSE(released_before_the_reap);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
}

TEST_F(JobSessionTest, WhenTheConnectionEndsTheNextLifecycleCallStopsTheChannelAndReaps) {
  watch_releases();
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_a_job());
  // The socket is shut down and the "process" lives on.
  peer.close();
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kBackendReset);
  // Nothing tells the session: it learns of the end at its next exchange.
  EXPECT_FALSE(peer.reaped());
  const auto unloaded = h.session->unload();
  ASSERT_FALSE(unloaded.has_value());
  EXPECT_EQ(unloaded.error().code, Error::Code::LoadFailed) << "the connection's own error";
  EXPECT_TRUE(peer.reaped());
  EXPECT_FALSE(released_before_the_reap);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
}

TEST_F(JobSessionTest, AnAnswerToAnotherRequestEndsTheConnection) {
  watch_releases();
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_a_job());
  Result<void> unloaded = unexpected(Error::Code::Internal, "not run");
  std::thread unload([&] { unloaded = h.session->unload(); });
  EXPECT_EQ(peer.sent("unload", 1).size(), 1U);
  // Of the kind the session waits for, but not its request's.
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", "tp-msg-another"},
                  {"kind", "unload_response"},
                  {"status", "ok"}});
  unload.join();
  ASSERT_FALSE(unloaded.has_value());
  EXPECT_EQ(unloaded.error().message, "sidecar response answers another request");
  EXPECT_TRUE(peer.reaped());
  EXPECT_FALSE(released_before_the_reap);
  ASSERT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kBackendReset);
}

TEST_F(JobSessionTest, ATensorRequestIsRefusedAndNothingIsSent) {
  ASSERT_NO_FATAL_FAILURE(up());
  const BufferRef buffer = h.manager->allocate(4).value();
  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{"x", buffer, TensorView::create(DType::Float32, {1, 1}).value()});
  const auto request = InferRequest::create("r", "/infer", std::move(inputs)).value();
  const auto result = h.session->infer(request);
  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result.value().is_failure());
  EXPECT_EQ(result.value().error().code, Error::Code::Unsupported);
  EXPECT_EQ(result.value().error().context.value_or(""), "tensor_inference_unsupported");
  // Health is answered in arrival order: an `infer` sent before it was read.
  ASSERT_TRUE(h.unary("health_check").has_value());
  EXPECT_TRUE(peer.sent("infer", 0).empty());
}

TEST(JobSessionFailure, AnExchangeThatTimesOutStopsTheChannelAndReapsTheSidecar) {
  ScriptedSidecar peer;
  PythonPytorchConfig config = with_jobs();
  // The health check below is never read, so any bound ends the same way.
  config.health_timeout = std::chrono::milliseconds{50};
  JobSessionHarness h(config, peer.launcher());
  ASSERT_TRUE(h.session->load(recorded_spec()).has_value());
  peer.stop_reading_after("prime");
  const auto primed = h.session->prime();
  ASSERT_FALSE(primed.has_value());
  EXPECT_EQ(primed.error().code, Error::Code::Timeout);
  // A late answer has no connection to arrive on.
  EXPECT_TRUE(peer.reaped());
  expect_refused(h.channel->submit(JobSessionHarness::tts(1, 1)), Error::Code::Unavailable,
                 "backend_unavailable");
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
