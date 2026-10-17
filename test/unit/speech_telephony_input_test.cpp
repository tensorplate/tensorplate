// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <fstream>
#include <memory>
#include <nlohmann/json.hpp>
#include <span>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/job_request.hpp"

#include "fake_scheduler_clock.hpp"
#include "fixture_job_bridge.hpp"
#include "serving/session/dispatch.hpp"
#include "serving/session/speech_dispatch.hpp"
#include "serving/session/telephony_resampler.hpp"
#include "session_dispatch_binding.hpp"
#include "speech_dispatch_binding.hpp"

// The speech dispatch's 8 kHz mu-law input: decoded and resampled onto the
// 16 kHz internal timeline, its offsets leaving at 8 kHz. What a session
// should have made of its input is worked out here with the resampler itself,
// which test/unit/telephony_resampler_test.cpp holds to the reference.

namespace tensorplate::testing {
namespace {
using namespace std::chrono_literals;
using namespace tensorplate::serving;
using Binding = ReferenceBinding;
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;
using State = LogicalSessionState;
using Bytes = std::vector<std::byte>;

constexpr StreamAudioFormat kPcm16k{StreamAudioEncoding::PcmS16Le, 16'000, 1};
constexpr StreamAudioFormat kMulaw8k{StreamAudioEncoding::Mulaw, 8'000, 1};
/// 20 ms of mu-law at 8 kHz, a byte a sample.
constexpr std::size_t kShortestFrame = 160;

SpeechDispatchOptions taking_mulaw(SpeechDispatchOptions options = {}) {
  options.input_formats.push_back(kMulaw8k);
  return options;
}

std::unique_ptr<DispatchUnderTest> mulaw_dispatch(const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, taking_mulaw());
}

std::unique_ptr<DispatchUnderTest> mulaw_dispatch_detecting_voice(const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, taking_mulaw(),
                                           VoiceDetection::On);
}

std::unique_ptr<DispatchUnderTest> mulaw_dispatch_with_short_bounds(const SchedulerClock& clock) {
  SpeechDispatchOptions options;
  options.job_deadline_base = 1s;
  options.job_deadline_per_audio_second = 500ms;
  options.release_bound = 2s;
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, taking_mulaw(std::move(options)));
}

std::uint64_t open_mulaw(Binding& binding) {
  auto request = binding.under_test->audio_request();
  request.input_format = kMulaw8k;
  return binding.open(request);
}

/// Mu-law codes of a full-scale 500 Hz square wave: speech once resampled.
Bytes loud(std::size_t samples) {
  Bytes codes(samples);
  for (std::size_t index = 0; index < samples; ++index) {
    codes[index] = std::byte{(index / 8) % 2 == 0 ? std::uint8_t{0x80} : std::uint8_t{0x00}};
  }
  return codes;
}

/// The mu-law code of zero.
Bytes hushed(std::size_t samples) {
  return Bytes(samples, std::byte{0xff});
}

Bytes join(Bytes first, const Bytes& second) {
  first.insert(first.end(), second.begin(), second.end());
  return first;
}

/// A session's input as the dispatch should have resampled it: each push, and
/// a flush wherever a Finalize or a half-close ended the input.
class Resampled {
 public:
  void push(std::span<const std::byte> codes) { resampler_.push(codes, samples_); }
  void flush() { resampler_.flush(samples_); }
  [[nodiscard]] std::uint64_t size() const { return samples_.size(); }
  /// Internal samples [first, end) as 16-bit little-endian PCM.
  [[nodiscard]] Bytes pcm(std::uint64_t first, std::uint64_t end) const {
    Bytes bytes;
    for (std::uint64_t index = first; index < end; ++index) {
      const auto bits = static_cast<std::uint16_t>(samples_.at(index));
      bytes.push_back(static_cast<std::byte>(bits & 0xffU));
      bytes.push_back(static_cast<std::byte>(bits >> 8U));
    }
    return bytes;
  }

 private:
  TelephonyResampler resampler_;
  std::vector<std::int16_t> samples_;
};

void expect_endpoint(Binding& binding, std::uint64_t key, EndpointReason reason,
                     std::uint64_t utterance_id, std::uint64_t input_end) {
  const auto endpoint = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(endpoint.has_value());
  EXPECT_EQ(endpoint->reason, reason);
  EXPECT_EQ(endpoint->utterance_id, utterance_id);
  EXPECT_EQ(endpoint->end_sample_offset, input_end);
}

/// Reads an utterance's final: it ends at `input_end`, and its one segment is
/// the decode of internal samples [first, end).
void expect_final(Binding& binding, std::uint64_t key, std::uint64_t utterance_id,
                  const Resampled& input, std::uint64_t first, std::uint64_t end,
                  std::uint64_t input_end, std::uint64_t finalize_sequence) {
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->utterance_id, utterance_id);
  EXPECT_EQ(transcript->end_sample_offset, input_end);
  EXPECT_EQ(transcript->finalize_sequence, finalize_sequence);
  const auto expected = binding.under_test->transcript(first, input.pcm(first, end));
  ASSERT_EQ(transcript->segments.size(), expected.size());
  for (std::size_t index = 0; index < expected.size(); ++index) {
    EXPECT_EQ(transcript->segments[index].text, expected[index].text);
    EXPECT_EQ(transcript->segments[index].start_us, expected[index].start_us);
    EXPECT_EQ(transcript->segments[index].end_us, expected[index].end_us);
  }
}

/// An utterance's endpoint, then its final.
void expect_utterance(Binding& binding, std::uint64_t key, EndpointReason reason,
                      std::uint64_t utterance_id, const Resampled& input, std::uint64_t first,
                      std::uint64_t end, std::uint64_t input_end, std::uint64_t finalize_sequence) {
  ASSERT_NO_FATAL_FAILURE(expect_endpoint(binding, key, reason, utterance_id, input_end));
  expect_final(binding, key, utterance_id, input, first, end, input_end, finalize_sequence);
}

std::string refusal(const SessionDispatch& dispatch, StreamAudioFormat format) {
  const auto refused = dispatch.negotiate({SessionInputKind::Audio, "en", "", format});
  if (refused.has_value()) {
    return "<granted>";
  }
  EXPECT_EQ(refused.error().code, Error::Code::Unsupported);
  return refused.error().context.value_or("");
}

void move_clock(Binding& binding, SchedulerClock::Duration by) {
  binding.clock.advance(by);
  speech_of(binding).strategy->notify_clock_advanced();
  ASSERT_TRUE(binding.settle());
}

TEST(SpeechTelephonyInput, CreateRefusesInputFormatsItCannotTake) {
  const FakeSchedulerClock clock;
  const auto buffers = BufferManager::create({}).value();
  const auto created = [&](std::vector<StreamAudioFormat> formats) {
    SpeechDispatchOptions options;
    options.input_formats = std::move(formats);
    const auto dispatch = SpeechJobDispatch::create(std::make_shared<FixtureJobBridge>(*buffers),
                                                    *buffers, clock, std::move(options));
    if (dispatch) {
      return std::string{"<created>"};
    }
    EXPECT_EQ(dispatch.error().code, Error::Code::ConfigInvalid);
    return dispatch.error().context.value_or("");
  };
  EXPECT_EQ(created({kPcm16k, kMulaw8k}), "<created>");
  // Every speech deployment takes 16 kHz PCM.
  EXPECT_EQ(created({kMulaw8k}), "input_formats_invalid");
  EXPECT_EQ(created({}), "input_formats_invalid");
  // PCM is its own internal audio only at 16 kHz, and mu-law is resampled
  // from 8 kHz mono.
  EXPECT_EQ(created({kPcm16k, {StreamAudioEncoding::PcmS16Le, 8'000, 1}}), "input_formats_invalid");
  EXPECT_EQ(created({kPcm16k, {StreamAudioEncoding::Mulaw, 16'000, 1}}), "input_formats_invalid");
  EXPECT_EQ(created({kPcm16k, {StreamAudioEncoding::Mulaw, 8'000, 2}}), "input_formats_invalid");
}

TEST(SpeechTelephonyInput, NegotiateGrantsMulawAt8kHzOnlyWhereListed) {
  const FakeSchedulerClock clock;
  const auto buffers = BufferManager::create({}).value();
  const auto dispatch = SpeechJobDispatch::create(std::make_shared<FixtureJobBridge>(*buffers),
                                                  *buffers, clock, taking_mulaw())
                            .value();
  const auto granted = dispatch->negotiate({SessionInputKind::Audio, "en", "", kMulaw8k});
  ASSERT_TRUE(granted.has_value());
  EXPECT_EQ(granted->audio_format, kMulaw8k);
  EXPECT_EQ(granted->min_frame, 20ms);
  EXPECT_EQ(granted->max_frame, 320ms);
  EXPECT_EQ(granted->max_utterance, 30s);
  const auto pcm = dispatch->negotiate({SessionInputKind::Audio, "en", "", kPcm16k});
  ASSERT_TRUE(pcm.has_value());
  EXPECT_EQ(pcm->audio_format, kPcm16k);
  EXPECT_EQ(refusal(*dispatch, {StreamAudioEncoding::Mulaw, 16'000, 1}),
            "unsupported_audio_format");
  EXPECT_EQ(refusal(*dispatch, {StreamAudioEncoding::Mulaw, 8'000, 2}), "unsupported_audio_format");
  EXPECT_EQ(refusal(*dispatch, {StreamAudioEncoding::PcmS16Le, 8'000, 1}),
            "unsupported_audio_format");
}

TEST(SpeechTelephonyInput, ASecondOfMulawEndsAtInputOffset8000AndOneSecond) {
  Binding binding(&mulaw_dispatch);
  const auto key = open_mulaw(binding);
  const Bytes clip = patterned_bytes(8'000, 3);
  ASSERT_TRUE(binding.audio_paced(key, clip, kShortestFrame));
  binding.finalize(key, 1);
  Resampled input;
  input.push(clip);
  input.flush();
  ASSERT_EQ(input.size(), 16'000U);
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::ClientFinalize, 1, input,
                                           0, 16'000, 8'000, 1));
  EXPECT_EQ(binding.under_test->transcript(0, input.pcm(0, 16'000)).front().end_us, 1'000'000U);
}

// The resampler's history goes on across utterances, and a half-close takes
// what it holds back: here all of the last utterance, ten inputs after a flush.
TEST(SpeechTelephonyInput, MulawUtterancesKeepOneTimelineAcrossTheirEnds) {
  Binding binding(&mulaw_dispatch);
  const auto key = open_mulaw(binding);
  const Bytes first = patterned_bytes(8'000, 4);
  const Bytes second = patterned_bytes(4'000, 5);
  const Bytes last = patterned_bytes(10, 6);
  Resampled input;
  for (const Bytes* part : {&first, &second, &last}) {
    input.push(*part);
    input.flush();
  }
  ASSERT_TRUE(binding.audio_paced(key, first, kShortestFrame));
  binding.finalize(key, 1);
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::ClientFinalize, 1, input,
                                           0, 16'000, 8'000, 1));
  ASSERT_TRUE(binding.audio_paced(key, second, kShortestFrame));
  binding.finalize(key, 2);
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::ClientFinalize, 2, input,
                                           16'000, 24'000, 12'000, 2));
  binding.audio(key, last);
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::HalfClose, 3, input,
                                           24'000, 24'020, 12'010, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

// A frame accepted before the half-close but handed over after it belongs to
// the utterance: the resampler gives up what it holds only after that frame.
TEST(SpeechTelephonyInput, AHalfCloseTakesTheHeldSamplesAfterItsLastFrame) {
  Binding binding(&mulaw_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = open_mulaw(binding);
  const Bytes first = patterned_bytes(800, 8);
  const Bytes last = patterned_bytes(800, 9);
  binding.audio(key, first);
  ASSERT_TRUE(binding.settle());
  const auto accepted = binding.manager->accept_input(key, last.size());
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::AcceptInput));
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_TRUE(binding.settle());
  speech.strategy->audio(key, last);
  Resampled input;
  input.push(first);
  input.push(last);
  input.flush();
  ASSERT_NO_FATAL_FAILURE(
      expect_utterance(binding, key, EndpointReason::HalfClose, 1, input, 0, 3'200, 1'600, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SpeechTelephonyInput, TheLongestMulawUtteranceSplitsAtInputOffset240000) {
  Binding binding(&mulaw_dispatch);
  const auto key = open_mulaw(binding);
  const Bytes clip = patterned_bytes(248'000, 7);
  ASSERT_TRUE(binding.audio_in_long_frames(key, clip));
  binding.finalize(key, 1);
  Resampled input;
  input.push(clip);
  input.flush();
  // 30 s at the internal 480,000, then the second that follows.
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::DurationLimit, 1, input, 0,
                                           480'000, 240'000, 0));
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::ClientFinalize, 2, input,
                                           480'000, 496'000, 248'000, 1));
}

// A mu-law utterance never ends on an odd internal sample: voice-activity
// frames, splits and the ends a Finalize or a half-close takes are all even.
// 16-bit PCM at 8 kHz, opened as if granted, is internal audio as it is and
// reaches one.
TEST(SpeechTelephonyInput, AnOddInternalEndLeavesAtTheNextInputSample) {
  Binding binding(&mulaw_dispatch);
  const auto key = binding.open_as(binding.under_test->audio_request(),
                                   {StreamAudioEncoding::PcmS16Le, 8'000, 1});
  binding.audio(key, patterned_bytes(2 * 641, 3));
  binding.finalize(key, 1);
  const auto endpoint = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(endpoint.has_value());
  EXPECT_EQ(endpoint->end_sample_offset, 321U);
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->end_sample_offset, 321U);
  ASSERT_EQ(transcript->segments.size(), 1U);
  EXPECT_EQ(transcript->segments.front().start_us, 0U);
  // 641 samples at 62.5 us, rounded up.
  EXPECT_EQ(transcript->segments.front().end_us, 40'063U);
}

// The limit falls inside the samples a Finalize or a half-close takes from
// the resampler: the utterance splits there, then ends ten inputs later.
TEST(SpeechTelephonyInput, ASplitAtTheLimitInsideAFinalizesFlush) {
  Binding binding(&mulaw_dispatch);
  const auto key = open_mulaw(binding);
  const Bytes clip = patterned_bytes(240'010, 7);
  ASSERT_TRUE(binding.audio_in_long_frames(key, clip));
  binding.finalize(key, 1);
  Resampled input;
  input.push(clip);
  input.flush();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::DurationLimit, 1, input, 0,
                                           480'000, 240'000, 0));
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::ClientFinalize, 2, input,
                                           480'000, 480'020, 240'010, 1));
}

TEST(SpeechTelephonyInput, ASplitAtTheLimitInsideAHalfClosesFlush) {
  Binding binding(&mulaw_dispatch);
  const auto key = open_mulaw(binding);
  const Bytes clip = patterned_bytes(240'010, 7);
  ASSERT_TRUE(binding.audio_in_long_frames(key, clip));
  ASSERT_TRUE(binding.half_close(key));
  Resampled input;
  input.push(clip);
  input.flush();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::DurationLimit, 1, input, 0,
                                           480'000, 240'000, 0));
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::HalfClose, 2, input,
                                           480'000, 480'020, 240'010, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

// The onset is the voice-activity frame of 512 internal samples holding
// internal sample 16,000; the utterance starts 2,560 samples (160 ms) before
// it.
TEST(SpeechTelephonyInput, MulawSpeechKeepsItsLeadingPadding) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  const auto key = open_mulaw(binding);
  const Bytes audio = join(hushed(8'000), loud(8'000));
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.finalize(key, 1);
  Resampled input;
  input.push(audio);
  input.flush();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::ClientFinalize, 1, input,
                                           31 * 512 - 2'560, 32'000, 16'000, 1));
}

TEST(SpeechTelephonyInput, AHalfCloseWithVoiceDetectionTakesTheHeldSamples) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  const auto key = open_mulaw(binding);
  const Bytes audio = join(hushed(8'000), loud(8'000));
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_TRUE(binding.half_close(key));
  Resampled input;
  input.push(audio);
  input.flush();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, key, EndpointReason::HalfClose, 1, input,
                                           31 * 512 - 2'560, 32'000, 16'000, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

// Five inputs past 30 s: the limit falls inside the samples the Finalize
// takes from the resampler, and voice detection splits there once.
TEST(SpeechTelephonyInput, AFinalizeWhoseHeldSamplesPassTheLongestSplitsOnce) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  const auto key = open_mulaw(binding);
  const Bytes audio = loud(240'005);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.finalize(key, 1);
  Resampled input;
  input.push(audio);
  input.flush();
  // Both endpoints are offered before the first final.
  ASSERT_NO_FATAL_FAILURE(expect_endpoint(binding, key, EndpointReason::DurationLimit, 1, 240'000));
  ASSERT_NO_FATAL_FAILURE(
      expect_endpoint(binding, key, EndpointReason::ClientFinalize, 2, 240'005));
  ASSERT_NO_FATAL_FAILURE(expect_final(binding, key, 1, input, 0, 480'000, 240'000, 0));
  ASSERT_NO_FATAL_FAILURE(expect_final(binding, key, 2, input, 480'000, 480'010, 240'005, 1));
}

TEST(SpeechTelephonyInput, AHalfCloseWhoseHeldSamplesPassTheLongestSplitsOnce) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  const auto key = open_mulaw(binding);
  const Bytes audio = loud(240'005);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_TRUE(binding.half_close(key));
  Resampled input;
  input.push(audio);
  input.flush();
  ASSERT_NO_FATAL_FAILURE(expect_endpoint(binding, key, EndpointReason::DurationLimit, 1, 240'000));
  ASSERT_NO_FATAL_FAILURE(expect_endpoint(binding, key, EndpointReason::HalfClose, 2, 240'005));
  ASSERT_NO_FATAL_FAILURE(expect_final(binding, key, 1, input, 0, 480'000, 240'000, 0));
  ASSERT_NO_FATAL_FAILURE(expect_final(binding, key, 2, input, 480'000, 480'010, 240'005, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SpeechTelephonyInput, AWorkerDrainFlushesOnlyWhatAFinalizeEnds) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const Error shutdown =
      Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown");
  // A drain ends no utterance: the session closes with no output.
  const auto drained = open_mulaw(binding);
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(drained, loud(2'400));
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.manager->apply(drained, Event::Drain, shutdown).has_value());
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(drained, State::Closed));
  EXPECT_FALSE(binding.output(drained)->take().has_value());

  // A Finalize taken before the drain ends where its input ended.
  const auto finalized = open_mulaw(binding);
  speech.fixture->hold_jobs(JobClass::VadFrames);
  const Bytes audio = loud(2'401);
  binding.audio(finalized, audio);
  binding.finalize(finalized, 3);
  ASSERT_TRUE(binding.manager->apply(finalized, Event::Drain, shutdown).has_value());
  speech.release_jobs();
  Resampled input;
  input.push(audio);
  input.flush();
  ASSERT_NO_FATAL_FAILURE(expect_utterance(binding, finalized, EndpointReason::ClientFinalize, 1,
                                           input, 0, 4'802, 2'401, 3));
  EXPECT_TRUE(binding.ends_as(finalized, State::Closed));
}

TEST(SpeechTelephonyInput, AWorkerDrainAtTheLimitEndsNoUtterance) {
  Binding binding(&mulaw_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const Error shutdown =
      Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown");
  const auto key = open_mulaw(binding);
  ASSERT_TRUE(binding.audio_in_long_frames(key, patterned_bytes(240'010, 7)));
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, shutdown).has_value());
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
  EXPECT_TRUE(speech.bridge->submitted().empty());
}

// A Finalize handed over after its session's cleanup was asked, as one that
// raced a cancel or a failure is, ends nothing: the cleanup released the open
// audio, and with it what the resampler gave.
TEST(SpeechTelephonyInput, AFinalizeAfterTheCleanupReservesNothing) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = open_mulaw(binding);
  binding.audio(key, hushed(kShortestFrame));
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.strategy->stats().audio_bytes_reserved, 31U * 32'000);
  speech.bridge->hold_acknowledgements();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.eventually([&] { return speech.bridge->releases(key) == 1U; }));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().audio_bytes_reserved, 0U);
  binding.dispatch().finalize(key, 1);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().audio_bytes_reserved, 0U);
  speech.bridge->let_acknowledgements_go();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SpeechTelephonyInput, APcmFinalizeAfterTheCleanupIsIgnored) {
  Binding binding(&mulaw_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  binding.audio(key, patterned_bytes(640, 1));
  ASSERT_TRUE(binding.settle());
  speech.bridge->hold_acknowledgements();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.eventually([&] { return speech.bridge->releases(key) == 1U; }));
  ASSERT_TRUE(binding.settle());
  binding.dispatch().finalize(key, 1);
  ASSERT_TRUE(binding.settle());
  speech.bridge->let_acknowledgements_go();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// The resampler gives nothing to a Finalize before any input, so nothing is
// reserved for it.
TEST(SpeechTelephonyInput, AMulawFinalizeBeforeAnyAudioReservesNothing) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = open_mulaw(binding);
  binding.finalize(key, 1);
  ASSERT_NO_FATAL_FAILURE(expect_endpoint(binding, key, EndpointReason::ClientFinalize, 1, 0));
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_TRUE(transcript->segments.empty());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().audio_bytes_reserved, 0U);
}

// Mu-law at 16 kHz is never granted, and intake reads only the encoding: N
// input samples become 2N internal ones, past the N the source covers.
TEST(SpeechTelephonyInput, AnUtteranceEndingPastItsInputFailsItsSession) {
  Binding binding(&mulaw_dispatch);
  constexpr StreamAudioFormat kMulaw16k{StreamAudioEncoding::Mulaw, 16'000, 1};
  const auto finalized = binding.open_as(binding.under_test->audio_request(), kMulaw16k);
  binding.audio(finalized, patterned_bytes(320, 1));
  binding.finalize(finalized, 1);
  const auto closed = binding.open_as(binding.under_test->audio_request(), kMulaw16k);
  binding.audio(closed, patterned_bytes(320, 2));
  ASSERT_TRUE(binding.half_close(closed));
  for (const auto key : {finalized, closed}) {
    ASSERT_TRUE(binding.ends_as(key, State::Failed));
    EXPECT_EQ(binding.manager->tombstone(key)->reason, "interval_out_of_range");
  }
  ASSERT_TRUE(binding.settle());
  EXPECT_TRUE(speech_of(binding).bridge->submitted().empty());
}

TEST(SpeechTelephonyInput, AMulawDecodesDeadlineCountsItsInternalAudio) {
  Binding binding(&mulaw_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = open_mulaw(binding);
  speech.hold_jobs();
  // One second of input, 16,000 internal samples: the deadline is 1.5 s.
  ASSERT_TRUE(binding.audio_paced(key, patterned_bytes(8'000, 1), kShortestFrame));
  binding.finalize(key, 1);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1500ms - 1ns));
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 0U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
}

// At its first frame a session reserves its longest utterance of internal
// audio, and with voice detection the second of unscored audio its credit
// allows: the same bytes at either input format.
TEST(SpeechTelephonyInput, AMulawSessionReservesItsAudioAtTheInternalRate) {
  for (const auto factory : {&mulaw_dispatch, &mulaw_dispatch_detecting_voice}) {
    Binding binding(factory);
    const std::uint64_t seconds = factory == &mulaw_dispatch ? 30 : 31;
    const auto mulaw = open_mulaw(binding);
    binding.audio(mulaw, hushed(kShortestFrame));
    ASSERT_TRUE(binding.settle());
    EXPECT_EQ(speech_of(binding).strategy->stats().audio_bytes_reserved, seconds * 32'000);
    const auto pcm = binding.open_audio();
    binding.audio(pcm, Bytes(640, std::byte{0}));
    ASSERT_TRUE(binding.settle());
    EXPECT_EQ(speech_of(binding).strategy->stats().audio_bytes_reserved, 2 * seconds * 32'000);
  }
}

const nlohmann::json& pattern_record() {
  static const nlohmann::json loaded = [] {
    std::ifstream in(std::string{TP_SOURCE_DIR} +
                     "/test/unit/fixtures/telephony_dispatch/speech_pattern.json");
    EXPECT_TRUE(in.is_open());
    return nlohmann::json::parse(in);
  }();
  return loaded;
}

std::int64_t triangle(std::uint64_t n, std::uint64_t period, std::int64_t amplitude) {
  const auto phase = static_cast<std::int64_t>(n % period);
  const auto length = static_cast<std::int64_t>(period);
  return phase < length / 2 ? -amplitude + 4 * amplitude * phase / length
                            : 3 * amplitude - 4 * amplitude * phase / length;
}

/// The recorded pattern's 16 kHz PCM, built as its recorder builds it.
Bytes pattern_pcm(const nlohmann::json& record) {
  Bytes pcm;
  for (const auto& segment : record.at("segments")) {
    const auto period = segment.at("tone_period").get<std::uint64_t>();
    for (std::uint64_t n = 0; n < segment.at("samples").get<std::uint64_t>(); ++n) {
      const auto bits = static_cast<std::uint16_t>(
          period == 0 ? triangle(n, record.at("quiet_period").get<std::uint64_t>(),
                                 record.at("quiet_amplitude").get<std::int64_t>())
                      : triangle(n, period, record.at("amplitude").get<std::int64_t>()));
      pcm.push_back(static_cast<std::byte>(bits & 0xffU));
      pcm.push_back(static_cast<std::byte>(bits >> 8U));
    }
  }
  return pcm;
}

std::uint64_t fnv1a64(std::span<const std::byte> bytes) {
  std::uint64_t digest = 0xcbf29ce484222325ULL;
  for (const auto byte : bytes) {
    digest = (digest ^ std::to_integer<std::uint64_t>(byte)) * 0x100000001b3ULL;
  }
  return digest;
}

Bytes bytes_of(const std::string& hex) {
  Bytes bytes;
  for (std::size_t at = 0; at + 1 < hex.size(); at += 2) {
    bytes.push_back(static_cast<std::byte>(std::stoi(hex.substr(at, 2), nullptr, 16)));
  }
  return bytes;
}

struct Heard {
  EndpointReason reason = EndpointReason::ClientFinalize;
  std::uint64_t end_us = 0;
  std::uint64_t segment_start_us = 0;
  std::uint64_t segment_end_us = 0;
};

/// The utterances of a session sent `audio` then a Finalize, its endpoints'
/// offsets in microseconds at `rate`.
std::vector<Heard> heard(std::span<const std::byte> audio, std::uint64_t rate) {
  Binding binding(&mulaw_dispatch_detecting_voice);
  const auto key = rate == 8'000 ? open_mulaw(binding) : binding.open_audio();
  EXPECT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.finalize(key, 1);
  std::vector<Heard> utterances;
  for (;;) {
    const auto endpoint = binding.read_as<EndpointDetectedOutput>(key);
    const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
    if (!endpoint || !transcript || transcript->segments.size() != 1) {
      ADD_FAILURE() << "utterance " << utterances.size() + 1 << " not read whole";
      return utterances;
    }
    utterances.push_back({endpoint->reason, endpoint->end_sample_offset * 1'000'000 / rate,
                          transcript->segments.front().start_us,
                          transcript->segments.front().end_us});
    if (endpoint->reason == EndpointReason::ClientFinalize) {
      return utterances;
    }
  }
}

// The derivation smears each edge of the pattern over the filter's 63 taps,
// which can move a voice-activity decision by a frame at an edge: the case
// holds each bound to one frame, 512 internal samples or 32 ms.
TEST(SpeechTelephonyInput, APatternAndItsMulawDerivationGiveTheSameUtterances) {
  const auto& record = pattern_record();
  const Bytes pcm = pattern_pcm(record);
  ASSERT_EQ(fnv1a64(pcm),
            std::stoull(record.at("pcm_s16le_fnv1a64").get<std::string>(), nullptr, 16));
  ASSERT_EQ(record.at("telephony").at("recipe_id").get<std::string>(),
            "tp-telephony8k-fir63-g711stl-v1");
  const Bytes mulaw = bytes_of(record.at("telephony").at("mulaw_hex").get<std::string>());
  ASSERT_EQ(2 * mulaw.size(), pcm.size() / 2);
  const auto wide = heard(pcm, 16'000);
  const auto narrow = heard(mulaw, 8'000);
  // Two silence endpoints, the second after a pause too short to end the
  // first utterance, then the Finalize.
  ASSERT_EQ(wide.size(), 3U);
  ASSERT_EQ(narrow.size(), wide.size());
  constexpr double kFrameUs = 32'000;
  for (std::size_t index = 0; index < wide.size(); ++index) {
    EXPECT_EQ(narrow[index].reason, wide[index].reason) << index;
    EXPECT_NEAR(static_cast<double>(narrow[index].end_us), static_cast<double>(wide[index].end_us),
                kFrameUs)
        << index;
    EXPECT_NEAR(static_cast<double>(narrow[index].segment_start_us),
                static_cast<double>(wide[index].segment_start_us), kFrameUs)
        << index;
    EXPECT_NEAR(static_cast<double>(narrow[index].segment_end_us),
                static_cast<double>(wide[index].segment_end_us), kFrameUs)
        << index;
  }
  EXPECT_EQ(wide.back().end_us, 3'672'375U);
  EXPECT_EQ(narrow.back().end_us, wide.back().end_us);
}
}  // namespace
}  // namespace tensorplate::testing
