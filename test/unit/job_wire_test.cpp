// SPDX-License-Identifier: Apache-2.0
//
// The adapter's job wire codec, replayed against the recorded golden frames
// (protocol/rust/tests/fixtures/python_pytorch_ipc_speech_jobs_*.jsonl) and
// the job seam's vectors (protocol/fixtures/job_seam.json), plus the faults
// neither file holds.

#include "adapters/python_pytorch/job_wire.hpp"

#include <gtest/gtest.h>

#if !defined(TP_ENABLE_PYTHON_PYTORCH_SIDECAR) || !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobWire, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <map>
#include <nlohmann/json.hpp>
#include <optional>
#include <set>
#include <span>
#include <string>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/ipc/sidecar_codec.hpp"

#include "job_seam_vectors.hpp"
#include "speech_job_golden.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using namespace tensorplate::testing::job_seam;       // NOLINT(google-build-using-namespace)
using namespace tensorplate::testing::speech_golden;  // NOLINT(google-build-using-namespace)

// -- Test-side renderings of the wire. ------------------------------------------

BufferRef borrowed(std::size_t bytes) {
  static std::uint64_t next_id = 1U << 20U;
  return BufferRef::create(next_id++, bytes, BufferOwnership::Borrowed).value();
}

// The request a recorded `job_submit` describes; PCM is a borrowed handle of
// the recorded length and text is that many letters.
JobRequest request_of_submit(const Json& line) {
  const Json& input = line.at("input");
  const auto bytes = input.at("payload_length").get<std::size_t>();
  const auto type = input.at("type").get<std::string>();
  JobPayload payload = TextSegment{std::string(bytes, 'a')};
  if (type == "audio_frames") {
    payload = AudioFrames{format_of(input.at("format")), PcmWindow{borrowed(bytes), 0, bytes},
                          input.at("start_sample").get<std::uint64_t>()};
  } else if (type == "vad_frames") {
    payload = VadFrames{format_of(input.at("format")), PcmWindow{borrowed(bytes), 0, bytes},
                        input.at("frame_samples").get<std::uint32_t>(),
                        input.at("frame_count").get<std::uint32_t>(),
                        input.at("utterance_id").get<std::uint64_t>()};
  }
  return JobRequest::create(identity_of(line), job_class_of(line.at("job_class")),
                            std::move(payload), options_of(line.at("options")),
                            line.at("progress_limit").get<std::uint32_t>())
      .value();
}

Json wire_format(const AudioFormat& format) {
  return Json{{"encoding", std::string(to_string(format.encoding))},
              {"sample_rate_hz", format.sample_rate_hz},
              {"channels", format.channels}};
}

// The wire `result` object of a payload; `pcm_bytes` receives an audio
// chunk's length.
Json wire_result(const JobResultPayload& payload, std::size_t& pcm_bytes) {
  if (const auto* t = std::get_if<TranscriptResult>(&payload)) {
    Json words = Json::array();
    for (const WordUnit& w : t->words) {
      words.push_back(Json{{"text", w.text},
                           {"token_begin", w.token_begin},
                           {"token_end", w.token_end},
                           {"start_sample", w.start_sample},
                           {"end_sample", w.end_sample}});
    }
    return Json{{"type", "transcript"}, {"text", t->text}, {"tokens", t->tokens}, {"words", words}};
  }
  if (const auto* a = std::get_if<AudioChunkResult>(&payload)) {
    pcm_bytes = a->pcm.byte_size;
    return Json{{"type", "audio_chunk"},
                {"format", wire_format(a->format)},
                {"clipped_samples", a->clipped_samples},
                {"payload_length", a->pcm.byte_size}};
  }
  Json probabilities = Json::array();
  for (const float p : std::get<VadResult>(payload).probabilities) {
    probabilities.push_back(static_cast<double>(p));
  }
  return Json{{"type", "vad"}, {"probabilities", probabilities}};
}

Json wire_envelope(const std::string& kind, const JobIdentity& identity) {
  return Json{{"schema_version", "0.1"},
              {"message_id", "s1"},
              {"kind", kind},
              {"job_id", identity.job_id},
              {"session_key", identity.session_key},
              {"generation", identity.generation}};
}

// The header of the message that carries `event`.
Json wire_of(const JobEvent& event, std::size_t& pcm_bytes) {
  Json header = wire_envelope("job_" + std::string(to_string(event.kind())), event.identity());
  if (event.kind() == JobEventKind::Progress) {
    header["progress_sequence"] = event.progress_sequence();
  }
  if (event.result().has_value()) {
    header["result"] = wire_result(event.result()->payload(), pcm_bytes);
  }
  if (event.error().has_value()) {
    Json error{{"schema_version", "0.1"},
               {"code", std::string(to_string(event.error()->code))},
               {"message", event.error()->message}};
    if (event.error()->context.has_value()) {
      error["context"] = *event.error()->context;
    }
    header["error"] = std::move(error);
  }
  return header;
}

// The header of the message a vector's event or trace step describes, whether
// or not the seam accepts it.
Json wire_of_vector(const Json& e, JobIdentity identity, std::size_t& pcm_bytes) {
  if (e.contains("identity")) {
    identity = identity_of(e.at("identity"));
  }
  Json header = wire_envelope("job_" + e.at("kind").get<std::string>(), identity);
  if (e.contains("progress_sequence")) {
    header["progress_sequence"] = e.at("progress_sequence");
  }
  if (e.contains("result")) {
    header["result"] = wire_result(result_payload_of(e.at("result")), pcm_bytes);
  }
  if (e.contains("error")) {
    const Json& error = e.at("error");
    header["error"] = Json{{"schema_version", "0.1"},
                           {"code", error.at("code")},
                           {"message", text_of(error.at("message"))}};
    if (error.contains("context")) {
      header["error"]["context"] = text_of(error.at("context"));
    }
  }
  return header;
}

// A vector the socket cannot carry: JSON holds neither ill-formed UTF-8 nor a
// non-finite number.
bool expressible(const Json& header) {
  if (header.dump(-1, ' ', false, Json::error_handler_t::replace) !=
      header.dump(-1, ' ', false, Json::error_handler_t::ignore)) {
    return false;
  }
  if (header.contains("result") && header.at("result").contains("probabilities")) {
    for (const Json& p : header.at("result").at("probabilities")) {
      if (!std::isfinite(p.get<double>())) {
        return false;
      }
    }
  }
  return true;
}

// JSON numbers as the decoder holds them: probabilities are floats.
Json as_decoded(Json line) {
  if (line.contains("result") && line.at("result").contains("probabilities")) {
    for (Json& p : line.at("result").at("probabilities")) {
      p = static_cast<double>(p.get<float>());
    }
  }
  return line;
}

ipc::SidecarFrame frame_of(const Json& header, std::size_t payload_bytes = 0) {
  return ipc::SidecarFrame{header.dump(), std::vector<std::byte>(payload_bytes, std::byte{0})};
}

std::size_t recorded_payload(const Json& line) {
  return line.contains("result") ? line.at("result").value("payload_length", std::size_t{0}) : 0;
}

// Publishes into handles of its own and keeps what it was given.
struct Published {
  std::vector<std::vector<std::byte>> chunks;
  std::vector<BufferRef> handles;
  [[nodiscard]] PcmPublisher publisher() {
    return [this](std::span<const std::byte> pcm) -> Result<BufferRef> {
      chunks.emplace_back(pcm.begin(), pcm.end());
      handles.push_back(
          BufferRef::create(1000 + chunks.size(), pcm.size(), BufferOwnership::Owned).value());
      return handles.back();
    };
  }
};

SidecarJobMessage decode(const Json& header, std::size_t payload_bytes = 0) {
  Published published;
  return decode_sidecar_message(frame_of(header, payload_bytes), published.publisher());
}

std::uint64_t job_of(const Json& line) {
  return line.value("job_id", std::uint64_t{0});
}

// The fault of a message of `kind`, which its text names first.
void expect_fault(const SidecarJobMessage& message, std::uint64_t job_id, std::string_view reason,
                  const std::string& name, std::string_view kind = {}) {
  const auto* fault = std::get_if<JobMessageFault>(&message);
  ASSERT_NE(fault, nullptr) << name << ": alternative " << message.index();
  EXPECT_EQ(fault->job_id, job_id) << name;
  EXPECT_EQ(fault->error.code, Error::Code::InferenceFailed) << name;
  EXPECT_EQ(fault->error.context.value_or("<none>"), reason)
      << name << ": " << fault->error.message;
  if (!kind.empty()) {
    EXPECT_TRUE(fault->error.message.starts_with("sidecar " + std::string(kind) + ": "))
        << name << ": " << fault->error.message;
  }
}

std::string malformed_text(const std::string& kind, const std::string& field) {
  return "sidecar " + kind + ": field '" + field + "' is missing or malformed";
}

// A malformed message's fault names the message's kind and the field.
void expect_malformed(const SidecarJobMessage& message, std::uint64_t job_id,
                      const std::string& kind, const std::string& field, const std::string& name) {
  expect_fault(message, job_id, kJobMessageMalformed, name);
  if (const auto* fault = std::get_if<JobMessageFault>(&message)) {
    EXPECT_EQ(fault->error.message, malformed_text(kind, field)) << name;
  }
}

// -- Golden frames. -------------------------------------------------------------

TEST(JobWireGolden, TheAdapterMessagesAreEncodedAsRecorded) {
  std::map<std::string, int> encoded;
  for (const std::string& file : golden_files()) {
    for (const Json& line : golden(file)) {
      const auto kind = line.at("kind").get<std::string>();
      const auto id = line.at("message_id").get<std::string>();
      Result<std::string> header = unexpected(Error::Code::Internal, "not an adapter job message");
      if (kind == "job_submit") {
        const JobRequest request = request_of_submit(line);
        header = encode_job_submit(request, id);
        EXPECT_EQ(job_submit_payload_size(request), line.at("input").at("payload_length"));
      } else if (kind == "job_cancel") {
        header = encode_job_cancel(identity_of(line), id);
      } else if (kind == "session_release") {
        header = encode_session_release(line.at("session_key").get<std::uint64_t>(),
                                        line.at("generation").get<std::uint64_t>(), id);
      } else {
        continue;
      }
      ASSERT_TRUE(header.has_value()) << file << " " << id << ": " << header.error().message;
      EXPECT_EQ(Json::parse(header.value()), line) << file << " " << id;
      ++encoded[kind];
    }
  }
  EXPECT_EQ(encoded, (std::map<std::string, int>{
                         {"job_submit", 8}, {"job_cancel", 1}, {"session_release", 1}}));
}

TEST(JobWireGolden, TheSidecarMessagesDecodeToTheRecordedEventsInAnOrderTheSeamAccepts) {
  std::map<std::string, int> decoded;
  for (const std::string& file : golden_files()) {
    if (file.starts_with("negotiation")) {
      continue;
    }
    std::map<std::uint64_t, JobEventSequence> jobs;
    for (const Json& line : golden(file)) {
      const auto kind = line.at("kind").get<std::string>();
      const std::string where = file + " " + line.at("message_id").get<std::string>();
      if (kind == "job_submit") {
        jobs.emplace(line.at("job_id").get<std::uint64_t>(),
                     JobEventSequence(request_of_submit(line)));
        continue;
      }
      if (kind == "job_cancel") {
        jobs.at(line.at("job_id").get<std::uint64_t>()).note_cancel_requested();
        continue;
      }
      if (kind == "session_release") {
        for (auto& [id, sequence] : jobs) {
          sequence.note_cancel_requested();
        }
        continue;
      }
      Published published;
      const std::size_t payload_bytes = recorded_payload(line);
      const auto message =
          decode_sidecar_message(frame_of(line, payload_bytes), published.publisher());
      ++decoded[kind];
      if (kind == "session_released") {
        ASSERT_TRUE(std::holds_alternative<SessionReleased>(message)) << where;
        EXPECT_EQ(std::get<SessionReleased>(message),
                  (SessionReleased{line.at("session_key"), line.at("generation")}))
            << where;
        continue;
      }
      ASSERT_TRUE(std::holds_alternative<JobEvent>(message))
          << where << ": alternative " << message.index();
      const JobEvent& event = std::get<JobEvent>(message);
      std::size_t pcm_bytes = 0;
      Json rendered = wire_of(event, pcm_bytes);
      rendered["message_id"] = line.at("message_id");
      EXPECT_EQ(rendered, as_decoded(line)) << where;
      EXPECT_EQ(pcm_bytes, payload_bytes) << where;
      EXPECT_EQ(published.chunks.size(), payload_bytes == 0 ? 0U : 1U) << where;
      const auto observed = jobs.at(event.identity().job_id).observe(event);
      EXPECT_TRUE(observed.has_value()) << where << ": " << observed.error().context.value_or("");
    }
    for (const auto& [id, sequence] : jobs) {
      EXPECT_TRUE(sequence.released()) << file << " job " << id;
    }
  }
  EXPECT_EQ(decoded, (std::map<std::string, int>{{"job_accepted", 7},
                                                 {"job_progress", 5},
                                                 {"job_completed", 5},
                                                 {"job_failed", 3},
                                                 {"job_cancel_acknowledged", 2},
                                                 {"job_released", 8},
                                                 {"session_released", 1}}));
}

TEST(JobWireGolden, NegotiationReadsTheCapabilityAndTheJobClasses) {
  const auto lines = golden("negotiation");
  const auto offered = offers_speech_jobs(lines.at(0).dump());
  ASSERT_TRUE(offered.has_value()) << offered.error().message;
  EXPECT_TRUE(offered.value());
  EXPECT_EQ(lines.at(1).at("capabilities"), Json::array({std::string(kSpeechJobsCapability)}));
  const auto classes = declared_job_classes(lines.at(2).dump());
  ASSERT_TRUE(classes.has_value()) << classes.error().message;
  EXPECT_EQ(classes.value(), (std::vector<JobClass>{JobClass::SttDecode, JobClass::VadFrames}));

  // The refused load declares no class, and is a unary response to the reader.
  const Json refused = golden("negotiation_refused").at(2);
  const auto none = declared_job_classes(refused.dump());
  ASSERT_FALSE(none.has_value());
  EXPECT_EQ(none.error().code, Error::Code::InferenceFailed);
  EXPECT_EQ(none.error().context.value_or(""), "job_classes_malformed");
  for (const std::string file : {"negotiation", "negotiation_refused"}) {
    for (const Json& line : golden(file)) {
      if (!sent_by_adapter(line)) {
        EXPECT_TRUE(std::holds_alternative<NotJobMessage>(decode(line))) << line.dump();
      }
    }
  }
}

// -- Job seam vectors. ----------------------------------------------------------

TEST(JobWireSeam, AnAcceptedRequestEncodesToTheFieldsItHolds) {
  int encoded = 0;
  for (const Json& v : vectors().at("requests")) {
    if (v.at("expect") != "accept") {
      continue;
    }
    const auto name = v.at("name").get<std::string>();
    const Json& r = v.at("request");
    const Json& payload = r.at("payload");
    const JobRequest request = request_of(r).value();
    const auto bytes = payload.contains("pcm") ? payload.at("pcm").at("size").get<std::size_t>()
                                               : text_of(payload.at("text")).size();
    Json input{{"type", payload.at("type")}, {"payload_length", bytes}};
    for (const char* field : {"start_sample", "frame_samples", "frame_count", "utterance_id"}) {
      if (payload.contains(field)) {
        input[field] = payload.at(field);
      }
    }
    if (payload.contains("format")) {
      input["format"] = wire_format(format_of(payload.at("format")));
    }
    Json expected = wire_envelope("job_submit", identity_of(r.at("identity")));
    expected["message_id"] = "a7";
    expected["job_class"] = r.at("job_class");
    expected["progress_limit"] = r.at("progress_limit");
    expected["options"] = r.at("options");
    expected["input"] = std::move(input);

    const auto header = encode_job_submit(request, "a7");
    ASSERT_TRUE(header.has_value()) << name << ": " << header.error().message;
    EXPECT_EQ(Json::parse(header.value()), expected) << name;
    EXPECT_EQ(job_submit_payload_size(request), bytes) << name;
    ++encoded;
  }
  EXPECT_EQ(encoded, 21);
}

TEST(JobWireSeam, AResultDecodesAsTheSeamJudgesIt) {
  const JobIdentity identity{1, 1, 1};
  std::set<std::string> reasons;
  std::set<std::string> not_on_the_wire;
  int accepted = 0;
  for (const Json& v : vectors().at("results")) {
    const auto name = v.at("name").get<std::string>();
    if (v.value("scope", "all") == "cpp") {
      not_on_the_wire.insert(name);
      continue;
    }
    std::size_t pcm_bytes = 0;
    Json header = wire_envelope("job_completed", identity);
    header["result"] = wire_result(result_payload_of(v.at("result")), pcm_bytes);
    if (!expressible(header)) {
      not_on_the_wire.insert(name);
      continue;
    }
    Published published;
    const auto message = decode_sidecar_message(frame_of(header, pcm_bytes), published.publisher());
    if (v.at("expect") != "accept") {
      const auto reason = v.at("expect").at("reject").get<std::string>();
      expect_fault(message, 1, reason, name);
      EXPECT_TRUE(published.chunks.empty()) << name << ": a refused chunk was published";
      reasons.insert(reason);
      continue;
    }
    ASSERT_TRUE(std::holds_alternative<JobEvent>(message))
        << name << ": alternative " << message.index();
    const JobEvent& event = std::get<JobEvent>(message);
    std::size_t decoded_bytes = 0;
    EXPECT_EQ(wire_of(event, decoded_bytes), header) << name;
    if (pcm_bytes == 0) {
      EXPECT_EQ(event, JobEvent::completed(identity, result_of(v.at("result")).value()).value())
          << name;
    } else {
      ASSERT_EQ(published.handles.size(), 1U) << name;
      const auto& chunk = std::get<AudioChunkResult>(event.result()->payload());
      EXPECT_EQ(chunk.pcm, (PcmWindow{published.handles.front(), 0, pcm_bytes})) << name;
    }
    ++accepted;
  }
  EXPECT_EQ(accepted, 12);
  EXPECT_EQ(not_on_the_wire, (std::set<std::string>{
                                 "transcript_text_invalid_utf8", "word_text_invalid_utf8",
                                 "audio_chunk_buffer_released", "audio_chunk_window_leaves_buffer",
                                 "vad_probability_nan", "vad_probability_infinite"}));
  // Every reason a result can be refused for reaches the job, but the three
  // only an in-process handle or ill-formed UTF-8 can produce.
  std::set<std::string> expected;
  for (const Json& v : vectors().at("results")) {
    if (v.at("expect") != "accept") {
      expected.insert(v.at("expect").at("reject").get<std::string>());
    }
  }
  for (const char* unreachable :
       {"text_invalid_utf8", "pcm_buffer_invalid", "pcm_window_out_of_bounds"}) {
    EXPECT_EQ(expected.erase(unreachable), 1U) << unreachable;
  }
  EXPECT_EQ(reasons, expected);
}

TEST(JobWireSeam, AnEventDecodesAsTheSeamJudgesIt) {
  int replayed = 0;
  for (const Json& v : vectors().at("events")) {
    const auto name = v.at("name").get<std::string>();
    const Json& e = v.at("event");
    const JobIdentity identity = identity_of(e.at("identity"));
    std::size_t pcm_bytes = 0;
    const Json header = wire_of_vector(e, identity, pcm_bytes);
    const auto message = decode(header, pcm_bytes);
    ++replayed;
    if (v.at("expect") == "accept") {
      ASSERT_TRUE(std::holds_alternative<JobEvent>(message)) << name;
      EXPECT_EQ(std::get<JobEvent>(message), event_of(e, identity).value()) << name;
      continue;
    }
    const auto reason = v.at("expect").at("reject").get<std::string>();
    if (reason != "error_text_too_large") {
      expect_fault(message, identity.job_id, reason, name);
      continue;
    }
    // The codec relays a sidecar's text, so it shortens what the seam refuses.
    ASSERT_TRUE(std::holds_alternative<JobEvent>(message)) << name;
    const Error& error = *std::get<JobEvent>(message).error();
    EXPECT_EQ(error.code, Error::Code::InferenceFailed) << name;
    EXPECT_EQ(std::max(error.message.size(), error.context.value_or("").size()),
              kMaxJobErrorTextBytes)
        << name;
  }
  EXPECT_EQ(replayed, 12);
}

TEST(JobWireSeam, ATraceKeepsItsOutcomeThroughTheCodec) {
  int refused = 0;
  for (const Json& trace : vectors().at("traces")) {
    const auto name = trace.at("name").get<std::string>();
    std::map<std::string, JobEventSequence> jobs;
    for (const auto& [alias, r] : trace.at("jobs").items()) {
      jobs.emplace(alias, JobEventSequence(request_of(r).value()));
    }
    const Json& expect = trace.at("expect");
    const Json& steps = trace.at("steps");
    const std::size_t refused_at =
        expect == "accept" ? steps.size() : expect.at("refused_at").get<std::size_t>();
    for (std::size_t i = 0; i < steps.size() && i <= refused_at; ++i) {
      const Json& step = steps[i];
      const auto alias = step.at("job").get<std::string>();
      JobEventSequence& sequence = jobs.at(alias);
      if (step.value("cancel", false)) {
        sequence.note_cancel_requested();
        continue;
      }
      std::size_t pcm_bytes = 0;
      const Json header =
          wire_of_vector(step, identity_of(trace.at("jobs").at(alias).at("identity")), pcm_bytes);
      const auto message = decode(header, pcm_bytes);
      ASSERT_TRUE(std::holds_alternative<JobEvent>(message))
          << name << " step " << i << ": alternative " << message.index();
      const auto observed = sequence.observe(std::get<JobEvent>(message));
      if (i < refused_at) {
        ASSERT_TRUE(observed.has_value())
            << name << " step " << i << ": " << observed.error().context.value_or("");
        continue;
      }
      ASSERT_FALSE(observed.has_value()) << name << " step " << i << " was accepted";
      EXPECT_EQ(observed.error().context.value_or("<none>"), expect.at("reason")) << name;
      ++refused;
    }
    if (expect == "accept") {
      for (const auto& [alias, sequence] : jobs) {
        EXPECT_TRUE(sequence.released()) << name << " job " << alias;
      }
    }
  }
  EXPECT_EQ(vectors().at("traces").size(), 46U);
  EXPECT_EQ(refused, 29);
}

// -- What neither file holds. ---------------------------------------------------

Json completed_transcript() {
  return golden("stt_decode").at(2);
}

Json completed_audio() {
  return golden("tts_synthesis").at(2);
}

TEST(JobWireDecode, AMessageWithAFieldOfTheWrongShapeFaultsItsJob) {
  struct Case {
    const char* name;
    Json base;
    const char* pointer;
    Json value;
    const char* field;
  };
  const Json accepted = golden("stt_decode").at(1);
  const Json progress = golden("interleaved_progress").at(4);
  const Json failed = golden("cancel").at(4);
  const Json vad = golden("vad_frames").at(2);
  const Json transcript = completed_transcript();
  const Json audio = completed_audio();
  const std::vector<Case> cases{
      {"job_id text", accepted, "/job_id", "1", "job_id"},
      {"job_id fraction", accepted, "/job_id", 1.5, "job_id"},
      {"job_id negative", accepted, "/job_id", -1, "job_id"},
      {"job_id above 2^53-1", accepted, "/job_id", kMaxWireInteger + 1, "job_id"},
      {"job_id true", accepted, "/job_id", true, "job_id"},
      {"session_key text", accepted, "/session_key", "1", "session_key"},
      {"generation above 2^53-1", accepted, "/generation", kMaxWireInteger + 1, "generation"},
      {"schema_version 0.2", accepted, "/schema_version", "0.2", "schema_version"},
      {"progress_sequence text", progress, "/progress_sequence", "1", "progress_sequence"},
      {"result is not an object", progress, "/result", 7, "result"},
      {"result type unknown", progress, "/result/type", "embedding", "result.type"},
      {"text is a number", progress, "/result/text", 7, "result.text"},
      {"tokens is an object", progress, "/result/tokens", Json::object(), "result.tokens"},
      {"a token is text", progress, "/result/tokens/0", "1", "result.tokens"},
      {"a token is negative", progress, "/result/tokens/0", -1, "result.tokens"},
      {"a token is a fraction", progress, "/result/tokens/0", 1.5, "result.tokens"},
      {"a token above 2^32-1", progress, "/result/tokens/0", std::uint64_t{1} << 32U,
       "result.tokens"},
      {"words is text", progress, "/result/words", "w", "result.words"},
      {"a word is a number", transcript, "/result/words/0", 7, "result.words"},
      {"a word's text is a number", transcript, "/result/words/0/text", 7, "result.words"},
      {"a word's token_end is text", transcript, "/result/words/0/token_end", "1", "result.words"},
      {"a word's start_sample is text", transcript, "/result/words/0/start_sample", "0",
       "result.words"},
      {"a word's end_sample is negative", transcript, "/result/words/0/end_sample", -1,
       "result.words"},
      {"probabilities is a number", vad, "/result/probabilities", 0.5, "result.probabilities"},
      {"a probability is text", vad, "/result/probabilities/1", "0.9", "result.probabilities"},
      {"error is text", failed, "/error", "cancelled", "error"},
      {"error code is a number", failed, "/error/code", 9, "error"},
      {"error message is a number", failed, "/error/message", 9, "error"},
      {"error context is a number", failed, "/error/context", 9, "error"},
      {"format is text", audio, "/result/format", "pcm", "result.format"},
      {"encoding is a number", audio, "/result/format/encoding", 7, "result.format"},
      {"sample rate is text", audio, "/result/format/sample_rate_hz", "24000", "result.format"},
      {"channels above 16 bits", audio, "/result/format/channels", 65537, "result.format"},
      {"clipped_samples is negative", audio, "/result/clipped_samples", -1,
       "result.clipped_samples"},
      {"payload_length is not the payload's", audio, "/result/payload_length", 1918,
       "result.payload_length"},
  };
  for (const Case& c : cases) {
    Json header = c.base;
    header[Json::json_pointer(c.pointer)] = c.value;
    // A message whose job id cannot be read names no job.
    const std::uint64_t job_id = std::string_view{c.field} == "job_id" ? 0 : job_of(c.base);
    expect_malformed(decode(header, recorded_payload(c.base)), job_id, c.base.at("kind"), c.field,
                     c.name);
  }
  const std::vector<std::pair<const char*, const char*>> missing{
      {"/session_key", "session_key"},
      {"/generation", "generation"},
      {"/result", "result"},
      {"/result/type", "result.type"},
      {"/result/text", "result.text"},
      {"/result/tokens", "result.tokens"},
      {"/result/words", "result.words"},
      {"/result/words/0/token_begin", "result.words"}};
  for (const auto& [path, field] : missing) {
    Json header = transcript;
    const Json::json_pointer pointer(path);
    header.at(pointer.parent_pointer()).erase(pointer.back());
    expect_malformed(decode(header), job_of(transcript), "job_completed", field, path);
  }
  Json no_error = failed;
  no_error.erase("error");
  expect_malformed(decode(no_error), job_of(failed), "job_failed", "error", "no error");
  Json no_sequence = progress;
  no_sequence.erase("progress_sequence");
  expect_malformed(decode(no_sequence), job_of(progress), "job_progress", "progress_sequence",
                   "no sequence");
}

TEST(JobWireDecode, ThePayloadRegionBelongsToAnAudioChunkOnly) {
  for (const std::string file : {"stt_decode", "cancel", "vad_frames", "session_release"}) {
    for (const Json& line : golden(file)) {
      if (!sent_by_adapter(line)) {
        expect_malformed(decode(line, 2), job_of(line), line.at("kind"), "payload region",
                         line.dump());
      }
    }
  }
  const Json audio = completed_audio();
  for (const std::size_t bytes : {std::size_t{1918}, std::size_t{0}}) {
    expect_malformed(decode(audio, bytes), job_of(audio), "job_completed", "result.payload_length",
                     std::to_string(bytes));
  }
}

TEST(JobWireDecode, AProgressSequenceOutsideTheSeamsRangeIsNamedByTheSeam) {
  Json progress = golden("interleaved_progress").at(4);
  const std::uint64_t job_id = job_of(progress);
  progress["progress_sequence"] = 0;
  expect_fault(decode(progress), job_id, "progress_sequence_zero", "0", "job_progress");
  for (const std::uint64_t above : {std::uint64_t{1501}, std::uint64_t{1} << 32U}) {
    progress["progress_sequence"] = above;
    expect_fault(decode(progress), job_id, "progress_sequence_too_large", std::to_string(above),
                 "job_progress");
  }
}

TEST(JobWireDecode, AProbabilityNoFloatHoldsIsOutOfRange) {
  Json vad = golden("vad_frames").at(2);
  for (const double beyond : {1e300, -1e300, 1.00000000001, -1e-50}) {
    vad["result"]["probabilities"][1] = beyond;
    expect_fault(decode(vad), job_of(vad), "vad_probability_out_of_range", std::to_string(beyond),
                 "job_completed");
  }
}

TEST(JobWireDecode, AnAudioChunkIsPublishedOnceWithItsBytes) {
  ipc::SidecarFrame frame = frame_of(completed_audio(), 1920);
  for (std::size_t i = 0; i < frame.payload.size(); ++i) {
    frame.payload[i] = static_cast<std::byte>(i % 251);
  }
  Published published;
  const auto message = decode_sidecar_message(frame, published.publisher());
  ASSERT_TRUE(std::holds_alternative<JobEvent>(message));
  ASSERT_EQ(published.chunks.size(), 1U);
  EXPECT_EQ(published.chunks.front(), frame.payload);
  const auto& chunk = std::get<AudioChunkResult>(std::get<JobEvent>(message).result()->payload());
  EXPECT_EQ(chunk, (AudioChunkResult{kJobOutputAudioFormat,
                                     PcmWindow{published.handles.front(), 0, 1920}, 0}));
}

TEST(JobWireDecode, AnAudioChunkTheCodecFaultsIsNeverPublished) {
  Json progress = completed_audio();
  progress["kind"] = "job_progress";
  struct Case {
    const char* pointer;
    Json value;
    std::string_view reason;
  };
  const std::vector<Case> cases{
      {"/progress_sequence", 0, "progress_sequence_zero"},
      {"/progress_sequence", 1501, "progress_sequence_too_large"},
      {"/session_key", 0, "session_key_zero"},
      {"/generation", 0, "generation_zero"},
      {"/job_id", 0, "job_id_zero"},
      {"/result/format/encoding", "opus", "audio_format_unsupported"},
      {"/result/format/channels", 2, "audio_format_unsupported"},
  };
  for (const Case& c : cases) {
    Json header = progress;
    header["progress_sequence"] = 1;
    header[Json::json_pointer(c.pointer)] = c.value;
    Published published;
    const auto message = decode_sidecar_message(frame_of(header, 1920), published.publisher());
    expect_fault(message, job_of(header), c.reason, c.pointer, "job_progress");
    EXPECT_TRUE(published.chunks.empty()) << c.pointer;
  }
  Json first = progress;
  first["progress_sequence"] = 1500;
  Published published;
  const auto message = decode_sidecar_message(frame_of(first, 1920), published.publisher());
  ASSERT_TRUE(std::holds_alternative<JobEvent>(message));
  EXPECT_EQ(std::get<JobEvent>(message).progress_sequence(), 1500U);
  EXPECT_EQ(published.chunks.size(), 1U);
}

TEST(JobWireDecode, AnAudioChunkWithoutAPublisherOrWithAFailingOneFaultsItsJob) {
  const Json audio = completed_audio();
  const std::uint64_t job_id = job_of(audio);
  const ipc::SidecarFrame frame = frame_of(audio, 1920);
  const auto none = decode_sidecar_message(frame, PcmPublisher{});
  ASSERT_TRUE(std::holds_alternative<JobMessageFault>(none));
  EXPECT_EQ(std::get<JobMessageFault>(none),
            (JobMessageFault{job_id,
                             Error::make(Error::Code::NotReady,
                                         "no buffer manager to publish an audio chunk into",
                                         "backend_not_ready"),
                             std::nullopt}));

  const Error full = Error::make(Error::Code::OOMError, "pool exhausted", "buffer_pool");
  const auto failing = decode_sidecar_message(
      frame, [&](std::span<const std::byte>) -> Result<BufferRef> { return unexpected(full); });
  ASSERT_TRUE(std::holds_alternative<JobMessageFault>(failing));
  EXPECT_EQ(std::get<JobMessageFault>(failing), (JobMessageFault{job_id, full, std::nullopt}));

  // A handle that cannot hold the chunk, or that was released, is the
  // publisher's fault, not the sidecar's, and stays its owner's to release.
  BufferRef released = BufferRef::create(9, 1920, BufferOwnership::Owned).value();
  released.mark_released();
  for (const BufferRef& unusable :
       {BufferRef{}, BufferRef::create(9, 1918, BufferOwnership::Owned).value(), released}) {
    const auto refused = decode_sidecar_message(
        frame, [&](std::span<const std::byte>) { return Result<BufferRef>{unusable}; });
    ASSERT_TRUE(std::holds_alternative<JobMessageFault>(refused));
    EXPECT_EQ(std::get<JobMessageFault>(refused),
              (JobMessageFault{
                  job_id,
                  Error::make(Error::Code::Internal, "the published buffer cannot hold the chunk",
                              "pcm_publisher_handle_unusable"),
                  std::nullopt}))
        << unusable.size_bytes();
  }
}

TEST(JobWireDecode, FieldsTheAdapterDoesNotReadAreIgnored) {
  Json accepted = golden("stt_decode").at(1);
  accepted["queued_ns"] = 12;
  accepted["result"] = "not read on job_accepted";
  const auto message = decode(accepted);
  ASSERT_TRUE(std::holds_alternative<JobEvent>(message));
  EXPECT_EQ(std::get<JobEvent>(message), JobEvent::accepted(identity_of(accepted)).value());

  Json transcript = completed_transcript();
  transcript["result"]["language_probability"] = 0.9;
  transcript["result"]["words"][0]["probability"] = 0.8;
  EXPECT_TRUE(std::holds_alternative<JobEvent>(decode(transcript)));
}

TEST(JobWireDecode, AnErrorOfAFailedJobIsRelayedWithinTheSeamsBound) {
  Json failed = golden("refused").at(1);
  auto message = decode(failed);
  ASSERT_TRUE(std::holds_alternative<JobEvent>(message));
  EXPECT_EQ(*std::get<JobEvent>(message).error(),
            Error::make(Error::Code::Unsupported, "m", "job_not_permitted"));

  // 200 three-byte characters: cut to 170 of them, never inside one.
  std::string euros;
  for (int i = 0; i < 200; ++i) {
    euros += "\xE2\x82\xAC";
  }
  failed["error"]["message"] = euros;
  failed["error"]["context"] = euros + "x";
  failed["error"]["code"] = "a_code_of_a_later_release";
  message = decode(failed);
  ASSERT_TRUE(std::holds_alternative<JobEvent>(message));
  const Error& error = *std::get<JobEvent>(message).error();
  EXPECT_EQ(error.code, Error::Code::Internal);
  EXPECT_EQ(error.message, euros.substr(0, 510));
  EXPECT_EQ(error.context, euros.substr(0, 510));

  failed["error"].erase("context");
  message = decode(failed);
  ASSERT_TRUE(std::holds_alternative<JobEvent>(message));
  EXPECT_FALSE(std::get<JobEvent>(message).error()->context.has_value());
}

TEST(JobWireDecode, AnErrorEventNamesTheRefusedMessageAndNoJob) {
  const Json refusal{{"schema_version", "0.1"},
                     {"message_id", "a4"},
                     {"kind", "error_event"},
                     {"status", "error"},
                     {"error",
                      {{"schema_version", "0.1"},
                       {"code", "config_invalid"},
                       {"message", "duplicate job_id"},
                       {"context", "duplicate_job_id"}}}};
  auto message = decode(refusal);
  ASSERT_TRUE(std::holds_alternative<MessageRefused>(message));
  EXPECT_EQ(std::get<MessageRefused>(message),
            (MessageRefused{"a4", Error::make(Error::Code::ConfigInvalid, "duplicate job_id",
                                              "duplicate_job_id")}));

  Json wordy = refusal;
  wordy["error"]["message"] = std::string(600, 'm');
  wordy["error"].erase("context");
  message = decode(wordy);
  ASSERT_TRUE(std::holds_alternative<MessageRefused>(message));
  EXPECT_EQ(std::get<MessageRefused>(message).error,
            Error::make(Error::Code::ConfigInvalid, std::string(kMaxJobErrorTextBytes, 'm')));

  // Without a readable message id it is no one's, whatever else is wrong
  // with it: no sidecar writes one.
  for (const Json& id : {Json(nullptr), Json(7), Json("")}) {
    Json anonymous = refusal;
    anonymous["message_id"] = id;
    EXPECT_TRUE(std::holds_alternative<UnreadableHeader>(decode(anonymous))) << id;
    anonymous["schema_version"] = "0.2";
    anonymous.erase("error");
    EXPECT_TRUE(std::holds_alternative<UnreadableHeader>(decode(anonymous))) << id;
  }
  Json no_id = refusal;
  no_id.erase("message_id");
  EXPECT_TRUE(std::holds_alternative<UnreadableHeader>(decode(no_id)));
}

TEST(JobWireDecode, AnErrorEventItCannotReadStillRefusesTheMessageItNames) {
  const Json refusal{{"schema_version", "0.1"},
                     {"message_id", "a4"},
                     {"kind", "error_event"},
                     {"status", "error"},
                     {"error", {{"code", "config_invalid"}, {"message", "m"}}}};
  struct Case {
    const char* pointer;
    Json value;
    const char* field;
  };
  const std::vector<Case> cases{
      {"/schema_version", "0.2", "schema_version"},
      {"/schema_version", 7, "schema_version"},
      {"/error", "config_invalid", "error"},
      {"/error/code", 9, "error"},
      {"/error/message", 9, "error"},
      {"/error/context", 9, "error"},
  };
  const auto expect_unreadable = [](const Json& header, const std::string& field) {
    const auto message = decode(header);
    ASSERT_TRUE(std::holds_alternative<MessageRefused>(message))
        << header << ": alternative " << message.index();
    EXPECT_EQ(std::get<MessageRefused>(message),
              (MessageRefused{
                  "a4", Error::make(Error::Code::Internal, malformed_text("error_event", field),
                                    std::string(kJobMessageMalformed))}))
        << header;
  };
  for (const Case& c : cases) {
    Json header = refusal;
    header[Json::json_pointer(c.pointer)] = c.value;
    expect_unreadable(header, c.field);
  }
  for (const char* field : {"schema_version", "error"}) {
    Json header = refusal;
    header.erase(field);
    expect_unreadable(header, field);
  }
}

TEST(JobWireDecode, AFrameThatIsNotAJobMessageIsLeftToTheUnaryExchange) {
  for (const char* kind : {"load_model_response", "prime_response", "infer_response",
                           "health_check_response", "unload_response", "ready_event",
                           "metric_event", "session_opened", "", "a_kind_of_a_later_release"}) {
    const Json header{
        {"schema_version", "0.1"}, {"message_id", "a1"}, {"kind", kind}, {"status", "ok"}};
    EXPECT_TRUE(std::holds_alternative<NotJobMessage>(decode(header, 4))) << kind;
  }
}

TEST(JobWireDecode, AHeaderThatIsNotAnObjectWithAKindIsUnreadable) {
  for (const char* text : {"", "{", "[]", "7", "\"job_accepted\"", "{\"kind\":7}",
                           "{\"message_id\":\"a1\"}", "{\"kind\":null,\"job_id\":1}",
                           "{\"kind\":\"job_accepted\",\"job_id\":1,\"kind\":\"\xFF\"}"}) {
    const auto message = decode_sidecar_message(ipc::SidecarFrame{text, {}}, PcmPublisher{});
    EXPECT_TRUE(std::holds_alternative<UnreadableHeader>(message))
        << text << ": alternative " << message.index();
  }
}

TEST(JobWireDecode, AMessageOnlyTheAdapterSendsFaultsTheJobItNames) {
  for (const Json& line :
       {golden("stt_decode").at(0), golden("cancel").at(2), golden("session_release").at(2)}) {
    ASSERT_TRUE(sent_by_adapter(line)) << line;
    const std::string kind = line.at("kind");
    expect_malformed(decode(line), job_of(line), kind, "kind", kind);
  }
  Json unknown = golden("stt_decode").at(1);
  unknown["kind"] = "job_of_a_later_release";
  expect_malformed(decode(unknown), job_of(unknown), "job_of_a_later_release", "kind", "unknown");
}

TEST(JobWireDecode, ASessionReleasedNeedsItsKeyAndGeneration) {
  const Json released = golden("session_release").at(6);
  const auto session_of = [](const SidecarJobMessage& message) {
    const auto* fault = std::get_if<JobMessageFault>(&message);
    return fault != nullptr ? fault->session : std::nullopt;
  };
  for (const char* field : {"session_key", "generation"}) {
    Json header = released;
    header[field] = 0;
    expect_fault(decode(header), 0, std::string(field) + "_zero", field, "session_released");
    EXPECT_EQ(session_of(decode(header)), std::nullopt) << field;
    header[field] = "1";
    expect_malformed(decode(header), 0, "session_released", field, field);
    EXPECT_EQ(session_of(decode(header)), std::nullopt) << field;
    header.erase(field);
    expect_malformed(decode(header), 0, "session_released", field, field);
    EXPECT_EQ(session_of(decode(header)), std::nullopt) << field;
  }
  Json other = released;
  other["session_key"] = 7;
  other["generation"] = 3;
  const auto message = decode(other);
  ASSERT_TRUE(std::holds_alternative<SessionReleased>(message));
  EXPECT_EQ(std::get<SessionReleased>(message), (SessionReleased{7, 3}));

  // A fault names the session when the message does.
  Json versioned = other;
  versioned["schema_version"] = "1.0";
  expect_malformed(decode(versioned), 0, "session_released", "schema_version", "version");
  EXPECT_EQ(session_of(decode(versioned)), (SessionReleased{7, 3}));
  expect_malformed(decode(other, 2), 0, "session_released", "payload region", "payload");
  EXPECT_EQ(session_of(decode(other, 2)), (SessionReleased{7, 3}));
  versioned["generation"] = 0;
  EXPECT_EQ(session_of(decode(versioned)), std::nullopt);
  versioned["generation"] = 3;
  versioned["session_key"] = 0;
  EXPECT_EQ(session_of(decode(versioned)), std::nullopt);
}

void expect_refused(const Result<std::string>& header, std::string_view reason) {
  ASSERT_FALSE(header.has_value()) << reason;
  EXPECT_EQ(header.error().code, Error::Code::ConfigInvalid) << reason;
  EXPECT_EQ(header.error().context.value_or("<none>"), reason);
}

TEST(JobWireEncode, RefusesWhatTheWireCannotCarry) {
  const JobRequest request = request_of_submit(golden("stt_decode").at(0));
  expect_refused(encode_job_submit(request, ""), "message_id_empty");
  expect_refused(encode_job_cancel(JobIdentity{1, 1, 1}, ""), "message_id_empty");
  expect_refused(encode_session_release(1, 1, ""), "message_id_empty");

  const std::uint64_t above = kMaxWireInteger + 1;
  const auto with_identity = [&](JobIdentity identity) {
    return JobRequest::create(identity, JobClass::TtsSynthesis, TextSegment{"hi"},
                              JobOptions{"en", "af_heart", 1000})
        .value();
  };
  for (const JobIdentity identity :
       {JobIdentity{above, 1, 1}, JobIdentity{1, above, 1}, JobIdentity{1, 1, above}}) {
    expect_refused(encode_job_submit(with_identity(identity), "a1"), "wire_integer_out_of_range");
    expect_refused(encode_job_cancel(identity, "a1"), "wire_integer_out_of_range");
  }
  EXPECT_TRUE(
      encode_job_cancel(JobIdentity{kMaxWireInteger, kMaxWireInteger, kMaxWireInteger}, "a1")
          .has_value());
  expect_refused(encode_session_release(above, 1, "a1"), "wire_integer_out_of_range");
  expect_refused(encode_session_release(1, above, "a1"), "wire_integer_out_of_range");

  const auto audio = [&](std::uint64_t start_sample) {
    return JobRequest::create(
               JobIdentity{1, 1, 1}, JobClass::SttDecode,
               AudioFrames{kJobInputAudioFormat, PcmWindow{borrowed(320), 0, 320}, start_sample},
               JobOptions{.language = "en"})
        .value();
  };
  expect_refused(encode_job_submit(audio(above), "a1"), "wire_integer_out_of_range");
  EXPECT_TRUE(encode_job_submit(audio(kMaxWireInteger), "a1").has_value());
  const auto vad = JobRequest::create(JobIdentity{1, 1, 1}, JobClass::VadFrames,
                                      VadFrames{kJobInputAudioFormat,
                                                PcmWindow{borrowed(1024), 0, 1024}, 512, 1, above})
                       .value();
  expect_refused(encode_job_submit(vad, "a1"), "wire_integer_out_of_range");

  expect_refused(encode_job_cancel(JobIdentity{0, 1, 1}, "a1"), "job_id_zero");
  expect_refused(encode_job_cancel(JobIdentity{1, 0, 1}, "a1"), "session_key_zero");
  expect_refused(encode_job_cancel(JobIdentity{1, 1, 0}, "a1"), "generation_zero");
  expect_refused(encode_session_release(0, 1, "a1"), "session_key_zero");
  expect_refused(encode_session_release(1, 0, "a1"), "generation_zero");
}

TEST(JobWireEncode, AWindowInsideALargerBufferSendsOnlyItsBytes) {
  const auto request = JobRequest::create(JobIdentity{3, 2, 1}, JobClass::SttDecode,
                                          AudioFrames{kJobInputAudioFormat,
                                                      PcmWindow{borrowed(6400), 640, 3200}, 160},
                                          JobOptions{.language = "en"})
                           .value();
  EXPECT_EQ(job_submit_payload_size(request), 3200U);
  const Json header = Json::parse(encode_job_submit(request, "a1").value());
  EXPECT_EQ(header.at("input"), (Json{{"type", "audio_frames"},
                                      {"format", wire_format(kJobInputAudioFormat)},
                                      {"start_sample", 160},
                                      {"payload_length", 3200}}));
}

TEST(JobWireNegotiation, ACapabilityListIsReadByNameAndHeldToItsShape) {
  const auto offers = [](const Json& capabilities) {
    Json ready{{"schema_version", "0.1"}, {"message_id", "s1"}, {"kind", "ready_event"}};
    if (!capabilities.is_null()) {
      ready["capabilities"] = capabilities;
    }
    return offers_speech_jobs(ready.dump());
  };
  EXPECT_FALSE(offers(nullptr).value());
  EXPECT_FALSE(offers(Json::array({"jobs_v9"})).value());
  EXPECT_TRUE(offers(Json::array({"speech_jobs_v10", "speech_jobs_v1"})).value());
  for (const Json& malformed : {Json("speech_jobs_v1"), Json::array({"speech_jobs_v1", 7}),
                                Json{{"speech_jobs_v1", true}}}) {
    const auto refused = offers(malformed);
    ASSERT_FALSE(refused.has_value()) << malformed;
    EXPECT_EQ(refused.error().code, Error::Code::InferenceFailed);
    EXPECT_EQ(refused.error().context.value_or(""), "capabilities_malformed");
  }
  EXPECT_FALSE(offers_speech_jobs("not json").has_value());
}

TEST(JobWireNegotiation, JobClassesAreReadByNameAndHeldToTheirShape) {
  const auto classes = [](const Json& job_classes) {
    Json response{{"schema_version", "0.1"},
                  {"message_id", "a1"},
                  {"kind", "load_model_response"},
                  {"status", "ok"}};
    if (!job_classes.is_null()) {
      response["job_classes"] = job_classes;
    }
    return declared_job_classes(response.dump());
  };
  EXPECT_EQ(classes(Json::array({"tts_synthesis"})).value(),
            (std::vector<JobClass>{JobClass::TtsSynthesis}));
  EXPECT_EQ(
      classes(Json::array({"vad_frames", "a_class_of_a_later_release", "stt_decode"})).value(),
      (std::vector<JobClass>{JobClass::VadFrames, JobClass::SttDecode}));
  // Names of a later release are left out, up to the bound of the list.
  Json many = Json::array({"vad_frames", "tts_synthesis", "stt_decode"});
  while (many.size() < kMaxDeclaredJobClasses) {
    many.push_back("a_class_of_a_later_release_" + std::to_string(many.size()));
  }
  EXPECT_EQ(
      classes(many).value(),
      (std::vector<JobClass>{JobClass::VadFrames, JobClass::TtsSynthesis, JobClass::SttDecode}));
  many.push_back("one_class_too_many");
  for (const Json& malformed :
       {Json(nullptr), Json::array(), Json("stt_decode"), Json::array({"stt_decode", 7}),
        Json::array({"stt_decode", "stt_decode"}), many}) {
    const auto refused = classes(malformed);
    ASSERT_FALSE(refused.has_value()) << malformed;
    EXPECT_EQ(refused.error().code, Error::Code::InferenceFailed);
    EXPECT_EQ(refused.error().context.value_or(""), "job_classes_malformed");
  }
  EXPECT_FALSE(declared_job_classes("[").has_value());
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
