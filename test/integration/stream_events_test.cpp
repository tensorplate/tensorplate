// SPDX-License-Identifier: Apache-2.0
//
// The client events a stream session serves after Open, with a real client
// over loopback and the synthetic session dispatch behind the listener. What
// the dispatch yields is a function of the bytes handed over, so each test
// that reads output also checks that the right bytes were handed over.

#include <grpcpp/grpcpp.h>
#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <functional>
#include <iostream>
#include <memory>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <thread>
#include <vector>

#include "fake_scheduler_clock.hpp"
#include "serving/session/output_queue.hpp"
#include "stream_listener_fixture.hpp"

namespace {

using namespace std::chrono_literals;
using tensorplate::serving::LogicalSessionEvent;
using tensorplate::serving::LogicalSessionState;
using tensorplate::testing::FakeSchedulerClock;
using tensorplate::testing::stream_listener::Client;
using tensorplate::testing::stream_listener::end_of;
using tensorplate::testing::stream_listener::eventually;
using tensorplate::testing::stream_listener::Served;
using Fake = tensorplate::testing::SyntheticSessionDispatch;
namespace wire = tensorplate::stream::v1;

/// What the synthetic dispatch grants: 16 kHz 16-bit mono in frames of 20 to
/// 320 ms, and utterances of 30 s.
constexpr std::uint32_t kFrame = 5'120;
constexpr std::uint32_t kShortestFrame = 320;
constexpr std::uint64_t kUtterance = 480'000;
constexpr std::uint64_t kCreditBytes = 32'000;

/// An Audio the client's accounts know nothing of.
wire::ClientEvent raw_audio(std::uint32_t samples, std::uint64_t offset, std::size_t bytes) {
  wire::ClientEvent event;
  auto& audio = *event.mutable_audio();
  audio.set_sample_count(samples);
  audio.set_sample_offset(offset);
  audio.set_data(std::string(bytes, '\1'));
  return event;
}
wire::ClientEvent raw_audio(std::uint32_t samples, std::uint64_t offset) {
  return raw_audio(samples, offset, std::size_t{samples} * 2);
}

wire::ClientEvent raw_text(std::uint64_t segment_id, std::string text) {
  wire::ClientEvent event;
  event.mutable_text_segment()->set_segment_id(segment_id);
  event.mutable_text_segment()->set_text(std::move(text));
  return event;
}

/// Reads to the session's SessionClosed, past whatever precedes it.
wire::SessionClosed closed_of(Client& client) {
  while (auto event = client.next()) {
    if (event->has_session_closed()) {
      return event->session_closed();
    }
  }
  ADD_FAILURE() << "the stream ended without SessionClosed";
  return {};
}

/// Reads an utterance's endpoint and its final, and checks the transcript
/// against the one the dispatch yields for `audio`, the utterance's bytes.
void expect_utterance(Client& client, wire::EndpointReason reason, std::uint64_t utterance_id,
                      std::uint64_t end_sample, std::string_view audio,
                      std::uint64_t finalize_sequence) {
  const auto endpoint = client.next();
  ASSERT_TRUE(endpoint.has_value());
  ASSERT_TRUE(endpoint->has_endpoint_detected()) << endpoint->ShortDebugString();
  EXPECT_EQ(endpoint->endpoint_detected().reason(), reason);
  EXPECT_EQ(endpoint->endpoint_detected().utterance_id(), utterance_id);
  EXPECT_EQ(endpoint->endpoint_detected().end_sample_offset(), end_sample);
  const auto transcript = client.next();
  ASSERT_TRUE(transcript.has_value());
  ASSERT_TRUE(transcript->has_final_transcript()) << transcript->ShortDebugString();
  const auto& final = transcript->final_transcript();
  EXPECT_EQ(final.utterance_id(), utterance_id);
  EXPECT_EQ(final.end_sample_offset(), end_sample);
  EXPECT_EQ(final.finalize_sequence(), finalize_sequence);
  if (audio.empty()) {
    EXPECT_EQ(final.segments_size(), 0);
    return;
  }
  ASSERT_EQ(final.segments_size(), 1);
  EXPECT_EQ(final.segments(0).text(),
            Fake::transcript_of(std::as_bytes(std::span{audio.data(), audio.size()})));
}

/// Reads a whole segment: contiguous chunks that carry the speech of `text`
/// in the session's output format, then the segment's completion.
void expect_segment(Client& client, std::uint64_t segment_id, std::string_view text,
                    const wire::AudioFormat& format) {
  const std::uint64_t total = text.size() * Fake::kSamplesPerTextByte;
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
    const std::uint64_t samples = std::min<std::uint64_t>(Fake::kChunkSamples, total - offset);
    ASSERT_EQ(chunk.data().size(), samples * 2);
    std::string expected(samples * 2, '\0');
    for (std::uint64_t index = 0; index < samples; ++index) {
      const auto sample = static_cast<std::uint16_t>(Fake::sample_of(text, offset + index));
      expected[index * 2] = static_cast<char>(sample & 0xffU);
      expected[index * 2 + 1] = static_cast<char>(sample >> 8U);
    }
    ASSERT_TRUE(chunk.data() == expected) << "chunk " << chunks << " is not the segment's speech";
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

TEST(StreamEvents, AnUtteranceIsTranscribedAndItsFinalizeAnswered) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.send_audio(3 * kFrame, kFrame));
  const auto last_audio = client.sequence;
  const auto asked = client.finalize();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 1,
                                           3 * kFrame, client.audio_sent, asked));
  ASSERT_FALSE(client.accepted.empty());
  EXPECT_EQ(client.accepted.back().accepted_sequence(), last_audio);
  EXPECT_EQ(served.dispatch().seen.finalizes(), std::vector<std::uint64_t>{asked});

  ASSERT_TRUE(client.call->WritesDone());
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_FALSE(closed.has_cause());
  EXPECT_EQ(closed.last_accepted_sequence(), last_audio);
  EXPECT_EQ(closed.totals().input_audio_samples(), 3U * kFrame);
  EXPECT_EQ(closed.totals().input_text_bytes(), 0U);
  EXPECT_EQ(closed.totals().utterances_completed(), 1U);
  EXPECT_EQ(closed.totals().output_audio_samples(), 0U);
  EXPECT_EQ(closed.totals().segments_completed(), 0U);
  EXPECT_TRUE(end_of(client).ok());
}

TEST(StreamEvents, ASegmentIsSynthesizedInChunksAndItsFinalizeAnswered) {
  Served served;
  wire::Ready ready;
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING, &ready);
  // 1,040 samples: two whole chunks and one of 80.
  const std::string text = "thirteen byte";
  ASSERT_TRUE(client.send_text(7, text));
  const auto segment = client.sequence;
  const auto asked = client.finalize();
  ASSERT_NO_FATAL_FAILURE(expect_segment(client, 7, text, ready.output_format()));
  const auto answer = client.next();
  ASSERT_TRUE(answer.has_value());
  ASSERT_TRUE(answer->has_synthesis_completed()) << answer->ShortDebugString();
  EXPECT_EQ(answer->synthesis_completed().segment_count(), 1U);
  EXPECT_EQ(answer->synthesis_completed().total_samples(), 1'040U);
  EXPECT_EQ(answer->synthesis_completed().finalize_sequence(), asked);

  ASSERT_TRUE(client.call->WritesDone());
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_FALSE(closed.has_cause());
  EXPECT_EQ(closed.last_accepted_sequence(), segment);
  EXPECT_EQ(closed.totals().input_text_bytes(), text.size());
  EXPECT_EQ(closed.totals().input_audio_samples(), 0U);
  EXPECT_EQ(closed.totals().output_audio_samples(), 1'040U);
  EXPECT_EQ(closed.totals().segments_completed(), 1U);
  EXPECT_EQ(closed.totals().utterances_completed(), 0U);
  EXPECT_TRUE(end_of(client).ok());
}

// Parked before its work, the dispatch returns no credit: each Accepted is
// then exactly one input, and the status is what the session holds.
TEST(StreamEvents, AcceptedAndTheStatusReplyCarryTheCreditOfTheirOwnTransition) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  for (std::uint64_t frames = 1; frames <= 2; ++frames) {
    ASSERT_TRUE(client.send_audio(kFrame));
    const auto event = client.read();
    ASSERT_TRUE(event.has_value());
    ASSERT_TRUE(event->has_accepted()) << event->ShortDebugString();
    EXPECT_EQ(event->accepted().accepted_sequence(), client.sequence);
    EXPECT_EQ(event->accepted().input_credit().bytes().used(), frames * kFrame * 2);
    EXPECT_EQ(event->accepted().input_credit().bytes().limit(), kCreditBytes);
    EXPECT_EQ(event->accepted().input_credit().waiting_segments().limit(), 0U);
  }
  const auto last_audio = client.sequence;

  wire::ClientEvent request;
  request.mutable_status_request();
  ASSERT_TRUE(client.send(request));
  const auto reply = client.read();
  ASSERT_TRUE(reply.has_value());
  ASSERT_TRUE(reply->has_status()) << reply->ShortDebugString();
  EXPECT_EQ(reply->status().state(), wire::SESSION_STATE_ACTIVE);
  EXPECT_EQ(reply->status().input_queue_depth(), 2U);
  EXPECT_EQ(reply->status().input_credit().bytes().used(), 2U * kFrame * 2);
  EXPECT_EQ(reply->status().input_credit().bytes().limit(), kCreditBytes);
  EXPECT_EQ(reply->status().output_pcm_bytes().limit(), 0U);
  EXPECT_EQ(reply->status().output_metadata_bytes().limit(), 16'384U);

  // Returned credit is announced under the sequence of the Accepted before.
  fake.resume();
  for (;;) {
    const auto event = client.read();
    ASSERT_TRUE(event.has_value());
    ASSERT_TRUE(event->has_accepted()) << event->ShortDebugString();
    EXPECT_EQ(event->accepted().accepted_sequence(), last_audio);
    if (event->accepted().input_credit().bytes().used() == 0) {
      break;
    }
  }
}

TEST(StreamEvents, ATextSessionReportsItsWaitingSegmentsAndFailsOnOneTooMany) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  std::uint64_t text_bytes = 0;
  for (std::uint64_t segments = 1; segments <= 2; ++segments) {
    const std::string text(segments + 1, 'a');
    text_bytes += text.size();
    ASSERT_TRUE(client.send_text(segments, text));
    const auto event = client.read();
    ASSERT_TRUE(event.has_value());
    ASSERT_TRUE(event->has_accepted()) << event->ShortDebugString();
    EXPECT_EQ(event->accepted().accepted_sequence(), client.sequence);
    EXPECT_EQ(event->accepted().input_credit().bytes().used(), text_bytes);
    EXPECT_EQ(event->accepted().input_credit().bytes().limit(), 8'192U);
    EXPECT_EQ(event->accepted().input_credit().waiting_segments().used(), segments);
    EXPECT_EQ(event->accepted().input_credit().waiting_segments().limit(), 2U);
  }
  const auto last_segment = client.sequence;

  wire::ClientEvent request;
  request.mutable_status_request();
  ASSERT_TRUE(client.send(request));
  const auto reply = client.read();
  ASSERT_TRUE(reply.has_value());
  ASSERT_TRUE(reply->has_status()) << reply->ShortDebugString();
  EXPECT_EQ(reply->status().input_queue_depth(), 2U);
  EXPECT_EQ(reply->status().input_credit().bytes().used(), text_bytes);
  EXPECT_EQ(reply->status().input_credit().waiting_segments().used(), 2U);
  EXPECT_EQ(reply->status().output_pcm_bytes().limit(), 96'000U);

  // A third waiting segment is one more than the credit.
  ASSERT_TRUE(client.send(raw_text(3, "c")));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_INPUT_CREDIT_EXCEEDED);
  EXPECT_EQ(closed.last_accepted_sequence(), last_segment);
  EXPECT_EQ(closed.totals().input_text_bytes(), text_bytes);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::RESOURCE_EXHAUSTED);
  fake.resume();
}

TEST(StreamEvents, AudioAboveTheCreditFailsTheSession) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  // Three frames are 30,720 of the 32,000 bytes; nothing returns meanwhile.
  for (int frame = 0; frame < 3; ++frame) {
    ASSERT_TRUE(client.send_audio(kFrame));
    const auto event = client.read();
    ASSERT_TRUE(event && event->has_accepted());
  }
  const auto last_audio = client.sequence;
  ASSERT_TRUE(client.send(raw_audio(kFrame, 3 * kFrame)));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_INPUT_CREDIT_EXCEEDED);
  EXPECT_EQ(closed.cause().code(), wire::ERROR_CODE_RESOURCE_EXHAUSTED);
  EXPECT_EQ(closed.last_accepted_sequence(), last_audio);
  EXPECT_EQ(closed.totals().input_audio_samples(), 3U * kFrame);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::RESOURCE_EXHAUSTED);
  fake.resume();
}

TEST(StreamEvents, AShortFrameIsValidAsTheLastBeforeAFinalize) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.send_audio(kFrame));
  ASSERT_TRUE(client.send_audio(100));
  const auto first = client.finalize();
  const std::uint64_t first_end = kFrame + 100;
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 1,
                                           first_end, client.audio_sent, first));
  // The client's own Finalize ended the utterance on the short frame, so
  // audio is taken again.
  ASSERT_TRUE(client.send_audio(kFrame));
  const auto second = client.finalize();
  ASSERT_NO_FATAL_FAILURE(
      expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 2, first_end + kFrame,
                       std::string_view{client.audio_sent}.substr(first_end * 2), second));
}

TEST(StreamEvents, AudioAfterAShortFrameFailsTheSession) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.send_audio(100));
  const auto short_frame = client.sequence;
  ASSERT_TRUE(client.send_audio(kFrame));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_INVALID_EVENT);
  EXPECT_EQ(closed.cause().detail(), "audio_after_short_frame");
  EXPECT_EQ(closed.last_accepted_sequence(), short_frame);
  EXPECT_EQ(closed.totals().input_audio_samples(), 100U);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::INVALID_ARGUMENT);
}

// The client cannot have known of the endpoint when it sent the short frame:
// only its own Finalize or half-close ends an utterance on one.
TEST(StreamEvents, AnAutomaticEndpointDoesNotMakeAShortFrameValid) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  // An utterance of exactly the longest granted, whose last frame is short:
  // the dispatch ends it there, at once.
  ASSERT_TRUE(client.send_audio(kUtterance - 100, kFrame));
  ASSERT_TRUE(client.send_audio(100));
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_DURATION_LIMIT, 1,
                                           kUtterance, client.audio_sent, 0));
  ASSERT_TRUE(served.dispatch().seen.saw(LogicalSessionEvent::AutomaticEndpoint));
  ASSERT_TRUE(client.send_audio(kFrame));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().detail(), "audio_after_short_frame");
  EXPECT_EQ(closed.totals().input_audio_samples(), kUtterance);
  EXPECT_EQ(closed.totals().utterances_completed(), 1U);
}

// The two orders in which a client's Finalize and an automatic endpoint meet.
// In both the Finalize is answered by the final that carries its sequence,
// the second one, and it alone is handed to the dispatch: the
// `start_finalize` an automatic endpoint shows the sink is not.
TEST(StreamEvents, AFinalizeTakesOverTheFinalizationOfAnAutomaticEndpoint) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(client.send_audio(kUtterance, kFrame));
  ASSERT_TRUE(eventually([&] { return seen.saw(LogicalSessionEvent::AutomaticEndpoint); }));
  ASSERT_TRUE(client.send_audio(kShortestFrame));
  const auto asked = client.finalize();
  ASSERT_TRUE(eventually([&] { return seen.finalizes().size() == 1; }));
  fake.resume();

  const std::string_view audio{client.audio_sent};
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_DURATION_LIMIT, 1,
                                           kUtterance, audio.substr(0, kUtterance * 2), 0));
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 2,
                                           kUtterance + kShortestFrame,
                                           audio.substr(kUtterance * 2), asked));
  EXPECT_EQ(seen.finalizes(), std::vector<std::uint64_t>{asked});
}

TEST(StreamEvents, AnAutomaticEndpointJoinsTheFinalizationOfAFinalize) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.send_audio(kUtterance - kShortestFrame, kFrame));
  ASSERT_TRUE(eventually([&] { return fake.wait_idle(50ms); }));
  // The frame that completes the utterance is parked behind the gate, so the
  // Finalize is applied before the endpoint it will raise.
  fake.pause(Fake::Gate::BeforeWork);
  ASSERT_TRUE(client.send_audio(kShortestFrame));
  const auto asked = client.finalize();
  ASSERT_TRUE(eventually([&] { return seen.finalizes().size() == 1; }));
  ASSERT_FALSE(seen.saw(LogicalSessionEvent::AutomaticEndpoint));
  fake.resume();

  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_DURATION_LIMIT, 1,
                                           kUtterance, client.audio_sent, 0));
  ASSERT_NO_FATAL_FAILURE(
      expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 2, kUtterance, {}, asked));
  EXPECT_TRUE(seen.saw(LogicalSessionEvent::AutomaticEndpoint));
  EXPECT_EQ(seen.finalizes(), std::vector<std::uint64_t>{asked});
}

// While a session is finalizing for a Finalize, a repeated Finalize starts
// nothing and input fails the session: the manager's rules, as a client
// meets them.
TEST(StreamEvents, WhileAFinalizeIsFinalizingARepeatStartsNothingAndInputFailsTheSession) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(client.send_audio(kFrame));
  const auto last_audio = client.sequence;
  const auto asked = client.finalize();
  (void)client.finalize();
  // Answered once both Finalizes were read.
  wire::ClientEvent ping;
  ping.mutable_ping();
  ASSERT_TRUE(client.send(ping));
  const auto pong = client.next();
  ASSERT_TRUE(pong.has_value());
  ASSERT_TRUE(pong->has_pong()) << pong->ShortDebugString();
  EXPECT_EQ(seen.finalizes(), std::vector<std::uint64_t>{asked});
  EXPECT_EQ(seen.state(), LogicalSessionState::Finalizing);

  ASSERT_TRUE(client.send(raw_audio(kFrame, kFrame)));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_ILLEGAL_TRANSITION);
  EXPECT_EQ(closed.last_accepted_sequence(), last_audio);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::FAILED_PRECONDITION);
  fake.resume();
}

// A repeated Finalize starts nothing, as one a drain does not take; only the
// latter stops the checks.
TEST(StreamEvents, InputIsCheckedAgainAfterARepeatedFinalizeWasAnswered) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(client.send_audio(kFrame));
  const auto asked = client.finalize();
  (void)client.finalize();
  // Answered once both Finalizes were read.
  wire::ClientEvent ping;
  ping.mutable_ping();
  ASSERT_TRUE(client.send(ping));
  const auto pong = client.next();
  ASSERT_TRUE(pong.has_value());
  ASSERT_TRUE(pong->has_pong()) << pong->ShortDebugString();
  fake.resume();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 1, kFrame,
                                           client.audio_sent, asked));
  ASSERT_TRUE(eventually([&] { return seen.state() == LogicalSessionState::Active; }));

  ASSERT_TRUE(client.send(raw_audio(kFrame, 0)));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_INVALID_EVENT);
  EXPECT_EQ(closed.cause().detail(), "sample_offset_mismatch");
}

TEST(StreamEvents, EveryInputCheckFailsTheSessionAndNamesItself) {
  struct Case {
    const char* name;
    wire::SessionMode mode;
    /// Sends valid input first, if the case needs some.
    std::function<bool(Client&)> before;
    wire::ClientEvent bad;
    wire::FailureReason reason;
    /// Empty where the reason says it all.
    const char* detail;
  };
  const auto nothing = [](Client&) { return true; };
  const auto stt = wire::SESSION_MODE_STT_STREAMING;
  const auto tts = wire::SESSION_MODE_TTS_STREAMING;
  const auto invalid = wire::FAILURE_REASON_INVALID_EVENT;
  const auto empty = wire::FAILURE_REASON_EMPTY_INPUT;
  auto long_turn = raw_text(1, "hi");
  long_turn.mutable_text_segment()->set_turn_id(std::string(65, 't'));
  const std::vector<Case> cases = {
      {"text in an audio session", stt, nothing, raw_text(1, "hi"), invalid, "wrong_input_kind"},
      {"audio in a text session", tts, nothing, raw_audio(kFrame, 0), invalid, "wrong_input_kind"},
      {"audio without data", stt, nothing, raw_audio(0, 0), empty, ""},
      {"a frame whose bytes are not its samples", stt, nothing,
       raw_audio(kFrame, 0, kFrame * 2 - 1), invalid, "frame_size_mismatch"},
      {"a frame above the longest granted", stt, nothing, raw_audio(kFrame + 1, 0), invalid,
       "frame_too_long"},
      {"a first frame that does not start at zero", stt, nothing, raw_audio(kFrame, 1), invalid,
       "sample_offset_mismatch"},
      {"a frame that skips a sample", stt, [](Client& client) { return client.send_audio(kFrame); },
       raw_audio(kFrame, kFrame + 1), invalid, "sample_offset_mismatch"},
      {"a segment without text", tts, nothing, raw_text(1, ""), empty, ""},
      {"a segment of whitespace", tts, nothing, raw_text(1, " \t\r\n"), empty, ""},
      {"a segment id that does not grow", tts,
       [](Client& client) { return client.send_text(5, "a"); }, raw_text(5, "b"), invalid,
       "segment_id_not_increasing"},
      {"a segment id above 2^53 - 1", tts, nothing, raw_text(std::uint64_t{1} << 53U, "a"), invalid,
       "segment_id_out_of_range"},
      {"a segment above the segment limit", tts, nothing, raw_text(1, std::string(4'097, 'a')),
       invalid, "segment_too_long"},
      {"a turn id above 64 characters", tts, nothing, long_turn, invalid, "turn_id_malformed"},
  };
  for (const auto& test : cases) {
    SCOPED_TRACE(test.name);
    Served served;
    auto client = served.ready(test.mode);
    ASSERT_TRUE(test.before(client));
    const auto accepted_before = client.sequence == 1 ? 0 : client.sequence;
    ASSERT_TRUE(client.send(test.bad));
    const auto closed = closed_of(client);
    EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
    EXPECT_EQ(closed.cause().reason(), test.reason);
    EXPECT_EQ(closed.cause().code(), wire::ERROR_CODE_CONFIG_INVALID);
    if (*test.detail != '\0') {
      EXPECT_EQ(closed.cause().detail(), test.detail);
    }
    EXPECT_EQ(closed.last_accepted_sequence(), accepted_before);
    EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::INVALID_ARGUMENT);
  }
}

TEST(StreamEvents, TheSynthesisLimitCountsFromTheLastFinalize) {
  Served served;
  wire::Ready ready;
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING, &ready, 120s);
  // Three segments of the largest size are all one synthesis takes.
  const std::string text(4'096, 'x');
  for (std::uint64_t id = 1; id <= 3; ++id) {
    ASSERT_TRUE(client.send_text(id, text));
  }
  const auto asked = client.finalize();
  for (std::uint64_t id = 1; id <= 3; ++id) {
    ASSERT_NO_FATAL_FAILURE(expect_segment(client, id, text, ready.output_format()));
  }
  const auto answer = client.next();
  ASSERT_TRUE(answer && answer->has_synthesis_completed());
  EXPECT_EQ(answer->synthesis_completed().segment_count(), 3U);
  EXPECT_EQ(answer->synthesis_completed().finalize_sequence(), asked);

  // The Finalize began a new count: three more are taken, and one byte more
  // than that is not.
  for (std::uint64_t id = 4; id <= 6; ++id) {
    ASSERT_TRUE(client.send_text(id, text));
  }
  const auto last_segment = client.sequence;
  ASSERT_TRUE(client.send(raw_text(7, "y")));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_INVALID_EVENT);
  EXPECT_EQ(closed.cause().detail(), "synthesis_text_limit_exceeded");
  EXPECT_EQ(closed.last_accepted_sequence(), last_segment);
  EXPECT_EQ(closed.totals().input_text_bytes(), 6U * text.size());
}

TEST(StreamEvents, ACancelStopsOutputAndClosesTheSessionAsCancelled) {
  Served served;
  // The client's window stays at its first 64 KiB, less than the segment's
  // speech: the Cancel finds output still to send.
  grpc::ChannelArguments arguments;
  arguments.SetInt(GRPC_ARG_HTTP2_BDP_PROBE, 0);
  served.use_channel(arguments);
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING);
  ASSERT_TRUE(client.send_text(1, std::string(4'096, 'x')));
  for (int chunk = 0; chunk < 2; ++chunk) {
    const auto event = client.next();
    ASSERT_TRUE(event && event->has_audio_chunk());
  }
  wire::ClientEvent cancel;
  cancel.mutable_cancel();
  ASSERT_TRUE(client.send(cancel));
  // Chunks already on their way may precede the acknowledgement.
  for (;;) {
    const auto event = client.next();
    ASSERT_TRUE(event.has_value());
    if (event->has_cancel_accepted()) {
      break;
    }
    ASSERT_TRUE(event->has_audio_chunk()) << event->ShortDebugString();
  }
  // Nothing but the outcome follows it: no chunk, and no credit either.
  const auto last = client.read();
  ASSERT_TRUE(last.has_value());
  ASSERT_TRUE(last->has_session_closed()) << last->ShortDebugString();
  const auto& closed = last->session_closed();
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_CLIENT_CANCELLED);
  EXPECT_EQ(closed.cause().code(), wire::ERROR_CODE_CANCELLED);
  EXPECT_EQ(closed.totals().segments_completed(), 0U);
  const auto status = end_of(client);
  EXPECT_EQ(status.error_code(), grpc::StatusCode::CANCELLED);
  EXPECT_NE(status.error_message().find("FAILURE_REASON_CLIENT_CANCELLED"), std::string::npos);
  EXPECT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
}

// A worker's drain takes no input. The client cannot know yet and goes on
// sending frames that follow each other; none is held against a history that
// stopped with the drain.
TEST(StreamEvents, InputADrainDoesNotTakeIsNeitherAcceptedNorChecked) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(client.send_audio(kFrame));
  const auto last_audio = client.sequence;
  const auto asked = client.finalize();
  ASSERT_TRUE(eventually([&] { return seen.finalizes().size() == 1; }));
  served.listener().stop_admission();
  ASSERT_TRUE(eventually([&] { return seen.state() == LogicalSessionState::Draining; }));
  ASSERT_TRUE(client.send(raw_audio(kFrame, kFrame)));
  ASSERT_TRUE(client.send(raw_audio(kFrame, 2 * kFrame)));
  // Answered once the two frames before it were read.
  wire::ClientEvent ping;
  ping.mutable_ping();
  ASSERT_TRUE(client.send(ping));
  const auto pong = client.next();
  ASSERT_TRUE(pong.has_value());
  ASSERT_TRUE(pong->has_pong()) << pong->ShortDebugString();
  fake.resume();

  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 1, kFrame,
                                           client.audio_sent, asked));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_WORKER_SHUTDOWN);
  EXPECT_EQ(closed.last_accepted_sequence(), last_audio);
  EXPECT_EQ(closed.totals().input_audio_samples(), kFrame);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::UNAVAILABLE);
}

// A Finalize the drain does not take would have made audio valid again after
// the short frame; the client sent it and cannot know.
TEST(StreamEvents, InputBehindAFinalizeADrainDoesNotTakeIsNotChecked) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  ASSERT_TRUE(client.send_audio(kShortestFrame / 2));
  const auto last_audio = client.sequence;
  ASSERT_TRUE(eventually([&] { return seen.inputs() == 1; }));
  served.listener().stop_admission();
  ASSERT_TRUE(eventually([&] { return seen.state() == LogicalSessionState::Draining; }));
  (void)client.finalize();
  ASSERT_TRUE(client.send(raw_audio(kFrame, kShortestFrame / 2)));
  // Answered once the two events before it were read.
  wire::ClientEvent ping;
  ping.mutable_ping();
  ASSERT_TRUE(client.send(ping));
  const auto pong = client.next();
  ASSERT_TRUE(pong.has_value());
  ASSERT_TRUE(pong->has_pong()) << pong->ShortDebugString();
  EXPECT_TRUE(seen.finalizes().empty());
  fake.resume();

  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_WORKER_SHUTDOWN);
  EXPECT_EQ(closed.last_accepted_sequence(), last_audio);
  EXPECT_EQ(closed.totals().input_audio_samples(), kShortestFrame / 2);
}

// A client's Finalize leaves the session finalizing while the frames before
// it are still worked through: their credit is the client's to use next.
TEST(StreamEvents, CreditReturnedWhileASessionFinalizesIsAnnounced) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  for (int frame = 0; frame < 2; ++frame) {
    ASSERT_TRUE(client.send_audio(kFrame));
    const auto event = client.read();
    ASSERT_TRUE(event && event->has_accepted());
  }
  const auto last_audio = client.sequence;
  const auto asked = client.finalize();
  ASSERT_TRUE(eventually(
      [&] { return served.dispatch().seen.state() == LogicalSessionState::Finalizing; }));
  fake.resume();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 1,
                                           2 * kFrame, client.audio_sent, asked));
  EXPECT_EQ(client.accepted.back().accepted_sequence(), last_audio);
  EXPECT_EQ(client.accepted.back().input_credit().bytes().used(), 0U);
}

TEST(StreamEvents, CreditReturnedWhileASessionDrainsIsNotAnnounced) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  for (int frame = 0; frame < 2; ++frame) {
    ASSERT_TRUE(client.send_audio(kFrame));
    const auto event = client.read();
    ASSERT_TRUE(event && event->has_accepted());
  }
  ASSERT_TRUE(client.call->WritesDone());
  ASSERT_TRUE(
      eventually([&] { return served.dispatch().seen.state() == LogicalSessionState::Draining; }));
  // Both frames give their credit back now, to a session no client can send
  // to any more.
  fake.resume();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_HALF_CLOSE, 1, 2 * kFrame,
                                           client.audio_sent, 0));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_FALSE(closed.has_cause());
  EXPECT_EQ(client.accepted.size(), 2U) << "credit returned to a draining session was announced";
}

TEST(StreamEvents, CreditReturnedAfterACancelIsNotAnnounced) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  for (int frame = 0; frame < 2; ++frame) {
    ASSERT_TRUE(client.send_audio(kFrame));
    const auto event = client.read();
    ASSERT_TRUE(event && event->has_accepted());
  }
  wire::ClientEvent cancel;
  cancel.mutable_cancel();
  ASSERT_TRUE(client.send(cancel));
  const auto acknowledged = client.read();
  ASSERT_TRUE(acknowledged.has_value());
  ASSERT_TRUE(acknowledged->has_cancel_accepted()) << acknowledged->ShortDebugString();
  // Both frames give their credit back now, to a cancelled session.
  fake.resume();
  const auto last = client.read();
  ASSERT_TRUE(last.has_value());
  ASSERT_TRUE(last->has_session_closed()) << last->ShortDebugString();
  EXPECT_EQ(last->session_closed().cause().reason(), wire::FAILURE_REASON_CLIENT_CANCELLED);
}

// gRPC reports a message it cannot decode as a read that failed, which is all
// a half-close is to the listener: the session closes with no cause.
TEST(StreamEvents, AnEventThatDoesNotDecodeClosesTheSessionAsAHalfCloseDoes) {
  Served served;
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING);
  // Not UTF-8, which the server's parse of a string field requires.
  ASSERT_TRUE(client.send(raw_text(1, "\xff")));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_FALSE(closed.has_cause());
  EXPECT_EQ(closed.totals().input_text_bytes(), 0U);
  EXPECT_TRUE(client.accepted.empty());
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::OK);
}

// A write completes when HTTP/2 flow control lets it pass: a reader that stops
// is noticed once the window its channel announced is full. Measured here for
// two client channels.
TEST(StreamEvents, AReaderThatStopsEndsItsSessionAsASlowConsumerOnceItsWindowIsFull) {
  struct Channel {
    const char* name;
    grpc::ChannelArguments arguments;
  };
  std::vector<Channel> channels(2);
  channels[0].name = "default client channel";
  channels[1].name = "client channel with the bandwidth probe off";
  channels[1].arguments.SetInt(GRPC_ARG_HTTP2_BDP_PROBE, 0);
  for (const auto& channel : channels) {
    SCOPED_TRACE(channel.name);
    FakeSchedulerClock clock;
    Served served{true, "127.0.0.1", &clock};
    served.use_channel(channel.arguments);
    auto& seen = served.dispatch().seen;
    auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING, nullptr, 300s);

    // Waits for `done`. False once no write has completed for a second: the
    // only wait here that is not for an event, and nothing is asserted on it.
    const auto unless_stalled = [&](const std::function<bool()>& done) {
      auto deliveries = seen.deliveries();
      for (int quiet = 0; quiet < 200;) {
        if (done()) {
          return true;
        }
        std::this_thread::sleep_for(5ms);
        const auto now = seen.deliveries();
        quiet = now == deliveries ? quiet + 1 : 0;
        deliveries = now;
      }
      return false;
    };
    // Text for as long as the server takes it, three segments to a Finalize,
    // and never a read.
    const std::string text(4'096, 'x');
    std::uint64_t segments = 0;
    std::uint64_t finalizes = 0;
    for (bool writing = true; writing;) {
      for (int in_synthesis = 0; writing && in_synthesis < 3; ++in_synthesis) {
        writing = unless_stalled([&] {
          return seen.inputs() == segments && seen.state() == LogicalSessionState::Active &&
                 seen.waiting_segments() < 2;
        });
        if (writing) {
          ASSERT_TRUE(client.send(raw_text(++segments, text)));
        }
      }
      writing = writing && unless_stalled([&] { return seen.inputs() == segments; });
      if (writing) {
        (void)client.finalize();
        ++finalizes;
        writing = unless_stalled([&] {
          return seen.finalizes().size() == finalizes &&
                 seen.state() == LogicalSessionState::Active;
        });
      }
    }
    const auto chunks_written = seen.audio_deliveries();
    ASSERT_EQ(seen.aborts(), 0) << "nothing ends the session while its clock stands still";

    // Just past the limit, and short of every other timer of the session. A
    // write that completed after all restarts the limit: hence the rounds.
    bool ended = false;
    for (int round = 0; round < 4 && !ended; ++round) {
      clock.advance(tensorplate::serving::SessionBudgets::kOutputNoProgressTimeout + 1ms);
      ended = eventually([&] { return seen.aborts() == 1; });
    }
    ASSERT_TRUE(ended);
    EXPECT_EQ(seen.cause(), "slow_consumer");

    // The client reads again: what the server wrote, then the outcome.
    std::uint64_t pcm_read = 0;
    std::optional<wire::SessionClosed> closed;
    while (const auto event = client.read()) {
      if (event->has_audio_chunk()) {
        pcm_read += event->audio_chunk().data().size();
      } else if (event->has_session_closed()) {
        closed = event->session_closed();
      }
    }
    ASSERT_TRUE(closed.has_value());
    EXPECT_EQ(closed->state(), wire::SESSION_STATE_CLOSED);
    EXPECT_EQ(closed->cause().reason(), wire::FAILURE_REASON_SLOW_CONSUMER);
    // The totals are what was written on this stream before them.
    EXPECT_EQ(closed->totals().output_audio_samples() * 2, pcm_read);
    EXPECT_GT(pcm_read, 0U);
    EXPECT_LT(pcm_read, segments * text.size() * Fake::kSamplesPerTextByte * 2)
        << "the server wrote everything: the reader was never noticed";
    EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::RESOURCE_EXHAUSTED);
    std::cout << "[ measured ] " << channel.name << ": " << pcm_read
              << " bytes of PCM written to a client that read nothing (" << chunks_written
              << " chunk writes had completed when writes stopped; " << segments
              << " segments sent)\n";
  }
}

// A session with work in progress cannot drain behind its finished call: the
// end of the call fails it.
TEST(StreamEvents, OutputTheListenerCannotSendEndsTheCallAndFailsItsSession) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::BeforeWork);
  ASSERT_TRUE(client.send_audio(kFrame));
  ASSERT_TRUE(eventually([&] { return seen.inputs() == 1; }));

  struct Unknown final : tensorplate::serving::OutputBody {};
  tensorplate::serving::OutputItem item{tensorplate::serving::OutputKind::Result, 8, 0,
                                        std::make_unique<Unknown>()};
  const auto queue = seen.output();
  ASSERT_NE(queue, nullptr);
  ASSERT_EQ(queue->offer(item, tensorplate::SystemSchedulerClock{}.now()).value(),
            tensorplate::serving::OutputOffer::Queued);
  while (const auto event = client.read()) {
    EXPECT_TRUE(event->has_accepted()) << event->ShortDebugString();
  }
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::INTERNAL);
  ASSERT_TRUE(eventually([&] { return seen.state() == LogicalSessionState::Failed; }));
  EXPECT_EQ(seen.cause(), "internal");
  // Its slot follows as soon as the dispatch lets go, and is served again.
  fake.resume();
  ASSERT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
  auto next = served.ready(wire::SESSION_MODE_STT_STREAMING);
  EXPECT_FALSE(next.session_id.empty());
}

// A cancelled call aborts its session, whether or not the half-close of its
// failed read was applied first. The held job keeps the drain from completing.
TEST(StreamEvents, ACancelledCallAbortsItsSessionAlsoBehindTheHalfCloseOfItsFailedRead) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(client.send_audio(kFrame));
  ASSERT_TRUE(eventually([&] { return seen.inputs() == 1; }));
  client.context->TryCancel();
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::CANCELLED);
  ASSERT_TRUE(eventually([&] { return seen.aborts() == 1; }));
  fake.resume();
  ASSERT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
  EXPECT_EQ(seen.cause(), "client_cancelled");
  std::vector<LogicalSessionEvent> ending;
  for (const auto event : seen.events()) {
    if (event == LogicalSessionEvent::HalfClose || event == LogicalSessionEvent::Abort) {
      ending.push_back(event);
    }
  }
  const std::vector<LogicalSessionEvent> aborted{LogicalSessionEvent::Abort};
  const std::vector<LogicalSessionEvent> drained_first{LogicalSessionEvent::HalfClose,
                                                       LogicalSessionEvent::Abort};
  EXPECT_TRUE(ending == aborted || ending == drained_first);
}

}  // namespace
