// SPDX-License-Identifier: Apache-2.0
//
// Job value objects and the per-job event order, driven by the
// cross-language vectors in protocol/fixtures/job_seam.json, plus the C++-only
// cases the vectors cannot express.

#include <gtest/gtest.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <fstream>
#include <limits>
#include <map>
#include <nlohmann/json.hpp>
#include <set>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

namespace tensorplate {
namespace {

using Json = nlohmann::json;

const Json& vectors() {
  static const Json doc = [] {
    std::ifstream in(std::string{TP_SOURCE_DIR} + "/protocol/fixtures/job_seam.json");
    EXPECT_TRUE(in.is_open()) << "protocol/fixtures/job_seam.json is missing";
    return Json::parse(in);
  }();
  return doc;
}

// -- Vector notation (see the file's "notation" block). ------------------------

JobIdentity identity_of(const Json& j) {
  return JobIdentity{j.at("job_id").get<std::uint64_t>(), j.at("session_key").get<std::uint64_t>(),
                     j.at("generation").get<std::uint64_t>()};
}

std::string text_of(const Json& j) {
  if (j.contains("utf8")) {
    return j.at("utf8").get<std::string>();
  }
  if (j.contains("hex")) {
    const auto hex = j.at("hex").get<std::string>();
    std::string bytes;
    for (std::size_t i = 0; i + 1 < hex.size(); i += 2) {
      bytes.push_back(static_cast<char>(std::stoi(hex.substr(i, 2), nullptr, 16)));
    }
    return bytes;
  }
  std::string out;
  const auto unit = j.at("repeat").get<std::string>();
  for (int i = 0; i < j.at("times").get<int>(); ++i) {
    out += unit;
  }
  return out + j.value("then", std::string{});
}

AudioFormat format_of(const Json& j) {
  const Json& f = j.is_string() ? vectors().at("formats").at(j.get<std::string>()) : j;
  const auto encoding = audio_encoding_from_string(f.at("encoding").get<std::string>());
  return AudioFormat{encoding.value_or(static_cast<AudioEncoding>(255)),
                     f.at("sample_rate_hz").get<std::uint32_t>(),
                     f.at("channels").get<std::uint16_t>()};
}

PcmWindow pcm_of(const Json& p) {
  static std::uint64_t next_id = 1;
  const auto size = p.at("size").get<std::size_t>();
  const auto offset = p.value("offset", std::size_t{0});
  const auto buffer_bytes = p.value("buffer_bytes", std::max<std::size_t>(offset + size, 1));
  auto buffer = BufferRef::create(next_id++, buffer_bytes, BufferOwnership::Borrowed).value();
  if (p.value("released", false)) {
    buffer.mark_released();
  }
  return PcmWindow{buffer, offset, size};
}

JobClass job_class_of(const Json& j) {
  return job_class_from_string(j.get<std::string>()).value_or(static_cast<JobClass>(255));
}

JobOptions options_of(const Json& o) {
  return JobOptions{o.value("language", std::string{}), o.value("voice", std::string{}),
                    o.value("speed_milli", std::uint16_t{0})};
}

JobPayload payload_of(const Json& p) {
  const auto type = p.at("type").get<std::string>();
  if (type == "audio_frames") {
    return AudioFrames{format_of(p.at("format")), pcm_of(p.at("pcm")),
                       p.at("start_sample").get<std::uint64_t>()};
  }
  if (type == "text_segment") {
    return TextSegment{text_of(p.at("text"))};
  }
  return VadFrames{
      format_of(p.at("format")), pcm_of(p.at("pcm")), p.at("frame_samples").get<std::uint32_t>(),
      p.at("frame_count").get<std::uint32_t>(), p.at("utterance_id").get<std::uint64_t>()};
}

Result<JobRequest> request_of(const Json& r) {
  return JobRequest::create(identity_of(r.at("identity")), job_class_of(r.at("job_class")),
                            payload_of(r.at("payload")), options_of(r.at("options")),
                            r.at("progress_limit").get<std::uint32_t>());
}

float probability_of(const Json& j) {
  if (j.is_string()) {
    const auto s = j.get<std::string>();
    if (s == "nan") {
      return std::numeric_limits<float>::quiet_NaN();
    }
    return s == "inf" ? std::numeric_limits<float>::infinity()
                      : -std::numeric_limits<float>::infinity();
  }
  return j.get<float>();
}

Result<JobResult> result_of(const Json& r) {
  const auto type = r.at("type").get<std::string>();
  if (type == "transcript") {
    TranscriptResult t;
    t.text = text_of(r.at("text"));
    const Json& tokens = r.at("tokens");
    if (tokens.is_array()) {
      t.tokens = tokens.get<std::vector<std::uint32_t>>();
    } else {
      for (std::uint32_t id = 1; id <= tokens.at("count").get<std::uint32_t>(); ++id) {
        t.tokens.push_back(id);
      }
    }
    const Json& words = r.at("words");
    if (words.is_array()) {
      for (const Json& w : words) {
        t.words.push_back(WordUnit{text_of(w.at("text")), w.at("token_begin").get<std::uint32_t>(),
                                   w.at("token_end").get<std::uint32_t>(),
                                   w.at("start_sample").get<std::uint64_t>(),
                                   w.at("end_sample").get<std::uint64_t>()});
      }
    } else {
      const std::string word = text_of(words.at("one_per_token").at("text"));
      for (std::uint32_t i = 0; i < t.tokens.size(); ++i) {
        t.words.push_back(WordUnit{word, i, i + 1, 512ULL * i, 512ULL * (i + 1)});
      }
    }
    return JobResult::create(std::move(t));
  }
  if (type == "audio_chunk") {
    return JobResult::create(AudioChunkResult{format_of(r.at("format")), pcm_of(r.at("pcm")),
                                              r.at("clipped_samples").get<std::uint32_t>()});
  }
  VadResult v;
  for (const Json& p : r.at("probabilities")) {
    v.probabilities.push_back(probability_of(p));
  }
  return JobResult::create(std::move(v));
}

Error error_of(const Json& e) {
  const auto code = error_code_from_string(e.at("code").get<std::string>());
  EXPECT_TRUE(code.has_value()) << e.dump();
  Error error{code.value_or(Error::Code::Internal), text_of(e.at("message")), std::nullopt};
  if (e.contains("context")) {
    error.context = text_of(e.at("context"));
  }
  return error;
}

Result<JobEvent> event_of(const Json& e, JobIdentity identity) {
  if (e.contains("identity")) {
    identity = identity_of(e.at("identity"));
  }
  const auto kind = e.at("kind").get<std::string>();
  if (kind == "accepted") {
    return JobEvent::accepted(identity);
  }
  if (kind == "progress" || kind == "completed") {
    auto result = result_of(e.at("result"));
    if (!result) {
      return unexpected(std::move(result).error());
    }
    return kind == "progress"
               ? JobEvent::progress(identity, e.at("progress_sequence").get<std::uint32_t>(),
                                    std::move(result).value())
               : JobEvent::completed(identity, std::move(result).value());
  }
  if (kind == "failed") {
    return JobEvent::failed(identity, error_of(e.at("error")));
  }
  if (kind == "cancel_acknowledged") {
    return JobEvent::cancel_acknowledged(identity);
  }
  EXPECT_EQ(kind, "released");
  return JobEvent::released(identity);
}

// Checks a construction outcome against a vector's "expect".
template <typename T>
void expect_outcome(const Result<T>& outcome, const Json& expect, const std::string& name) {
  if (expect == "accept") {
    EXPECT_TRUE(outcome.has_value()) << name << ": " << outcome.error().message;
    return;
  }
  ASSERT_FALSE(outcome.has_value()) << name << " was accepted";
  EXPECT_EQ(outcome.error().code, Error::Code::ConfigInvalid) << name;
  EXPECT_EQ(outcome.error().context.value_or("<none>"), expect.at("reject").get<std::string>())
      << name;
}

// -- Vector replay. ------------------------------------------------------------

TEST(JobSeamVectors, LimitsMatchTheHeaders) {
  const std::map<std::string, std::uint64_t> header{
      {"job_input_sample_rate_hz", kJobInputSampleRateHz},
      {"job_output_sample_rate_hz", kJobOutputSampleRateHz},
      {"audio_frames_max_bytes", kMaxAudioFramesBytes},
      {"text_segment_max_bytes", kMaxTextSegmentBytes},
      {"vad_frames_max_per_job", kMaxVadFramesPerJob},
      {"vad_frames_max_bytes", kMaxVadFramesBytes},
      {"progress_events_max", kMaxJobProgressEvents},
      {"language_tag_max_bytes", kMaxLanguageTagBytes},
      {"voice_id_max_bytes", kMaxVoiceIdBytes},
      {"transcript_text_max_bytes", kMaxTranscriptTextBytes},
      {"transcript_tokens_max", kMaxTranscriptTokens},
      {"audio_chunk_max_bytes", kMaxAudioChunkBytes},
      {"vad_probabilities_max", kMaxVadProbabilities},
      {"error_text_max_bytes", kMaxJobErrorTextBytes},
  };
  std::map<std::string, std::uint64_t> fixture;
  for (const auto& [name, value] : vectors().at("limits").items()) {
    fixture.emplace(name, value.get<std::uint64_t>());
  }
  EXPECT_EQ(fixture, header);
  EXPECT_EQ(format_of(Json("input")), kJobInputAudioFormat);
  EXPECT_EQ(format_of(Json("output")), kJobOutputAudioFormat);
}

TEST(JobSeamVectors, NamesMatchTheHeadersInOrder) {
  const Json& names = vectors().at("names");
  auto check = [](const Json& list, auto to_name, auto from_name, auto make) {
    for (std::size_t i = 0; i < list.size(); ++i) {
      const auto value = make(i);
      EXPECT_EQ(std::string(to_name(value)), list[i].template get<std::string>()) << i;
      const auto parsed = from_name(list[i].template get<std::string>());
      ASSERT_TRUE(parsed.has_value()) << list[i];
      EXPECT_EQ(*parsed, value);
    }
    EXPECT_EQ(std::string(to_name(make(list.size()))), "unknown") << "a name the vectors lack";
    EXPECT_FALSE(from_name("unknown").has_value());
  };
  check(
      names.at("audio_encoding"), [](AudioEncoding v) { return to_string(v); },
      audio_encoding_from_string, [](std::size_t i) { return static_cast<AudioEncoding>(i); });
  check(
      names.at("job_class"), [](JobClass v) { return to_string(v); }, job_class_from_string,
      [](std::size_t i) { return static_cast<JobClass>(i); });
  check(
      names.at("result_kind"), [](JobResultKind v) { return to_string(v); },
      job_result_kind_from_string, [](std::size_t i) { return static_cast<JobResultKind>(i); });
  check(
      names.at("job_event_kind"), [](JobEventKind v) { return to_string(v); },
      job_event_kind_from_string, [](std::size_t i) { return static_cast<JobEventKind>(i); });
  for (const auto& [job_class, kind] : names.at("result_kind_for_job_class").items()) {
    EXPECT_EQ(std::string(to_string(result_kind_for(job_class_of(Json(job_class))))),
              kind.get<std::string>());
  }
  for (const char* near_miss : {"", "STT_DECODE", "stt-decode", " stt_decode", "tts_segment"}) {
    EXPECT_FALSE(job_class_from_string(near_miss).has_value()) << '"' << near_miss << '"';
  }
  for (const char* near_miss : {"cancel_ack", "Released", "canceled"}) {
    EXPECT_FALSE(job_event_kind_from_string(near_miss).has_value()) << '"' << near_miss << '"';
  }
}

TEST(JobSeamVectors, Requests) {
  for (const Json& v : vectors().at("requests")) {
    expect_outcome(request_of(v.at("request")), v.at("expect"), v.at("name").get<std::string>());
  }
}

TEST(JobSeamVectors, Results) {
  for (const Json& v : vectors().at("results")) {
    expect_outcome(result_of(v.at("result")), v.at("expect"), v.at("name").get<std::string>());
  }
}

TEST(JobSeamVectors, Events) {
  for (const Json& v : vectors().at("events")) {
    const Json& e = v.at("event");
    expect_outcome(event_of(e, identity_of(e.at("identity"))), v.at("expect"),
                   v.at("name").get<std::string>());
  }
}

TEST(JobSeamVectors, Traces) {
  for (const Json& trace : vectors().at("traces")) {
    const auto name = trace.at("name").get<std::string>();
    std::map<std::string, JobEventSequence> jobs;
    for (const auto& [alias, r] : trace.at("jobs").items()) {
      auto request = request_of(r);
      ASSERT_TRUE(request.has_value()) << name << ": " << request.error().message;
      jobs.emplace(alias, JobEventSequence(request.value()));
    }
    const Json& expect = trace.at("expect");
    const Json& steps = trace.at("steps");
    const std::size_t refused_at =
        expect == "accept" ? steps.size() : expect.at("refused_at").get<std::size_t>();
    ASSERT_LT(expect == "accept" ? 0U : refused_at, steps.size()) << name;
    for (std::size_t i = 0; i < steps.size(); ++i) {
      const Json& step = steps[i];
      const auto alias = step.at("job").get<std::string>();
      JobEventSequence& sequence = jobs.at(alias);
      if (step.value("cancel", false)) {
        ASSERT_LT(i, refused_at) << name << ": a refused step must be an event";
        sequence.note_cancel_requested();
        continue;
      }
      auto event = event_of(step, identity_of(trace.at("jobs").at(alias).at("identity")));
      ASSERT_TRUE(event.has_value()) << name << " step " << i << ": " << event.error().message;
      const JobEventSequence before = sequence;
      const auto observed = sequence.observe(event.value());
      if (i < refused_at) {
        ASSERT_TRUE(observed.has_value())
            << name << " step " << i << " refused: " << observed.error().context.value_or("");
        continue;
      }
      ASSERT_FALSE(observed.has_value()) << name << " step " << i << " was accepted";
      EXPECT_EQ(observed.error().code, Error::Code::InferenceFailed) << name;
      EXPECT_EQ(observed.error().context.value_or("<none>"), expect.at("reason").get<std::string>())
          << name;
      EXPECT_EQ(sequence, before) << name << ": a refused event changed the sequence";
      break;
    }
    if (expect == "accept") {
      for (const auto& [alias, sequence] : jobs) {
        EXPECT_TRUE(sequence.terminal() && sequence.released()) << name << " job " << alias;
      }
    }
  }
}

TEST(JobSeamVectors, EveryListedReasonHasAVectorAndNoOther) {
  std::set<std::string> construction;
  for (const char* section : {"requests", "results", "events"}) {
    for (const Json& v : vectors().at(section)) {
      if (v.at("expect") != "accept") {
        construction.insert(v.at("expect").at("reject").get<std::string>());
      }
    }
  }
  std::set<std::string> sequence;
  for (const Json& trace : vectors().at("traces")) {
    if (trace.at("expect") != "accept") {
      sequence.insert(trace.at("expect").at("reason").get<std::string>());
    }
  }
  const Json& reasons = vectors().at("reasons");
  EXPECT_EQ(construction, reasons.at("construction").get<std::set<std::string>>());
  EXPECT_EQ(sequence, reasons.at("sequence").get<std::set<std::string>>());
}

TEST(JobSeamVectors, RequiredCasesArePresent) {
  auto find = [](const char* section, const std::string& name) -> const Json* {
    for (const Json& v : vectors().at(section)) {
      if (v.at("name") == name) {
        return &v;
      }
    }
    return nullptr;
  };
  const std::map<std::string, std::string> refused{
      {"progress_sequence_gap", "progress_sequence_gap"},
      {"progress_after_completed", "event_after_terminal"},
      {"progress_cap_overflow", "progress_limit_exceeded"},
      {"speech_progress_refused", "progress_limit_exceeded"},
  };
  for (const auto& [name, reason] : refused) {
    const Json* v = find("traces", name);
    ASSERT_NE(v, nullptr) << name;
    EXPECT_EQ(v->at("expect").at("reason"), reason) << name;
  }
  for (const char* name : {"speech_stt_decode_empty_transcript", "delegated_interleaved_progress",
                           "delegated_cancel_one_while_other_advances"}) {
    const Json* v = find("traces", name);
    ASSERT_NE(v, nullptr) << name;
    EXPECT_EQ(v->at("expect"), "accept") << name;
  }
  const Json* empty = find("results", "transcript_empty");
  ASSERT_NE(empty, nullptr);
  EXPECT_EQ(empty->at("expect"), "accept");
}

// -- C++-only cases. -----------------------------------------------------------

JobRequest vad_request(std::uint32_t progress_limit = 0) {
  auto pcm = BufferRef::create(7, 2048, BufferOwnership::Borrowed).value();
  return JobRequest::create(JobIdentity{1, 2, 3}, JobClass::VadFrames,
                            VadFrames{kJobInputAudioFormat, PcmWindow{pcm, 0, 2048}, 512, 2, 9}, {},
                            progress_limit)
      .value();
}

TEST(JobRequest, WindowNearTheTopOfSizeTIsOutOfBoundsWithoutOverflow) {
  auto pcm = BufferRef::create(7, 4096, BufferOwnership::Borrowed).value();
  const std::size_t top = std::numeric_limits<std::size_t>::max() - 1;
  for (const PcmWindow window : {PcmWindow{pcm, top, 2}, PcmWindow{pcm, 2, top}}) {
    auto r = JobRequest::create(JobIdentity{1, 2, 3}, JobClass::SttDecode,
                                AudioFrames{kJobInputAudioFormat, window, 0},
                                JobOptions{.language = "en"});
    ASSERT_FALSE(r.has_value());
    EXPECT_EQ(r.error().context.value_or(""), "pcm_window_out_of_bounds");
  }
}

TEST(JobRequest, CopiesCompareEqualAndEveryFieldDistinguishes) {
  const JobRequest a = vad_request();
  const JobRequest copy = a;
  EXPECT_EQ(copy, a);
  EXPECT_NE(vad_request(1), a) << "progress_limit takes part in equality";
  EXPECT_EQ(a.progress_limit(), 0U);
  EXPECT_EQ(a.options(), JobOptions{});
}

TEST(JobEventSequence, NoteCancelRequestedIsIdempotentAndPartOfTheState) {
  JobEventSequence sequence(vad_request());
  const JobEventSequence fresh = sequence;
  sequence.note_cancel_requested();
  EXPECT_NE(sequence, fresh);
  const JobEventSequence once = sequence;
  sequence.note_cancel_requested();
  EXPECT_EQ(sequence, once);
  EXPECT_TRUE(sequence.cancel_requested());
}

TEST(JobResult, EmptyTranscriptIsAcceptedAndReportsItsKind) {
  const auto result = JobResult::create(TranscriptResult{});
  ASSERT_TRUE(result.has_value());
  EXPECT_EQ(result.value().kind(), JobResultKind::Transcript);
  EXPECT_EQ(result.value().kind(), result_kind_for(JobClass::SttDecode));
}

}  // namespace
}  // namespace tensorplate
