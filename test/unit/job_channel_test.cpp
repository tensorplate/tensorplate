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

#include "adapters/python_pytorch/job_wire.hpp"
#include "job_channel_harness.hpp"
#include "scripted_sidecar.hpp"
#include "speech_job_golden.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using testing::JobChannelHarness;
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

// The error of the job's `failed` event, when it has one.
std::optional<Error> failure_of(const JobChannelHarness& h, std::uint64_t job_id) {
  for (const JobEvent& event : h.sink->events()) {
    if (event.identity().job_id == job_id && event.kind() == Failed) {
      return event.error();
    }
  }
  return std::nullopt;
}

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

TEST(JobChannelGolden, EveryRecordedJobConversationReplaysThroughTheChannel) {
  std::set<std::string> replayed;
  for (const std::string& file : golden_files()) {
    if (file.starts_with("negotiation") || file == "vad_frames") {
      continue;  // the load exchange is the session's; no lane runs vad_frames
    }
    SCOPED_TRACE(file);
    ScriptedSidecar peer;
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
        // The session_release file submits a class no lane runs: the same
        // identity is sent as stt_decode, as the sidecar's own replay does.
        const bool recorded_class = input.at("type") != "vad_frames";
        if (file == "refused") {
          peer.refuse_next_submit();
          ended_by_the_runner.insert(job_id);
        }
        const JobRequest request =
            input.at("type") == "text_segment"
                ? JobChannelHarness::tts(job_id, session_key, std::string(bytes, 'a'))
                : h.stt(job_id, session_key, recorded_class ? bytes : 3'200,
                        line.at("progress_limit").get<std::uint32_t>());
        ASSERT_TRUE(h.submit(request).has_value());
        const auto sent = peer.sent(kind, ++submits);
        ASSERT_EQ(sent.size(), submits);
        if (recorded_class) {
          EXPECT_EQ(without_id(sent.back()), without_id(line));
        }
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
  EXPECT_EQ(replayed, (std::set<std::string>{"cancel", "interleaved_progress", "refused",
                                             "session_release", "stt_decode", "tts_synthesis"}));
}

// The request the recorded `vad_frames` submit describes.
JobRequest recorded_vad_request(JobChannelHarness& h) {
  const Json input = golden("vad_frames").at(0).at("input");
  const auto bytes = input.at("payload_length").get<std::size_t>();
  const BufferRef buffer = h.manager->allocate(bytes).value();
  return JobRequest::create(JobIdentity{1, 1, 1}, JobClass::VadFrames,
                            VadFrames{kJobInputAudioFormat, PcmWindow{buffer, 0, bytes},
                                      input.at("frame_samples").get<std::uint32_t>(),
                                      input.at("frame_count").get<std::uint32_t>(), 1})
      .value();
}

TEST_F(JobChannelTest, AClassNoLaneRunsIsRefusedAndNothingIsWritten) {
  up();
  expect_refused(h.channel.submit(recorded_vad_request(h)), Error::Code::Unsupported,
                 "job_class_unsupported");
  settle();
  EXPECT_TRUE(peer.sent("job_submit", 0).empty());
}

TEST(JobChannelClasses, OnlyTheClassesTheLoadDeclaredAreTaken) {
  ScriptedSidecar peer(ScriptedSidecar::Options{.job_classes = {"stt_decode", "vad_frames"}});
  JobChannelHarness h;
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  h.channel.admit_jobs();
  expect_refused(h.channel.submit(JobChannelHarness::tts(1, 1)), Error::Code::Unsupported,
                 "job_class_unsupported");
  // Declared, but no lane runs it yet.
  expect_refused(h.channel.submit(recorded_vad_request(h)), Error::Code::Unsupported,
                 "job_class_unsupported");
  EXPECT_TRUE(h.submit(h.stt(2, 1)).has_value());
  EXPECT_TRUE(h.wait_kinds(2, {Accepted}));
}

TEST(JobChannelClasses, ALoadThatDeclaresNoClassALaneRunsDoesNotStartTheChannel) {
  // The runner refuses such a load itself; a sidecar that did not is not served.
  ScriptedSidecar peer(ScriptedSidecar::Options{.job_classes = {"vad_frames"}});
  JobChannelHarness h;
  const auto loaded = h.connect(peer.launcher());
  ASSERT_FALSE(loaded.has_value());
  EXPECT_EQ(loaded.error().code, Error::Code::Unsupported);
  EXPECT_EQ(loaded.error().context.value_or(""), "job_class_unsupported");
  EXPECT_EQ(h.unary("health_check").error().code, Error::Code::NotReady);
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

TEST_F(JobChannelTest, TheTableAndThePendingReleasesAreBounded) {
  up();
  peer.stop_reading();  // nothing is read, so nothing leaves the table
  for (std::uint64_t job = 1; job <= kMaxOpenJobs; ++job) {
    ASSERT_TRUE(h.submit(JobChannelHarness::tts(job, 1000 + job)).has_value()) << job;
  }
  expect_refused(h.channel.submit(JobChannelHarness::tts(99, 99)), Error::Code::ResourceExhausted,
                 "job_capacity_exhausted");
  for (std::uint64_t session = 1; session <= kMaxPendingSessionReleases; ++session) {
    ASSERT_TRUE(h.channel.release_session(session, 1).has_value()) << session;
  }
  expect_refused(h.channel.release_session(999, 1), Error::Code::ResourceExhausted,
                 "session_release_capacity_exhausted");
  EXPECT_TRUE(h.channel.release_session(1, 1).has_value()) << "a repeat is not a new release";
}

TEST_F(JobChannelTest, AFullQueueRefusesTheCallerAndKeepsNothingOfTheCall) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  peer.stop_reading();
  // The writer is inside a frame larger than the socket holds, so whatever
  // is queued from here on stays queued.
  ASSERT_TRUE(h.submit(h.stt(2, 1, kLargestWindow)).has_value());
  ASSERT_TRUE(h.wait([&] { return peer.unread_bytes() >= 65'536; }));
  // An exchange that gives up leaves its request behind.
  const std::chrono::milliseconds at_once{0};
  for (std::size_t queued = 0; queued < kMaxQueuedFrames; ++queued) {
    ASSERT_EQ(h.unary("health_check", at_once).error().code, Error::Code::Timeout) << queued;
  }

  expect_refused(h.channel.submit(h.stt(3, 1)), Error::Code::ResourceExhausted, "send_queue_full");
  expect_refused(h.channel.cancel(1), Error::Code::ResourceExhausted, "send_queue_full");
  expect_refused(h.channel.release_session(5, 1), Error::Code::ResourceExhausted,
                 "send_queue_full");
  const auto health = h.unary("health_check", at_once);
  ASSERT_FALSE(health.has_value());
  EXPECT_EQ(health.error().code, Error::Code::ResourceExhausted);
  EXPECT_EQ(health.error().context.value_or(""), "send_queue_full");

  h.stop(kSessionUnloaded);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  EXPECT_EQ(h.sink->kinds_for(2), (Kinds{Failed, Released}));
  EXPECT_EQ(h.sink->events().size(), 5U) << "the refused job was not kept";
  EXPECT_TRUE(h.sink->session_releases().empty()) << "nor the refused release";
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

TEST_F(JobChannelTest, ACallbackThatFindsTheQueueFullIsRefusedLikeAnyOtherCaller) {
  up();
  ASSERT_TRUE(h.submit(h.stt(1, 1, 3'200, /*progress_limit=*/1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  peer.stop_reading();
  // The writer is inside a frame larger than the socket holds, and the queue
  // behind it is filled by exchanges that give up.
  ASSERT_TRUE(h.submit(h.stt(2, 1, kLargestWindow)).has_value());
  ASSERT_TRUE(h.wait([&] { return peer.unread_bytes() >= 65'536; }));
  for (std::size_t queued = 0; queued < kMaxQueuedFrames; ++queued) {
    ASSERT_EQ(h.unary("health_check", std::chrono::milliseconds{0}).error().code,
              Error::Code::Timeout)
        << queued;
  }
  h.on_event = [this](const JobEvent& event) {
    if (event.kind() != Progress) {
      return;
    }
    expect_refused(h.channel.cancel(1), Error::Code::ResourceExhausted, "send_queue_full");
    expect_refused(h.channel.release_session(5, 1), Error::Code::ResourceExhausted,
                   "send_queue_full");
    noted = true;
  };
  // The sidecar still writes, and the reader still reads.
  peer.emit("job_progress", 1, {{"progress_sequence", 1}, {"result", transcript()}});
  ASSERT_TRUE(h.wait([&] { return noted.load(); }));
  h.stop(kSessionUnloaded);
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Accepted, Progress, Failed, Released}));
  EXPECT_TRUE(h.sink->session_releases().empty()) << "the refused release was not kept";
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

TEST(JobChannelSessions, OnlyTheLastAcknowledgedSessionsAreRemembered) {
  ScriptedSidecar peer;
  JobChannelHarness h;
  ASSERT_TRUE(h.connect(peer.launcher()).has_value());
  ASSERT_TRUE(h.channel.set_event_sink(h.sink).has_value());
  for (std::uint64_t session = 1; session <= kMaxRememberedSessions + 1; ++session) {
    ASSERT_TRUE(h.channel.release_session(session, 1).has_value());
    ASSERT_TRUE(h.wait([&] { return h.sink->session_releases().size() == session; })) << session;
  }
  // The second is still remembered. The first is past the memory the header
  // states, so its key is a new session's: a job for it is taken, and its
  // release is sent again and acknowledged again by the sidecar, which forgot
  // it too. The recording sink holds "at most once", as the caller has to.
  h.channel.admit_jobs();
  expect_refused(h.channel.submit(JobChannelHarness::tts(1, 2)), Error::Code::NotReady,
                 "session_releasing");
  ASSERT_TRUE(h.submit(JobChannelHarness::tts(2, 1)).has_value());
  ASSERT_TRUE(h.wait_kinds(2, {Accepted}));
  ASSERT_TRUE(h.channel.release_session(2, 1).has_value());
  ASSERT_TRUE(h.release(1).has_value());
  EXPECT_EQ(peer.sent("session_release", kMaxRememberedSessions + 2).size(),
            kMaxRememberedSessions + 2);
  ASSERT_TRUE(
      h.wait([&] { return h.sink->session_releases().size() == kMaxRememberedSessions + 2; }));
  EXPECT_EQ(h.sink->session_releases().back(), (std::pair<std::uint64_t, std::uint64_t>{1, 1}));
  EXPECT_EQ(h.sink->kinds_for(2), (Kinds{Accepted, CancelAcknowledged, Failed, Released}));
  h.expected_violations = {"duplicate_session_released 1"};
  EXPECT_EQ(h.sink->violations(), h.expected_violations);
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
  EXPECT_EQ(h.sink->kinds_for(2), (Kinds{Accepted, Failed, Released}));
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
  // However often it is asked, an exchange gets the error the connection
  // ended with: nothing is queued for a connection that has ended.
  for (std::size_t asked = 0; asked <= kMaxQueuedFrames; ++asked) {
    const auto health = h.unary("health_check");
    ASSERT_FALSE(health.has_value());
    ASSERT_EQ(health.error().code, Error::Code::InferenceFailed) << asked;
  }
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

TEST_F(JobChannelTest, AFrameThatDeclaresMoreThanAnAudioChunkEndsTheConnection) {
  up();
  ASSERT_TRUE(h.submit(JobChannelHarness::tts(1, 1)).has_value());
  ASSERT_EQ(peer.take_job(), 1U);
  ASSERT_TRUE(h.wait_kinds(1, {Accepted}));
  const std::size_t beyond = kMaxJobFramePayloadBytes + 2;
  peer.emit("job_completed", 1, {{"result", audio(beyond)}}, beyond);
  // Read whole, the frame would fault the job for its size and leave the
  // connection up.
  ASSERT_TRUE(h.wait_kinds(1, {Accepted, Failed}));
  EXPECT_EQ(failure_of(h, 1)->context.value_or(""), kBackendReset);
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

  const Json no_kind = answer_to({{"schema_version", "0.1"}, {"message_id", "tp-msg-y"}});
  EXPECT_EQ(no_kind.value("kind", ""), "error_event");
  EXPECT_EQ(no_kind.value("message_id", ""), "tp-msg-y");
  EXPECT_EQ(no_kind.value("error", Json::object()).value("code", ""), "config_invalid");
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
  EXPECT_EQ(h.sink->kinds_for(1), (Kinds{Failed, Released}));
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

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
