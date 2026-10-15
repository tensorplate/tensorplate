// SPDX-License-Identifier: Apache-2.0
//
// The client events a stream session serves after Open, with a real client
// over loopback and the synthetic session dispatch behind the listener. What
// the dispatch yields is a function of the bytes handed over, so each test
// that reads output also checks that the right bytes were handed over.

#include <arpa/inet.h>
#include <grpcpp/generic/generic_stub.h>
#include <grpcpp/grpcpp.h>
#include <grpcpp/impl/proto_utils.h>
#include <grpcpp/support/byte_buffer.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstdint>
#include <functional>
#include <iostream>
#include <memory>
#include <mutex>
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
using tensorplate::testing::stream_listener::open_event;
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

/// A call that sends the bytes it is given, for messages no typed client
/// builds. One operation at a time, each waited for.
class RawCall {
 public:
  explicit RawCall(const std::shared_ptr<grpc::Channel>& channel) : stub_(channel) {
    context_.set_deadline(std::chrono::system_clock::now() + std::chrono::seconds{20});
    call_ = stub_.PrepareCall(&context_, "/tensorplate.stream.v1.SessionService/Session", &queue_);
    call_->StartCall(this);
    (void)done();
  }

  bool write(const std::string& bytes) {
    const grpc::Slice slice(bytes);
    call_->Write(grpc::ByteBuffer(&slice, 1), this);
    return done();
  }
  bool write(const wire::ClientEvent& event) { return write(event.SerializeAsString()); }

  std::optional<wire::ServerEvent> read() {
    grpc::ByteBuffer bytes;
    call_->Read(&bytes, this);
    wire::ServerEvent event;
    if (!done() ||
        !grpc::SerializationTraits<wire::ServerEvent>::Deserialize(&bytes, &event).ok()) {
      return std::nullopt;
    }
    return event;
  }

  grpc::Status finish() {
    grpc::Status status;
    call_->Finish(&status, this);
    (void)done();
    return status;
  }

 private:
  bool done() {
    void* tag = nullptr;
    bool ok = false;
    return queue_.Next(&tag, &ok) && ok;
  }

  grpc::ClientContext context_;
  grpc::CompletionQueue queue_;
  grpc::GenericStub stub_;
  std::unique_ptr<grpc::GenericClientAsyncReaderWriter> call_;
};

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

constexpr const char* kUnparsedDetail = "client message that does not parse";

// The listener parses each message itself, so one that does not parse is a
// refused event and not a read that failed, which is all a half-close is.
TEST(StreamEvents, AnEventThatDoesNotParseFailsItsSession) {
  Served served;
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_TTS_STREAMING);
  // Not UTF-8, which the parse of a string field requires.
  ASSERT_TRUE(client.send(raw_text(1, "\xff")));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_INVALID_EVENT);
  EXPECT_EQ(closed.cause().detail(), kUnparsedDetail);
  EXPECT_EQ(closed.last_accepted_sequence(), 0U);
  EXPECT_EQ(closed.totals().input_text_bytes(), 0U);
  EXPECT_TRUE(client.accepted.empty());
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::INVALID_ARGUMENT);
  EXPECT_FALSE(seen.saw(LogicalSessionEvent::HalfClose));
  EXPECT_EQ(seen.inputs(), 0U) << "the event that did not parse was accepted";
}

// Applied as a half-close, the failed read used to finalize and transcribe
// the open utterance: one frame opens it and nothing here ends it.
TEST(StreamEvents, AnEventThatDoesNotParseDoesNotFinalizeTheOpenUtterance) {
  Served served;
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  ASSERT_TRUE(client.send_audio(kFrame));
  ASSERT_TRUE(eventually([&] { return seen.inputs() == 1; }));
  const auto accepted_sequence = client.sequence;
  wire::ClientEvent unparsed;
  unparsed.set_sequence(++client.sequence);
  unparsed.set_session_id("\xff");
  ASSERT_TRUE(client.call->Write(unparsed));
  // Valid, and behind the event that failed the session: never applied.
  (void)client.send(raw_audio(kFrame, kFrame));
  bool closed_seen = false;
  while (const auto event = client.next()) {
    if (event->has_session_closed()) {
      closed_seen = true;
      const auto& closed = event->session_closed();
      EXPECT_EQ(closed.state(), wire::SESSION_STATE_FAILED);
      EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_INVALID_EVENT);
      EXPECT_EQ(closed.cause().detail(), kUnparsedDetail);
      EXPECT_EQ(closed.last_accepted_sequence(), accepted_sequence);
      EXPECT_EQ(closed.totals().input_audio_samples(), kFrame);
      EXPECT_EQ(closed.totals().utterances_completed(), 0U);
      break;
    }
    EXPECT_FALSE(event->has_endpoint_detected() || event->has_final_transcript())
        << event->ShortDebugString();
  }
  ASSERT_TRUE(closed_seen) << "the call ended without SessionClosed";
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::INVALID_ARGUMENT);
  EXPECT_FALSE(seen.saw(LogicalSessionEvent::HalfClose));
  EXPECT_EQ(seen.inputs(), 1U);
  ASSERT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
}

// A whole Open with a byte behind it that is no field: were the result of
// the parse ignored, the Open would be served.
TEST(StreamEvents, AFirstMessageThatDoesNotParseOpensNoSession) {
  Served served;
  RawCall call{served.channel()};
  ASSERT_TRUE(
      call.write(open_event(wire::SESSION_MODE_STT_STREAMING).SerializeAsString() + "\x0f"));
  EXPECT_FALSE(call.read().has_value()) << "a refused Open has no event";
  const auto status = call.finish();
  EXPECT_EQ(status.error_code(), grpc::StatusCode::INVALID_ARGUMENT);
  EXPECT_NE(status.error_message().find("FAILURE_REASON_INVALID_EVENT"), std::string::npos)
      << status.error_message();
  EXPECT_TRUE(served.dispatch().seen.events().empty()) << "a session was opened";
  EXPECT_EQ(served.listener().live_sessions(), 0U);
}

// Bytes no client library would send: gRPC hands them over as they came.
TEST(StreamEvents, BytesThatAreNoClientEventFailTheirSession) {
  auto truncated = raw_text(1, "abc");
  truncated.set_sequence(2);
  struct Case {
    const char* name;
    std::string bytes;
  };
  const std::vector<Case> cases = {
      // The text's length prefix promises a byte the message does not hold.
      {"a truncated event", truncated.SerializeAsString().substr(0, truncated.ByteSizeLong() - 1)},
      // Field 1 with wire type 7, which protobuf does not define.
      {"an undefined wire type", "\x0f"},
  };
  for (const auto& entry : cases) {
    SCOPED_TRACE(entry.name);
    Served served;
    RawCall call{served.channel()};
    ASSERT_TRUE(call.write(open_event(wire::SESSION_MODE_TTS_STREAMING)));
    const auto ready = call.read();
    ASSERT_TRUE(ready && ready->has_ready());
    ASSERT_TRUE(call.write(entry.bytes));
    const auto last = call.read();
    ASSERT_TRUE(last && last->has_session_closed());
    EXPECT_EQ(last->session_closed().state(), wire::SESSION_STATE_FAILED);
    EXPECT_EQ(last->session_closed().cause().reason(), wire::FAILURE_REASON_INVALID_EVENT);
    EXPECT_EQ(last->session_closed().cause().detail(), kUnparsedDetail);
    EXPECT_FALSE(call.read().has_value());
    EXPECT_EQ(call.finish().error_code(), grpc::StatusCode::INVALID_ARGUMENT);
    EXPECT_FALSE(served.dispatch().seen.saw(LogicalSessionEvent::HalfClose));
  }
}

// The transport rejects an event above the 64 KiB the listener receives
// before the listener sees it: the call ends with no SessionClosed, and all
// the session learns is that its call was cancelled. The held job keeps the
// half-close of the failed read from closing the session first.
TEST(StreamEvents, AnEventAboveTheReceiveLimitEndsTheCallWithNoSessionClosed) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(client.send_audio(kFrame));
  ASSERT_TRUE(eventually([&] { return seen.inputs() == 1; }));
  (void)client.send(raw_audio(kFrame, kFrame, 70'000));
  while (const auto event = client.read()) {
    EXPECT_FALSE(event->has_session_closed()) << event->ShortDebugString();
  }
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::RESOURCE_EXHAUSTED);
  ASSERT_TRUE(eventually([&] { return seen.aborts() == 1; }));
  EXPECT_EQ(seen.cause(), "client_cancelled");
  EXPECT_EQ(seen.inputs(), 1U) << "the event the transport rejected was accepted";
  fake.resume();
  ASSERT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
}

constexpr auto kNoProgressLimit = tensorplate::serving::SessionBudgets::kOutputNoProgressTimeout;
/// What HTTP/2 lets a sender pass before its peer reads, unless the peer
/// announces another window (RFC 9113, 6.9.2).
constexpr std::uint64_t kHttp2DefaultWindow = 65'535;
constexpr std::uint64_t kChunkBytes = std::uint64_t{Fake::kChunkSamples} * 2;
/// One segment: ten times the default window in PCM.
constexpr std::size_t kStalledTextBytes = 4'096;
constexpr std::uint64_t kStalledPcmBytes = kStalledTextBytes * Fake::kSamplesPerTextByte * 2;

/// No chunk of the segment is longer on the wire, its message prefix included.
std::uint64_t longest_chunk_on_the_wire() {
  wire::ServerEvent event;
  event.set_sequence(kStalledPcmBytes);
  auto& chunk = *event.mutable_audio_chunk();
  chunk.set_segment_id(1);
  chunk.set_chunk_index(static_cast<std::uint32_t>(kStalledPcmBytes / kChunkBytes));
  chunk.set_segment_sample_offset(kStalledPcmBytes);
  auto& format = *chunk.mutable_format();
  format.set_encoding(wire::AUDIO_ENCODING_PCM_S16LE);
  format.set_sample_rate_hz(48'000);
  format.set_channels(2);
  chunk.set_data(std::string(kChunkBytes, '\1'));
  return event.ByteSizeLong() + 5;
}

/// A text session on a listener with an injected clock, or the system
/// clock, whose client reads nothing after Ready.
struct UnreadSession {
  explicit UnreadSession(const grpc::ChannelArguments& arguments, bool on_the_system_clock = false,
                         std::chrono::seconds deadline = 300s)
      : served(true, "127.0.0.1", on_the_system_clock ? nullptr : &clock) {
    served.use_channel(arguments);
    client = served.ready(wire::SESSION_MODE_TTS_STREAMING, nullptr, deadline);
  }

  /// Sends one segment and returns once its chunk writes have all but filled
  /// a window of `window` bytes: with no read, HTTP/2 lets at most two more
  /// complete, so the next write to start stays in flight.
  void fill(std::uint64_t window) {
    ASSERT_TRUE(client.send(raw_text(1, std::string(kStalledTextBytes, 'x'))));
    // One chunk short, for Ready and Accepted, which the window also carried.
    const auto all_but_full = window / longest_chunk_on_the_wire() - 1;
    ASSERT_TRUE(eventually([&] { return seen().audio_deliveries() >= all_but_full; }))
        << seen().audio_deliveries() << " chunk writes completed, short of " << all_but_full;
  }

  /// Sends one segment and returns once a chunk write is in flight, on a
  /// channel whose window no chunk fits: that write never completes. A write
  /// is in flight while one more item is taken than delivered, and it is a
  /// chunk when no lifecycle message awaits delivery. The counts are read on
  /// both sides of that, so all three held at one instant.
  void block() {
    ASSERT_TRUE(client.send(raw_text(1, std::string(kStalledTextBytes, 'x'))));
    ASSERT_TRUE(eventually([&] {
      const auto output = seen().output();
      if (output == nullptr) {
        return false;
      }
      const auto delivered = static_cast<std::uint64_t>(seen().deliveries());
      const auto taken = output->last_sequence();
      const bool only_pcm_waits =
          output->metadata_usage().used() == 0 && output->pcm_usage().used() > 0;
      return only_pcm_waits && taken == delivered + 1 && output->last_sequence() == taken &&
             static_cast<std::uint64_t>(seen().deliveries()) == delivered;
    })) << "no chunk write came to be in flight";
    EXPECT_EQ(seen().audio_deliveries(), 0U) << "a chunk passed a window it does not fit";
  }

  /// Moves the clock past the no-progress limit until the session ends. The
  /// limit restarts at a write that completes after all, and at output
  /// offered to a queue the writes had emptied: hence the rounds, each of
  /// which waits for the end or for one of those two.
  void end_as_slow_consumer() {
    for (int round = 0; round < 8 && seen().aborts() == 0; ++round) {
      std::shared_ptr<tensorplate::serving::BoundedOutputQueue> output;
      ASSERT_TRUE(eventually([&] { return (output = seen().output()) != nullptr; }));
      const auto completed = seen().deliveries();
      const auto stalled = output->stalled_since();
      advance(kNoProgressLimit + 1ms);
      ASSERT_TRUE(eventually([&] {
        return seen().aborts() == 1 || seen().deliveries() > completed ||
               output->stalled_since() != stalled;
      })) << "round "
          << round;
    }
    ASSERT_EQ(seen().aborts(), 1);
    EXPECT_EQ(seen().cause(), "slow_consumer");
  }

  [[nodiscard]] tensorplate::testing::stream_listener::Seen& seen() {
    return served.dispatch().seen;
  }

  /// Moves the clock and says so: the listener's watch thread sleeps until
  /// an instant it knows of, and this clock's lie an hour ahead.
  void advance(tensorplate::SchedulerClock::Duration by) {
    clock.advance(by);
    served.listener().notify_clock_advanced();
  }

  FakeSchedulerClock clock;
  Served served;
  Client client;
};

grpc::ChannelArguments without_the_bandwidth_probe() {
  grpc::ChannelArguments arguments;
  arguments.SetInt(GRPC_ARG_HTTP2_BDP_PROBE, 0);
  return arguments;
}

/// A client channel that announces a stream window smaller than one chunk
/// message: Ready and the replies pass it, a chunk only once the client reads.
grpc::ChannelArguments with_no_room_for_a_chunk() {
  auto arguments = without_the_bandwidth_probe();
  arguments.SetInt(GRPC_ARG_HTTP2_STREAM_LOOKAHEAD_BYTES, static_cast<int>(kChunkBytes / 2));
  return arguments;
}

// A write completes when HTTP/2 flow control lets it pass, so the no-progress
// limit trails a reader that stops by the window its channel announced: here
// the default one, which a client channel keeps with its bandwidth probe off.
TEST(StreamEvents, AReaderThatStopsIsNoticedOnceTheWindowItsChannelAnnouncedIsFull) {
  UnreadSession session{without_the_bandwidth_probe()};
  session.fill(kHttp2DefaultWindow);
  if (HasFatalFailure()) {
    return;
  }
  session.end_as_slow_consumer();
  if (HasFatalFailure()) {
    return;
  }
  // The client reads again within the limit: what the server wrote, then the
  // outcome.
  auto& client = session.client;
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
  EXPECT_LT(pcm_read, kStalledPcmBytes) << "the server wrote everything it was asked for";
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::RESOURCE_EXHAUSTED);
  ASSERT_TRUE(eventually([&] { return session.served.listener().open_calls() == 0; }));
  // Each chunk the client read is a write the server completed. At most the
  // last one completed only because the client read: the others passed
  // unread, and no more of them than the window holds. Read once the call is
  // done, so whether a write was still in flight when the session ended does
  // not matter.
  const auto written = session.seen().audio_deliveries();
  EXPECT_EQ(written * kChunkBytes, pcm_read);
  ASSERT_GT(written, 0U);
  EXPECT_LE((written - 1) * (kChunkBytes + 5), kHttp2DefaultWindow)
      << "more passed unread than the window the client announced";
}

// The same reader on a default client channel, whose bandwidth probe widens
// the window it announces: more passes unread than the default window holds.
TEST(StreamEvents, ADefaultClientChannelLetsMorePassUnreadThanTheDefaultWindow) {
  UnreadSession session{grpc::ChannelArguments{}};
  ASSERT_TRUE(session.client.send(raw_text(1, std::string(kStalledTextBytes, 'x'))));
  EXPECT_TRUE(eventually(
      [&] { return session.seen().audio_deliveries() * kChunkBytes > kHttp2DefaultWindow; }));
  session.client.context->TryCancel();
}

// A reader that never resumes holds a write in flight for as long as it
// likes, and the session's last event behind it. The call is cancelled once
// nothing was written for the limit after the session ended.
TEST(StreamEvents, ACallWhoseReaderNeverResumesIsCancelledAfterItsSessionEnded) {
  UnreadSession session{with_no_room_for_a_chunk()};
  auto& listener = session.served.listener();
  session.block();
  if (HasFatalFailure()) {
    return;
  }
  session.end_as_slow_consumer();
  if (HasFatalFailure()) {
    return;
  }
  ASSERT_TRUE(eventually([&] { return listener.live_sessions() == 0; }));
  // The clock has not moved since the end: the call still has its limit.
  EXPECT_EQ(listener.open_calls(), 1U);
  session.advance(kNoProgressLimit + 1ms);
  ASSERT_TRUE(eventually([&] { return listener.open_calls() == 0; }))
      << "the call outlived the limit: its reactor was never done";

  // Only now does the client look: its call is over and told it nothing.
  auto& client = session.client;
  while (const auto event = client.read()) {
    EXPECT_FALSE(event->has_session_closed()) << "SessionClosed was never written";
  }
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::CANCELLED);
}

// A reader that stopped cannot take its terminal event. A stop that outlasts
// the drain window fails its session all the same and cancels the call at the
// window's end, before the client's own deadline. The listener's clock never
// moves, so no limit of the listener's ends the call.
TEST(StreamEvents, AStopCancelsAtTheWindowsEndACallWhoseReaderStopped) {
  UnreadSession session{with_no_room_for_a_chunk(), false, 30s};
  session.block();
  if (HasFatalFailure()) {
    return;
  }
  auto& listener = session.served.listener();
  listener.stop(std::chrono::steady_clock::now() + 2 * tensorplate::serving::kTerminalEventReserve);
  EXPECT_EQ(listener.open_calls(), 0U);
  EXPECT_EQ(session.seen().state(), LogicalSessionState::Failed);
  EXPECT_EQ(session.seen().cause(), "worker_shutdown");
  auto& client = session.client;
  while (const auto event = client.read()) {
    EXPECT_FALSE(event->has_session_closed()) << "SessionClosed passed a window it does not fit";
  }
  const auto code = client.call->Finish().error_code();
  EXPECT_TRUE(code == grpc::StatusCode::CANCELLED || code == grpc::StatusCode::UNAVAILABLE) << code;
}

// A reader that fell behind and reads again once the stop has failed its
// session still gets the terminal event: the reserve leaves the call time to
// write what the window holds back and the event behind it.
TEST(StreamEvents, AReaderThatCatchesUpWhileTheStopWaitsGetsTheTerminalEvent) {
  UnreadSession session{without_the_bandwidth_probe(), false, 30s};
  session.fill(kHttp2DefaultWindow);
  if (HasFatalFailure()) {
    return;
  }
  auto& listener = session.served.listener();
  const std::jthread stopping([&] {
    listener.stop(std::chrono::steady_clock::now() +
                  2 * tensorplate::serving::kTerminalEventReserve);
  });
  ASSERT_TRUE(eventually([&] { return session.seen().state() == LogicalSessionState::Failed; }));
  auto& client = session.client;
  std::optional<wire::SessionClosed> closed;
  while (const auto event = client.read()) {
    if (event->has_session_closed()) {
      closed = event->session_closed();
    }
  }
  ASSERT_TRUE(closed.has_value()) << "the call was cancelled before its terminal event";
  EXPECT_EQ(closed->state(), wire::SESSION_STATE_FAILED);
  EXPECT_EQ(closed->cause().reason(), wire::FAILURE_REASON_WORKER_SHUTDOWN);
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::UNAVAILABLE);
}

// The stop lets a session drain until the reserve of the window is left: a
// decode that ends inside that time closes the session as a drain does.
TEST(StreamEvents, AStopLetsASessionThatDrainsInsideTheWindowClose) {
  Served served;
  auto& fake = served.dispatch().synthetic();
  auto& seen = served.dispatch().seen;
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(client.send_audio(kFrame));
  const auto asked = client.finalize();
  // The decode the Finalize started is held: the stop finds it running.
  ASSERT_TRUE(eventually([&] { return seen.finalizes() == std::vector<std::uint64_t>{asked}; }));
  const std::jthread stopping(
      [&] { served.listener().stop(std::chrono::steady_clock::now() + 20s); });
  ASSERT_TRUE(eventually([&] { return seen.state() == LogicalSessionState::Draining; }));
  fake.resume();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(client, wire::ENDPOINT_REASON_CLIENT_FINALIZE, 1, kFrame,
                                           client.audio_sent, asked));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_EQ(closed.cause().reason(), wire::FAILURE_REASON_WORKER_SHUTDOWN);
  EXPECT_EQ(end_of(client).error_code(), grpc::StatusCode::UNAVAILABLE);
}

// The limit after the end is its own: a session that failed with a write in
// flight never came to the no-progress limit of a live session.
TEST(StreamEvents, ACallIsCancelledWhenItsSessionFailedBehindAWriteThatNeverCompletes) {
  UnreadSession session{with_no_room_for_a_chunk()};
  auto& listener = session.served.listener();
  auto& seen = session.seen();
  session.block();
  if (HasFatalFailure()) {
    return;
  }
  // Out of sequence: the session fails at once, with its clock where it was.
  wire::ClientEvent stray;
  stray.set_sequence(99);
  stray.set_session_id(session.client.session_id);
  stray.mutable_ping();
  ASSERT_TRUE(session.client.call->Write(stray));
  ASSERT_TRUE(eventually([&] { return seen.state() == LogicalSessionState::Failed; }));
  EXPECT_EQ(seen.cause(), "invalid_event");
  ASSERT_TRUE(eventually([&] { return listener.live_sessions() == 0; }));
  EXPECT_EQ(listener.open_calls(), 1U);
  session.advance(kNoProgressLimit + 1ms);
  ASSERT_TRUE(eventually([&] { return listener.open_calls() == 0; }));
  EXPECT_EQ(seen.aborts(), 0) << "the no-progress limit of a live session ended it";
  // The slot was free all along, and is served.
  auto next = session.served.ready(wire::SESSION_MODE_TTS_STREAMING);
  EXPECT_FALSE(next.session_id.empty());
}

// On the system clock nothing tells the watch thread that time has passed:
// the end of the session wakes it, and at the limit it wakes by itself. The
// thread must be asleep with no end when the session ends, or the sleep it
// began for the call's own Open would end in time and look for it: a silent
// call is refused first, which takes the thread past that instant and leaves
// it nothing to wait for. The test waits out two limits; it checks what
// happened to each call, not when.
TEST(StreamEvents, OnTheSystemClockTheWatchThreadCancelsACallWhoseReaderNeverResumes) {
  UnreadSession session{with_no_room_for_a_chunk(), true};
  auto& listener = session.served.listener();
  auto silent = session.served.connect();
  EXPECT_FALSE(silent.read().has_value());
  const auto refused = silent.call->Finish();
  ASSERT_NE(refused.error_message().find("FAILURE_REASON_IDLE_TIMEOUT"), std::string::npos)
      << refused.error_message();
  session.block();
  if (HasFatalFailure()) {
    return;
  }
  // Out of sequence: the session fails at once, behind the write in flight.
  wire::ClientEvent stray;
  stray.set_sequence(99);
  stray.set_session_id(session.client.session_id);
  stray.mutable_ping();
  ASSERT_TRUE(session.client.call->Write(stray));
  ASSERT_TRUE(eventually([&] { return listener.live_sessions() == 0; }));
  // A patience of its own: the wait is the limit itself, and the client must
  // not read meanwhile, which would let the write complete.
  const auto patience = std::chrono::steady_clock::now() + 60s;
  while (listener.open_calls() != 0 && std::chrono::steady_clock::now() < patience) {
    std::this_thread::sleep_for(20ms);
  }
  ASSERT_EQ(listener.open_calls(), 0U) << "nothing woke the watch thread at the limit";
  auto& client = session.client;
  while (const auto event = client.read()) {
    EXPECT_FALSE(event->has_session_closed()) << "SessionClosed was never written";
  }
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::CANCELLED);
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

/// A TCP relay on loopback between one client connection and the listener,
/// which can drop both of its sockets at once: a client that is lost, as the
/// listener sees it, under a real client.
class Relay {
 public:
  explicit Relay(std::uint16_t target) : listening_(::socket(AF_INET, SOCK_STREAM, 0)) {
    auto address = loopback(0);
    socklen_t size = sizeof(address);
    // NOLINTBEGIN(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket addresses.
    if (::bind(listening_, reinterpret_cast<const sockaddr*>(&address), sizeof(address)) != 0 ||
        ::listen(listening_, 1) != 0 ||
        ::getsockname(listening_, reinterpret_cast<sockaddr*>(&address), &size) != 0) {
      return;
    }
    // NOLINTEND(cppcoreguidelines-pro-type-reinterpret-cast)
    port_ = ntohs(address.sin_port);
    relaying_ = std::thread([this, target] { relay(target); });
  }
  ~Relay() {
    drop();
    if (relaying_.joinable()) {
      relaying_.join();
    }
    for (const int fd : {listening_, client_, server_}) {
      if (fd >= 0) {
        ::close(fd);
      }
    }
  }
  Relay(const Relay&) = delete;
  Relay& operator=(const Relay&) = delete;

  [[nodiscard]] std::uint16_t port() const noexcept { return port_; }

  /// Ends both connections without a word to either peer's gRPC.
  void drop() {
    const std::lock_guard guard(mu_);
    dropped_ = true;
    for (const int fd : {listening_, client_, server_}) {
      if (fd >= 0) {
        ::shutdown(fd, SHUT_RDWR);
      }
    }
  }

 private:
  static sockaddr_in loopback(std::uint16_t port) {
    sockaddr_in address{};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    address.sin_port = htons(port);
    return address;
  }

  void relay(std::uint16_t target) {
    const int client = ::accept(listening_, nullptr, nullptr);
    const int server = ::socket(AF_INET, SOCK_STREAM, 0);
    const auto address = loopback(target);
    {
      const std::lock_guard guard(mu_);
      client_ = client;
      server_ = server;
      // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
      if (dropped_ || client < 0 || server < 0 ||
          ::connect(server, reinterpret_cast<const sockaddr*>(&address), sizeof(address)) != 0) {
        return;
      }
    }
    std::array<pollfd, 2> ends{{{client, POLLIN, 0}, {server, POLLIN, 0}}};
    std::array<char, 16'384> bytes{};
    while (::poll(ends.data(), ends.size(), -1) > 0) {
      for (std::size_t from = 0; from < ends.size(); ++from) {
        if (ends.at(from).revents == 0) {
          continue;
        }
        const auto got = ::read(ends.at(from).fd, bytes.data(), bytes.size());
        if (got <= 0 || !write_all(ends.at(1 - from).fd, bytes.data(), got)) {
          return;
        }
      }
    }
  }

  /// False at the relay's end: once drop() has shut the sockets, a write fails
  /// with EPIPE or ECONNRESET and, sent this way, raises no SIGPIPE.
  static bool write_all(int fd, const char* data, ssize_t size) {
    while (size > 0) {
      const auto put = ::send(fd, data, static_cast<std::size_t>(size), MSG_NOSIGNAL);
      if (put <= 0) {
        return false;
      }
      data += put;
      size -= put;
    }
    return true;
  }

  int listening_;
  std::uint16_t port_ = 0;
  std::mutex mu_;
  bool dropped_ = false;
  int client_ = -1;
  int server_ = -1;
  std::thread relaying_;
};

/// A call that is gone abandons its session: it is aborted for the client,
/// and no half-close finalized the utterance its one frame opened.
void expect_abandoned(Served& served) {
  auto& seen = served.dispatch().seen;
  ASSERT_TRUE(eventually([&] { return served.listener().live_sessions() == 0; }));
  EXPECT_GE(seen.aborts(), 1);
  EXPECT_EQ(seen.cause(), "client_cancelled");
  EXPECT_FALSE(seen.saw(LogicalSessionEvent::HalfClose)) << "the open utterance was finalized";
  EXPECT_TRUE(seen.finalizes().empty());
  EXPECT_EQ(seen.inputs(), 1U);
}

/// A client whose one frame was accepted, its utterance open. On `relay`,
/// when given.
Client with_an_open_utterance(Served& served, const Relay* relay = nullptr) {
  if (relay != nullptr) {
    served.use_channel(grpc::ChannelArguments{}, relay->port());
  }
  auto client = served.ready(wire::SESSION_MODE_STT_STREAMING);
  EXPECT_TRUE(client.send_audio(kFrame));
  return client;
}

// gRPC reports a cancel to the reactor twice, unordered: as a failed read and
// as OnCancel. Here nothing holds the listener, which reads on once it has
// answered the frame: whichever comes first, the session is abandoned.
TEST(StreamEvents, ACallCancelledWhileItsReadWaitsAbandonsItsOpenUtterance) {
  Served served;
  auto client = with_an_open_utterance(served);
  while (client.accepted.empty()) {
    ASSERT_TRUE(client.read().has_value());
  }
  client.context->TryCancel();
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::CANCELLED);
  expect_abandoned(served);
}

// The listener is held inside the frame's hand-over, with no read of the
// call pending: OnCancel comes first, and the read that follows fails.
TEST(StreamEvents, ACallCancelledWhileTheListenerHandsOverAFrameAbandonsItsOpenUtterance) {
  Served served;
  auto& dispatch = served.dispatch();
  dispatch.hold_audio();
  auto client = with_an_open_utterance(served);
  ASSERT_TRUE(dispatch.wait_until_handing_audio());
  client.context->TryCancel();
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::CANCELLED);
  ASSERT_TRUE(eventually([&] { return dispatch.seen.aborts() == 1; }));
  dispatch.release_audio();
  expect_abandoned(served);
}

TEST(StreamEvents, ACallWhoseConnectionIsLostWhileItsReadWaitsAbandonsItsOpenUtterance) {
  Served served;
  Relay relay{served.listener().bound_port()};
  auto client = with_an_open_utterance(served, &relay);
  while (client.accepted.empty()) {
    ASSERT_TRUE(client.read().has_value());
  }
  relay.drop();
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::UNAVAILABLE);
  expect_abandoned(served);
}

TEST(StreamEvents, ACallWhoseConnectionIsLostWhileTheListenerHandsOverAFrameAbandonsIt) {
  Served served;
  auto& dispatch = served.dispatch();
  Relay relay{served.listener().bound_port()};
  dispatch.hold_audio();
  auto client = with_an_open_utterance(served, &relay);
  ASSERT_TRUE(dispatch.wait_until_handing_audio());
  relay.drop();
  EXPECT_EQ(client.call->Finish().error_code(), grpc::StatusCode::UNAVAILABLE);
  ASSERT_TRUE(eventually([&] { return dispatch.seen.aborts() == 1; }));
  dispatch.release_audio();
  expect_abandoned(served);
}

// Only the end of the client's stream ends the open utterance as a half-close.
TEST(StreamEvents, AHalfCloseFinalizesTheOpenUtteranceAndClosesTheSession) {
  Served served;
  auto client = with_an_open_utterance(served);
  ASSERT_TRUE(client.call->WritesDone());
  ASSERT_NO_FATAL_FAILURE(
      expect_utterance(client, wire::ENDPOINT_REASON_HALF_CLOSE, 1, kFrame, client.audio_sent, 0));
  const auto closed = closed_of(client);
  EXPECT_EQ(closed.state(), wire::SESSION_STATE_CLOSED);
  EXPECT_FALSE(closed.has_cause());
  EXPECT_EQ(closed.totals().utterances_completed(), 1U);
  EXPECT_TRUE(end_of(client).ok());
  auto& seen = served.dispatch().seen;
  EXPECT_TRUE(seen.saw(LogicalSessionEvent::HalfClose));
  EXPECT_EQ(seen.aborts(), 0);
}

}  // namespace
