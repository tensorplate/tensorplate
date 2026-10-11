// SPDX-License-Identifier: Apache-2.0
//
// The notation of protocol/fixtures/job_seam.json (see the file's "notation"
// block) read into the job value objects, for every test that replays the
// vectors.

#pragma once

#include <gtest/gtest.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <fstream>
#include <limits>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::testing::job_seam {

using Json = nlohmann::json;

inline const Json& vectors() {
  static const Json doc = [] {
    std::ifstream in(std::string{TP_SOURCE_DIR} + "/protocol/fixtures/job_seam.json");
    EXPECT_TRUE(in.is_open()) << "protocol/fixtures/job_seam.json is missing";
    return Json::parse(in);
  }();
  return doc;
}

// -- Vector notation (see the file's "notation" block). ------------------------

inline JobIdentity identity_of(const Json& j) {
  return JobIdentity{j.at("job_id").get<std::uint64_t>(), j.at("session_key").get<std::uint64_t>(),
                     j.at("generation").get<std::uint64_t>()};
}

inline std::string text_of(const Json& j) {
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

inline AudioFormat format_of(const Json& j) {
  const Json& f = j.is_string() ? vectors().at("formats").at(j.get<std::string>()) : j;
  const auto encoding = audio_encoding_from_string(f.at("encoding").get<std::string>());
  return AudioFormat{encoding.value_or(static_cast<AudioEncoding>(255)),
                     f.at("sample_rate_hz").get<std::uint32_t>(),
                     f.at("channels").get<std::uint16_t>()};
}

inline PcmWindow pcm_of(const Json& p) {
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

inline JobClass job_class_of(const Json& j) {
  return job_class_from_string(j.get<std::string>()).value_or(static_cast<JobClass>(255));
}

inline JobOptions options_of(const Json& o) {
  return JobOptions{o.value("language", std::string{}), o.value("voice", std::string{}),
                    o.value("speed_milli", std::uint16_t{0})};
}

inline JobPayload payload_of(const Json& p) {
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

inline Result<JobRequest> request_of(const Json& r) {
  return JobRequest::create(identity_of(r.at("identity")), job_class_of(r.at("job_class")),
                            payload_of(r.at("payload")), options_of(r.at("options")),
                            r.at("progress_limit").get<std::uint32_t>());
}

inline float probability_of(const Json& j) {
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

// The payload a result vector describes, before JobResult::create judges it.
inline JobResultPayload result_payload_of(const Json& r) {
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
    return t;
  }
  if (type == "audio_chunk") {
    return AudioChunkResult{format_of(r.at("format")), pcm_of(r.at("pcm")),
                            r.at("clipped_samples").get<std::uint32_t>()};
  }
  VadResult v;
  for (const Json& p : r.at("probabilities")) {
    v.probabilities.push_back(probability_of(p));
  }
  return v;
}

inline Result<JobResult> result_of(const Json& r) {
  return JobResult::create(result_payload_of(r));
}

inline Error error_of(const Json& e) {
  const auto code = error_code_from_string(e.at("code").get<std::string>());
  EXPECT_TRUE(code.has_value()) << e.dump();
  Error error{code.value_or(Error::Code::Internal), text_of(e.at("message")), std::nullopt};
  if (e.contains("context")) {
    error.context = text_of(e.at("context"));
  }
  return error;
}

inline Result<JobEvent> event_of(const Json& e, JobIdentity identity) {
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

}  // namespace tensorplate::testing::job_seam
