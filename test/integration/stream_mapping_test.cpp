// SPDX-License-Identifier: Apache-2.0
//
// The stream listener's mapping from the session layer to the wire.

#include "serving/stream/stream_mapping.hpp"

#include <google/protobuf/descriptor.h>
#include <gtest/gtest.h>

#include <chrono>
#include <fstream>
#include <map>
#include <regex>
#include <sstream>
#include <string>
#include <variant>
#include <vector>

namespace {

namespace stream = tensorplate::serving::stream;
namespace wire = tensorplate::stream::v1;
using tensorplate::Error;

std::string upper(std::string text) {
  for (auto& c : text) {
    c = static_cast<char>(std::toupper(static_cast<unsigned char>(c)));
  }
  return text;
}

TEST(StreamMapping, EveryReasonCarriesTheCodeTheTaxonomyPairsItWith) {
  std::ifstream input{std::string{TP_SOURCE_DIR} + "/docs/observability/failure-reasons.md"};
  ASSERT_TRUE(input.is_open());
  std::stringstream text;
  text << input.rdbuf();
  const std::regex row{R"(^\|\s*`([a-z_]+)`\s*\|.*\|\s*`([a-z_]+)`\s*\|\s*$)"};
  std::map<std::string, std::string> codes;
  std::string line;
  while (std::getline(text, line)) {
    std::smatch match;
    if (std::regex_match(line, match, row)) {
      codes.emplace(match[1].str(), match[2].str());
    }
  }
  const auto* reasons = wire::FailureReason_descriptor();
  ASSERT_GT(reasons->value_count(), 1);
  for (int index = 1; index < reasons->value_count(); ++index) {
    const auto* value = reasons->value(index);
    const auto reason = static_cast<wire::FailureReason>(value->number());
    const std::string name =
        std::string{value->name()}.substr(std::string{"FAILURE_REASON_"}.size());
    SCOPED_TRACE(name);
    std::string lower = name;
    for (auto& c : lower) {
      c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
    }
    const auto paired = codes.find(lower);
    ASSERT_NE(paired, codes.end()) << "failure-reasons.md has no row";
    EXPECT_EQ(wire::ErrorCode_Name(stream::taxonomy_code(reason)),
              "ERROR_CODE_" + upper(paired->second));
    const auto cause = stream::end_cause(Error::make(Error::Code::Internal, "m", lower));
    EXPECT_EQ(cause.reason(), reason) << "a context naming a reason maps to it";
  }
}

TEST(StreamMapping, AContextThatNamesNoReasonMapsByItsCode) {
  EXPECT_EQ(stream::failure_reason_of(Error::make(Error::Code::NotReady, "m", "session_ended")),
            wire::FAILURE_REASON_WORKER_NOT_READY);
  EXPECT_EQ(stream::failure_reason_of(
                Error::make(Error::Code::ConfigInvalid, "m", "invalid_output_item")),
            wire::FAILURE_REASON_CONFIG_INVALID);
  EXPECT_EQ(stream::failure_reason_of(
                Error::make(Error::Code::ResourceExhausted, "m", "session_key_exhausted")),
            wire::FAILURE_REASON_INTERNAL);
  EXPECT_EQ(stream::failure_reason_of(Error::make(Error::Code::InferenceFailed, "m")),
            wire::FAILURE_REASON_INTERNAL);
  EXPECT_EQ(stream::failure_reason_of(Error::make(Error::Code::Timeout, "m", "UNSPECIFIED")),
            wire::FAILURE_REASON_TIMEOUT);
}

TEST(StreamMapping, DetailIsCutToSixtyFourPrintableCharacters) {
  const auto cause = stream::end_cause(wire::FAILURE_REASON_INTERNAL, std::string(100, 'a') + "\n");
  EXPECT_EQ(cause.detail(), std::string(64, 'a'));
  EXPECT_EQ(stream::end_cause(wire::FAILURE_REASON_INTERNAL, "a\tb").detail(), "a?b");
}

TEST(StreamMapping, RefusalStatusCodesFollowTheContractTable) {
  const std::map<wire::FailureReason, grpc::StatusCode> expected{
      {wire::FAILURE_REASON_INVALID_EVENT, grpc::StatusCode::INVALID_ARGUMENT},
      {wire::FAILURE_REASON_BACKEND_UNSUPPORTED_CAPABILITY, grpc::StatusCode::FAILED_PRECONDITION},
      {wire::FAILURE_REASON_STALE_GENERATION, grpc::StatusCode::FAILED_PRECONDITION},
      {wire::FAILURE_REASON_SESSION_COUNT_LIMIT, grpc::StatusCode::RESOURCE_EXHAUSTED},
      {wire::FAILURE_REASON_ADMISSION_CLOSED, grpc::StatusCode::UNAVAILABLE},
      {wire::FAILURE_REASON_IDLE_TIMEOUT, grpc::StatusCode::DEADLINE_EXCEEDED},
      {wire::FAILURE_REASON_CLIENT_CANCELLED, grpc::StatusCode::CANCELLED},
      {wire::FAILURE_REASON_INTERNAL, grpc::StatusCode::INTERNAL},
  };
  for (const auto& [reason, code] : expected) {
    SCOPED_TRACE(wire::FailureReason_Name(reason));
    wire::OpenRefused refused;
    *refused.mutable_cause() = stream::end_cause(reason, "");
    refused.set_admission(wire::ADMISSION_NOT_ADMITTED);
    EXPECT_EQ(stream::refused_open_status(refused).error_code(), code);
  }
}

TEST(StreamMapping, ASessionEndsItsCallWithTheStatusOfItsCause) {
  // The stream's error mapping, then the class of the reason's error code.
  const std::map<std::string, grpc::StatusCode> expected{
      {"CONFIG_INVALID", grpc::StatusCode::INVALID_ARGUMENT},
      {"BUNDLE_SCHEMA_INVALID", grpc::StatusCode::INVALID_ARGUMENT},
      {"BUNDLE_INTEGRITY_FAILED", grpc::StatusCode::INVALID_ARGUMENT},
      {"EMPTY_INPUT", grpc::StatusCode::INVALID_ARGUMENT},
      {"INVALID_EVENT", grpc::StatusCode::INVALID_ARGUMENT},
      {"SHAPE_MISMATCH", grpc::StatusCode::INVALID_ARGUMENT},
      {"UNSUPPORTED_RUNTIME", grpc::StatusCode::FAILED_PRECONDITION},
      {"UNSUPPORTED_HARDWARE", grpc::StatusCode::FAILED_PRECONDITION},
      {"BACKEND_UNSUPPORTED_CAPABILITY", grpc::StatusCode::FAILED_PRECONDITION},
      {"PERMISSION_DENIED", grpc::StatusCode::FAILED_PRECONDITION},
      {"TARGET_UNRESOLVED", grpc::StatusCode::FAILED_PRECONDITION},
      {"TARGET_MISMATCH", grpc::StatusCode::FAILED_PRECONDITION},
      {"STALE_GENERATION", grpc::StatusCode::FAILED_PRECONDITION},
      {"ILLEGAL_TRANSITION", grpc::StatusCode::FAILED_PRECONDITION},
      {"SESSION_COUNT_LIMIT", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"INPUT_CREDIT_EXCEEDED", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"SLOW_CONSUMER", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"OOM", grpc::StatusCode::RESOURCE_EXHAUSTED},
      {"TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"DEADLINE_MISSED", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"IDLE_TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"HEARTBEAT_TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"MAX_DURATION", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"FINALIZE_TIMEOUT", grpc::StatusCode::DEADLINE_EXCEEDED},
      {"ADMISSION_CLOSED", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_SHUTDOWN", grpc::StatusCode::UNAVAILABLE},
      {"DEPLOYMENT_RETIRED", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_NOT_READY", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_EXIT", grpc::StatusCode::UNAVAILABLE},
      {"WORKER_CRASH_LOOP", grpc::StatusCode::UNAVAILABLE},
      {"NO_HEARTBEAT", grpc::StatusCode::UNAVAILABLE},
      {"BACKEND_UNAVAILABLE", grpc::StatusCode::UNAVAILABLE},
      {"BACKEND_RESET", grpc::StatusCode::UNAVAILABLE},
      {"SIDECAR_PROCESS_EXIT", grpc::StatusCode::UNAVAILABLE},
      {"SIDECAR_STARTUP_FAILED", grpc::StatusCode::UNAVAILABLE},
      {"CLIENT_CANCELLED", grpc::StatusCode::CANCELLED},
      {"SIDECAR_MALFORMED_RESPONSE", grpc::StatusCode::INTERNAL},
      {"INTERNAL", grpc::StatusCode::INTERNAL},
  };
  const auto* reasons = wire::FailureReason_descriptor();
  for (int index = 1; index < reasons->value_count(); ++index) {
    const auto* value = reasons->value(index);
    const std::string name =
        std::string{value->name()}.substr(std::string{"FAILURE_REASON_"}.size());
    SCOPED_TRACE(name);
    const auto row = expected.find(name);
    ASSERT_NE(row, expected.end()) << "a reason with no status in this table";
    wire::SessionClosed closed;
    closed.set_state(wire::SESSION_STATE_FAILED);
    *closed.mutable_cause() =
        stream::end_cause(static_cast<wire::FailureReason>(value->number()), "");
    const auto status = stream::session_end_status(closed);
    EXPECT_EQ(status.error_code(), row->second);
    EXPECT_NE(status.error_message().find(value->name()), std::string::npos);
  }
  EXPECT_EQ(expected.size(), static_cast<std::size_t>(reasons->value_count() - 1));
}

TEST(StreamMapping, OnlyASessionClosedWithoutACauseEndsItsCallOk) {
  wire::SessionClosed closed;
  closed.set_state(wire::SESSION_STATE_CLOSED);
  EXPECT_TRUE(stream::session_end_status(closed).ok());
  closed.set_state(wire::SESSION_STATE_FAILED);
  EXPECT_EQ(stream::session_end_status(closed).error_code(), grpc::StatusCode::INTERNAL);
  closed.set_state(wire::SESSION_STATE_CLOSED);
  *closed.mutable_cause() = stream::end_cause(wire::FAILURE_REASON_WORKER_SHUTDOWN, "");
  EXPECT_EQ(stream::session_end_status(closed).error_code(), grpc::StatusCode::UNAVAILABLE);
}

using tensorplate::serving::SessionInputKind;
using tensorplate::serving::SessionTerms;

/// 16 kHz 16-bit mono in frames of 20 to 320 ms.
SessionTerms audio_terms() {
  SessionTerms terms;
  terms.input_kind = SessionInputKind::Audio;
  terms.audio_format = {tensorplate::serving::StreamAudioEncoding::PcmS16Le, 16'000, 1};
  terms.min_frame = std::chrono::milliseconds{20};
  terms.max_frame = std::chrono::milliseconds{320};
  return terms;
}

SessionTerms text_terms() {
  SessionTerms terms;
  terms.input_kind = SessionInputKind::Text;
  terms.max_segment_text_bytes = 100;
  terms.max_synthesis_text_bytes = 250;
  return terms;
}

wire::Audio frame(std::uint32_t samples, std::uint64_t offset) {
  wire::Audio audio;
  audio.set_sample_count(samples);
  audio.set_sample_offset(offset);
  audio.set_data(std::string(std::size_t{samples} * 2, '\0'));
  return audio;
}

wire::TextSegment segment(std::uint64_t id, std::string text) {
  wire::TextSegment out;
  out.set_segment_id(id);
  out.set_text(std::move(text));
  return out;
}

/// The check a refusal names: its message for an invalid event, else its
/// reason.
std::string refusal(const tensorplate::Result<void>& checked) {
  if (checked) {
    return "accepted";
  }
  const auto& error = checked.error();
  EXPECT_EQ(error.code, Error::Code::ConfigInvalid);
  return error.context == "invalid_event" ? error.message : error.context.value_or("");
}

TEST(StreamMapping, AnAudioFrameIsCheckedAgainstTheTermsAndWhatCameBefore) {
  const auto terms = audio_terms();
  stream::InputHistory history;
  history.samples = 640;
  EXPECT_EQ(refusal(stream::check_audio(frame(320, 640), terms, history)), "accepted");
  EXPECT_EQ(refusal(stream::check_audio(frame(5'120, 640), terms, history)), "accepted");
  EXPECT_EQ(refusal(stream::check_audio(frame(5'121, 640), terms, history)), "frame_too_long");
  // Shorter than the smallest granted is still a frame: it may be the last.
  EXPECT_EQ(refusal(stream::check_audio(frame(1, 640), terms, history)), "accepted");
  EXPECT_EQ(refusal(stream::check_audio(frame(320, 639), terms, history)),
            "sample_offset_mismatch");
  EXPECT_EQ(refusal(stream::check_audio(frame(320, 641), terms, history)),
            "sample_offset_mismatch");
  EXPECT_EQ(refusal(stream::check_audio(frame(0, 640), terms, history)), "empty_input");

  auto odd = frame(320, 640);
  odd.mutable_data()->pop_back();
  EXPECT_EQ(refusal(stream::check_audio(odd, terms, history)), "frame_size_mismatch");
  odd.mutable_data()->append(2, '\0');
  EXPECT_EQ(refusal(stream::check_audio(odd, terms, history)), "frame_size_mismatch");

  history.short_frame = true;
  EXPECT_EQ(refusal(stream::check_audio(frame(320, 640), terms, history)),
            "audio_after_short_frame");
  EXPECT_EQ(refusal(stream::check_audio(frame(320, 0), text_terms(), {})), "wrong_input_kind");
}

TEST(StreamMapping, AFramesBytesFollowTheFormatsSampleWidthAndChannels) {
  auto terms = audio_terms();
  terms.audio_format = {tensorplate::serving::StreamAudioEncoding::Mulaw, 8'000, 2};
  wire::Audio audio;
  audio.set_sample_count(160);
  audio.set_data(std::string(320, '\0'));
  EXPECT_EQ(refusal(stream::check_audio(audio, terms, {})), "accepted");
  audio.set_data(std::string(160, '\0'));
  EXPECT_EQ(refusal(stream::check_audio(audio, terms, {})), "frame_size_mismatch");
  // 20 ms of 8 kHz is 160 samples.
  EXPECT_FALSE(stream::is_short_frame(160, terms));
  EXPECT_TRUE(stream::is_short_frame(159, terms));
}

TEST(StreamMapping, ATextSegmentIsCheckedAgainstTheTermsAndWhatCameBefore) {
  const auto terms = text_terms();
  stream::InputHistory history;
  history.last_segment_id = 7;
  history.synthesis_text_bytes = 200;
  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, "hi"), terms, history)), "accepted");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(7, "hi"), terms, history)),
            "segment_id_not_increasing");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(6, "hi"), terms, history)),
            "segment_id_not_increasing");
  EXPECT_EQ(
      refusal(stream::check_text_segment(segment(stream::kMaxWireInteger, "hi"), terms, history)),
      "accepted");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(stream::kMaxWireInteger + 1, "hi"), terms,
                                               history)),
            "segment_id_out_of_range");
  // The first segment of a session may carry any id, zero included.
  EXPECT_EQ(refusal(stream::check_text_segment(segment(0, "hi"), terms, {})), "accepted");

  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, std::string(50, 'a')), terms, history)),
            "accepted");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, std::string(51, 'a')), terms, history)),
            "synthesis_text_limit_exceeded");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, std::string(100, 'a')), terms, {})),
            "accepted");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, std::string(101, 'a')), terms, {})),
            "segment_too_long");

  for (const char* blank : {"", " ", "\t\n\v\f\r "}) {
    EXPECT_EQ(refusal(stream::check_text_segment(segment(8, blank), terms, history)),
              "empty_input");
  }
  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, " a "), terms, history)), "accepted");
  // A control character that is not whitespace is text.
  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, "\x1f"), terms, history)), "accepted");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(8, "\x08"), terms, history)), "accepted");

  auto turn = segment(8, "hi");
  turn.set_turn_id(std::string(64, 't'));
  EXPECT_EQ(refusal(stream::check_text_segment(turn, terms, history)), "accepted");
  turn.set_turn_id(std::string(65, 't'));
  EXPECT_EQ(refusal(stream::check_text_segment(turn, terms, history)), "turn_id_malformed");
  turn.set_turn_id("caf\xc3\xa9");
  EXPECT_EQ(refusal(stream::check_text_segment(turn, terms, history)), "turn_id_malformed");
  EXPECT_EQ(refusal(stream::check_text_segment(segment(1, "hi"), audio_terms(), {})),
            "wrong_input_kind");
}

TEST(StreamMapping, CreditAndStatusCarryTheNumbersOfOneTransition) {
  tensorplate::serving::LogicalSessionStatus status;
  status.state = tensorplate::serving::LogicalSessionState::Finalizing;
  status.input_queue_depth = 3;
  status.input_credit_bytes = tensorplate::serving::LogicalSessionUsage::create(5, 8'192).value();
  status.output_pcm_bytes = tensorplate::serving::LogicalSessionUsage::create(7, 96'000).value();
  status.output_metadata_bytes =
      tensorplate::serving::LogicalSessionUsage::create(9, 16'384).value();
  const auto waiting = tensorplate::serving::LogicalSessionUsage::create(1, 2).value();

  const auto credit = stream::wire_credit(status, waiting);
  EXPECT_EQ(credit.bytes().used(), 5U);
  EXPECT_EQ(credit.bytes().limit(), 8'192U);
  EXPECT_EQ(credit.waiting_segments().used(), 1U);
  EXPECT_EQ(credit.waiting_segments().limit(), 2U);

  const auto out = stream::wire_status(status, waiting);
  EXPECT_EQ(out.state(), wire::SESSION_STATE_FINALIZING);
  EXPECT_EQ(out.input_queue_depth(), 3U);
  EXPECT_EQ(out.input_credit().SerializeAsString(), credit.SerializeAsString());
  EXPECT_EQ(out.output_pcm_bytes().used(), 7U);
  EXPECT_EQ(out.output_pcm_bytes().limit(), 96'000U);
  EXPECT_EQ(out.output_metadata_bytes().used(), 9U);
  EXPECT_EQ(out.output_metadata_bytes().limit(), 16'384U);
}

TEST(StreamMapping, EachTaskOutputBecomesItsServerEvent) {
  namespace serving = tensorplate::serving;
  const serving::StreamAudioFormat format{serving::StreamAudioEncoding::Mulaw, 8'000, 1};

  const std::vector<std::pair<serving::EndpointReason, wire::EndpointReason>> reasons = {
      {serving::EndpointReason::Silence, wire::ENDPOINT_REASON_SILENCE},
      {serving::EndpointReason::ClientFinalize, wire::ENDPOINT_REASON_CLIENT_FINALIZE},
      {serving::EndpointReason::DurationLimit, wire::ENDPOINT_REASON_DURATION_LIMIT},
      {serving::EndpointReason::HalfClose, wire::ENDPOINT_REASON_HALF_CLOSE},
  };
  for (const auto& [reason, expected] : reasons) {
    const auto event =
        stream::task_event(serving::EndpointDetectedOutput{3, 4'800, reason}, format);
    ASSERT_TRUE(event && event->has_endpoint_detected());
    EXPECT_EQ(event->endpoint_detected().utterance_id(), 3U);
    EXPECT_EQ(event->endpoint_detected().end_sample_offset(), 4'800U);
    EXPECT_EQ(event->endpoint_detected().reason(), expected);
    EXPECT_EQ(event->sequence(), 0U) << "the stream assigns the sequence";
  }

  serving::FinalTranscriptOutput transcript;
  transcript.utterance_id = 3;
  transcript.segments = {{10, 20, "first"}, {20, 35, " second"}};
  transcript.end_sample_offset = 4'800;
  transcript.finalize_sequence = 12;
  auto event = stream::task_event(transcript, format);
  ASSERT_TRUE(event && event->has_final_transcript());
  EXPECT_EQ(event->final_transcript().utterance_id(), 3U);
  EXPECT_EQ(event->final_transcript().end_sample_offset(), 4'800U);
  EXPECT_EQ(event->final_transcript().finalize_sequence(), 12U);
  ASSERT_EQ(event->final_transcript().segments_size(), 2);
  EXPECT_EQ(event->final_transcript().segments(1).start_us(), 20U);
  EXPECT_EQ(event->final_transcript().segments(1).end_us(), 35U);
  EXPECT_EQ(event->final_transcript().segments(1).text(), " second");
  EXPECT_EQ(event->final_transcript().segments(0).text(), "first");

  event = stream::task_event(
      serving::AudioChunkOutput{9, 2, 320, {std::byte{0xff}, std::byte{0}, std::byte{0x7f}}},
      format);
  ASSERT_TRUE(event && event->has_audio_chunk());
  EXPECT_EQ(event->audio_chunk().segment_id(), 9U);
  EXPECT_EQ(event->audio_chunk().chunk_index(), 2U);
  EXPECT_EQ(event->audio_chunk().segment_sample_offset(), 320U);
  EXPECT_EQ(event->audio_chunk().data(), std::string("\xff\0\x7f", 3));
  EXPECT_EQ(event->audio_chunk().format().encoding(), wire::AUDIO_ENCODING_MULAW);
  EXPECT_EQ(event->audio_chunk().format().sample_rate_hz(), 8'000U);
  EXPECT_EQ(event->audio_chunk().format().channels(), 1U);

  event = stream::task_event(serving::SegmentCompletedOutput{9, 6'400, 40}, format);
  ASSERT_TRUE(event && event->has_segment_completed());
  EXPECT_EQ(event->segment_completed().segment_id(), 9U);
  EXPECT_EQ(event->segment_completed().total_samples(), 6'400U);
  EXPECT_EQ(event->segment_completed().chunk_count(), 40U);

  event = stream::task_event(serving::SynthesisCompletedOutput{2, 12'800, 14}, format);
  ASSERT_TRUE(event && event->has_synthesis_completed());
  EXPECT_EQ(event->synthesis_completed().segment_count(), 2U);
  EXPECT_EQ(event->synthesis_completed().total_samples(), 12'800U);
  EXPECT_EQ(event->synthesis_completed().finalize_sequence(), 14U);
}

TEST(StreamMapping, LocalhostIsTheIpv4LoopbackAndIpv6IsBracketed) {
  EXPECT_EQ(stream::listen_address("localhost", 7001), "127.0.0.1:7001");
  EXPECT_EQ(stream::listen_address("127.0.0.1", 0), "127.0.0.1:0");
  EXPECT_EQ(stream::listen_address("::1", 7001), "[::1]:7001");
  EXPECT_EQ(stream::listen_address("2001:db8::7", 7001), "[2001:db8::7]:7001");
}

}  // namespace
