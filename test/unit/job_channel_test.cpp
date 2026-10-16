// SPDX-License-Identifier: Apache-2.0
//
// The adapter's job channel against a scripted sidecar that keeps the in-tree
// runner's rules: the recorded golden conversations, the routing of every
// kind of frame, the bounds, and what each way of ending a connection does to
// the jobs that are open.

#include "adapters/python_pytorch/job_channel.hpp"

#include <gtest/gtest.h>

#if !defined(TP_ENABLE_PYTHON_PYTORCH_SIDECAR) || !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobChannel, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <future>
#include <limits>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <set>
#include <string>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"
#include "tensorplate/ipc/unix_socket.hpp"
#include "tensorplate/serving/session.hpp"

#include "adapters/python_pytorch/job_wire.hpp"
#include "job_channel_harness.hpp"
#include "scripted_sidecar.hpp"
#include "speech_job_golden.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using testing::failure_of;
using testing::JobChannelHarness;
using testing::ParkedCallback;
using testing::ScriptedSidecar;
using testing::speech_golden::golden;
using testing::speech_golden::golden_files;
using testing::speech_golden::sent_by_adapter;
using Json = nlohmann::json;
using Kinds = JobChannelHarness::Kinds;
using enum JobEventKind;

constexpr std::size_t kLargestWindow = kMaxAudioFramesBytes;
constexpr std::size_t kLargestChunk = 1'440'000;

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

// A job message as a sidecar writes it; the jobs of these tests have
// generation 1.
Json job_message(const std::string& kind, std::uint64_t job_id, std::uint64_t session_key) {
  return Json{{"schema_version", "0.1"}, {"message_id", "s90"},        {"kind", kind},
              {"job_id", job_id},        {"session_key", session_key}, {"generation", 1}};
}

// An exchange that waits on a thread of its own, joined before it goes.
class WaitingExchange {
 public:
  WaitingExchange(JobChannelHarness& harness, std::string kind)
      : thread_([this, &harness, kind = std::move(kind)] { outcome_ = harness.unary(kind); }) {}
  ~WaitingExchange() {
    if (thread_.joinable()) {
      thread_.join();
    }
  }

  WaitingExchange(const WaitingExchange&) = delete;
  WaitingExchange& operator=(const WaitingExchange&) = delete;
  WaitingExchange(WaitingExchange&&) = delete;
  WaitingExchange& operator=(WaitingExchange&&) = delete;

  // Waits for the exchange to end.
  [[nodiscard]] const Result<Json>& outcome() {
    if (thread_.joinable()) {
      thread_.join();
    }
    return outcome_;
  }

 private:
  Result<Json> outcome_ = unexpected(Error::Code::Internal, "the exchange did not end");
  std::thread thread_;
};

// A `health_check_response` as a sidecar with a loaded backend writes it.
Json health_report(const std::string& message_id) {
  return Json{{"schema_version", "0.1"},
              {"message_id", message_id},
              {"kind", "health_check_response"},
              {"status", "ok"},
              {"health", {{"ready", true}}}};
}

// A deadline that has passed, and one a test that works never reaches.
JobChannel::TimePoint passed() {
  return JobChannel::TimePoint::clock::now();
}
JobChannel::TimePoint patient() {
  return passed() + JobChannelHarness::kPatience;
}

// A health call that waits on a thread of its own, joined before it goes.
class WaitingHealth {
 public:
  explicit WaitingHealth(JobChannel& channel, JobChannel::TimePoint deadline = patient())
      : thread_([this, &channel, deadline] { outcome_ = channel.probe_health(deadline); }) {}
  ~WaitingHealth() {
    if (thread_.joinable()) {
      thread_.join();
    }
  }

  WaitingHealth(const WaitingHealth&) = delete;
  WaitingHealth& operator=(const WaitingHealth&) = delete;
  WaitingHealth(WaitingHealth&&) = delete;
  WaitingHealth& operator=(WaitingHealth&&) = delete;

  // Waits for the call to end.
  [[nodiscard]] const Result<void>& outcome() {
    if (thread_.joinable()) {
      thread_.join();
    }
    return outcome_;
  }

 private:
  Result<void> outcome_ = unexpected(Error::Code::Internal, "the call did not end");
  std::thread thread_;
};

class JobChannelTest : public ::testing::Test {
 protected:
  // The sidecar is loaded with jobs enabled, primed, and the channel admits.
  void up() {
    const auto loaded = h.connect(peer.launcher());
    ASSERT_TRUE(loaded.has_value()) << loaded.error().message;
    ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
    ASSERT_TRUE(h.unary("prime").has_value());
    h.channel.admit_jobs();
  }

  // Everything either side sent before this call has been acted on: health is
  // answered in arrival order and both directions are FIFO.
  void settle() {
    const auto health = h.unary("health_check");
    ASSERT_TRUE(health.has_value()) << health.error().message;
    ASSERT_EQ(health.value().value("kind", ""), "health_check_response");
  }

  // Leaves the writer inside the submit of `job_id`, a frame larger than the
  // socket holds, with the sidecar reading nothing: whatever is queued from
  // here on stays queued until the sidecar reads again.
  void hold_the_writer(std::uint64_t job_id, std::uint64_t session_key) {
    peer.stop_reading();
    ASSERT_TRUE(h.submit(h.stt(job_id, session_key, kLargestWindow)).has_value());
    ASSERT_TRUE(h.wait([&] { return peer.unread_bytes() >= 65'536; }));
    ASSERT_EQ(h.channel.queued_frames(), 0U) << "the frame the writer took is not queued";
  }

  [[nodiscard]] bool released(std::uint64_t job_id) {
    return h.wait([&] {
      const Kinds kinds = h.sink->kinds_for(job_id);
      return !kinds.empty() && kinds.back() == Released;
    });
  }

  // What a sink callback notes. A member, declared before the harness: a
  // callback can still run in the harness's stop after an assertion has
  // ended the test's body.
  std::atomic<bool> noted{false};
  ScriptedSidecar peer;
  // No write bound a test could run into: the tests of the bound set their own.
  JobChannelHarness h{std::chrono::hours{1}};
};

// -- The golden conversations. ---------------------------------------------------

// The event the codec reads from a recorded sidecar line.
SidecarJobMessage decoded(const Json& line) {
  const std::size_t payload =
      line.contains("result") ? line.at("result").value("payload_length", std::size_t{0}) : 0;
  return decode_sidecar_message(
      ipc::SidecarFrame{line.dump(), std::vector<std::byte>(payload, std::byte{0})},
      [&](std::span<const std::byte> pcm) {
        return BufferRef::create(7, pcm.size(), BufferOwnership::Borrowed);
      });
}

Json without_id(Json header) {
  header.erase("message_id");
  return header;
}

JobRequest vad_request(JobChannelHarness& h, std::uint64_t job_id, std::uint64_t session_key,
                       const Json& input) {
  const auto bytes = input.at("payload_length").get<std::size_t>();
  const BufferRef buffer = h.manager->allocate(bytes).value();
  return JobRequest::create(JobIdentity{job_id, session_key, 1}, JobClass::VadFrames,
                            VadFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, bytes},
                                      input.at("frame_samples").get<std::uint32_t>(),
                                      input.at("frame_count").get<std::uint32_t>(),
                                      input.at("utterance_id").get<std::uint64_t>()})
      .value();
}

TEST(JobChannelGolden, EveryRecordedJobConversationReplaysThroughTheChannel) {
  std::set<std::string> replayed;
  for (const std::string& file : golden_files()) {
    if (file.starts_with("negotiation")) {
      continue;  // the load exchange is the session's
    }
    SCOPED_TRACE(file);
    ScriptedSidecar peer(
        ScriptedSidecar::Options{.job_classes = {"stt_decode", "tts_synthesis", "vad_frames"},
                                 .declares_any_job_class = true});
    JobChannelHarness h;
    ASSERT_TRUE(h.connect(peer.launcher()).has_value());
    ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
    h.channel.admit_jobs();

    std::vector<Json> from_sidecar;
    std::set<std::uint64_t> ended_by_the_runner;
    std::size_t submits = 0;
    for (const Json& line : golden(file)) {
      const auto kind = line.at("kind").get<std::string>();
      const std::uint64_t job_id = line.value("job_id", std::uint64_t{0});
      const auto session_key = line.at("session_key").get<std::uint64_t>();
      if (!sent_by_adapter(line)) {
        from_sidecar.push_back(without_id(line));
        // The runner's own answers follow from its rules; the backend's are played.
        const bool by_rule = kind == "job_accepted" || kind == "session_released" ||
                             ended_by_the_runner.contains(job_id);
        if (!by_rule) {
          Json extra = line;
          for (const char* envelope :
               {"schema_version", "message_id", "kind", "job_id", "session_key", "generation"}) {
            extra.erase(envelope);
          }
          const std::size_t payload =
              line.contains("result") ? line.at("result").value("payload_length", std::size_t{0})
                                      : 0;
          peer.emit(kind, job_id, extra, payload);
        }
        continue;
      }
      if (kind == "job_submit") {
        const Json& input = line.at("input");
        const auto bytes = input.at("payload_length").get<std::size_t>();
        if (file == "refused") {
          peer.refuse_next_submit();
          ended_by_the_runner.insert(job_id);
        }
        const JobRequest request =
            input.at("type") == "text_segment"
                ? JobChannelHarness::tts(job_id, session_key, std::string(bytes, 'a'))
            : input.at("type") == "vad_frames"
                ? vad_request(h, job_id, session_key, input)
                : h.stt(job_id, session_key, bytes, line.at("progress_limit").get<std::uint32_t>());
        ASSERT_TRUE(h.submit(request).has_value());
        const auto sent = peer.sent(kind, ++submits);
        ASSERT_EQ(sent.size(), submits);
        EXPECT_EQ(without_id(sent.back()), without_id(line));
      } else if (kind == "job_cancel") {
        ended_by_the_runner.insert(job_id);
        ASSERT_TRUE(h.cancel(job_id).has_value());
        const auto sent = peer.sent(kind, 1);
        ASSERT_EQ(sent.size(), 1U);
        EXPECT_EQ(without_id(sent.back()), without_id(line));
      } else {
        for (const Json& earlier : golden(file)) {
          if (earlier.value("session_key", std::uint64_t{0}) == session_key) {
            ended_by_the_runner.insert(earlier.value("job_id", std::uint64_t{0}));
          }
        }
        ASSERT_TRUE(h.release(session_key).has_value());
        const auto sent = peer.sent(kind, 1);
        ASSERT_EQ(sent.size(), 1U);
        EXPECT_EQ(without_id(sent.back()), without_id(line));
      }
    }

    // The sink receives what the codec reads from the recorded frames.
    std::vector<JobEvent> expected;
    std::size_t sessions = 0;
    for (const Json& line : from_sidecar) {
      Json header = line;
      header["message_id"] = "s1";
      const auto message = decoded(header);
      if (std::holds_alternative<JobEvent>(message)) {
        expected.push_back(std::get<JobEvent>(message));
      } else {
        ASSERT_TRUE(std::holds_alternative<SessionReleased>(message)) << line;
        ++sessions;
      }
    }
    ASSERT_TRUE(h.wait([&] {
      return h.sink->events().size() == expected.size() &&
             h.sink->session_releases().size() == sessions;
    })) << h.sink->events().size()
        << " of " << expected.size();

    // The scripted sidecar wrote those frames itself, in the recorded order.
    std::vector<Json> written;
    for (const Json& header : peer.written()) {
      const auto kind = header.value("kind", "");
      if (kind.starts_with("job_") || kind == "session_released") {
        written.push_back(without_id(header));
      }
    }
    EXPECT_EQ(written, from_sidecar);

    const std::vector<JobEvent> received = h.sink->events();
    for (std::size_t i = 0; i < expected.size(); ++i) {
      SCOPED_TRACE(i);
      EXPECT_EQ(received[i].kind(), expected[i].kind());
      EXPECT_EQ(received[i].identity(), expected[i].identity());
      EXPECT_EQ(received[i].error(), expected[i].error());
      EXPECT_EQ(received[i].progress_sequence(), expected[i].progress_sequence());
      ASSERT_EQ(received[i].result().has_value(), expected[i].result().has_value());
      if (received[i].result().has_value()) {
        const JobResultPayload& got = received[i].result()->payload();
        const JobResultPayload& want = expected[i].result()->payload();
        if (const auto* chunk = std::get_if<AudioChunkResult>(&got)) {
          EXPECT_EQ(chunk->pcm.byte_size, std::get<AudioChunkResult>(want).pcm.byte_size);
          EXPECT_TRUE(h.manager->release(chunk->pcm.buffer).has_value());
        } else {
          EXPECT_EQ(got, want);
        }
      }
    }
    EXPECT_EQ(h.sink->violations(), std::vector<std::string>{});
    EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
    replayed.insert(file);
  }
  EXPECT_EQ(replayed,
            (std::set<std::string>{"cancel", "interleaved_progress", "refused", "session_release",
                                   "stt_decode", "tts_synthesis", "vad_frames"}));
}

// The request the recorded `vad_frames` submit describes.
JobRequest recorded_vad_request(JobChannelHarness& h, std::uint64_t job_id = 1) {
  return vad_request(h, job_id, 1, golden("vad_frames").at(0).at("input"));
}

TEST_F(JobChannelTest, AClassTheLoadDidNotDeclareIsRefusedAndNothingIsWritten) {
  up();
  expect_refused(h.channel.submit(recorded_vad_request(h)), Error::Code::Unsupported,
                 "job_class_unsupported");
  settle();
  EXPECT_TRUE(peer.sent("job_submit", 0).empty());
}

TEST(JobChannelClasses, OnlyTheClassesTheLoadDeclaredAreTaken) {
  ScriptedSidecar peer(ScriptedSidecar::Options{.job_classes = {"stt_decode", "vad_frames"},
                                                .declares_any_job_class = true});
  JobChannelHarness h;
  const auto loaded = h.connect(peer.launcher());
  ASSERT_TRUE(loaded.has_value());
  ASSERT_EQ(loaded.value().at("job_classes"), Json::array({"stt_decode", "vad_frames"}));
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  h.channel.admit_jobs();
  expect_refused(h.channel.submit(JobChannelHarness::tts(1, 1)), Error::Code::Unsupported,
                 "job_class_unsupported");
  ASSERT_TRUE(h.submit(recorded_vad_request(h, 3)).has_value());
  ASSERT_TRUE(h.wait_kinds(3, {Accepted}));
  peer.emit("job_completed", 3,
            {{"result", {{"type", "vad"}, {"probabilities", {0.1, 0.9, 0.5}}}}});
  ASSERT_TRUE(h.wait_kinds(3, {Accepted, Completed}));
  std::optional<JobResultPayload> vad;
  for (const JobEvent& event : h.sink->events()) {
    if (event.identity().job_id == 3 && event.result().has_value()) {
      vad = event.result()->payload();
    }
  }
  EXPECT_EQ(vad, (JobResultPayload{VadResult{{0.1F, 0.9F, 0.5F}}}));
  EXPECT_TRUE(h.submit(h.stt(2, 1)).has_value());
  EXPECT_TRUE(h.wait_kinds(2, {Accepted}));
}

TEST(JobChannelClasses, ALoadThatDeclaresNoClassTheAdapterKnowsDoesNotStartTheChannel) {
  ScriptedSidecar peer(
      ScriptedSidecar::Options{.job_classes = {"diarize_frames"}, .declares_any_job_class = true});
  JobChannelHarness h;
  const auto loaded = h.connect(peer.launcher());
  ASSERT_FALSE(loaded.has_value());
  EXPECT_EQ(loaded.error().code, Error::Code::Unsupported);
  EXPECT_EQ(loaded.error().context.value_or(""), "job_class_unsupported");
  EXPECT_EQ(h.unary("health_check").error().code, Error::Code::NotReady);
}

// The scripted sidecar's own rule, which the two tests above switch off.
TEST(JobChannelClasses, TheScriptedSidecarDeclaresOnlyLaneClassesAndRefusesALoadWithNone) {
  ScriptedSidecar peer(ScriptedSidecar::Options{.job_classes = {"stt_decode", "vad_frames"}});
  JobChannelHarness h;
  const auto loaded = h.connect(peer.launcher());
  ASSERT_TRUE(loaded.has_value());
  EXPECT_EQ(loaded.value().at("job_classes"), Json::array({"stt_decode"}));

  ScriptedSidecar without(ScriptedSidecar::Options{.job_classes = {"vad_frames"}});
  JobChannelHarness other;
  const auto refused = other.connect(without.launcher());
  ASSERT_FALSE(refused.has_value());
  // The sidecar's refusal of the load, not the channel's of its answer.
  EXPECT_EQ(refused.error().code, Error::Code::LoadFailed);
  EXPECT_EQ(Json::parse(refused.error().message).at("error").at("code"), "unsupported");
}

// -- Admission. ------------------------------------------------------------------

TEST_F(JobChannelTest, SubmitIsRefusedUntilTheChannelAdmitsAndOnceItNoLongerDoes) {
  expect_refused(h.channel.set_event_sink(nullptr), Error::Code::ConfigInvalid, "event_sink_null");
  expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::NotReady, "event_sink_missing");
  expect_refused(h.channel.release_session(1, 1), Error::Code::NotReady, "event_sink_missing");
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  expect_refused(h.channel.set_event_sink(h.sink), Error::Code::ConfigInvalid,
                 "event_sink_already_set");
  expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::NotReady, "backend_not_ready");
  expect_refused(h.channel.release_session(1, 1), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_EQ(h.unary("prime").error().code, Error::Code::NotReady);

  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  EXPECT_EQ(h.channel.start(h.process->socket(), {}).error().code, Error::Code::Internal);
  expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::NotReady, "backend_not_ready");
  ASSERT_TRUE(h.unary("prime").has_value());
  h.channel.admit_jobs();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::ConfigInvalid, "duplicate_job_id");

  h.channel.close_admission();
  expect_refused(h.channel.submit(h.stt(2, 1)), Error::Code::Unavailable, "backend_unavailable");
  h.channel.admit_jobs();
  expect_refused(h.channel.submit(h.stt(2, 1)), Error::Code::Unavailable, "backend_unavailable");

  h.stop(kSessionUnloaded);
  expect_refused(h.channel.submit(h.stt(2, 1)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.channel.release_session(1, 1), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_EQ(h.unary("prime").error().code, Error::Code::NotReady);
}

TEST(JobChannelAdmission, AdmissionClosedBeforeItOpenedStaysClosed) {
  ScriptedSidecar peer;
  JobChannelHarness h;
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  h.channel.close_admission();
  h.channel.admit_jobs();
  expect_refused(h.channel.submit(JobChannelHarness::tts(1, 1)), Error::Code::Unavailable,
                 "backend_unavailable");
}

TEST(JobChannelAdmission, StoppingAChannelThatNeverStartedOnlyReaps) {
  JobChannelHarness h;
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  bool reaped = false;
  h.channel.stop(kSessionUnloaded, [&] { reaped = true; });
  EXPECT_TRUE(reaped);
  h.channel.stop(kSessionUnloaded, {});  // and with nothing to call, does nothing
  expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::NotReady, "backend_not_ready");
}

TEST(JobChannelAdmission, AClosedSocketIsRefusedAtTheStart) {
  JobChannelHarness h;
  ipc::UnixSocket closed;
  expect_refused(h.channel.start(closed, {JobClass::SttDecode}), Error::Code::Internal,
                 "socket_closed");
  // Nothing was started, so this stop has no thread to wait for.
  bool reaped = false;
  h.channel.stop(kSessionUnloaded, [&] { reaped = true; });
  EXPECT_TRUE(reaped);
  EXPECT_EQ(h.unary("prime").error().code, Error::Code::NotReady);
}

TEST(JobChannelAdmission, WithoutABufferManagerEveryJobIsRefused) {
  ScriptedSidecar peer;
  JobChannelHarness h(std::chrono::seconds{5}, /*with_manager=*/false);
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  h.channel.admit_jobs();
  expect_refused(h.channel.submit(JobChannelHarness::tts(1, 1)), Error::Code::NotReady,
                 "backend_not_ready");
}

TEST_F(JobChannelTest, ASubmitWhoseWindowTheBufferManagerDoesNotHoldIsRefused) {
  up();
  const JobRequest request = h.stt(1, 1);
  const PcmWindow& window = std::get<AudioFrames>(request.payload()).pcm;
  ASSERT_TRUE(h.manager->release(window.buffer).has_value());
  expect_refused(h.channel.submit(request), Error::Code::ConfigInvalid, "pcm_buffer_invalid");

  // A handle that claims more bytes than the buffer manager holds for it.
  const BufferRef held = h.manager->allocate(3'200).value();
  const BufferRef larger = BufferRef::create(held.id(), 6'400, BufferOwnership::Borrowed).value();
  for (const PcmWindow& beyond : {PcmWindow{larger, 4'000, 320}, PcmWindow{larger, 0, 6'400}}) {
    const JobRequest outside = JobRequest::create(JobIdentity{1, 1, 1}, JobClass::SttDecode,
                                                  AudioFrames{kJobInputAudioFormat, beyond, 0},
                                                  JobOptions{.language = "en"})
                                   .value();
    expect_refused(h.channel.submit(outside), Error::Code::ConfigInvalid, "pcm_buffer_invalid");
  }

  // An identity the wire cannot carry: the encoder's refusal is the caller's.
  const JobRequest unsendable =
      JobRequest::create(JobIdentity{kMaxWireInteger + 1, 1, 1}, JobClass::TtsSynthesis,
                         TextSegment{"a"},
                         JobOptions{.language = "en-US", .voice = "af_heart", .speed_milli = 1000})
          .value();
  expect_refused(h.channel.submit(unsendable), Error::Code::ConfigInvalid,
                 "wire_integer_out_of_range");
  EXPECT_TRUE(h.submit(h.stt(1, 1)).has_value()) << "the refused jobs left nothing behind";
}

TEST_F(JobChannelTest, ASubmitCarriesTheBytesOfItsWindowAndOfItsText) {
  up();
  // A window inside a buffer whose every byte tells where it is.
  constexpr std::size_t kOffset = 640;
  constexpr std::size_t kSize = 3'200;
  const BufferRef buffer = h.manager->allocate(8'000).value();
  const auto bytes = h.manager->data(buffer).value();
  for (std::size_t i = 0; i < bytes.size(); ++i) {
    bytes[i] = static_cast<std::byte>(i % 251);
  }
  const JobRequest heard =
      JobRequest::create(JobIdentity{1, 1, 1}, JobClass::SttDecode,
                         AudioFrames{kJobInputAudioFormat, PcmWindow{buffer, kOffset, kSize}, 0},
                         JobOptions{.language = "en"})
          .value();
  const std::string text = "hello there.";
  ASSERT_TRUE(h.submit(heard).has_value());
  ASSERT_TRUE(h.submit(JobChannelHarness::tts(2, 1, text)).has_value());
  ASSERT_EQ(peer.sent("job_submit", 2).size(), 2U);

  const std::vector<std::byte> window(bytes.begin() + kOffset, bytes.begin() + kOffset + kSize);
  EXPECT_EQ(peer.payload("job_submit", 0), window);
  const std::vector<std::byte> spoken = peer.payload("job_submit", 1);
  EXPECT_EQ(std::string(reinterpret_cast<const char*>(spoken.data()), spoken.size()),
            text);  // NOLINT
  EXPECT_TRUE(h.wait_kinds(1, {Accepted})) << "its length is the one the header gives";
}

// -- The send queue. -------------------------------------------------------------

TEST_F(JobChannelTest, TheQueueHasAPlaceForEveryFrameOfEveryRecordAndRefusesNoCancelOrRelease) {
  ASSERT_NO_FATAL_FAILURE(up());
  constexpr std::uint64_t kHeld = 100;
  constexpr std::uint64_t kJobsSession = 5'000;
  constexpr std::uint64_t kLastJob = kMaxOpenJobs;
  constexpr std::uint64_t kLastSession = kMaxPendingSessionReleases;
  // On the reader thread, as the held job leaves the table: the last job, its
  // cancel and the last release take the last three places in the queue.
  h.on_event = [this](const JobEvent& event) {
    if (event.kind() != Released || event.identity().job_id != kHeld) {
      return;
    }
    EXPECT_TRUE(h.submit(JobChannelHarness::tts(kLastJob, kJobsSession)).has_value());
    EXPECT_TRUE(h.cancel(kLastJob).has_value());
    EXPECT_TRUE(h.release(kLastSession).has_value());
    noted = true;
  };
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(kHeld, kJobsSession));
  for (std::uint64_t job = 1; job < kLastJob; ++job) {
    ASSERT_TRUE(h.submit(JobChannelHarness::tts(job, kJobsSession)).has_value()) << job;
    ASSERT_TRUE(h.cancel(job).has_value()) << job;
  }
  expect_refused(h.channel.submit(JobChannelHarness::tts(99, kJobsSession)),
                 Error::Code::ResourceExhausted, "job_capacity_exhausted");
  for (std::uint64_t session = 1; session < kLastSession; ++session) {
    ASSERT_TRUE(h.release(session).has_value()) << session;
  }
  // Both requests: the probe's, which stands though its caller gave up, and
  // the exchange's.
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  WaitingExchange health(h, "health_check");
  ASSERT_TRUE(h.wait([&] { return h.channel.queued_frames() == kMaxQueuedFrames - 3; }));

  // No runner lets go of a job it has not read. The channel forgets the job
  // all the same, and the frame the writer began outlives it.
  peer.write(job_message("job_released", kHeld, kJobsSession));
  ASSERT_TRUE(h.wait([&] { return noted.load(); }));
  EXPECT_EQ(h.channel.queued_frames(), kMaxQueuedFrames);

  // Only the two tables refuse, and a repeat asks for no second place.
  expect_refused(h.channel.submit(JobChannelHarness::tts(99, kJobsSession)),
                 Error::Code::ResourceExhausted, "job_capacity_exhausted");
  expect_refused(h.channel.release_session(kLastSession + 1, 1), Error::Code::ResourceExhausted,
                 "session_release_capacity_exhausted");
  EXPECT_TRUE(h.channel.cancel(1).has_value());
  EXPECT_TRUE(h.channel.release_session(1, 1).has_value());
  EXPECT_EQ(h.channel.queued_frames(), kMaxQueuedFrames);

  h.stop(kSessionUnloaded);
  EXPECT_FALSE(health.outcome().has_value());
  EXPECT_EQ(h.channel.queued_frames(), 0U);
  EXPECT_EQ(h.sink->session_releases().size(), kMaxPendingSessionReleases);
  EXPECT_EQ(h.sink->kinds_for(kLastJob), (Kinds{Failed, Released}));
}

TEST(JobChannelBounds, ThePendingReleasesAreTheMostSessionsASessionManagerAdmits) {
  using namespace std::chrono_literals;
  const auto most = static_cast<std::uint32_t>(kMaxPendingSessionReleases);
  const auto at_the_ceiling = serving::SessionLimits::create(60s, 10s, 30s, 60min, most);
  ASSERT_TRUE(at_the_ceiling.has_value());
  EXPECT_EQ(at_the_ceiling->max_sessions(), kMaxPendingSessionReleases);
  EXPECT_FALSE(serving::SessionLimits::create(60s, 10s, 30s, 60min, most + 1).has_value());
}

TEST_F(JobChannelTest, ACancelStillQueuedWhenItsJobIsReleasedLeavesTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(2, 1));
  ASSERT_TRUE(h.cancel(1).has_value());
  EXPECT_EQ(h.channel.queued_frames(), 1U);
  // The job ends by itself before the sidecar has read its cancel.
  peer.complete(1, transcript());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Completed, Released}));
  EXPECT_EQ(h.channel.queued_frames(), 0U);

  peer.resume_reading();
  ASSERT_NO_FATAL_FAILURE(settle());
  EXPECT_TRUE(peer.sent("job_cancel", 0).empty());
}

TEST_F(JobChannelTest, AFrameTheWriterHasBegunIsWrittenWholeThoughItsJobIsReleased) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  ASSERT_TRUE(h.cancel(1).has_value());
  ASSERT_TRUE(h.submit(JobChannelHarness::tts(2, 1)).has_value());
  EXPECT_EQ(h.channel.queued_frames(), 2U);
  // The writer is inside job 1's submit when the job leaves the table.
  peer.write(job_message("job_released", 1, 1));
  ASSERT_TRUE(released(1));
  EXPECT_EQ(h.channel.queued_frames(), 1U) << "its cancel left; the other job's submit stayed";

  peer.resume_reading();
  const auto submits = peer.sent("job_submit", 2);
  ASSERT_EQ(submits.size(), 2U);
  EXPECT_EQ(submits.at(0).value("job_id", std::uint64_t{0}), 1U);
  EXPECT_EQ(peer.payload("job_submit", 0).size(), kLargestWindow);
  EXPECT_EQ(submits.at(1).value("job_id", std::uint64_t{0}), 2U);
  ASSERT_NO_FATAL_FAILURE(settle());
  EXPECT_TRUE(peer.sent("job_cancel", 0).empty());
}

TEST_F(JobChannelTest, ASubmitStillQueuedWhenItsJobIsReleasedLeavesTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  ASSERT_TRUE(h.submit(JobChannelHarness::tts(2, 1)).has_value());
  ASSERT_TRUE(h.cancel(2).has_value());
  EXPECT_EQ(h.channel.queued_frames(), 2U);
  peer.write(job_message("job_released", 2, 1));
  ASSERT_TRUE(released(2));
  EXPECT_EQ(h.channel.queued_frames(), 0U);

  peer.resume_reading();
  ASSERT_NO_FATAL_FAILURE(settle());
  EXPECT_EQ(peer.sent("job_submit", 0).size(), 1U) << "only the job the writer had begun";
  EXPECT_TRUE(peer.sent("job_cancel", 0).empty());
}

TEST_F(JobChannelTest, AReleaseStillQueuedWhenItIsAcknowledgedLeavesTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  ASSERT_TRUE(h.release(7).has_value());
  EXPECT_EQ(h.channel.queued_frames(), 1U);
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", "s90"},
                  {"kind", "session_released"},
                  {"session_key", 7},
                  {"generation", 1}});
  ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == 1; }));
  EXPECT_EQ(h.channel.queued_frames(), 0U);

  peer.resume_reading();
  ASSERT_NO_FATAL_FAILURE(settle());
  EXPECT_TRUE(peer.sent("session_release", 0).empty());
}

TEST_F(JobChannelTest, AnExchangeThatGivesUpTakesItsRequestOutOfTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  // The writer is held, so no answer can come: a deadline that has passed
  // ends each exchange with its request still queued.
  for (int asked = 0; asked < 3; ++asked) {
    ASSERT_EQ(h.unary("prime", std::chrono::milliseconds{0}).error().code, Error::Code::Timeout);
    ASSERT_EQ(h.channel.queued_frames(), 0U) << asked;
  }

  // The sidecar logs a frame after it has answered it, so only what arrived
  // ahead of settle()'s request is certain to be in the log.
  peer.resume_reading();
  ASSERT_NO_FATAL_FAILURE(settle());
  EXPECT_EQ(peer.sent("prime", 0).size(), 1U) << "the one up() sent";
}

TEST_F(JobChannelTest, AnExchangeThatFailsWithTheConnectionTakesItsRequestOutOfTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  WaitingExchange prime(h, "prime");
  ASSERT_TRUE(h.wait([&] { return h.channel.queued_frames() == 1; }));
  peer.write_raw("not a header");
  ASSERT_FALSE(prime.outcome().has_value());
  EXPECT_EQ(prime.outcome().error().code, Error::Code::InferenceFailed);
  EXPECT_EQ(h.channel.queued_frames(), 0U) << "before any stop";
}

TEST_F(JobChannelTest, AnExchangeAnsweredBeforeItsRequestWasBegunTakesItOutOfTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  WaitingExchange prime(h, "prime");
  ASSERT_TRUE(h.wait([&] { return h.channel.queued_frames() == 1; }));
  // No frame is matched to a request: this one answers the exchange that
  // waits, whose request the sidecar has not read.
  peer.write(Json{{"schema_version", "0.1"}, {"message_id", "s90"}, {"kind", "prime_response"}});
  ASSERT_TRUE(prime.outcome().has_value());
  EXPECT_EQ(prime.outcome().value().value("kind", ""), "prime_response");
  EXPECT_EQ(h.channel.queued_frames(), 0U);

  peer.resume_reading();
  ASSERT_NO_FATAL_FAILURE(settle());
  EXPECT_EQ(peer.sent("prime", 0).size(), 1U) << "the one up() sent";
}

TEST_F(JobChannelTest,
       AWaitingExchangeKeepsItsQueuedRequestThroughAStrayRefusalAndASecondExchange) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  WaitingExchange prime(h, "prime");
  ASSERT_TRUE(h.wait([&] { return h.channel.queued_frames() == 1; }));
  // An `error_event` that names another message is not this exchange's answer.
  peer.refuse("tp-msg-unknown", "unsupported", nullptr);
  ASSERT_TRUE(h.wait([&] { return h.channel.counters() == JobChannelCounters{0, 1}; }));
  EXPECT_EQ(h.channel.queued_frames(), 1U);
  // One exchange at a time, and the one refused takes nothing out either.
  EXPECT_EQ(h.unary("health_check").error().code, Error::Code::Internal);
  EXPECT_EQ(h.channel.queued_frames(), 1U);

  peer.resume_reading();
  ASSERT_EQ(peer.take_job(), 1U);
  peer.complete(1, transcript());
  ASSERT_TRUE(prime.outcome().has_value());
  EXPECT_EQ(prime.outcome().value().value("kind", ""), "prime_response");
}

TEST_F(JobChannelTest, AHealthProbeStillQueuedWhenTheConnectionEndsLeavesTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  // No caller is left: what is queued is the probe's request alone, once.
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  ASSERT_EQ(h.channel.queued_frames(), 1U);
  peer.write_raw("not a header");
  ASSERT_TRUE(h.wait_kinds(1, {Failed}));
  EXPECT_EQ(h.channel.queued_frames(), 0U) << "before any stop";
}

TEST_F(JobChannelTest, AHealthProbeAnsweredBeforeItsRequestWasBegunLeavesTheQueue) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  // Ids come from one counter and nothing else draws one here: the probe's
  // is the next.
  const std::string drawn = next_message_id();
  const std::string probes = "tp-msg-" + std::to_string(std::stoull(drawn.substr(7)) + 1);
  WaitingHealth health(h.channel);
  ASSERT_TRUE(h.wait([&] { return h.channel.queued_frames() == 1; }));
  // Frames that name another message leave the request where it is.
  peer.write(health_report("s90"));
  peer.refuse("tp-msg-unknown", "unsupported", nullptr);
  ASSERT_TRUE(h.wait([&] { return h.channel.counters() == JobChannelCounters{0, 2}; }));
  EXPECT_EQ(h.channel.queued_frames(), 1U);
  // No sidecar answers a request it has not read. Its id is what makes a
  // frame the probe's answer all the same.
  peer.write(health_report(probes));
  EXPECT_TRUE(health.outcome().has_value());
  EXPECT_EQ(h.channel.queued_frames(), 0U);

  peer.resume_reading();
  ASSERT_EQ(peer.take_job(), 1U);
  peer.complete(1, transcript());
  ASSERT_TRUE(h.unary("prime").has_value());
  EXPECT_TRUE(peer.sent("health_check", 0).empty());
}

TEST_F(JobChannelTest, AnExchangeThatGivesUpLeavesTheHealthProbesQueuedRequest) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  ASSERT_EQ(h.channel.queued_frames(), 1U);
  ASSERT_EQ(h.unary("prime", std::chrono::milliseconds{0}).error().code, Error::Code::Timeout);
  EXPECT_EQ(h.channel.queued_frames(), 1U) << "the probe's request stays";
}

TEST_F(JobChannelTest, AHealthProbeThatIsAnsweredLeavesTheWaitingExchangesQueuedRequest) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_NO_FATAL_FAILURE(hold_the_writer(1, 1));
  WaitingExchange prime(h, "prime");
  ASSERT_TRUE(h.wait([&] { return h.channel.queued_frames() == 1; }));
  // Ids come from one counter and nothing else draws one here.
  const std::string drawn = next_message_id();
  const std::string probes = "tp-msg-" + std::to_string(std::stoull(drawn.substr(7)) + 1);
  WaitingHealth health(h.channel);
  ASSERT_TRUE(h.wait([&] { return h.channel.queued_frames() == 2; }));
  peer.write(health_report(probes));
  EXPECT_TRUE(health.outcome().has_value());
  EXPECT_EQ(h.channel.queued_frames(), 1U) << "the exchange's request stays";

  peer.resume_reading();
  ASSERT_EQ(peer.take_job(), 1U);
  peer.complete(1, transcript());
  ASSERT_TRUE(prime.outcome().has_value());
  EXPECT_EQ(prime.outcome().value().value("kind", ""), "prime_response");
}

// The number, not the constant: nothing else in the tree states it. With the
// table full the last three entries of submit's list all apply, so their
// order shows: a duplicate id is refused as one, and an id the wire cannot
// carry for capacity.
TEST_F(JobChannelTest, TheSeventeenthUnreleasedJobIsRefused) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.stop_reading();
  for (std::uint64_t job = 1; job <= 16; ++job) {
    ASSERT_TRUE(h.submit(JobChannelHarness::tts(job, 1)).has_value()) << job;
  }
  expect_refused(h.channel.submit(JobChannelHarness::tts(17, 1)), Error::Code::ResourceExhausted,
                 "job_capacity_exhausted");
  expect_refused(h.channel.submit(JobChannelHarness::tts(16, 1)), Error::Code::ConfigInvalid,
                 "duplicate_job_id");
  expect_refused(h.channel.submit(JobChannelHarness::tts(kMaxWireInteger + 1, 1)),
                 Error::Code::ResourceExhausted, "job_capacity_exhausted");
}

// -- Cancel and session release. -------------------------------------------------

TEST_F(JobChannelTest, ACancelIsWrittenOnceAndOnlyWhileTheJobCanStillEnd) {
  up();
  expect_refused(h.channel.cancel(1), Error::Code::NotReady, "unknown_job");
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.cancel(1).has_value());
  ASSERT_TRUE(h.cancel(1).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged}));
  peer.complete(1, transcript());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->code, Error::Code::Cancelled);
  expect_refused(h.channel.cancel(1), Error::Code::NotReady, "unknown_job");

  // A terminal event that overtook the cancel: the job is not cancelled.
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 2U);
  peer.emit("job_completed", 2, {{"result", transcript()}});
  ASSERT_TRUE(h.wait_kinds(2, {Accepted, Completed}));
  EXPECT_TRUE(h.channel.cancel(2).has_value());
  settle();
  EXPECT_EQ(peer.sent("job_cancel", 1).size(), 1U);
}

TEST_F(JobChannelTest, ACallbackCancelsAndReleasesWhileTheReaderReadsOn) {
  up();
  // On the reader thread: both only queue.
  h.on_event = [this](const JobEvent& event) {
    if (event.kind() == Accepted && event.identity().job_id == 1) {
      EXPECT_TRUE(h.cancel(1).has_value());
    } else if (event.kind() == Accepted && event.identity().job_id == 2) {
      EXPECT_TRUE(h.release(8).has_value());
    }
  };
  ASSERT_TRUE(h.submit(h.stt(1, 7)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->code, Error::Code::Cancelled);
  ASSERT_TRUE(h.submit(h.stt(2, 8)).has_value());
  ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == 1; }));
  EXPECT_EQ(h.sink->kinds_for(2), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
}

TEST_F(JobChannelTest, ASessionIsReleasedOnceAndNeverAskedAgain) {
  up();
  expect_refused(h.channel.release_session(0, 1), Error::Code::ConfigInvalid, "session_key_zero");
  expect_refused(h.channel.release_session(7, 0), Error::Code::ConfigInvalid, "generation_zero");
  ASSERT_TRUE(h.submit(h.stt(1, 7)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.release(7).has_value());
  ASSERT_TRUE(h.release(7).has_value());
  expect_refused(h.channel.submit(h.stt(2, 7)), Error::Code::NotReady, "session_releasing");
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged}));
  EXPECT_TRUE(h.sink->session_releases().empty());
  peer.complete(1, transcript());
  ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == 1; }));
  EXPECT_EQ(h.sink->session_releases().front(), (std::pair<std::uint64_t, std::uint64_t>{7, 1}));
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));

  // The sidecar has forgotten the session and would answer a repeat again.
  ASSERT_TRUE(h.release(7).has_value());
  expect_refused(h.channel.submit(h.stt(3, 7)), Error::Code::NotReady, "session_releasing");
  settle();
  EXPECT_EQ(peer.sent("session_release", 1).size(), 1U);
  EXPECT_EQ(h.sink->session_releases().size(), 1U);
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
}

TEST_F(JobChannelTest, AReleaseTheSidecarRefusesAfterAnUnloadStaysPendingUntilTheStop) {
  up();
  h.channel.close_admission();
  ASSERT_TRUE(h.unary("unload").has_value());
  ASSERT_TRUE(h.release(3).has_value());
  ASSERT_EQ(peer.sent("session_release", 1).size(), 1U);
  settle();
  EXPECT_EQ(h.channel.counters(), (JobChannelCounters{0, 1})) << "the refusal names no job";
  EXPECT_TRUE(h.sink->session_releases().empty());
  h.stop(kSessionUnloaded);
  EXPECT_EQ(h.sink->session_releases().size(), 1U);
}

TEST_F(JobChannelTest, AReleaseRefusedOnceTheConnectionIsGoneIsRecordedAndItsKeyTakesNoJobAgain) {
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  // Never started: there was no session to release, and nothing is recorded.
  expect_refused(h.channel.release_session(1, 1), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_EQ(h.channel.released_session_runs(), 0U);

  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  peer.close();
  // An exchange fails once the reader has left: the connection has ended.
  ASSERT_FALSE(h.unary("prime").has_value());
  expect_refused(h.channel.release_session(3, 1), Error::Code::Unavailable, "backend_unavailable");
  // Arguments a live channel refuses name no session.
  expect_refused(h.channel.release_session(0, 1), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.channel.release_session(7, 0), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.channel.release_session(kMaxWireInteger + 1, 1), Error::Code::Unavailable,
                 "backend_unavailable");
  EXPECT_EQ(h.channel.released_session_runs(), 1U);
  h.stop(kSessionUnloaded);
  expect_refused(h.channel.release_session(5, 1), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_EQ(h.channel.released_session_runs(), 2U);
  expect_refused(h.channel.release_session(4, 1), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_EQ(h.channel.released_session_runs(), 1U) << "3 to 5";

  // On the next connection those keys are released ones.
  ScriptedSidecar next;
  ASSERT_TRUE(h.connect(next.launcher()).has_value());
  h.channel.admit_jobs();
  for (const std::uint64_t key : {3U, 4U, 5U}) {
    expect_refused(h.channel.submit(h.stt(key, key)), Error::Code::NotReady, "session_releasing");
    EXPECT_TRUE(h.channel.release_session(key, 1).has_value()) << key;
  }
  EXPECT_TRUE(h.submit(h.stt(1, 1)).has_value()) << "its release was refused before any start";
  EXPECT_TRUE(h.submit(h.stt(2, 7)).has_value()) << "its release named no generation";
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  settle();
  EXPECT_TRUE(next.sent("session_release", 0).empty());
  EXPECT_TRUE(h.sink->session_releases().empty());
  h.stop(kSessionUnloaded);
}

TEST(JobChannelSessions, AReleasedSessionStaysReleasedHoweverManyFollowIt) {
  ScriptedSidecar peer;
  JobChannelHarness h;
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  constexpr std::uint64_t kLater = 1'025;
  for (std::uint64_t session = 1; session <= 1 + kLater; ++session) {
    ASSERT_TRUE(h.release(session).has_value());
    ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == session; })) << session;
  }
  EXPECT_EQ(h.channel.released_session_runs(), 1U);

  h.channel.admit_jobs();
  expect_refused(h.channel.submit(JobChannelHarness::tts(1, 1)), Error::Code::NotReady,
                 "session_releasing");
  ASSERT_TRUE(h.channel.release_session(1, 1).has_value());
  // Health is answered in arrival order: a repeat that was sent has arrived.
  ASSERT_TRUE(h.unary("health_check").has_value());
  EXPECT_EQ(peer.sent("session_release", 1 + kLater).size(), 1 + kLater);
  EXPECT_EQ(h.sink->session_releases().size(), 1 + kLater);
}

TEST(JobChannelSessions, ReleasesAcknowledgedOutOfOrderJoinIntoOneRun) {
  ScriptedSidecar peer;
  JobChannelHarness h;
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  const std::vector<std::pair<std::uint64_t, std::size_t>> runs_after{
      {5, 1}, {1, 2}, {3, 3}, {2, 2}, {4, 1}, {7, 2}, {6, 1}};
  std::size_t acknowledged = 0;
  for (const auto& [session, runs] : runs_after) {
    ASSERT_TRUE(h.release(session).has_value());
    ++acknowledged;
    ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == acknowledged; }));
    EXPECT_EQ(h.channel.released_session_runs(), runs) << session;
  }
  h.channel.admit_jobs();
  for (std::uint64_t session = 1; session <= 7; ++session) {
    expect_refused(h.channel.submit(JobChannelHarness::tts(session, session)),
                   Error::Code::NotReady, "session_releasing");
  }
  EXPECT_TRUE(h.submit(JobChannelHarness::tts(8, 8)).has_value());
}

TEST(ReleasedSessionKeys, AKeyJoinsTheRunsItTouchesAndNoOther) {
  ReleasedSessionKeys keys;
  EXPECT_FALSE(keys.contains(4));
  keys.insert(4);
  keys.insert(4);
  EXPECT_EQ(keys.runs(), 1U);
  EXPECT_TRUE(keys.contains(4));
  EXPECT_FALSE(keys.contains(3));
  EXPECT_FALSE(keys.contains(5));

  keys.insert(6);  // touches nothing
  EXPECT_EQ(keys.runs(), 2U);
  EXPECT_FALSE(keys.contains(5));
  keys.insert(7);  // the end of a run
  keys.insert(3);  // the start of one
  EXPECT_EQ(keys.runs(), 2U);
  keys.insert(5);  // both
  EXPECT_EQ(keys.runs(), 1U);
  for (std::uint64_t key = 3; key <= 7; ++key) {
    EXPECT_TRUE(keys.contains(key)) << key;
  }
  EXPECT_FALSE(keys.contains(2));
  EXPECT_FALSE(keys.contains(8));
  keys.insert(5);  // inside a run
  EXPECT_EQ(keys.runs(), 1U);
}

TEST(ReleasedSessionKeys, TheEndsOfTheKeySpaceDoNotWrap) {
  constexpr std::uint64_t kLast = std::numeric_limits<std::uint64_t>::max();
  ReleasedSessionKeys keys;
  keys.insert(kLast);
  keys.insert(0);
  EXPECT_EQ(keys.runs(), 2U) << "the last key and the first are not neighbours";
  EXPECT_TRUE(keys.contains(kLast));
  EXPECT_TRUE(keys.contains(0));
  EXPECT_FALSE(keys.contains(1));
  EXPECT_FALSE(keys.contains(kLast - 1));
  keys.insert(kLast - 1);
  keys.insert(1);
  EXPECT_EQ(keys.runs(), 2U);
  EXPECT_TRUE(keys.contains(kLast - 1));
  EXPECT_TRUE(keys.contains(1));
}

TEST_F(JobChannelTest, ASessionAcknowledgedBeforeItsLastJobWaitsForThatJob) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 7)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.release(7).has_value());
  const Json early{{"schema_version", "0.1"},
                   {"message_id", "s90"},
                   {"kind", "session_released"},
                   {"session_key", 7},
                   {"generation", 1}};
  Json other_generation = early;
  other_generation["generation"] = 2;
  peer.write(other_generation);
  settle();
  EXPECT_EQ(h.channel.counters().dropped_job_messages, 1U);
  peer.write(early);
  peer.write(early);
  settle();
  EXPECT_TRUE(h.sink->session_releases().empty());
  EXPECT_EQ(h.channel.counters().dropped_job_messages, 2U);
  peer.complete(1, transcript());
  // The runner's own acknowledgement follows the job and finds nothing pending.
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged, Failed, Released}));
  settle();
  EXPECT_EQ(h.sink->session_releases().size(), 1U);
  EXPECT_EQ(h.channel.counters().dropped_job_messages, 3U);
}

// -- Routing. --------------------------------------------------------------------

TEST_F(JobChannelTest, AnEventOfAJobTheTableDoesNotHoldIsDroppedAndItsChunkReleased) {
  up();
  const std::size_t before = h.bytes_in_use();
  Json stray = audio(1'920);
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", "s90"},
                  {"kind", "job_completed"},
                  {"job_id", 99},
                  {"session_key", 1},
                  {"generation", 1},
                  {"result", stray}},
             1'920);
  settle();
  EXPECT_EQ(h.channel.counters(), (JobChannelCounters{1, 0}));
  EXPECT_EQ(h.bytes_in_use(), before);
  EXPECT_TRUE(h.sink->events().empty());
}

TEST_F(JobChannelTest, AnEventOutOfOrderFailsItsJobWithTheSeamsReason) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  const std::size_t before = h.bytes_in_use();
  // An event that names the job with another session is not the job's.
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", "s90"},
                  {"kind", "job_completed"},
                  {"job_id", 1},
                  {"session_key", 2},
                  {"generation", 1},
                  {"result", transcript()}});
  settle();
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted}));
  // An audio chunk is not what a decode returns.
  peer.emit("job_completed", 1, {{"result", audio(1'920)}}, 1'920);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1),
            Error::make(Error::Code::InferenceFailed, h.sink->events().back().error()->message,
                        "result_kind_mismatch"));
  EXPECT_EQ(h.bytes_in_use(), before) << "the refused chunk was released";

  // What follows the failure is dropped, but for the release.
  peer.emit("job_completed", 1, {{"result", transcript()}});
  peer.emit("job_released", 1);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed, Released}));
  EXPECT_EQ(h.channel.counters(), (JobChannelCounters{2, 0}));
}

TEST_F(JobChannelTest, AReleaseBeforeTheTerminalEventFailsTheJobAndStillReleasesIt) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  peer.emit("job_released", 1);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), "released_before_terminal");
  EXPECT_TRUE(h.submit(h.stt(1, 1)).has_value()) << "the id is free again";
}

TEST_F(JobChannelTest, AJobMessageTheCodecFaultsFailsTheJobItNames) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  peer.emit("job_completed", 1, {{"result", {{"type", "transcript"}}}});
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1)->code, Error::Code::InferenceFailed);
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kJobMessageMalformed);

  // A kind longer than a failure's text may be: the text is cut, the job ends.
  ASSERT_EQ(peer.sent("job_submit", 2).size(), 2U) << "the sidecar holds the second job";
  peer.emit("job_" + std::string(600, 'x'), 2);
  ASSERT_TRUE(h.wait_kinds(2, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 2)->message.size(), kMaxJobErrorTextBytes);

  // Faults that name a finished job, a session, or nothing change nothing.
  peer.emit("job_completed", 1, {{"result", 7}});
  peer.write(Json{{"schema_version", "0.2"},
                  {"message_id", "s90"},
                  {"kind", "session_released"},
                  {"session_key", 5},
                  {"generation", 1}});
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", "s91"},
                  {"kind", "job_accepted"},
                  {"job_id", "one"},
                  {"session_key", 1},
                  {"generation", 1}});
  settle();
  EXPECT_EQ(h.channel.counters(), (JobChannelCounters{2, 1}));
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed}));
}

TEST_F(JobChannelTest, ARefusalEndsTheJobWhoseSubmitItNames) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.cancel(1).has_value());
  const auto cancel_id = peer.sent("job_cancel", 1).at(0).at("message_id").get<std::string>();
  const auto submit_id = peer.sent("job_submit", 1).at(0).at("message_id").get<std::string>();
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged}));

  // A refused cancel and a refusal of a message the channel never sent: counted.
  peer.refuse(cancel_id, "config_invalid", "identity_mismatch");
  peer.refuse("tp-msg-unknown", "unsupported", nullptr);
  settle();
  EXPECT_EQ(h.channel.counters(), (JobChannelCounters{1, 1}));
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, CancelAcknowledged}));

  // The sidecar says it never took the job: it ends and is released at once.
  peer.refuse(submit_id, "config_invalid", "duplicate_job_id");
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1), Error::make(Error::Code::ConfigInvalid, "m", "duplicate_job_id"));
  // Both ids left with the job: a later refusal of either names nothing.
  peer.refuse(submit_id, "config_invalid", "duplicate_job_id");
  peer.refuse(cancel_id, "config_invalid", "identity_mismatch");
  settle();
  EXPECT_EQ(h.channel.counters(), (JobChannelCounters{1, 3}));
}

TEST_F(JobChannelTest, AUnaryExchangeIsAnsweredBetweenJobEventsAndOnlyByItsOwnAnswer) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  settle();  // health is answered while the job is held
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted}));

  // `prime` waits at the sidecar behind the running job. Nothing between the
  // thread's start and its join ends the test.
  Result<Json> primed = unexpected(Error::Code::Internal, "not run");
  std::thread exchange([&] { primed = h.unary("prime"); });
  EXPECT_EQ(peer.sent("prime", 2).size(), 2U);
  const auto second = h.unary("health_check");
  EXPECT_TRUE(!second.has_value() && second.error().code == Error::Code::Internal)
      << "one exchange at a time";
  peer.refuse("tp-msg-unknown", "unsupported", nullptr);  // not this exchange's
  peer.complete(1, transcript());
  exchange.join();
  ASSERT_TRUE(primed.has_value()) << primed.error().message;
  EXPECT_EQ(primed.value().value("kind", ""), "prime_response");
  EXPECT_EQ(h.channel.counters(), (JobChannelCounters{0, 1}));

  // An `error_event` that names the waiting request is its answer.
  const auto unknown = h.unary("a_kind_of_a_later_release");
  ASSERT_TRUE(unknown.has_value());
  EXPECT_EQ(unknown.value().value("kind", ""), "error_event");

  // A frame nobody waits for is counted.
  peer.write(Json{{"schema_version", "0.1"},
                  {"message_id", "tp-msg-0"},
                  {"kind", "prime_response"},
                  {"status", "ok"}});
  EXPECT_TRUE(h.wait([&] { return h.channel.counters() == JobChannelCounters{0, 2}; }));
}

TEST_F(JobChannelTest, AnExchangeThatIsNotAnsweredByItsDeadlineTimesOut) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  const auto primed = h.unary("prime", std::chrono::milliseconds{20});
  ASSERT_FALSE(primed.has_value());
  EXPECT_EQ(primed.error().code, Error::Code::Timeout);
}

// -- Health. ---------------------------------------------------------------------

TEST_F(JobChannelTest, HealthAnswersFromThePhaseAndAsksOnlyWhileTheChannelAdmits) {
  expect_refused(h.channel.probe_health(patient()), Error::Code::NotReady, "backend_not_ready");
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  expect_refused(h.channel.probe_health(patient()), Error::Code::NotReady, "backend_not_ready");
  // `prime` arrives behind whatever was sent before it.
  ASSERT_TRUE(h.unary("prime").has_value());
  EXPECT_TRUE(peer.sent("health_check", 0).empty());

  h.channel.admit_jobs();
  EXPECT_TRUE(h.channel.probe_health(patient()).has_value());
  peer.report_not_ready();
  expect_refused(h.channel.probe_health(patient()), Error::Code::NotReady, "backend_not_ready");

  h.channel.close_admission();
  expect_refused(h.channel.probe_health(patient()), Error::Code::Unavailable,
                 "backend_unavailable");
  ASSERT_TRUE(h.unary("prime").has_value());
  EXPECT_EQ(peer.sent("health_check", 0).size(), 2U) << "the two asked while it admitted";
  h.stop(kSessionUnloaded);
  expect_refused(h.channel.probe_health(patient()), Error::Code::Unavailable,
                 "backend_unavailable");
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
}

TEST_F(JobChannelTest, AfterItsConnectionEndedHealthIsUnavailableWithoutAsking) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.close();
  // An exchange fails once the reader has left: the connection has ended.
  ASSERT_FALSE(h.unary("prime").has_value());
  expect_refused(h.channel.probe_health(patient()), Error::Code::Unavailable,
                 "backend_unavailable");
  EXPECT_EQ(h.channel.queued_frames(), 0U);
}

TEST_F(JobChannelTest, HealthIsAnsweredBesideARunningJobAndAnExchangeThatWaits) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  WaitingExchange prime(h, "prime");
  ASSERT_EQ(peer.sent("prime", 2).size(), 2U);
  // The sidecar answers health at once, while `prime` waits behind the job.
  EXPECT_TRUE(h.channel.probe_health(patient()).has_value());
  peer.complete(1, transcript());
  ASSERT_TRUE(prime.outcome().has_value());
  EXPECT_EQ(prime.outcome().value().value("kind", ""), "prime_response");
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
}

TEST_F(JobChannelTest, ARefusalThatNamesTheHealthProbeIsItsAnswerAndNotTheExchanges) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  WaitingExchange prime(h, "prime");
  ASSERT_EQ(peer.sent("prime", 2).size(), 2U);
  peer.hold_health();
  WaitingHealth health(h.channel);
  const auto asked = peer.sent("health_check", 1);
  ASSERT_EQ(asked.size(), 1U);
  peer.refuse(asked[0].value("message_id", ""), "unsupported", nullptr);
  expect_refused(health.outcome(), Error::Code::Unavailable, "backend_unavailable");
  peer.complete(1, transcript());
  ASSERT_TRUE(prime.outcome().has_value());
  EXPECT_EQ(prime.outcome().value().value("kind", ""), "prime_response");
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
}

TEST_F(JobChannelTest, AHealthAnswerThatIsNoHealthReportMakesTheBackendUnavailable) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  WaitingHealth health(h.channel);
  const auto asked = peer.sent("health_check", 1);
  ASSERT_EQ(asked.size(), 1U);
  Json answer = health_report(asked[0].value("message_id", ""));
  answer.erase("health");
  peer.write(answer);
  expect_refused(health.outcome(), Error::Code::Unavailable, "backend_unavailable");
}

TEST_F(JobChannelTest, AHealthCallThatTimesOutLeavesItsProbeForALaterCallToBeAnsweredBy) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  WaitingHealth later(h.channel);
  ASSERT_TRUE(h.wait([&] { return h.channel.health_waiters() == 1; }));
  // `prime` arrives behind whatever was sent before it.
  ASSERT_TRUE(h.unary("prime").has_value());
  EXPECT_EQ(peer.sent("health_check", 0).size(), 1U) << "one probe, however many callers";

  peer.release_health();
  EXPECT_TRUE(later.outcome().has_value());
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
  // Answered, so the next call asks again.
  EXPECT_TRUE(h.channel.probe_health(patient()).has_value());
  EXPECT_EQ(peer.sent("health_check", 2).size(), 2U);
}

TEST_F(JobChannelTest, AHealthAnswerThatComesAfterEveryCallerGaveUpIsDroppedAndFreesTheProbe) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  peer.hold_health();
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  WaitingExchange prime(h, "prime");
  ASSERT_EQ(peer.sent("prime", 2).size(), 2U);
  ASSERT_EQ(peer.sent("health_check", 1).size(), 1U);
  // The late answer arrives while the exchange waits for its own.
  peer.release_health();
  ASSERT_TRUE(h.wait([&] { return h.channel.counters() == JobChannelCounters{0, 1}; }));
  peer.complete(1, transcript());
  ASSERT_TRUE(prime.outcome().has_value());
  EXPECT_EQ(prime.outcome().value().value("kind", ""), "prime_response");
  // The next call sends a probe of its own.
  EXPECT_TRUE(h.channel.probe_health(patient()).has_value());
  EXPECT_EQ(peer.sent("health_check", 2).size(), 2U);
}

TEST_F(JobChannelTest, AWaitingHealthCallEndsWithTheConnection) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  WaitingHealth health(h.channel);
  ASSERT_EQ(peer.sent("health_check", 1).size(), 1U);
  peer.close();
  expect_refused(health.outcome(), Error::Code::Unavailable, "backend_unavailable");
}

TEST_F(JobChannelTest, AWaitingHealthCallIsReleasedByAStop) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  WaitingHealth health(h.channel);
  ASSERT_TRUE(h.wait([&] { return h.channel.health_waiters() == 1; }));
  h.stop(kSessionUnloaded);
  expect_refused(health.outcome(), Error::Code::Unavailable, "backend_unavailable");
}

TEST_F(JobChannelTest, AnUnloadThatBeginsWhileAHealthCallWaitsMakesItUnavailable) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  WaitingHealth health(h.channel);
  ASSERT_EQ(peer.sent("health_check", 1).size(), 1U);
  h.channel.close_admission();
  // The sidecar still reports ready; the channel no longer takes a job.
  peer.release_health();
  expect_refused(health.outcome(), Error::Code::Unavailable, "backend_unavailable");
}

// No answer comes: the close itself ends the call, well before its deadline.
TEST_F(JobChannelTest, AnUnloadThatBeginsWhileAHealthCallWaitsEndsItWithoutAnAnswer) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  WaitingHealth health(h.channel, passed() + 10 * JobChannelHarness::kPatience);
  ASSERT_TRUE(h.wait([&] { return h.channel.health_waiters() == 1; }));
  h.channel.close_admission();
  EXPECT_TRUE(h.wait([&] { return h.channel.health_waiters() == 0; }));
  // A caller the close had not woken ends here, as a stop ends it.
  h.stop(kSessionUnloaded);
  expect_refused(health.outcome(), Error::Code::Unavailable, "backend_unavailable");
}

TEST_F(JobChannelTest, AHealthCallMadeOnceAdmissionClosedDoesNotJoinTheProbeThatStands) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  h.channel.close_admission();
  expect_refused(h.channel.probe_health(passed()), Error::Code::Unavailable, "backend_unavailable");
}

TEST_F(JobChannelTest, AfterAConnectionEndedUnderAStandingProbeTheNextConnectionAsksAgain) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  expect_refused(h.channel.probe_health(passed()), Error::Code::Timeout, "health_timeout");
  h.stop(kSessionUnloaded);

  ScriptedSidecar next;
  ASSERT_TRUE(h.connect(next.launcher()).has_value());
  ASSERT_TRUE(h.unary("prime").has_value());
  h.channel.admit_jobs();
  EXPECT_TRUE(h.channel.probe_health(patient()).has_value());
  EXPECT_EQ(next.sent("health_check", 1).size(), 1U);
  h.stop(kSessionUnloaded);
}

// Their deadline lies beyond the test's patience: a caller the answer does not
// wake returns only then.
TEST_F(JobChannelTest, TwoHealthCallsThatWaitAreBothWokenByTheOneAnswer) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  std::atomic<int> answered{0};
  const JobChannel::TimePoint late = passed() + 2 * JobChannelHarness::kPatience;
  struct Joined {
    std::vector<std::thread> threads;
    ~Joined() {
      for (std::thread& thread : threads) {
        thread.join();
      }
    }
  } callers;
  for (int caller = 0; caller < 2; ++caller) {
    callers.threads.emplace_back([&] {
      if (h.channel.probe_health(late).has_value()) {
        ++answered;
      }
    });
  }
  EXPECT_TRUE(h.wait([&] { return h.channel.health_waiters() == 2; }));
  EXPECT_EQ(peer.sent("health_check", 1).size(), 1U);
  peer.release_health();
  EXPECT_TRUE(h.wait([&] { return answered.load() == 2; }));
}

// -- The end of a connection. ----------------------------------------------------

TEST_F(JobChannelTest, AWaitingExchangeEndsWithTheConnection) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  // With a deadline of an hour, an end that does not wake the exchange hangs.
  Result<Json> primed = Json{};
  std::thread exchange([&] { primed = h.unary("prime", std::chrono::hours{1}); });
  EXPECT_EQ(peer.sent("prime", 2).size(), 2U);
  peer.close();
  exchange.join();
  ASSERT_FALSE(primed.has_value());
  EXPECT_EQ(primed.error().code, Error::Code::LoadFailed);
}

TEST_F(JobChannelTest, WhenTheSidecarGoesAwayOpenJobsFailAndAreReleasedOnlyAfterTheReap) {
  up();
  std::atomic<bool>& released_before_the_reap = noted;
  h.on_event = [this](const JobEvent& event) {
    if (event.kind() == Released && !peer.reaped()) {
      noted = true;
    }
  };
  h.on_session_release = [this](std::uint64_t session_key, std::uint64_t) {
    if (session_key == 6 && !peer.reaped()) {
      noted = true;
    }
  };
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  peer.emit("job_completed", 1, {{"result", transcript()}});
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Completed}));
  ASSERT_TRUE(h.release(5).has_value());
  ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == 1; }));
  peer.stop_reading();
  ASSERT_TRUE(h.release(6).has_value());  // never read: unanswered when the sidecar dies
  peer.close();
  ASSERT_TRUE(h.wait_kinds(2, {Accepted, Failed}));
  EXPECT_EQ(h.sink->session_releases().size(), 1U) << "session 6 waits for the reap";
  EXPECT_EQ(failure_of(h, 2)->code, Error::Code::Unavailable);
  EXPECT_EQ(failure_of(h, 2)->context.value_or(""), kBackendReset);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Completed})) << "it had ended already";
  expect_refused(h.channel.submit(h.stt(3, 1)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.channel.release_session(8, 1), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_TRUE(h.channel.cancel(2).has_value());
  EXPECT_FALSE(h.unary("health_check").has_value());
  EXPECT_FALSE(peer.reaped());

  h.stop(kSessionUnloaded);
  EXPECT_TRUE(peer.reaped());
  EXPECT_FALSE(released_before_the_reap);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
  ASSERT_EQ(h.sink->kinds_for(2), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 2)->context.value_or(""), kBackendReset) << "not the stop's reason";
  EXPECT_EQ(h.sink->session_releases().size(), 2U);
}

TEST_F(JobChannelTest, StopFailsOpenJobsWithItsReasonAndTheChannelStartsAgain) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  // The stop finds a release queued behind a frame the sidecar is not taking.
  peer.stop_reading();
  ASSERT_TRUE(h.submit(h.stt(2, 1, kLargestWindow)).has_value());
  ASSERT_TRUE(h.wait([&] { return peer.unread_bytes() >= 65'536; }));
  ASSERT_TRUE(h.release(4).has_value());
  h.stop(kSessionUnloaded);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(failure_of(h, 1), Error::make(Error::Code::Unavailable, "the sidecar connection ended",
                                          std::string(kSessionUnloaded)));
  EXPECT_EQ(h.sink->kinds_for(2), (Kinds{Failed, Released}));
  EXPECT_EQ(h.sink->session_releases().size(), 1U);
  h.stop(kSessionUnloaded);  // a second stop finds nothing to do

  // The next connection gets nothing that was queued for the last one, and
  // ends for its own reason with its own error.
  ScriptedSidecar next;
  ASSERT_TRUE(h.connect(next.launcher()).has_value());
  h.channel.admit_jobs();
  ASSERT_TRUE(h.submit(h.stt(3, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(3, {Accepted}));
  // The session the stop acknowledged is still released on this connection.
  expect_refused(h.channel.submit(h.stt(5, 4)), Error::Code::NotReady, "session_releasing");
  EXPECT_TRUE(h.channel.release_session(4, 1).has_value());
  settle();
  EXPECT_TRUE(next.sent("session_release", 0).empty());
  EXPECT_EQ(h.channel.counters(), JobChannelCounters{});
  next.write_raw("not a header");
  ASSERT_TRUE(h.wait_kinds(3, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 3)->context.value_or(""), kBackendReset);
  const auto health = h.unary("health_check");
  ASSERT_FALSE(health.has_value());
  EXPECT_EQ(health.error().code, Error::Code::InferenceFailed);
  h.stop(kSessionUnloaded);
}

TEST_F(JobChannelTest, AStopReasonLongerThanAFailureCarriesIsCutAtACharacter) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  std::string reason;
  for (int i = 0; i < 171; ++i) {
    reason += "\xE2\x82\xAC";  // three bytes: the bound falls inside the last one
  }
  h.stop(reason);
  const auto failure = failure_of(h, 1);
  ASSERT_TRUE(failure.has_value());
  EXPECT_EQ(failure->context.value_or(""), reason.substr(0, 510));
}

TEST_F(JobChannelTest, AFrameNobodyCanAttributeEndsTheConnection) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  peer.write_raw("not a header");
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kBackendReset);
  // An exchange gets the error the connection ended with, and nothing is
  // queued for a connection that has ended.
  const auto health = h.unary("health_check");
  ASSERT_FALSE(health.has_value());
  EXPECT_EQ(health.error().code, Error::Code::InferenceFailed);
  EXPECT_EQ(h.channel.queued_frames(), 0U);
}

TEST_F(JobChannelTest, AnEndTheReaderDecidesIsEndOfFileForTheSidecarBeforeAnyStop) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  peer.write_raw("not a header");
  // The sidecar is alive and holds the job: only the socket can tell it.
  EXPECT_TRUE(peer.read_end_of_file());
  EXPECT_FALSE(peer.gave_up()) << "the end of file is the adapter's doing";
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
}

TEST_F(JobChannelTest, AFrameOfTheLargestChunkIsTakenAndOneByteMoreEndsTheConnection) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(JobChannelHarness::tts(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  peer.complete(1, audio(kMaxJobFramePayloadBytes), kMaxJobFramePayloadBytes);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Completed, Released}));
  const std::vector<JobEvent> events = h.sink->events();
  EXPECT_EQ(std::get<AudioChunkResult>(events.at(1).result()->payload()).pcm.byte_size,
            kMaxJobFramePayloadBytes);

  ASSERT_TRUE(h.submit(JobChannelHarness::tts(2, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 2U);
  ASSERT_TRUE(h.wait_kinds(2, {Accepted}));
  const std::size_t beyond = kMaxJobFramePayloadBytes + 1;
  peer.emit("job_completed", 2, {{"result", audio(beyond)}}, beyond);
  // Read whole, the frame would fault the job for its size and leave the
  // connection up.
  ASSERT_TRUE(h.wait_kinds(2, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 2)->context.value_or(""), kBackendReset);
  const auto health = h.unary("health_check");
  ASSERT_FALSE(health.has_value());
  EXPECT_EQ(health.error().code, Error::Code::InferenceFailed);
  EXPECT_EQ(health.error().message, "sidecar frame size exceeds limit");
}

// -- The scripted sidecar's own rules. -------------------------------------------

TEST_F(JobChannelTest, TheScriptedSidecarRefusesAFrameTheRunnerWouldNotRead) {
  up();
  const auto answer_to = [this](const Json& header) -> Json {
    const auto answered =
        h.channel.exchange({header.dump(), {}}, header.at("message_id").get<std::string>(),
                           JobChannel::TimePoint::clock::now() + JobChannelHarness::kPatience);
    EXPECT_TRUE(answered.has_value());
    return answered.has_value() ? Json::parse(answered.value().json_header) : Json::object();
  };
  const Json another_version =
      answer_to({{"schema_version", "0.2"}, {"message_id", "tp-msg-x"}, {"kind", "prime"}});
  EXPECT_EQ(another_version.value("kind", ""), "prime_response");
  EXPECT_EQ(another_version.value("message_id", ""), "tp-msg-x");
  EXPECT_EQ(another_version.value("status", ""), "error");
  EXPECT_EQ(another_version.value("error", Json::object()).value("code", ""), "unsupported");

  const Json unnamed =
      answer_to({{"schema_version", "0.1"}, {"message_id", ""}, {"kind", "prime"}});
  EXPECT_EQ(unnamed.value("error", Json::object()).value("code", ""), "config_invalid");
  EXPECT_FALSE(unnamed.value("message_id", "").empty()) << "an id of the sidecar's own";

  // The version is judged before the id, and the answer still carries one.
  const Json neither =
      answer_to({{"schema_version", "0.2"}, {"message_id", ""}, {"kind", "prime"}});
  EXPECT_EQ(neither.value("error", Json::object()).value("code", ""), "unsupported");
  EXPECT_FALSE(neither.value("message_id", "").empty());

  const Json no_kind = answer_to({{"schema_version", "0.1"}, {"message_id", "tp-msg-y"}});
  EXPECT_EQ(no_kind.value("kind", ""), "error_event");
  EXPECT_EQ(no_kind.value("message_id", ""), "tp-msg-y");
  EXPECT_EQ(no_kind.value("error", Json::object()).value("code", ""), "config_invalid");
}

TEST_F(JobChannelTest, TheScriptedSidecarEndsTheConnectionOnAHeaderThatIsNotAnObject) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  // The runner ends the connection on such a frame, and so does this
  // sidecar: the exchange fails at once and the open job with it.
  const auto answered = h.channel.exchange(
      {"[]", {}}, "tp-msg-z", JobChannel::TimePoint::clock::now() + JobChannelHarness::kPatience);
  ASSERT_FALSE(answered.has_value());
  EXPECT_NE(answered.error().code, Error::Code::Timeout);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kBackendReset);
}

// -- The writer. -----------------------------------------------------------------

TEST(JobChannelWriter, AFrameTheSidecarDoesNotTakeWithinTheWriteBoundEndsTheConnection) {
  ScriptedSidecar peer;
  JobChannelHarness h(std::chrono::milliseconds{50});
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  h.channel.admit_jobs();
  peer.stop_reading();
  ASSERT_TRUE(h.submit(h.stt(1, 1, kLargestWindow)).has_value());
  ASSERT_TRUE(h.submit(h.stt(2, 1, kLargestWindow)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Failed}));
  ASSERT_TRUE(h.wait_kinds(2, {Failed}));
  EXPECT_EQ(failure_of(h, 2)->context.value_or(""), kBackendReset);
  const auto health = h.unary("health_check");
  ASSERT_FALSE(health.has_value());
  EXPECT_EQ(health.error().code, Error::Code::Timeout) << "the write's error, not the reader's";
}

TEST(JobChannelWriter, StopWakesAWriterTheSidecarIsNotReading) {
  ScriptedSidecar peer;
  // With a bound of an hour, a stop that does not wake the writer hangs here.
  JobChannelHarness h(std::chrono::hours{1});
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  h.channel.admit_jobs();
  peer.stop_reading();
  ASSERT_TRUE(h.submit(h.stt(1, 1, kLargestWindow)).has_value());
  // The frame is larger than the socket holds, so the writer is inside it.
  ASSERT_TRUE(h.wait([&] { return peer.unread_bytes() >= 65'536; }));
  h.stop(kSessionUnloaded);
  ASSERT_EQ(h.sink->kinds_for(1), (Kinds{Failed, Released}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kSessionUnloaded);
}

TEST_F(JobChannelTest, TheReaderKeepsReadingWhileASinkCallbackSubmits) {
  up();
  std::atomic<bool>& submitted = noted;
  h.on_event = [this](const JobEvent& event) {
    if (event.kind() != Accepted || event.identity().job_id != 1) {
      return;
    }
    // The sidecar now holds its job lock and is writing a result larger than
    // the socket holds. Its reader cannot act on the first submit below, so
    // the second cannot be written until this thread reads on.
    EXPECT_TRUE(peer.wait_until_writing("job_completed"));
    EXPECT_TRUE(h.submit(h.stt(2, 1, kLargestWindow)).has_value());
    EXPECT_TRUE(h.submit(h.stt(3, 1, kLargestWindow)).has_value());
    noted = true;
  };
  ASSERT_TRUE(h.submit(JobChannelHarness::tts(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  peer.complete(1, audio(kLargestChunk), kLargestChunk);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(h.wait_kinds(3, {Accepted}));
  EXPECT_TRUE(submitted);
  EXPECT_EQ(h.sink->kinds_for(2), (Kinds{Accepted}));
  EXPECT_FALSE(peer.gave_up());
  const std::vector<JobEvent> events = h.sink->events();
  const auto& chunk = std::get<AudioChunkResult>(events.at(1).result()->payload());
  EXPECT_EQ(chunk.pcm.byte_size, kLargestChunk);
}

// -- A job the sidecar fails for an unload. --------------------------------------

// The `error` of a `job_failed`.
Json failure(const std::string& code, const std::string& context) {
  return Json{
      {"error",
       {{"schema_version", "0.1"}, {"code", code}, {"message", "m"}, {"context", context}}}};
}

TEST_F(JobChannelTest, AJobFailedAsUnavailableOnceAdmissionClosedEndsAsSessionUnloaded) {
  up();
  for (const std::uint64_t job_id : {1U, 2U, 3U}) {
    ASSERT_TRUE(h.submit(h.stt(job_id, 1)).has_value());
  }
  ASSERT_TRUE(h.wait_kinds(3, {Accepted}));
  h.channel.close_admission();
  peer.emit("job_failed", 1, failure("unavailable", "backend_unavailable"));
  // Another reason, and another code with the same reason, are the sidecar's own.
  peer.emit("job_failed", 2, failure("unavailable", "lane_closed"));
  peer.emit("job_failed", 3, failure("not_ready", "backend_unavailable"));
  ASSERT_TRUE(h.wait_kinds(3, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1), Error::make(Error::Code::Unavailable, "m", "session_unloaded"));
  EXPECT_EQ(failure_of(h, 2), Error::make(Error::Code::Unavailable, "m", "lane_closed"));
  EXPECT_EQ(failure_of(h, 3), Error::make(Error::Code::NotReady, "m", "backend_unavailable"));
}

TEST_F(JobChannelTest, AJobFailedAsUnavailableWhileTheChannelAdmitsKeepsTheSidecarsReason) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  peer.emit("job_failed", 1, failure("unavailable", "backend_unavailable"));
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1), Error::make(Error::Code::Unavailable, "m", "backend_unavailable"));
}

// -- A channel whose session is gone. --------------------------------------------

TEST(JobChannelAdmission, ARetiredChannelThatNeverStartedIsUnavailableAndStartsNoMore) {
  ScriptedSidecar peer;
  JobChannelHarness h;
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  expect_refused(h.channel.probe_health(patient()), Error::Code::NotReady, "backend_not_ready");
  for (int times = 0; times < 2; ++times) {
    h.channel.retire();
    expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::Unavailable, "backend_unavailable");
    expect_refused(h.channel.probe_health(patient()), Error::Code::Unavailable,
                   "backend_unavailable");
    expect_refused(h.release(1), Error::Code::Unavailable, "backend_unavailable");
  }
  const auto started = h.connect(peer.launcher());
  ASSERT_FALSE(started.has_value());
  EXPECT_EQ(started.error().context.value_or(""), "job_channel_retired");
  EXPECT_TRUE(h.sink->events().empty());
  EXPECT_TRUE(h.sink->session_releases().empty());
}

// Stopped and without a buffer manager it answered "backend_not_ready".
TEST(JobChannelAdmission, ARetiredChannelIsUnavailableAlsoWithoutABufferManager) {
  ScriptedSidecar peer;
  JobChannelHarness h(std::chrono::seconds{5}, /*with_manager=*/false);
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  h.stop(kSessionUnloaded);
  expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::NotReady, "backend_not_ready");
  expect_refused(h.channel.probe_health(patient()), Error::Code::NotReady, "backend_not_ready");
  h.channel.retire();
  expect_refused(h.channel.submit(h.stt(1, 1)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.channel.probe_health(patient()), Error::Code::Unavailable,
                 "backend_unavailable");
}

// Retired while it runs, which no session does: unavailable at once, its open
// job still ends, and the stop that follows is its last.
TEST_F(JobChannelTest, AChannelRetiredWhileItRunsIsUnavailableEndsItsJobAndStartsNoMore) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  h.channel.retire();
  expect_refused(h.channel.submit(h.stt(2, 1)), Error::Code::Unavailable, "backend_unavailable");
  expect_refused(h.channel.probe_health(patient()), Error::Code::Unavailable,
                 "backend_unavailable");
  expect_refused(h.release(1), Error::Code::Unavailable, "backend_unavailable");
  peer.complete(1, transcript());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Completed, Released}));
  EXPECT_TRUE(peer.sent("session_release", 0).empty());

  h.stop(kSessionUnloaded);
  ScriptedSidecar next;
  const auto started = h.connect(next.launcher());
  // Reaps the process of the refused start while its sidecar stands.
  h.stop(kSessionUnloaded);
  ASSERT_FALSE(started.has_value());
  EXPECT_EQ(started.error().context.value_or(""), "job_channel_retired");
}

// The record itself: the refusal's return value is the same either way.
TEST_F(JobChannelTest, ARetiredChannelRecordsNoReleaseItRefuses) {
  ASSERT_NO_FATAL_FAILURE(up());
  h.stop(kSessionUnloaded);
  // Stopped and not retired: the caller takes session 5 as released, and so
  // does the channel.
  expect_refused(h.release(5), Error::Code::Unavailable, "backend_unavailable");
  ASSERT_EQ(h.channel.released_session_runs(), 1U);
  h.channel.retire();
  // Not next to 5: recorded, it would be a run of its own.
  expect_refused(h.release(8), Error::Code::Unavailable, "backend_unavailable");
  EXPECT_EQ(h.channel.released_session_runs(), 1U);
}

TEST_F(JobChannelTest, RetiringAChannelEndsAHealthCallThatWaits) {
  ASSERT_NO_FATAL_FAILURE(up());
  peer.hold_health();
  WaitingHealth health(h.channel, passed() + 10 * JobChannelHarness::kPatience);
  ASSERT_TRUE(h.wait([&] { return h.channel.health_waiters() == 1; }));
  h.channel.retire();
  EXPECT_TRUE(h.wait([&] { return h.channel.health_waiters() == 0; }));
  // A caller the retire had not woken ends here, as a stop ends it.
  h.stop(kSessionUnloaded);
  expect_refused(health.outcome(), Error::Code::Unavailable, "backend_unavailable");
}

// -- A channel whose sink was detached. ------------------------------------------

// The job side is its bridge's, and the bridge is gone: each call is refused
// and none reaches the sidecar, also for the job that is still open.
TEST_F(JobChannelTest, OnceItsSinkIsDetachedTheJobSideIsRefusedAndSendsNothing) {
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  for (int times = 0; times < 2; ++times) {
    h.channel.detach_event_sink();
    EXPECT_EQ(h.sink.use_count(), 1) << "the channel holds the sink no more";
    expect_refused(h.channel.submit(h.stt(2, 1)), Error::Code::NotReady, "event_sink_missing");
    expect_refused(h.channel.cancel(1), Error::Code::NotReady, "unknown_job");
    expect_refused(h.channel.release_session(1, 1), Error::Code::NotReady, "event_sink_missing");
    expect_refused(h.channel.set_event_sink(h.sink), Error::Code::ConfigInvalid,
                   "event_sink_already_set");
  }
  EXPECT_FALSE(h.channel.in_sink_callback());
  EXPECT_EQ(h.channel.backlog(), (JobChannelBacklog{1, 0, 0}));
  // Health does not go through the sink, and is answered behind whatever the
  // calls above had queued.
  settle();
  EXPECT_TRUE(peer.sent("job_cancel", 0).empty());
  EXPECT_TRUE(peer.sent("session_release", 0).empty());
  EXPECT_EQ(peer.sent("job_submit", 1).size(), 1U);
}

// The detach holds the last reference, and the deleter stands where the
// sink's destructor runs: a thread that reads the channel meanwhile is
// answered, so the channel's mutex was let go first.
TEST(JobChannelDetach, TheLastReferenceToTheSinkIsLetGoOutsideTheChannelsMutex) {
  JobChannel channel(nullptr, std::chrono::seconds{5});
  testing::RecordingJobEventSink recording;
  std::promise<void> read;
  std::thread reader;
  std::optional<bool> answered;
  bool detached = false;
  std::shared_ptr<JobEventSink> sink(&recording, [&](JobEventSink*) {
    if (detached) {
      return;  // Kept past the detach, which the test reports below.
    }
    reader = std::thread([&] {
      (void)channel.backlog();
      read.set_value();
    });
    answered =
        read.get_future().wait_for(JobChannelHarness::kPatience) == std::future_status::ready;
  });
  const auto set = channel.set_event_sink(std::move(sink));
  channel.detach_event_sink();
  detached = true;
  if (reader.joinable()) {
    reader.join();
  }
  ASSERT_TRUE(set.has_value());
  ASSERT_TRUE(answered.has_value()) << "the channel kept the sink";
  EXPECT_TRUE(*answered) << "the sink was let go under the channel's mutex";
}

// -- The backlog. ----------------------------------------------------------------

class JobChannelBacklogTest : public JobChannelTest {
 protected:
  // The first callback of `kind` parks.
  void park_at(JobEventKind kind) {
    h.on_event = [this, kind](const JobEvent& event) {
      if (event.kind() == kind && !parked.entered()) {
        parked.park();
      }
    };
  }

  [[nodiscard]] bool backlog_is(const JobChannelBacklog& expected) {
    return h.wait([&] { return h.channel.backlog() == expected; });
  }

  // Declared after the harness's sidecar and before nothing that a callback
  // uses: it lets a parked callback go before the harness stops the channel.
  ParkedCallback parked;
};

TEST_F(JobChannelBacklogTest, ItIsEmptyOnlyOnceNoJobNoReleaseAndNoSinkCallIsOwed) {
  park_at(Accepted);
  up();
  EXPECT_EQ(h.channel.backlog(), JobChannelBacklog{});
  peer.stop_reading();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  EXPECT_EQ(h.channel.backlog(), (JobChannelBacklog{1, 0, 0}));
  ASSERT_TRUE(h.release(9).has_value());
  EXPECT_EQ(h.channel.backlog(), (JobChannelBacklog{1, 1, 0}));

  peer.resume_reading();
  ASSERT_TRUE(h.wait([&] { return parked.entered(); }));
  // The acknowledgement of session 9 waits behind the callback that is parked.
  EXPECT_EQ(h.channel.backlog(), (JobChannelBacklog{1, 1, 1}));
  parked.let_go();
  EXPECT_TRUE(backlog_is({1, 0, 0}));

  ASSERT_EQ(peer.take_job(), 1U);
  peer.complete(1, transcript());
  EXPECT_TRUE(backlog_is({0, 0, 0}));
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Completed, Released}));
}

TEST_F(JobChannelBacklogTest, TheFailuresOfAConnectionsEndAreOwedUntilTheirCallbacksReturn) {
  park_at(Failed);
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.submit(h.stt(2, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(2, {Accepted}));
  peer.close();
  ASSERT_TRUE(h.wait([&] { return parked.entered(); }));
  EXPECT_EQ(h.channel.backlog(), (JobChannelBacklog{2, 0, 2}));
  parked.let_go();
  EXPECT_TRUE(backlog_is({2, 0, 0})) << "failed, and unreleased until the stop";
}

TEST_F(JobChannelBacklogTest, WhatAStopDeliversIsOwedUntilItsCallbacksReturn) {
  park_at(Released);
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  // Running, so its session's release ends nothing before the stop.
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.release(1).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, CancelAcknowledged}));
  std::thread stopping([this] { h.stop(kSessionUnloaded); });
  const bool entered = h.wait([&] { return parked.entered(); });
  // The job's `released` and its session's acknowledgement; the job's failure
  // was the reader's, made before the stop's batch.
  const JobChannelBacklog during = h.channel.backlog();
  parked.let_go();
  stopping.join();
  ASSERT_TRUE(entered);
  EXPECT_EQ(during, (JobChannelBacklog{0, 0, 2}));
  EXPECT_EQ(h.channel.backlog(), JobChannelBacklog{});
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
