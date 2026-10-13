// SPDX-License-Identifier: Apache-2.0
//
// A serving worker whose deployment is a speech model, with streaming
// enabled: its stream sessions run as jobs of the backend, over gRPC in
// process, against a scripted sidecar and against the in-tree sidecar runner
// with its fixture backend.
//
// The runner cases are skipped when the Python package is not importable; the
// interpreter comes from TP_TEST_PYTHON, else `python3`.

#include <gtest/gtest.h>

#if !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(SpeechWorker, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <arpa/inet.h>
#include <grpcpp/grpcpp.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <sstream>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/serving/config.hpp"
#include "tensorplate/serving/serialization.hpp"
#include "tensorplate/serving/worker.hpp"
#include "tensorplate/stream/v1/session.grpc.pb.h"

#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "fixture_job_bridge.hpp"
#include "job_bridge_harness.hpp"
#include "recording_job_event_sink.hpp"
#include "scripted_sidecar.hpp"
#include "serving/mock_session.hpp"
#include "serving_http_client.hpp"
#include "stream_listener_fixture.hpp"

namespace {

using namespace std::chrono_literals;
using tensorplate::BackendEntry;
using tensorplate::Error;
using tensorplate::ExecutionSession;
using tensorplate::ExecutionSessionRuntimeHooks;
using tensorplate::ModelClass;
using tensorplate::ModelSpec;
using tensorplate::Result;
using tensorplate::ServingConfig;
using tensorplate::ServingWorker;
using tensorplate::SessionWithBridge;
using tensorplate::testing::ScriptedBackend;
using tensorplate::testing::ScriptedSidecar;
using tensorplate::testing::stream_listener::Client;
using tensorplate::testing::stream_listener::end_of;
using tensorplate::testing::stream_listener::eventually;
using tensorplate::testing::stream_listener::kDeployment;
using tensorplate::testing::stream_listener::open_event;
using Json = nlohmann::json;
namespace adapter = tensorplate::adapters::python_pytorch;
namespace wire = tensorplate::stream::v1;

/// 100 ms of the input format.
constexpr std::uint32_t kFrame = 1'600;

/// The fields of every line of `log` the worker wrote with `message`.
std::vector<Json> logged(const std::string& log, std::string_view message) {
  std::vector<Json> found;
  std::istringstream lines{log};
  for (std::string line; std::getline(lines, line);) {
    const auto entry = Json::parse(line, nullptr, false);
    if (entry.is_object() && entry.value("message", "") == message) {
      found.push_back(entry.value("fields", Json::object()));
    }
  }
  return found;
}

/// How many of `frames` are of `kind` and name `session_key`.
std::size_t count_of(const std::vector<Json>& frames, std::string_view kind,
                     std::uint64_t session_key) {
  return static_cast<std::size_t>(std::count_if(frames.begin(), frames.end(), [&](const Json& f) {
    return f.value("kind", "") == kind && f.value("session_key", std::uint64_t{0}) == session_key;
  }));
}

/// The kinds of the frames of `frames` that carry `job_id`, in order.
std::vector<std::string> kinds_of(const std::vector<Json>& frames, std::uint64_t job_id) {
  std::vector<std::string> kinds;
  for (const Json& frame : frames) {
    if (frame.value("job_id", std::uint64_t{0}) == job_id) {
      kinds.push_back(frame.value("kind", ""));
    }
  }
  return kinds;
}

ServingConfig streaming_config(ModelSpec model) {
  ServingConfig config;
  config.deployment.endpoint = kDeployment;
  config.deployment.use_mock_session = false;
  config.deployment.backend = std::string{model.backend_hint()};
  config.deployment.model = std::move(model);
  config.streaming.enabled = true;
  return config;
}

ModelSpec speech_model() {
  return ModelSpec::create("speech", ModelClass::Speech, "/dev/null", "python_pytorch").value();
}

/// A started worker, and a stream client's side of it.
struct Serving {
  explicit Serving(std::unique_ptr<ServingWorker> built) : worker(std::move(built)) {
    ::testing::internal::CaptureStderr();
    const auto started = worker->start();
    const auto bound = logged(::testing::internal::GetCapturedStderr(), "stream listener bound");
    EXPECT_TRUE(started.has_value());
    if (bound.empty()) {
      return;
    }
    listens = true;
    port = bound.front().at("port").get<std::uint16_t>();
    serves_sessions = bound.front().at("serves_sessions").get<bool>();
    stub = wire::SessionService::NewStub(grpc::CreateChannel("127.0.0.1:" + std::to_string(port),
                                                             grpc::InsecureChannelCredentials()));
  }

  [[nodiscard]] Client connect() const {
    Client client;
    client.context->set_deadline(std::chrono::system_clock::now() + 60s);
    client.call = stub->Session(client.context.get());
    return client;
  }

  /// A client whose Open was answered by Ready. The worker serves one session
  /// at a time, so the Open is asked again while the session before still
  /// holds its place, and for no other refusal.
  [[nodiscard]] Client ready(wire::SessionMode mode, wire::Ready* ready_out = nullptr) const {
    Client client;
    std::string refusal;
    bool admitted = false;
    (void)eventually([&] {
      client = connect();
      client.call->Write(open_event(mode));
      const auto first = client.read();
      if (first.has_value() && first->has_ready()) {
        EXPECT_EQ(first->sequence(), 1U) << "Ready is the session's first event";
        client.took(first->ready());
        if (ready_out != nullptr) {
          *ready_out = first->ready();
        }
        admitted = true;
        return true;
      }
      refusal = client.call->Finish().error_message();
      return refusal.find("FAILURE_REASON_SESSION_COUNT_LIMIT") == std::string::npos;
    });
    EXPECT_TRUE(admitted) << refusal;
    return client;
  }

  /// The status of a call whose Open the worker refuses.
  [[nodiscard]] grpc::Status refused(wire::SessionMode mode) const {
    auto client = connect();
    client.call->Write(open_event(mode));
    client.call->WritesDone();
    EXPECT_FALSE(client.read().has_value()) << "a refused Open has no event";
    return client.call->Finish();
  }

  /// Stops the worker. Returns what it logged meanwhile.
  [[nodiscard]] std::string stop() const {
    ::testing::internal::CaptureStderr();
    const auto code = worker->stop();
    auto log = ::testing::internal::GetCapturedStderr();
    EXPECT_EQ(code, tensorplate::ServingExitCode::Ok);
    return log;
  }

  /// Buffers the worker's buffer manager has out.
  [[nodiscard]] std::size_t buffers() const {
    return worker->buffer_manager().accounting().active_count;
  }

  std::unique_ptr<ServingWorker> worker;
  bool listens = false;
  std::uint16_t port = 0;
  bool serves_sessions = false;
  std::unique_ptr<wire::SessionService::Stub> stub;
};

/// A listening TCP socket on a loopback port of its own.
struct TakenPort {
  TakenPort() : fd(::socket(AF_INET, SOCK_STREAM, 0)) {
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
    EXPECT_EQ(::bind(fd, reinterpret_cast<const sockaddr*>(&address), sizeof(address)), 0);
    EXPECT_EQ(::listen(fd, 1), 0);
    socklen_t length = sizeof(address);
    // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
    EXPECT_EQ(::getsockname(fd, reinterpret_cast<sockaddr*>(&address), &length), 0);
    port = ntohs(address.sin_port);
  }
  ~TakenPort() { give_back(); }
  TakenPort(const TakenPort&) = delete;
  TakenPort& operator=(const TakenPort&) = delete;

  void give_back() {
    if (fd >= 0) {
      (void)::close(fd);
      fd = -1;
    }
  }

  int fd;
  std::uint16_t port = 0;
};

/// The SessionClosed that `event` must be: the next event of a session whose
/// output has all been read. Any other event there is a failure, not skipped.
wire::SessionClosed closed_of(const std::optional<wire::ServerEvent>& event) {
  if (!event.has_value()) {
    ADD_FAILURE() << "the stream ended without SessionClosed";
    return {};
  }
  if (!event->has_session_closed()) {
    ADD_FAILURE() << "an event the session did not owe precedes SessionClosed: "
                  << event->ShortDebugString();
    return {};
  }
  return event->session_closed();
}

/// One session's events after Ready, as its client reads them, each held to
/// the schema's rules for every event: `sequence` is one more than the event
/// before, and an Accepted names an input the client sent, never an earlier
/// one than the Accepted before it, with credit inside what Ready granted and
/// no more of it used than the inputs it covers hold.
class Reader {
 public:
  /// `inputs` Audio or TextSegment events of `input_bytes` each were sent,
  /// with the client sequences from 2 on.
  Reader(Client& client, const wire::Ready& ready, std::uint64_t inputs, std::uint64_t input_bytes)
      : client_(client),
        granted_(ready.status().input_credit()),
        last_input_(1 + inputs),
        input_bytes_(input_bytes) {}

  /// The next event that is not an Accepted; none at the end of the stream.
  std::optional<wire::ServerEvent> next() {
    for (;;) {
      auto event = client_.read();
      if (!event.has_value()) {
        return event;
      }
      EXPECT_EQ(event->sequence(), ++sequence_) << event->ShortDebugString();
      if (!event->has_accepted()) {
        return event;
      }
      check(event->accepted());
      accepted_ = event->accepted();
    }
  }

  /// Holds that every input was accepted and that the server had consumed
  /// all of it when the last event was read.
  void expect_all_input_taken() const {
    ASSERT_TRUE(accepted_.has_value()) << "no Accepted was read";
    EXPECT_EQ(accepted_->accepted_sequence(), last_input_);
    EXPECT_EQ(accepted_->input_credit().bytes().used(), 0U);
    EXPECT_EQ(accepted_->input_credit().waiting_segments().used(), 0U);
  }

  /// The sequence of the last input sent.
  [[nodiscard]] std::uint64_t last_input() const { return last_input_; }

 private:
  void check(const wire::Accepted& accepted) const {
    SCOPED_TRACE(accepted.ShortDebugString());
    const std::uint64_t covered = accepted.accepted_sequence();
    ASSERT_GE(covered, accepted_.has_value() ? accepted_->accepted_sequence() : 2U);
    ASSERT_LE(covered, last_input_);
    const auto& credit = accepted.input_credit();
    EXPECT_EQ(credit.bytes().limit(), granted_.bytes().limit());
    EXPECT_LE(credit.bytes().used(), (covered - 1) * input_bytes_);
    EXPECT_EQ(credit.waiting_segments().limit(), granted_.waiting_segments().limit());
    EXPECT_LE(credit.waiting_segments().used(), credit.waiting_segments().limit());
  }

  Client& client_;
  wire::InputCredit granted_;
  std::uint64_t last_input_;
  std::uint64_t input_bytes_;
  /// Of the last event read: Ready's to begin with.
  std::uint64_t sequence_ = 1;
  std::optional<wire::Accepted> accepted_;
};

/// What a test can know of the Ready that answers `open_event(mode)`.
void expect_ready(const wire::Ready& ready, wire::SessionMode mode) {
  SCOPED_TRACE(ready.ShortDebugString());
  EXPECT_FALSE(ready.session_id().empty());
  EXPECT_LE(ready.session_id().size(), 64U);
  // The worker serves no generation.
  EXPECT_EQ(ready.generation(), 0U);
  EXPECT_TRUE(ready.descriptor_digest().empty());
  const auto& limits = ready.limits();
  EXPECT_NE(limits.idle_timeout_ms(), 0U);
  EXPECT_NE(limits.heartbeat_interval_ms(), 0U);
  EXPECT_NE(limits.liveness_timeout_ms(), 0U);
  EXPECT_NE(limits.max_duration_ms(), 0U);
  EXPECT_NE(limits.finalize_deadline_ms(), 0U);
  EXPECT_NE(limits.output_stall_timeout_ms(), 0U);
  EXPECT_NE(ready.remaining_duration_ms(), 0U);
  EXPECT_LE(ready.remaining_duration_ms(), limits.max_duration_ms());
  const bool audio_in = mode == wire::SESSION_MODE_STT_STREAMING;
  EXPECT_EQ(limits.has_speech_to_text(), audio_in);
  EXPECT_EQ(limits.has_text_to_speech(), !audio_in);
  EXPECT_EQ(ready.has_input_format(), audio_in);
  EXPECT_EQ(ready.has_output_format(), !audio_in);
  const auto& credit = ready.status().input_credit();
  if (audio_in) {
    EXPECT_NE(limits.speech_to_text().min_frame_ms(), 0U);
    EXPECT_NE(limits.speech_to_text().max_frame_ms(), 0U);
    EXPECT_NE(limits.speech_to_text().max_utterance_ms(), 0U);
    EXPECT_EQ(ready.input_format().SerializeAsString(),
              open_event(mode).open().input_format().SerializeAsString());
    EXPECT_EQ(credit.waiting_segments().limit(), 0U);
  } else {
    EXPECT_NE(limits.text_to_speech().max_segment_text_bytes(), 0U);
    EXPECT_NE(limits.text_to_speech().max_segment_audio_ms(), 0U);
    EXPECT_NE(limits.text_to_speech().max_synthesis_text_bytes(), 0U);
    EXPECT_NE(limits.text_to_speech().max_synthesis_audio_ms(), 0U);
    // The fixture's speech.
    EXPECT_EQ(ready.output_format().encoding(), wire::AUDIO_ENCODING_PCM_S16LE);
    EXPECT_EQ(ready.output_format().sample_rate_hz(), 24'000U);
    EXPECT_EQ(ready.output_format().channels(), 1U);
    EXPECT_NE(credit.waiting_segments().limit(), 0U);
  }
  EXPECT_EQ(ready.status().state(), wire::SESSION_STATE_ACTIVE);
  EXPECT_NE(credit.bytes().limit(), 0U);
  EXPECT_EQ(credit.bytes().used(), 0U);
  EXPECT_EQ(credit.waiting_segments().used(), 0U);
  EXPECT_EQ(ready.capabilities_size(), 0);
}

/// The session's state, as the answer to a StatusRequest gives it.
wire::SessionState state_of(Client& client) {
  wire::ClientEvent request;
  request.mutable_status_request();
  EXPECT_TRUE(client.send(request));
  const auto answer = client.next();
  if (!answer.has_value() || !answer->has_status()) {
    ADD_FAILURE() << "a StatusRequest is answered by a status";
    return wire::SESSION_STATE_UNSPECIFIED;
  }
  return answer->status().state();
}

/// Reads a whole segment of the fixture's speech, which is silence of
/// kFixtureSpeechBytesPerTextByte bytes for each byte of text: 20 ms chunks
/// in order, then the segment's completion. `client` is a Client or a Reader.
template <typename Events>
void expect_segment(Events& client, std::uint64_t segment_id, std::size_t text_bytes,
                    const wire::AudioFormat& format) {
  ASSERT_EQ(format.sample_rate_hz(), 24'000U);
  const std::uint64_t total = text_bytes * tensorplate::testing::kFixtureSpeechBytesPerTextByte / 2;
  const std::uint64_t chunk_samples = format.sample_rate_hz() / 50;
  std::uint64_t offset = 0;
  std::uint32_t chunks = 0;
  while (offset < total) {
    const auto event = client.next();
    ASSERT_TRUE(event.has_value());
    ASSERT_TRUE(event->has_audio_chunk()) << event->ShortDebugString();
    const auto& chunk = event->audio_chunk();
    EXPECT_EQ(chunk.segment_id(), segment_id);
    EXPECT_EQ(chunk.chunk_index(), chunks);
    EXPECT_EQ(chunk.segment_sample_offset(), offset);
    EXPECT_EQ(chunk.format().SerializeAsString(), format.SerializeAsString());
    const std::uint64_t samples = std::min(chunk_samples, total - offset);
    ASSERT_TRUE(chunk.data() == std::string(samples * 2, '\0'))
        << "chunk " << chunks << " is not the segment's speech";
    offset += samples;
    ++chunks;
  }
  const auto completed = client.next();
  ASSERT_TRUE(completed.has_value());
  ASSERT_TRUE(completed->has_segment_completed()) << completed->ShortDebugString();
  EXPECT_EQ(completed->segment_completed().segment_id(), segment_id);
  EXPECT_EQ(completed->segment_completed().total_samples(), total);
  EXPECT_EQ(completed->segment_completed().chunk_count(), chunks);
}

/// One speech-to-text exchange: Open, audio, a Finalize, the utterance's
/// endpoint and its final transcript, a half-close and the session closed.
void transcribes_one_utterance(const Serving& served) {
  wire::Ready ready;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING, &ready);
  ASSERT_NO_FATAL_FAILURE(expect_ready(ready, wire::SESSION_MODE_STT_STREAMING));
  ASSERT_TRUE(client.send_audio(3 * kFrame, kFrame));
  const auto asked = client.finalize();
  Reader events{client, ready, 3, 2 * std::uint64_t{kFrame}};

  const auto endpoint = events.next();
  ASSERT_TRUE(endpoint.has_value());
  // The dispatch takes each frame before it ends the utterance.
  ASSERT_NO_FATAL_FAILURE(events.expect_all_input_taken());
  ASSERT_TRUE(endpoint->has_endpoint_detected()) << endpoint->ShortDebugString();
  EXPECT_EQ(endpoint->endpoint_detected().reason(), wire::ENDPOINT_REASON_CLIENT_FINALIZE);
  EXPECT_EQ(endpoint->endpoint_detected().utterance_id(), 1U);
  EXPECT_EQ(endpoint->endpoint_detected().end_sample_offset(), 3U * kFrame);
  const auto transcript = events.next();
  ASSERT_TRUE(transcript.has_value());
  ASSERT_TRUE(transcript->has_final_transcript()) << transcript->ShortDebugString();
  const auto& final = transcript->final_transcript();
  EXPECT_EQ(final.utterance_id(), 1U);
  EXPECT_EQ(final.end_sample_offset(), 3U * kFrame);
  EXPECT_EQ(final.finalize_sequence(), asked);
  ASSERT_EQ(final.segments_size(), 1);
  EXPECT_EQ(final.segments(0).text(), "hello");
  EXPECT_EQ(final.segments(0).start_us(), 0U);
  EXPECT_EQ(final.segments(0).end_us(), 300'000U);

  ASSERT_TRUE(client.call->WritesDone());
  const auto closed = closed_of(events.next());
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_FALSE(closed.has_cause()) << closed.ShortDebugString();
  EXPECT_EQ(closed.admission(), wire::ADMISSION_ADMITTED);
  EXPECT_EQ(closed.last_accepted_sequence(), events.last_input());
  EXPECT_EQ(closed.totals().input_audio_samples(), 3U * kFrame);
  EXPECT_EQ(closed.totals().utterances_completed(), 1U);
  ASSERT_NO_FATAL_FAILURE(events.expect_all_input_taken());
  EXPECT_TRUE(end_of(client).ok());
}

/// One text-to-speech segment: Open, a text segment, its audio chunks and
/// its completion, a half-close and the session closed.
void synthesizes_one_segment(const Serving& served) {
  wire::Ready ready;
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING, &ready);
  ASSERT_NO_FATAL_FAILURE(expect_ready(ready, wire::SESSION_MODE_TTS_STREAMING));
  // 1,200 samples: two whole chunks and one of 240.
  const std::string text = "fifteen bytes!!";
  ASSERT_TRUE(client.send_text(1, text));
  Reader events{client, ready, 1, text.size()};
  ASSERT_NO_FATAL_FAILURE(expect_segment(events, 1, text.size(), ready.output_format()));
  // The dispatch starts the segment before it has any of its audio.
  ASSERT_NO_FATAL_FAILURE(events.expect_all_input_taken());

  ASSERT_TRUE(client.call->WritesDone());
  const auto closed = closed_of(events.next());
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_FALSE(closed.has_cause()) << closed.ShortDebugString();
  EXPECT_EQ(closed.admission(), wire::ADMISSION_ADMITTED);
  EXPECT_EQ(closed.last_accepted_sequence(), events.last_input());
  EXPECT_EQ(closed.totals().input_text_bytes(), text.size());
  EXPECT_EQ(closed.totals().segments_completed(), 1U);
  EXPECT_EQ(closed.totals().output_audio_samples(), 1'200U);
  EXPECT_TRUE(end_of(client).ok());
}

std::string infer_body() {
  const std::vector<std::byte> payload(4);
  Json input;
  input["name"] = "image";
  input["tensor"] = {{"dtype", "uint8"}, {"shape", {2, 2}}, {"byte_size", 4}};
  input["payload_b64"] = tensorplate::serving::base64_encode(payload.data(), payload.size());
  Json body;
  body["schema_version"] = "0.1";
  body["request_id"] = "unary";
  body["endpoint"] = kDeployment;
  body["inputs"] = Json::array({input});
  return body.dump();
}

void expect_unsupported(const grpc::Status& status) {
  EXPECT_EQ(status.error_code(), grpc::StatusCode::FAILED_PRECONDITION);
  EXPECT_NE(status.error_message().find("FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY"),
            std::string::npos)
      << status.error_message();
}

/// The Python/PyTorch adapter's backend over a scripted sidecar whose backend
/// a thread plays, in a registry of its own.
class SpeechWorkerScripted : public ::testing::Test {
 protected:
  SpeechWorkerScripted() = default;
  explicit SpeechWorkerScripted(ScriptedSidecar::Options options) : sidecar(std::move(options)) {}

  static adapter::PythonPytorchConfig patient() {
    adapter::PythonPytorchConfig config;
    config.startup_timeout = ScriptedSidecar::kPatience;
    config.health_timeout = ScriptedSidecar::kPatience;
    return config;
  }

  /// Registers the backend. Without `with_bridge` the entry has no factory
  /// for a session with its bridge.
  void register_backend(bool with_bridge = true) {
    BackendEntry entry{
        std::string{adapter::kBackendName},
        adapter::make_python_pytorch_capability(),
        [this](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
          ++sessions_built;
          return adapter::make_python_pytorch_session(hooks, patient(), sidecar.launcher());
        },
    };
    if (with_bridge) {
      entry.session_with_bridge_factory =
          [this](ExecutionSessionRuntimeHooks hooks) -> Result<SessionWithBridge> {
        ++pairs_built;
        if (pair_refusal.has_value()) {
          return tensorplate::unexpected(*pair_refusal);
        }
        auto built =
            adapter::make_python_pytorch_session_with_bridge(hooks, patient(), sidecar.launcher());
        if (sink_taken) {
          EXPECT_TRUE(
              built.bridge
                  ->set_event_sink(std::make_shared<tensorplate::testing::RecordingJobEventSink>())
                  .has_value());
        }
        return SessionWithBridge{std::move(built.session), std::move(built.bridge)};
      };
    }
    ASSERT_TRUE(registry.register_backend(std::move(entry)).has_value());
  }

  /// A worker over `config` and the registry; null when it could not be built.
  std::unique_ptr<ServingWorker> worker(const ServingConfig& config) {
    auto created = ServingWorker::create(config, registry);
    EXPECT_TRUE(created.has_value()) << (created.has_value() ? "" : created.error().message);
    return created.has_value() ? std::move(created).value() : nullptr;
  }

  /// The `load_model` request the sidecar was sent.
  Json load_request() {
    const auto loads = sidecar.sent("load_model", 1);
    EXPECT_EQ(loads.size(), 1U);
    return loads.empty() ? Json::object() : loads.front();
  }

  /// Waits until the backend runs a job. Returns its submit; job 0 for none.
  Json running_job() {
    std::uint64_t running = 0;
    EXPECT_TRUE(eventually([&] {
      running = sidecar.lane().running;
      return running != 0;
    }));
    // The sidecar records a frame once it has acted on it, which is after the
    // backend could take the job: sent() waits for the first record, and the
    // job a case asks about here is the first one submitted.
    for (const Json& submit : sidecar.sent("job_submit", 1)) {
      if (submit.value("job_id", std::uint64_t{0}) == running) {
        return submit;
      }
    }
    ADD_FAILURE() << "no submit was recorded for the running job " << running;
    return Json{{"job_id", 0}, {"session_key", 0}};
  }

  /// The `job_cancel` frames the worker sent. A session's release is what
  /// cancels its job at the sidecar, so the worker sends none. Read after the
  /// stop, as cancel_requests() is: every frame sent ahead of the unload has
  /// been acted on and recorded by then.
  std::size_t job_cancels() { return sidecar.sent("job_cancel", 0).size(); }

  /// Holds that the worker submitted one job, which ran to its end and was
  /// never asked to be cancelled.
  void expect_one_job_that_ran_uncancelled() {
    const auto submits = sidecar.sent("job_submit", 0);
    ASSERT_EQ(submits.size(), 1U);
    const auto job_id = submits.front().value("job_id", std::uint64_t{0});
    EXPECT_EQ(kinds_of(sidecar.written(), job_id),
              (std::vector<std::string>{"job_accepted", "job_completed", "job_released"}));
    EXPECT_EQ(sidecar.cancel_requests(job_id), 0U);
    EXPECT_EQ(job_cancels(), 0U);
  }

  ScriptedSidecar sidecar;
  /// Reset by a case that plays the backend itself.
  std::optional<ScriptedBackend> backend{std::in_place, sidecar};
  tensorplate::BackendRegistry registry;
  int sessions_built = 0;
  int pairs_built = 0;
  /// What the factory of a session with its bridge answers instead of a pair.
  std::optional<Error> pair_refusal;
  /// The bridge is handed over with an event sink already registered.
  bool sink_taken = false;
};

/// The runner declares the decode's job class and no other.
class SpeechWorkerDecodeOnly : public SpeechWorkerScripted {
 protected:
  SpeechWorkerDecodeOnly() : SpeechWorkerScripted(options()) {}
  static ScriptedSidecar::Options options() {
    ScriptedSidecar::Options options;
    options.job_classes = {"stt_decode"};
    return options;
  }
};

/// The runner lists no capability, so it offers no speech jobs.
class SpeechWorkerNoSpeechJobs : public SpeechWorkerScripted {
 protected:
  SpeechWorkerNoSpeechJobs() : SpeechWorkerScripted(options()) {}
  static ScriptedSidecar::Options options() {
    ScriptedSidecar::Options options;
    options.capabilities = {};
    return options;
  }
};

/// The runner offers speech jobs and has no job class a lane runs.
class SpeechWorkerNoJobClass : public SpeechWorkerScripted {
 protected:
  SpeechWorkerNoJobClass() : SpeechWorkerScripted(options()) {}
  static ScriptedSidecar::Options options() {
    ScriptedSidecar::Options options;
    options.job_classes = {};
    return options;
  }
};

TEST_F(SpeechWorkerScripted, OneUtteranceIsTranscribed) {
  register_backend();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  EXPECT_EQ(pairs_built, 1);
  EXPECT_EQ(sessions_built, 0);
  EXPECT_EQ(load_request().value("capabilities", Json::array()), Json::array({"speech_jobs_v1"}));
  const Serving served{std::move(built)};
  ASSERT_TRUE(served.serves_sessions);
  ASSERT_NO_FATAL_FAILURE(transcribes_one_utterance(served));
  EXPECT_TRUE(eventually([&] { return served.buffers() == 0; }));

  const auto stopped = logged(served.stop(), "speech dispatch stopped");
  ASSERT_EQ(stopped.size(), 1U);
  EXPECT_EQ(stopped.front().value("jobs_submitted", 0), 1);
  EXPECT_EQ(stopped.front().value("jobs_completed", 0), 1);
  EXPECT_EQ(stopped.front().value("jobs_failed", -1), 0);
  EXPECT_EQ(stopped.front().value("jobs_discarded", -1), 0);
  EXPECT_EQ(stopped.front().value("releases_refused", -1), 0);
  EXPECT_TRUE(stopped.front().contains("longest_release_wait_ms"));
  EXPECT_EQ(sidecar.sent("unload", 1).size(), 1U);
  expect_one_job_that_ran_uncancelled();
}

TEST_F(SpeechWorkerScripted, OneSegmentIsSynthesized) {
  register_backend();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  ASSERT_TRUE(served.serves_sessions);
  ASSERT_NO_FATAL_FAILURE(synthesizes_one_segment(served));
  EXPECT_TRUE(eventually([&] { return served.buffers() == 0; }));
  (void)served.stop();
  expect_one_job_that_ran_uncancelled();
}

// The backend holds the segment's job, so the cancel finds it running.
TEST_F(SpeechWorkerScripted, ACancelMidSynthesisEndsTheSessionAndReleasesItsJobOnce) {
  register_backend();
  backend->hold();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING);
  ASSERT_TRUE(client.send_text(1, "held"));
  const Json job = running_job();
  const auto job_id = job.at("job_id").get<std::uint64_t>();
  const auto session_key = job.at("session_key").get<std::uint64_t>();
  ASSERT_NE(job_id, 0U);

  wire::ClientEvent cancel;
  cancel.mutable_cancel();
  ASSERT_TRUE(client.send(cancel));
  const auto accepted = client.next();
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->has_cancel_accepted()) << accepted->ShortDebugString();
  // The session is released while the backend still runs its job.
  ASSERT_EQ(sidecar.sent("session_release", 1).size(), 1U);
  EXPECT_EQ(sidecar.lane().running, job_id);

  backend->release();
  const auto closed = closed_of(client.next());
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_CLIENT_CANCELLED);
  EXPECT_EQ(closed.totals().segments_completed(), 0U);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::CANCELLED);

  // The lane and the session's place are free: the next session is served.
  ASSERT_NO_FATAL_FAILURE(synthesizes_one_segment(served));
  const auto stopped = logged(served.stop(), "speech dispatch stopped");
  ASSERT_EQ(stopped.size(), 1U);
  EXPECT_EQ(stopped.front().value("jobs_submitted", 0), 2);
  EXPECT_EQ(stopped.front().value("jobs_completed", 0), 1);
  EXPECT_EQ(stopped.front().value("jobs_failed", -1), 0);
  EXPECT_EQ(stopped.front().value("jobs_discarded", 0), 1) << "the cancelled session's job";
  EXPECT_EQ(count_of(sidecar.sent("session_release", 0), "session_release", session_key), 1U);
  // Asked once, by that release; the next session's job was never asked.
  const auto submits = sidecar.sent("job_submit", 0);
  ASSERT_EQ(submits.size(), 2U);
  EXPECT_EQ(sidecar.cancel_requests(job_id), 1U);
  EXPECT_EQ(sidecar.cancel_requests(submits.back().value("job_id", std::uint64_t{0})), 0U);
  EXPECT_EQ(job_cancels(), 0U);
  const auto written = sidecar.written();
  EXPECT_EQ(kinds_of(written, job_id),
            (std::vector<std::string>{"job_accepted", "job_cancel_acknowledged", "job_failed",
                                      "job_released"}));
  EXPECT_EQ(count_of(written, "session_released", session_key), 1U);
  EXPECT_EQ(served.buffers(), 0U);
}

// gRPC may report a call that was cancelled as the end of the client's stream
// before it reports the cancellation. The open utterance is then ended as a
// half-close ends it, and its decode is submitted ahead of the release that
// cancels it: both orders are held here.
TEST_F(SpeechWorkerScripted, AClientThatGoesAwayMidUtteranceEndsItsSession) {
  register_backend();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  {
    auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
    ASSERT_TRUE(client.send_audio(kFrame));
    const auto last = client.sequence;
    while (client.accepted.empty() || client.accepted.back().accepted_sequence() < last) {
      ASSERT_TRUE(client.read().has_value()) << "the stream ended before the frame was accepted";
    }
    client.context->TryCancel();
    (void)client.call->Finish();
  }
  const auto first_release = sidecar.sent("session_release", 1);
  ASSERT_EQ(first_release.size(), 1U);
  const auto lost = first_release.front().value("session_key", std::uint64_t{0});
  // Read while the worker is up and before the next session: a decode the
  // lost session submitted has given its audio back.
  EXPECT_TRUE(eventually([&] { return served.buffers() == 0; }));
  // A submit of the lost session precedes its release, so none follows.
  const auto submitted_by_the_lost_session =
      count_of(sidecar.sent("job_submit", 0), "job_submit", lost);
  EXPECT_LE(submitted_by_the_lost_session, 1U) << "the open utterance, in the order that ends it";

  // Its place is free: the next session is served.
  ASSERT_NO_FATAL_FAILURE(transcribes_one_utterance(served));
  const auto stopped = logged(served.stop(), "speech dispatch stopped");
  ASSERT_EQ(stopped.size(), 1U);
  EXPECT_EQ(stopped.front().value("releases_refused", -1), 0);
  const auto releases = sidecar.sent("session_release", 0);
  ASSERT_EQ(releases.size(), 2U) << "one release for each session";
  const auto next = releases.back().value("session_key", std::uint64_t{0});
  EXPECT_NE(next, lost);
  EXPECT_EQ(count_of(releases, "session_release", lost), 1U);
  EXPECT_EQ(count_of(releases, "session_release", next), 1U);
  const auto written = sidecar.written();
  const auto submits = sidecar.sent("job_submit", 0);
  EXPECT_EQ(count_of(submits, "job_submit", lost), submitted_by_the_lost_session);
  EXPECT_EQ(count_of(submits, "job_submit", next), 1U);
  EXPECT_EQ(submits.size(), submitted_by_the_lost_session + 1) << "a job of neither session";
  for (const Json& submit : submits) {
    const auto job_id = submit.at("job_id").get<std::uint64_t>();
    const auto kinds = kinds_of(written, job_id);
    EXPECT_EQ(std::count(kinds.begin(), kinds.end(), "job_released"), 1) << submit.dump();
    // The lost session's decode may have ended before the release found it.
    const bool of_the_lost_session = submit.value("session_key", std::uint64_t{0}) == lost;
    EXPECT_LE(sidecar.cancel_requests(job_id), of_the_lost_session ? 1U : 0U) << submit.dump();
  }
  EXPECT_EQ(count_of(written, "session_released", lost), 1U);
  EXPECT_EQ(count_of(written, "session_released", next), 1U);
  EXPECT_EQ(job_cancels(), 0U);
  EXPECT_EQ(served.buffers(), 0U);
}

// The backend holds the utterance's job, so the client goes while it runs.
TEST_F(SpeechWorkerScripted, AClientThatGoesAwayWhileItsUtteranceIsDecodedLeavesNothingHeld) {
  register_backend();
  backend->hold();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  std::uint64_t job_id = 0;
  std::uint64_t session_key = 0;
  {
    auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
    ASSERT_TRUE(client.send_audio(kFrame));
    (void)client.finalize();
    const Json job = running_job();
    job_id = job.at("job_id").get<std::uint64_t>();
    session_key = job.at("session_key").get<std::uint64_t>();
    ASSERT_NE(job_id, 0U);
    EXPECT_EQ(served.buffers(), 1U) << "the job's audio";
    client.context->TryCancel();
    (void)client.call->Finish();
  }
  ASSERT_EQ(sidecar.sent("session_release", 1).size(), 1U);
  EXPECT_EQ(sidecar.lane().running, job_id);
  EXPECT_EQ(served.buffers(), 1U) << "given back while the backend still runs the job";

  backend->release();
  EXPECT_TRUE(eventually([&] { return served.buffers() == 0; }));
  ASSERT_NO_FATAL_FAILURE(transcribes_one_utterance(served));
  (void)served.stop();
  EXPECT_EQ(count_of(sidecar.sent("session_release", 0), "session_release", session_key), 1U);
  EXPECT_EQ(sidecar.cancel_requests(job_id), 1U) << "asked once, by that release";
  EXPECT_EQ(job_cancels(), 0U);
  const auto written = sidecar.written();
  EXPECT_EQ(kinds_of(written, job_id),
            (std::vector<std::string>{"job_accepted", "job_cancel_acknowledged", "job_failed",
                                      "job_released"}));
  EXPECT_EQ(count_of(written, "session_released", session_key), 1U);
  EXPECT_EQ(served.buffers(), 0U);
}

TEST_F(SpeechWorkerScripted, AShutdownDrainsTheOpenSessionAndDeliversTheSegmentInFlight) {
  register_backend();
  backend->hold();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  wire::Ready ready;
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING, &ready);
  const std::string text = "fifteen bytes!!";
  ASSERT_TRUE(client.send_text(1, text));
  (void)running_job();

  served.worker->shutdown("test");
  EXPECT_EQ(state_of(client), wire::SESSION_STATE_DRAINING);
  backend->release();
  ASSERT_NO_FATAL_FAILURE(expect_segment(client, 1, text.size(), ready.output_format()));
  const auto closed = closed_of(client.next());
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_WORKER_SHUTDOWN);
  EXPECT_EQ(closed.totals().segments_completed(), 1U);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::UNAVAILABLE);
  (void)served.stop();
  expect_one_job_that_ran_uncancelled();
  EXPECT_EQ(served.buffers(), 0U);
}

// The utterance's job outlasts the drain window. The stop releases the
// session, and the unload it runs afterwards is what ends the job, with the
// stopped dispatch and the bridge still in place: the job's audio is out
// while the unload waits behind the job and back once the stop returns.
TEST_F(SpeechWorkerScripted, AStopAtTheEndOfTheDrainWindowLeavesTheJobToTheUnload) {
  register_backend();
  backend.reset();
  auto config = streaming_config(speech_model());
  config.shutdown.drain_deadline = 300ms;
  auto built = worker(config);
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};

  // The backend, played here: it ends its job only once an unload waits
  // behind it, and reads the worker's buffers at that moment.
  struct Played {
    std::atomic<bool> abandoned{false};
    std::atomic<bool> ended_its_job{false};
    std::atomic<std::size_t> buffers_at_unload{0};
    std::thread thread;
    void finish() {
      if (thread.joinable()) {
        thread.join();
      }
    }
    ~Played() {
      abandoned = true;
      finish();
    }
  } played;
  played.thread = std::thread([&] {
    ScriptedSidecar::TakenJob taken;
    while (taken.job_id == 0 && !played.abandoned) {
      taken = sidecar.take(20ms);
    }
    while (!played.abandoned && !sidecar.lane().unload_waits) {
      std::this_thread::sleep_for(1ms);
    }
    if (taken.job_id != 0 && !played.abandoned) {
      played.buffers_at_unload = served.buffers();
      played.ended_its_job = true;
      sidecar.complete(taken.job_id, Json{{"type", "transcript"},
                                          {"text", "hello"},
                                          {"tokens", {1}},
                                          {"words", Json::array()}});
    }
  });

  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.send_audio(kFrame));
  (void)client.finalize();
  const Json job = running_job();
  const auto job_id = job.at("job_id").get<std::uint64_t>();
  const auto session_key = job.at("session_key").get<std::uint64_t>();
  ASSERT_NE(job_id, 0U);

  served.worker->shutdown("test");
  EXPECT_EQ(state_of(client), wire::SESSION_STATE_DRAINING);
  EXPECT_EQ(served.buffers(), 1U) << "the job's audio";
  const auto stopped = logged(served.stop(), "speech dispatch stopped");
  // Said before the join: the backend ends its job only behind an unload, so
  // without one the join would wait for good.
  EXPECT_TRUE(played.ended_its_job) << "the stop returned without an unload behind the job";
  played.abandoned = true;
  played.finish();
  EXPECT_EQ(played.buffers_at_unload, 1U) << "given back while the backend still ran the job";
  EXPECT_EQ(served.buffers(), 0U) << "the job's audio outlived the unload";

  // Of this session the client read Ready, its frame accepted and the status
  // above. Nothing follows the stop but the end of the call.
  ASSERT_FALSE(client.accepted.empty());
  EXPECT_EQ(client.accepted.back().accepted_sequence(), 2U);
  // The schema promises a terminal event; the listener does not write one here yet.
  std::vector<std::string> read_after_the_stop;
  while (const auto event = client.read()) {
    read_after_the_stop.push_back(event->ShortDebugString());
  }
  EXPECT_EQ(read_after_the_stop, std::vector<std::string>{});
  // The server cancels the call as it shuts down; the client reads that as
  // the server going away or as the cancellation itself.
  const auto code = client.call->Finish().error_code();
  EXPECT_TRUE(code == grpc::StatusCode::UNAVAILABLE || code == grpc::StatusCode::CANCELLED) << code;

  const auto written = sidecar.written();
  EXPECT_EQ(count_of(sidecar.sent("session_release", 0), "session_release", session_key), 1U);
  EXPECT_EQ(sidecar.cancel_requests(job_id), 1U) << "asked once, by that release";
  EXPECT_EQ(job_cancels(), 0U);
  // Cancelled by the release, ended only once the unload waited behind it.
  EXPECT_EQ(kinds_of(written, job_id),
            (std::vector<std::string>{"job_accepted", "job_cancel_acknowledged", "job_failed",
                                      "job_released"}));
  EXPECT_EQ(count_of(written, "session_released", session_key), 1U);
  ASSERT_FALSE(written.empty());
  EXPECT_EQ(written.back().value("kind", ""), "unload_response");
  ASSERT_EQ(stopped.size(), 1U);
  EXPECT_EQ(stopped.front().value("jobs_submitted", 0), 1);
  EXPECT_EQ(stopped.front().value("jobs_completed", -1), 0);
}

// The backend takes the job and then refuses it: a failed job.
TEST_F(SpeechWorkerScripted, AJobTheBackendRefusesFailsItsSession) {
  register_backend();
  sidecar.refuse_next_submit();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING);
  ASSERT_TRUE(client.send_text(1, "refused"));
  const auto closed = closed_of(client.next());
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::FAILED_PRECONDITION);

  const auto stopped = logged(served.stop(), "speech dispatch stopped");
  ASSERT_EQ(stopped.size(), 1U);
  EXPECT_EQ(stopped.front().value("jobs_submitted", 0), 1);
  EXPECT_EQ(stopped.front().value("jobs_completed", -1), 0);
  EXPECT_EQ(stopped.front().value("jobs_failed", 0), 1);
  EXPECT_EQ(stopped.front().value("jobs_discarded", -1), 0);
}

// A worker whose listener never started has a dispatch no manager was
// attached to. What this holds is that its stop does not wait for one.
TEST_F(SpeechWorkerScripted, AWorkerThatWasNeverStartedUnloadsItsSession) {
  register_backend();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  EXPECT_EQ(pairs_built, 1);
  built.reset();
  EXPECT_EQ(sidecar.sent("unload", 1).size(), 1U);
  EXPECT_TRUE(sidecar.reaped());
}

// The listener's failed start stops the dispatch, which is attached once: a
// later start could only admit sessions nothing serves, so it returns the
// first one's error and opens no listener.
TEST_F(SpeechWorkerScripted, AStartThatCouldNotBindItsStreamPortIsNotRetried) {
  register_backend();
  TakenPort taken;
  auto config = streaming_config(speech_model());
  config.streaming.port = taken.port;
  auto built = worker(config);
  ASSERT_NE(built, nullptr);
  const auto first = built->start();
  ASSERT_FALSE(first.has_value()) << "the stream port was taken";
  EXPECT_EQ(first.error().context.value_or(""), "stream_bind_failed");

  taken.give_back();
  ::testing::internal::CaptureStderr();
  const auto second = built->start();
  const auto log = ::testing::internal::GetCapturedStderr();
  ASSERT_FALSE(second.has_value()) << "a second start served with a stopped dispatch";
  EXPECT_EQ(second.error().code, first.error().code);
  EXPECT_EQ(second.error().context, first.error().context);
  EXPECT_TRUE(logged(log, "stream listener bound").empty());
  EXPECT_TRUE(logged(log, "stream listener start failed").empty()) << "the start was tried again";
  EXPECT_TRUE(logged(log, "http server bound").empty()) << "the HTTP port was opened for nothing";
  EXPECT_EQ(built->stop(), tensorplate::ServingExitCode::Ok);
}

// The bridge refuses a job of a class the loaded backend did not declare
// when it is submitted: the session fails at its first job, nothing reaches
// the sidecar and no counter of the dispatch moves. The worker goes on
// serving the mode the backend does run.
TEST_F(SpeechWorkerDecodeOnly, ASessionOfAModeTheBackendDoesNotRunFailsAtItsFirstJob) {
  register_backend();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  ASSERT_TRUE(served.serves_sessions);
  {
    auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING);
    ASSERT_TRUE(client.send_text(1, "no synthesis here"));
    const auto closed = closed_of(client.next());
    EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
    EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY);
    EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::FAILED_PRECONDITION);
  }
  EXPECT_TRUE(sidecar.sent("job_submit", 0).empty());
  ASSERT_NO_FATAL_FAILURE(transcribes_one_utterance(served));

  const auto stopped = logged(served.stop(), "speech dispatch stopped");
  ASSERT_EQ(stopped.size(), 1U);
  EXPECT_EQ(stopped.front().value("jobs_submitted", 0), 1) << "the decode alone";
  EXPECT_EQ(stopped.front().value("jobs_completed", 0), 1);
  EXPECT_EQ(stopped.front().value("jobs_failed", -1), 0);
}

// A speech deployment that is to serve streams is loaded with speech jobs,
// and a runner that offers none refuses that load: the worker is not built.
TEST_F(SpeechWorkerNoSpeechJobs, ARunnerThatOffersNoSpeechJobsFailsTheWorker) {
  register_backend();
  const auto created = ServingWorker::create(streaming_config(speech_model()), registry);
  ASSERT_FALSE(created.has_value());
  EXPECT_EQ(created.error().code, Error::Code::Unsupported);
  EXPECT_EQ(created.error().context.value_or(""), "speech_jobs_not_offered");
  EXPECT_EQ(sessions_built, 0) << "a session without jobs was built instead";
}

TEST_F(SpeechWorkerNoJobClass, ARunnerWithNoJobClassOfALaneFailsTheWorker) {
  register_backend();
  const auto created = ServingWorker::create(streaming_config(speech_model()), registry);
  ASSERT_FALSE(created.has_value());
  EXPECT_EQ(created.error().code, Error::Code::Unsupported);
  EXPECT_EQ(sessions_built, 0) << "a session without jobs was built instead";
}

// A session built with its bridge takes no tensor request.
TEST_F(SpeechWorkerScripted, AWorkerThatServesStreamSessionsRefusesUnaryInference) {
  register_backend();
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  const Serving served{std::move(built)};
  tensorplate::testing::HttpClient http("127.0.0.1", served.worker->bound_port());
  const auto answered = http.post("/infer", infer_body());
  const auto answer = Json::parse(answered.body, nullptr, false);
  ASSERT_TRUE(answer.is_object()) << answered.body;
  EXPECT_EQ(answer.value("status", ""), "failure") << answered.body;
  EXPECT_NE(answered.body.find("tensor_inference_unsupported"), std::string::npos) << answered.body;
  EXPECT_TRUE(sidecar.sent("infer", 0).empty());
  (void)served.stop();
}

TEST_F(SpeechWorkerScripted, ASpeechDeploymentOnABackendWithoutJobsRefusesEveryOpen) {
  register_backend(/*with_bridge=*/false);
  auto built = worker(streaming_config(speech_model()));
  ASSERT_NE(built, nullptr);
  EXPECT_EQ(sessions_built, 1);
  EXPECT_FALSE(load_request().contains("capabilities"));
  const Serving served{std::move(built)};
  ASSERT_TRUE(served.listens);
  EXPECT_FALSE(served.serves_sessions);
  expect_unsupported(served.refused(wire::SESSION_MODE_STT_STREAMING));
  EXPECT_TRUE(logged(served.stop(), "speech dispatch stopped").empty());
}

TEST_F(SpeechWorkerScripted,
       WithStreamingDisabledASpeechDeploymentLoadsWithoutJobsAndDoesNotListen) {
  register_backend();
  auto config = streaming_config(speech_model());
  config.streaming.enabled = false;
  auto built = worker(config);
  ASSERT_NE(built, nullptr);
  EXPECT_EQ(pairs_built, 0);
  EXPECT_EQ(sessions_built, 1);
  EXPECT_FALSE(load_request().contains("capabilities"));
  const Serving served{std::move(built)};
  EXPECT_FALSE(served.listens);
  (void)served.stop();
}

TEST_F(SpeechWorkerScripted, AFailureToBuildTheSessionWithItsBridgeFailsTheWorker) {
  register_backend();
  // Neither is the registry's answer for a backend that runs no jobs.
  for (const Error& refusal :
       {Error::make(Error::Code::LoadFailed, "no pair", "job_bridge_unsupported"),
        Error::make(Error::Code::Unsupported, "no pair", "another_reason")}) {
    SCOPED_TRACE(*refusal.context);
    pair_refusal = refusal;
    const auto created = ServingWorker::create(streaming_config(speech_model()), registry);
    ASSERT_FALSE(created.has_value());
    EXPECT_EQ(created.error().code, refusal.code);
    EXPECT_EQ(created.error().context, refusal.context);
  }
  EXPECT_EQ(pairs_built, 2);
  EXPECT_EQ(sessions_built, 0) << "a session without jobs was built instead";
}

TEST_F(SpeechWorkerScripted, ABridgeThatRefusesTheDispatchFailsTheWorker) {
  register_backend();
  sink_taken = true;
  const auto created = ServingWorker::create(streaming_config(speech_model()), registry);
  ASSERT_FALSE(created.has_value());
  EXPECT_EQ(created.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(created.error().context.value_or(""), "event_sink_already_set");
  EXPECT_TRUE(sidecar.reaped()) << "the session that was loaded is gone with the worker";
}

// A deployment of another model class is loaded as it is without streaming:
// the backend's factory for a session with its bridge is not called, unary
// requests are served and every Open is refused.
TEST(SpeechWorker, ADeploymentOfAnotherClassServesUnaryRequestsAndRefusesEveryOpen) {
  int sessions_built = 0;
  int pairs_built = 0;
  tensorplate::BackendRegistry registry;
  auto capability =
      tensorplate::BackendCapability::create("mock", {tensorplate::PrecisionHint::Auto});
  ASSERT_TRUE(capability.has_value()) << capability.error().message;
  ASSERT_TRUE(
      registry
          .register_backend(BackendEntry{
              "mock",
              capability.value(),
              [&](ExecutionSessionRuntimeHooks hooks) -> Result<std::unique_ptr<ExecutionSession>> {
                ++sessions_built;
                return std::unique_ptr<ExecutionSession>(
                    new tensorplate::serving::MockServingSession(*hooks.buffer_manager, {}, hooks));
              },
              [&](ExecutionSessionRuntimeHooks /*hooks*/) -> Result<SessionWithBridge> {
                ++pairs_built;
                return tensorplate::unexpected(Error::Code::Internal, "not a speech deployment");
              },
          })
          .has_value());
  auto created = ServingWorker::create(
      streaming_config(ModelSpec::create("policy", ModelClass::Custom, "mock://", "mock").value()),
      registry);
  ASSERT_TRUE(created.has_value()) << created.error().message;
  EXPECT_EQ(pairs_built, 0);
  EXPECT_EQ(sessions_built, 1);
  const Serving served{std::move(created).value()};
  ASSERT_TRUE(served.listens);
  EXPECT_FALSE(served.serves_sessions);
  expect_unsupported(served.refused(wire::SESSION_MODE_STT_STREAMING));
  tensorplate::testing::HttpClient http("127.0.0.1", served.worker->bound_port());
  const auto answered = http.post("/infer", infer_body());
  EXPECT_EQ(answered.status, 200) << answered.body;
  const auto answer = Json::parse(answered.body, nullptr, false);
  ASSERT_TRUE(answer.is_object()) << answered.body;
  EXPECT_EQ(answer.value("status", ""), "success") << answered.body;
  (void)served.stop();
}

// A worker that has no dispatch loses nothing by a failed start of its
// listener: it starts again as before.
TEST(SpeechWorker, AWorkerWithoutADispatchStartsAgainAfterItsStreamPortWasTaken) {
  TakenPort taken;
  ServingConfig config;
  config.deployment.endpoint = kDeployment;
  config.streaming.enabled = true;
  config.streaming.port = taken.port;
  auto created = ServingWorker::create(config);
  ASSERT_TRUE(created.has_value()) << created.error().message;
  auto built = std::move(created).value();
  const auto first = built->start();
  ASSERT_FALSE(first.has_value()) << "the stream port was taken";
  EXPECT_EQ(first.error().context.value_or(""), "stream_bind_failed");

  taken.give_back();
  const Serving served{std::move(built)};
  EXPECT_TRUE(served.listens);
  EXPECT_FALSE(served.serves_sessions);
  (void)served.stop();
}

/// The registered Python/PyTorch backend: a sidecar process that runs the
/// in-tree runner with its fixture backend.
class SpeechWorkerRunner : public ::testing::Test {
 protected:
  void SetUp() override {
    const char* named = std::getenv("TP_TEST_PYTHON");  // NOLINT(concurrency-mt-unsafe)
    const std::string python = named != nullptr && *named != '\0' ? named : "python3";
    const std::string probe = python + " -c 'import tensorplate_pytorch_backend' >/dev/null 2>&1";
    if (std::system(probe.c_str()) != 0) {  // NOLINT(concurrency-mt-unsafe)
      GTEST_SKIP() << "tensorplate_pytorch_backend is not importable by " << python;
    }
  }

  /// A worker over the built-in registry; null when it could not be built.
  static std::unique_ptr<ServingWorker> worker() {
    auto created = ServingWorker::create(streaming_config(speech_model()));
    EXPECT_TRUE(created.has_value()) << (created.has_value() ? "" : created.error().message);
    return created.has_value() ? std::move(created).value() : nullptr;
  }

  /// The pids of this process's children that run the runner's package and
  /// have not exited, read from /proc. Nothing but the adapter's launcher
  /// starts one, and a scripted sidecar is no process.
  static std::vector<long> runners() {
    std::vector<long> found;
    std::error_code failed;
    for (std::filesystem::directory_iterator entry("/proc", failed), end; !failed && entry != end;
         entry.increment(failed)) {
      const std::string name = entry->path().filename().string();
      if (name.find_first_not_of("0123456789") != std::string::npos) {
        continue;
      }
      // "<pid> (<name>) <state> <parent> ...", and the name may hold a ")".
      std::string stat;
      std::getline(std::ifstream(entry->path() / "stat"), stat);
      const auto name_end = stat.rfind(')');
      if (name_end == std::string::npos) {
        continue;
      }
      std::istringstream fields(stat.substr(name_end + 1));
      char state = 'Z';
      long parent = 0;
      fields >> state >> parent;
      if (parent != static_cast<long>(::getpid()) || state == 'Z') {
        continue;
      }
      std::ifstream arguments(entry->path() / "cmdline", std::ios::binary);
      const std::string command{std::istreambuf_iterator<char>(arguments), {}};
      if (command.find("tensorplate_pytorch_backend") != std::string::npos) {
        found.push_back(std::stol(name));
      }
    }
    return found;
  }
};

// The results are the scripted backend's, so what tells this worker from one
// that never started a runner is the runner itself: one child process from
// the load on, the same one once the job was served, and none after the stop.
TEST_F(SpeechWorkerRunner, OneUtteranceIsTranscribed) {
  ASSERT_TRUE(runners().empty());
  auto built = worker();
  ASSERT_NE(built, nullptr);
  const auto loaded = runners();
  ASSERT_EQ(loaded.size(), 1U) << "the load started no runner process";
  const Serving served{std::move(built)};
  ASSERT_TRUE(served.serves_sessions);
  ASSERT_NO_FATAL_FAILURE(transcribes_one_utterance(served));
  EXPECT_TRUE(eventually([&] { return served.buffers() == 0; }));
  EXPECT_EQ(runners(), loaded) << "the runner that was loaded did not live through the job";
  (void)served.stop();
  EXPECT_TRUE(runners().empty()) << "the stop left the runner process";
}

TEST_F(SpeechWorkerRunner, OneSegmentIsSynthesized) {
  ASSERT_TRUE(runners().empty());
  auto built = worker();
  ASSERT_NE(built, nullptr);
  const auto loaded = runners();
  ASSERT_EQ(loaded.size(), 1U) << "the load started no runner process";
  const Serving served{std::move(built)};
  ASSERT_TRUE(served.serves_sessions);
  ASSERT_NO_FATAL_FAILURE(synthesizes_one_segment(served));
  EXPECT_TRUE(eventually([&] { return served.buffers() == 0; }));
  EXPECT_EQ(runners(), loaded) << "the runner that was loaded did not live through the job";
  (void)served.stop();
  EXPECT_TRUE(runners().empty()) << "the stop left the runner process";
}

}  // namespace

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
