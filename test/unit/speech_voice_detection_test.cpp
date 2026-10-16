// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <map>
#include <memory>
#include <mutex>
#include <span>
#include <string>
#include <tuple>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/job_request.hpp"

#include "fake_scheduler_clock.hpp"
#include "fixture_job_bridge.hpp"
#include "serving/session/dispatch.hpp"
#include "serving/session/speech_dispatch.hpp"
#include "session_dispatch_binding.hpp"
#include "speech_dispatch_binding.hpp"

// The speech dispatch over a backend that runs `vad_frames`: the fixture's
// voice activity is each 512-sample frame's peak over full scale, so loud
// samples are speech and quiet ones silence.

namespace tensorplate::testing {
namespace {
using namespace std::chrono_literals;
using namespace tensorplate::serving;
using Binding = ReferenceBinding;
using Event = LogicalSessionEvent;
using State = LogicalSessionState;
using Bytes = std::vector<std::byte>;
/// Decode windows: the end of each, by its first sample.
using Windows = std::map<std::uint64_t, std::uint64_t>;

constexpr std::uint64_t kVadFrame = 512;
constexpr std::uint64_t kLongest = 480'000;

Bytes pcm_of(std::uint64_t samples, unsigned seed, int floor, int spread) {
  Bytes pcm(2 * samples);
  for (std::uint64_t index = 0; index < samples; ++index) {
    const int magnitude = floor + static_cast<int>((index * 131 + seed * 17) % spread);
    const auto bits = static_cast<std::uint16_t>(
        static_cast<std::int16_t>(index % 2 == 0 ? magnitude : -magnitude));
    pcm[2 * index] = static_cast<std::byte>(bits & 0xffU);
    pcm[2 * index + 1] = static_cast<std::byte>(bits >> 8U);
  }
  return pcm;
}

/// Every frame of it is speech: its peak is at least half of full scale.
Bytes voiced(std::uint64_t samples, unsigned seed) {
  return pcm_of(samples, seed, 16'384, 8'000);
}

/// Every frame of it is silence.
Bytes quiet(std::uint64_t samples, unsigned seed) {
  return pcm_of(samples, seed, 1, 96);
}

Bytes join(Bytes first, const Bytes& second) {
  first.insert(first.end(), second.begin(), second.end());
  return first;
}

std::span<const std::byte> samples_of(const Bytes& pcm, std::uint64_t from, std::uint64_t to) {
  return std::span{pcm}.subspan(2 * from, 2 * (to - from));
}

std::vector<JobRequest> jobs_of(const SpeechUnderTest& speech, JobClass job_class) {
  std::vector<JobRequest> of_class;
  for (auto& request : speech.bridge->submitted()) {
    if (request.job_class() == job_class) {
      of_class.push_back(std::move(request));
    }
  }
  return of_class;
}

/// Where the frames of the session's voice-activity jobs end, in order.
std::vector<std::uint64_t> scored_ends(const SpeechUnderTest& speech, std::uint64_t key) {
  std::vector<std::uint64_t> ends;
  std::uint64_t end = 0;
  for (const auto& request : jobs_of(speech, JobClass::VadFrames)) {
    if (request.identity().session_key == key) {
      const auto& frames = std::get<VadFrames>(request.payload());
      end += std::uint64_t{frames.frame_count} * frames.frame_samples;
      ends.push_back(end);
    }
  }
  return ends;
}

/// The window of each utterance's final decode: the last decode of the
/// utterance's first sample, after its partial decodes.
Windows final_decodes(const SpeechUnderTest& speech, std::uint64_t key) {
  Windows finals;
  for (const auto& request : jobs_of(speech, JobClass::SttDecode)) {
    if (request.identity().session_key == key) {
      const auto& audio = std::get<AudioFrames>(request.payload());
      finals[audio.start_sample] = audio.start_sample + audio.pcm.byte_size / 2;
    }
  }
  return finals;
}

std::uint32_t queue_depth(Binding& binding, std::uint64_t key) {
  const auto status = binding.manager->status(key);
  return status ? status->input_queue_depth : 0;
}

std::unique_ptr<DispatchUnderTest> detecting_with_short_bounds(const SchedulerClock& clock) {
  SpeechDispatchOptions options;
  options.job_deadline_base = 1s;
  options.job_deadline_per_audio_second = 500ms;
  options.release_bound = 2s;
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, std::move(options),
                                           VoiceDetection::On);
}

/// The one decode submitted is a partial of the utterance ending at `end`,
/// held: the utterance's own decode waits behind it.
void expect_partial_held_before(const SpeechUnderTest& speech, std::uint64_t key,
                                std::uint64_t end) {
  const auto decodes = jobs_of(speech, JobClass::SttDecode);
  ASSERT_EQ(decodes.size(), 1U);
  EXPECT_EQ(decodes.front().identity().session_key, key);
  const auto& audio = std::get<AudioFrames>(decodes.front().payload());
  EXPECT_EQ(audio.start_sample, 0U);
  EXPECT_LT(audio.pcm.byte_size / 2, end);
}

/// Releases the decodes expect_partial_held_before() saw held: the partial
/// ends, its release lets the endpoint's own decode start, and that ends.
/// False if any of it outlasts the patience.
bool end_endpoints_decode(SpeechUnderTest& speech) {
  speech.fixture->release_jobs(JobClass::SttDecode);
  return speech.fixture->settled(Binding::kPatience) &&
         speech.strategy->wait_idle(Binding::kPatience) &&
         speech.fixture->settled(Binding::kPatience);
}

void move_clock(Binding& binding, SchedulerClock::Duration by) {
  binding.clock.advance(by);
  speech_of(binding).strategy->notify_clock_advanced();
  ASSERT_TRUE(binding.settle());
}

TEST(SpeechVoiceDetection, ASilenceEndpointIsOfferedBeforeItsUtterancesDecodeEnds) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  // Frames 0 to 31 hold speech; the tenth quiet frame after them ends at 42.
  const Bytes audio = join(voiced(16'000, 1), quiet(6'000, 2));
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  const auto endpoint = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(endpoint.has_value());
  EXPECT_EQ(endpoint->utterance_id, 1U);
  EXPECT_EQ(endpoint->end_sample_offset, 42 * kVadFrame);
  EXPECT_EQ(endpoint->reason, EndpointReason::Silence);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);

  speech.release_jobs();
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->utterance_id, 1U);
  EXPECT_EQ(transcript->end_sample_offset, 42 * kVadFrame);
  EXPECT_EQ(transcript->finalize_sequence, 0U);
  const auto expected = speech.transcript(0, samples_of(audio, 0, 42 * kVadFrame));
  ASSERT_EQ(transcript->segments.size(), 1U);
  EXPECT_EQ(transcript->segments.front().text, expected.front().text);
  EXPECT_EQ(transcript->segments.front().end_us, expected.front().end_us);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(final_decodes(speech, key), (Windows{{0, 42 * kVadFrame}}));
}

TEST(SpeechVoiceDetection, AFinalizeEndsTheUtteranceOnceTheFramesBeforeItWereScored) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  // Nine whole frames and 192 samples, in one frame of the stream.
  const Bytes audio = voiced(4'800, 3);
  binding.audio(key, audio);
  binding.finalize(key, 4);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
  EXPECT_TRUE(jobs_of(speech, JobClass::SttDecode).empty());
  EXPECT_EQ(queue_depth(binding, key), 1U);

  // The frame left unscored belongs to the utterance.
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, audio, 4));
  EXPECT_EQ(queue_depth(binding, key), 0U);

  // A Finalize on a frame's end waits for that frame too: its score makes
  // the onset.
  const auto aligned = binding.open_audio();
  const Bytes first = voiced(kVadFrame, 43);
  const Bytes last = voiced(kVadFrame, 44);
  binding.audio(aligned, first);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(scored_ends(speech, aligned), std::vector<std::uint64_t>{kVadFrame});
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(aligned, last);
  binding.finalize(aligned, 5);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(aligned)->take().has_value());
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(aligned, EndpointReason::ClientFinalize, 1, 0, join(first, last), 5));
}

TEST(SpeechVoiceDetection, AFinalizesEndpointIsOfferedBeforeItsDecodeEnds) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  const Bytes audio = voiced(4'800, 5);
  binding.audio(key, audio);
  binding.finalize(key, 2);
  const auto endpoint = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(endpoint.has_value());
  EXPECT_EQ(endpoint->reason, EndpointReason::ClientFinalize);
  EXPECT_EQ(endpoint->end_sample_offset, 4'800U);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
  speech.release_jobs();
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->utterance_id, 1U);
  EXPECT_EQ(transcript->finalize_sequence, 2U);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST(SpeechVoiceDetection, AnEndpointWaitingForRoomLetsItsDecodeStart) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const auto queue = binding.output(key);
  // A lifecycle message the peer has not read leaves the endpoint no room.
  OutputItem unread{
      OutputKind::Control,
      SessionBudgets::kOutputMetadataBytes - SessionBudgets::kOutputControlReserveBytes, 0,
      std::make_unique<OutputBody>()};
  ASSERT_EQ(queue->offer(unread, binding.clock.now()).value(), OutputOffer::Queued);
  const Bytes audio = voiced(4'800, 65);
  binding.audio(key, audio);
  binding.finalize(key, 9);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(jobs_of(speech, JobClass::SttDecode).size(), 1U);
  EXPECT_EQ(queue->metadata_usage().used(), unread.bytes);
  // The finalization completes only once its endpoint was queued.
  EXPECT_EQ(binding.live_state(key), State::Finalizing);

  const auto lifecycle = queue->take();
  ASSERT_TRUE(lifecycle.has_value());
  ASSERT_TRUE(queue->delivered(lifecycle->sequence, binding.clock.now()));
  binding.dispatch().output_delivered(key, OutputKind::Control);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, audio, 9));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(jobs_of(speech, JobClass::SttDecode).size(), 1U);
}

TEST(SpeechVoiceDetection, SilenceBeforeTheOnsetIsDroppedButItsLeadingPaddingKept) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  // Speech from 16,000 makes frame 31 the run's first: the onset is 160 ms
  // before it.
  const Bytes audio = join(quiet(16'000, 6), voiced(8'000, 7));
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.finalize(key, 3);
  const std::uint64_t onset = 31 * kVadFrame - 2'560;
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 1, onset,
                                               samples_of(audio, onset, 24'000), 3));
  EXPECT_EQ(final_decodes(speech, key), (Windows{{onset, 24'000}}));
}

TEST(SpeechVoiceDetection, AFinalizeInSilenceYieldsAFinalWithoutSegmentsAndNoDecode) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  binding.audio(key, quiet(4'800, 8));
  binding.finalize(key, 1);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 4'800, {}, 1));
  // One speech frame is not an onset: the frames start again at the Finalize.
  binding.audio(key, join(voiced(kVadFrame, 9), quiet(1'000, 10)));
  binding.finalize(key, 2);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               4'800 + kVadFrame + 1'000, {}, 2));
  ASSERT_TRUE(binding.settle());
  EXPECT_TRUE(jobs_of(speech, JobClass::SttDecode).empty());
  EXPECT_EQ(binding.live_state(key), State::Active);
  // Each Finalize starts the backend's state afresh.
  const auto vad = jobs_of(speech, JobClass::VadFrames);
  ASSERT_EQ(vad.size(), 2U);
  EXPECT_EQ(std::get<VadFrames>(vad[0].payload()).utterance_id, 1U);
  EXPECT_EQ(std::get<VadFrames>(vad[1].payload()).utterance_id, 2U);
}

TEST(SpeechVoiceDetection, AHalfCloseEndsTheUtteranceOnceItsFramesWereScored) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  const Bytes audio = voiced(4'800, 11);
  binding.audio(key, audio);
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::HalfClose, 1, 0, audio, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));

  // In silence: a final without segments.
  const auto quiet_key = binding.open_audio();
  binding.audio(quiet_key, quiet(1'600, 12));
  ASSERT_TRUE(binding.half_close(quiet_key));
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(quiet_key, EndpointReason::HalfClose, 1, 1'600, {}, 0));
  EXPECT_TRUE(binding.ends_as(quiet_key, State::Closed));

  // Nothing since the last final: no output.
  const auto ended_key = binding.open_audio();
  binding.audio(ended_key, quiet(1'600, 13));
  binding.finalize(ended_key, 1);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(ended_key, EndpointReason::ClientFinalize, 1, 1'600, {}, 1));
  ASSERT_TRUE(binding.half_close(ended_key));
  EXPECT_TRUE(binding.ends_as(ended_key, State::Closed));
  EXPECT_FALSE(binding.output(ended_key)->take().has_value());

  // A half-close behind a Finalize whose frames wait leaves its answer.
  const auto finalized = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  const Bytes finalized_audio = voiced(4'800, 40);
  binding.audio(finalized, finalized_audio);
  binding.finalize(finalized, 9);
  ASSERT_TRUE(binding.half_close(finalized));
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(finalized, EndpointReason::ClientFinalize, 1, 0, finalized_audio, 9));
  EXPECT_TRUE(binding.ends_as(finalized, State::Closed));
  EXPECT_FALSE(binding.output(finalized)->take().has_value());
}

TEST(SpeechVoiceDetection, AFramesCreditReturnsOnceAllOfItWasScored) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  // Ten frames of 20 ms: six whole frames to score.
  ASSERT_TRUE(binding.audio_paced(key, voiced(3'200, 14), 640));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(queue_depth(binding, key), 10U);
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  // The last frame ends at 3,200, past the sixth scored frame.
  EXPECT_EQ(queue_depth(binding, key), 1U);
  binding.finalize(key, 1);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(queue_depth(binding, key), 0U);
}

TEST(SpeechVoiceDetection, AudioAfterAnEndpointWaitsUnscoredUntilItsUtterancesFinal) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  const Bytes first = join(voiced(16'000, 15), quiet(42 * kVadFrame - 16'000, 16));
  ASSERT_TRUE(binding.audio_in_long_frames(key, first));
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  const auto scored_before = scored_ends(speech, key);
  ASSERT_EQ(scored_before.back(), 42 * kVadFrame);

  // Half a second more, in two frames: held, its credit with it.
  const Bytes following = voiced(8'000, 17);
  ASSERT_TRUE(binding.audio_in_long_frames(key, following));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(scored_ends(speech, key), scored_before);
  EXPECT_EQ(queue_depth(binding, key), 2U);
  EXPECT_FALSE(binding.output(key)->take().has_value());

  speech.release_jobs();
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->utterance_id, 1U);
  binding.finalize(key, 6);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 2, 42 * kVadFrame, following, 6));
  // The scoring went on from the endpoint, its state afresh.
  const auto vad = jobs_of(speech, JobClass::VadFrames);
  ASSERT_GT(vad.size(), scored_before.size());
  for (std::size_t index = 0; index < vad.size(); ++index) {
    EXPECT_EQ(std::get<VadFrames>(vad[index].payload()).utterance_id,
              index < scored_before.size() ? 1U : 2U);
  }
}

// One job scores the frames on both sides of a silence endpoint: the credit
// of the audio after it is held until the utterance's final, as when that
// audio waits unscored.
TEST(SpeechVoiceDetection, AnEndpointInsideAJobHoldsTheCreditOfTheFramesAfterIt) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs();
  // In frames of 320 ms and a last of 32 ms: the first job scores the first
  // frame, the second the rest, the endpoint at the end of the third.
  const std::uint64_t endpoint = 30 * kVadFrame;
  const Bytes audio =
      join(join(voiced(20 * kVadFrame, 96), quiet(10 * kVadFrame, 97)), voiced(kVadFrame, 98));
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(queue_depth(binding, key), 4U);
  speech.fixture->release_jobs(JobClass::VadFrames);
  const auto silence = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(silence.has_value());
  EXPECT_EQ(silence->end_sample_offset, endpoint);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(scored_ends(speech, key),
            (std::vector<std::uint64_t>{10 * kVadFrame, endpoint + kVadFrame}));
  EXPECT_EQ(queue_depth(binding, key), 1U);

  speech.release_jobs();
  ASSERT_TRUE(binding.read_as<FinalTranscriptOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(queue_depth(binding, key), 0U);
}

TEST(SpeechVoiceDetection, AFinalizeAfterAnEndpointIsCompletedByItsOwnFinal) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  const Bytes first = join(voiced(16'000, 18), quiet(42 * kVadFrame - 16'000, 19));
  ASSERT_TRUE(binding.audio_in_long_frames(key, first));
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  speech.fixture->hold_jobs(JobClass::VadFrames);
  const Bytes following = voiced(8'000, 20);
  ASSERT_TRUE(binding.audio_in_long_frames(key, following));
  binding.finalize(key, 7);
  ASSERT_TRUE(binding.settle());

  // The endpoint's final does not complete the finalization the Finalize
  // joined while that Finalize's frames are unscored.
  speech.fixture->release_jobs(JobClass::SttDecode);
  ASSERT_TRUE(binding.read_as<FinalTranscriptOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 2, 42 * kVadFrame, following, 7));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
}

// The endpoint's decode ends inside the call that applies the Finalize, so
// the finalization completes before the dispatch sees that Finalize.
TEST(SpeechVoiceDetection, AFinalizeTakenWhileAnotherWaitsForItsFramesIsAnsweredInTurn) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  const Bytes first = join(voiced(16'000, 61), quiet(42 * kVadFrame - 16'000, 62));
  ASSERT_TRUE(binding.audio_in_long_frames(key, first));
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  ASSERT_NO_FATAL_FAILURE(expect_partial_held_before(speech, key, 42 * kVadFrame));
  speech.fixture->hold_jobs(JobClass::VadFrames);
  const Bytes following = voiced(3'200, 63);
  ASSERT_TRUE(binding.audio_in_long_frames(key, following));
  ASSERT_TRUE(binding.settle());
  bool decode_ended = false;
  binding.before_next(key, Event::Finalize, [&] { decode_ended = end_endpoints_decode(speech); });
  binding.finalize(key, 7);
  ASSERT_TRUE(decode_ended);
  const auto endpoints_final = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(endpoints_final.has_value());
  EXPECT_EQ(endpoints_final->finalize_sequence, 0U);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(binding.live_state(key), State::Active);

  const Bytes more = voiced(3'200, 64);
  binding.audio(key, more);
  binding.finalize(key, 8);
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 2, 42 * kVadFrame, following, 7));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 3,
                                               42 * kVadFrame + 3'200, more, 8));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
}

// The race above while the session's frames wait for the voice-activity
// lane: by the time its job is made, audio past the first Finalize has
// arrived, and only the frames before that Finalize are scored for it.
TEST(SpeechVoiceDetection, AFinalizeWaitingInTurnIsScoredOnlyToItsOwnEnd) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  const std::uint64_t endpoint = 42 * kVadFrame;
  const Bytes first = join(voiced(16'000, 90), quiet(endpoint - 16'000, 91));
  ASSERT_TRUE(binding.audio_in_long_frames(key, first));
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  ASSERT_NO_FATAL_FAILURE(expect_partial_held_before(speech, key, endpoint));
  speech.fixture->hold_jobs(JobClass::VadFrames);
  for (int session = 0; session < 8; ++session) {
    binding.audio(binding.open_audio(), voiced(1'024, 92));
  }
  const Bytes following = voiced(6 * kVadFrame, 93);
  ASSERT_TRUE(binding.audio_in_long_frames(key, following));
  ASSERT_TRUE(binding.settle());
  bool decode_ended = false;
  binding.before_next(key, Event::Finalize, [&] { decode_ended = end_endpoints_decode(speech); });
  binding.finalize(key, 7);
  ASSERT_TRUE(decode_ended);
  ASSERT_TRUE(binding.read_as<FinalTranscriptOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(binding.live_state(key), State::Active);
  const Bytes more = voiced(6 * kVadFrame, 94);
  binding.audio(key, more);
  binding.finalize(key, 8);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(scored_ends(speech, key).back(), endpoint);

  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 2, endpoint, following, 7));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 3,
                                               endpoint + 6 * kVadFrame, more, 8));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  const auto ends = scored_ends(speech, key);
  const auto after_race = std::upper_bound(ends.begin(), ends.end(), endpoint);
  ASSERT_NE(after_race, ends.end());
  EXPECT_EQ(*after_race, endpoint + 6 * kVadFrame);
}

TEST(SpeechVoiceDetection, TheLongestUtteranceIsSplitAndScoringGoesOnUnderOneState) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  const Bytes audio = voiced(kLongest + 4'000, 21);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  const auto endpoint = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(endpoint.has_value());
  EXPECT_EQ(endpoint->reason, EndpointReason::DurationLimit);
  EXPECT_EQ(endpoint->end_sample_offset, kLongest);
  ASSERT_TRUE(binding.settle());
  // The split falls in frame 937; no job after the one that scored it.
  const auto ends = scored_ends(speech, key);
  ASSERT_GE(ends.size(), 2U);
  EXPECT_GE(ends.back(), 938 * kVadFrame);
  EXPECT_LE(ends[ends.size() - 2], 937 * kVadFrame);

  speech.release_jobs();
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->end_sample_offset, kLongest);
  binding.finalize(key, 5);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2, kLongest,
                                               samples_of(audio, kLongest, kLongest + 4'000), 5));
  for (const auto& request : jobs_of(speech, JobClass::VadFrames)) {
    EXPECT_EQ(std::get<VadFrames>(request.payload()).utterance_id, 1U);
  }
}

// After a split in the middle of a frame the next limit falls on a frame's
// end: a silence run that ends there ends the utterance as silence.
TEST(SpeechVoiceDetection, ASilenceRunEndingAtTheLimitEndsTheUtteranceAsSilence) {
  Binding binding(&speech_dispatch_detecting_voice);
  const auto key = binding.open_audio();
  const std::uint64_t limit = 2 * kLongest;
  const Bytes audio = join(voiced(limit - 10 * kVadFrame, 22), quiet(10 * kVadFrame + 1'000, 23));
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0,
                                               samples_of(audio, 0, kLongest), 0));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::Silence, 2, kLongest,
                                               samples_of(audio, kLongest, limit), 0));
}

// After a split the next utterance can lie wholly inside the silence run
// that ends it: the 256 samples left of a quiet frame that holds the limit
// and ends the run, the run's last frames after a split on an earlier quiet
// frame, or the whole run after a second split on a frame's end. It is
// silence, with no decode.
TEST(SpeechVoiceDetection, AnUtteranceWhollyInsideTheSilenceRunEndingItIsSilence) {
  struct Opening {
    std::uint64_t voiced_samples = 0;
    std::uint64_t splits = 0;
  };
  for (const auto [voiced_samples, splits] :
       {Opening{928 * kVadFrame, 1}, Opening{930 * kVadFrame, 1}, Opening{2 * kLongest, 2}}) {
    Binding binding(&speech_dispatch_detecting_voice);
    const auto key = binding.open_audio();
    const std::uint64_t end = voiced_samples + 10 * kVadFrame;
    const Bytes audio = join(voiced(voiced_samples, 34), quiet(10 * kVadFrame + 1'000, 35));
    ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
    std::vector<EndpointDetectedOutput> endpoints;
    std::vector<FinalTranscriptOutput> finals;
    while (finals.size() < splits + 1) {
      auto next = binding.read(key);
      ASSERT_TRUE(next.has_value());
      if (auto* endpoint = std::get_if<EndpointDetectedOutput>(&*next)) {
        endpoints.push_back(*endpoint);
      } else if (auto* transcript = std::get_if<FinalTranscriptOutput>(&*next)) {
        finals.push_back(std::move(*transcript));
      }
    }
    ASSERT_EQ(endpoints.size(), splits + 1);
    for (std::uint64_t split = 0; split < splits; ++split) {
      const std::uint64_t first = split * kLongest;
      EXPECT_EQ(endpoints[split].reason, EndpointReason::DurationLimit);
      EXPECT_EQ(endpoints[split].end_sample_offset, first + kLongest);
      const auto expected =
          speech_of(binding).transcript(first, samples_of(audio, first, first + kLongest));
      ASSERT_EQ(finals[split].segments.size(), 1U);
      EXPECT_EQ(finals[split].segments.front().text, expected.front().text);
      EXPECT_EQ(finals[split].segments.front().start_us, expected.front().start_us);
      EXPECT_EQ(finals[split].segments.front().end_us, expected.front().end_us);
    }
    EXPECT_EQ(endpoints.back().utterance_id, splits + 1);
    EXPECT_EQ(endpoints.back().reason, EndpointReason::Silence);
    EXPECT_EQ(endpoints.back().end_sample_offset, end);
    EXPECT_EQ(finals.back().utterance_id, splits + 1);
    EXPECT_EQ(finals.back().end_sample_offset, end);
    EXPECT_TRUE(finals.back().segments.empty());
    ASSERT_TRUE(binding.settle());
    EXPECT_EQ(final_decodes(speech_of(binding), key).size(), splits);
  }
}

TEST(SpeechVoiceDetection, AFinalizeWhoseUnscoredTailPassesTheLongestSplitsAtTheLimit) {
  Binding binding(&speech_dispatch_detecting_voice);
  const auto key = binding.open_audio();
  // 937 whole frames stay short of the limit; the 511 samples after them do not.
  const Bytes audio = voiced(937 * kVadFrame + 511, 24);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.finalize(key, 7);
  // Both end at once, so both endpoints come before the finals.
  const std::uint64_t end = 937 * kVadFrame + 511;
  const auto split = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(split.has_value());
  EXPECT_EQ(split->utterance_id, 1U);
  EXPECT_EQ(split->end_sample_offset, kLongest);
  EXPECT_EQ(split->reason, EndpointReason::DurationLimit);
  const auto asked = binding.read_as<EndpointDetectedOutput>(key);
  ASSERT_TRUE(asked.has_value());
  EXPECT_EQ(asked->utterance_id, 2U);
  EXPECT_EQ(asked->end_sample_offset, end);
  EXPECT_EQ(asked->reason, EndpointReason::ClientFinalize);
  for (const std::uint64_t utterance : {1U, 2U}) {
    const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
    ASSERT_TRUE(transcript.has_value());
    EXPECT_EQ(transcript->utterance_id, utterance);
    const std::uint64_t first = utterance == 1 ? 0 : kLongest;
    const std::uint64_t last = utterance == 1 ? kLongest : end;
    EXPECT_EQ(transcript->end_sample_offset, last);
    EXPECT_EQ(transcript->finalize_sequence, utterance == 1 ? 0U : 7U);
    const auto expected = speech_of(binding).transcript(first, samples_of(audio, first, last));
    ASSERT_EQ(transcript->segments.size(), 1U);
    EXPECT_EQ(transcript->segments.front().text, expected.front().text);
    EXPECT_EQ(transcript->segments.front().start_us, expected.front().start_us);
  }
}

// 937 whole frames and 256 samples: the Finalize ends the utterance on the
// limit itself, so it is not split.
TEST(SpeechVoiceDetection, AFinalizeWhoseUnscoredTailReachesTheLongestIsNotSplit) {
  Binding binding(&speech_dispatch_detecting_voice);
  const auto key = binding.open_audio();
  const Bytes audio = voiced(kLongest, 95);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.finalize(key, 7);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, audio, 7));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
  EXPECT_EQ(binding.live_state(key), State::Active);
}

TEST(SpeechVoiceDetection, ASessionsNextFramesWaitForItsVoiceActivityJobInFlight) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(key, voiced(1'024, 41));
  ASSERT_TRUE(binding.settle());
  binding.audio(key, voiced(1'024, 42));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(scored_ends(speech, key), (std::vector<std::uint64_t>{1'024}));
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(scored_ends(speech, key), (std::vector<std::uint64_t>{1'024, 2'048}));
}

TEST(SpeechVoiceDetection, AtMostEightVoiceActivityJobsRunOneASession) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  speech.fixture->hold_jobs(JobClass::VadFrames);
  std::vector<std::uint64_t> keys;
  for (int session = 0; session < 9; ++session) {
    keys.push_back(binding.open_audio());
    binding.audio(keys.back(), voiced(1'024, 25));
  }
  binding.audio(keys.front(), voiced(1'024, 26));
  ASSERT_TRUE(binding.settle());
  const auto submitted = jobs_of(speech, JobClass::VadFrames);
  // As many as the sidecar's voice-activity lane lets wait.
  ASSERT_EQ(submitted.size(), 8U);
  for (std::size_t index = 0; index < submitted.size(); ++index) {
    EXPECT_EQ(submitted[index].identity().session_key, keys[index]);
  }
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  // The ninth session's turn came, then the first session's second job.
  const auto all = jobs_of(speech, JobClass::VadFrames);
  ASSERT_EQ(all.size(), 10U);
  EXPECT_EQ(all[8].identity().session_key, keys[8]);
  EXPECT_EQ(all[9].identity().session_key, keys.front());
  EXPECT_EQ(speech.strategy->stats().jobs_failed, 0U);
  for (const auto key : keys) {
    EXPECT_EQ(binding.live_state(key), State::Active);
  }
}

TEST(SpeechVoiceDetection, ARefusedVoiceActivitySubmitLeavesItsPlaceToTheNext) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  speech.fixture->hold_jobs(JobClass::VadFrames);
  const auto refused = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  speech.bridge->refuse_next_submit();
  binding.audio(refused, voiced(1'024, 66));
  ASSERT_TRUE(binding.ends_as(refused, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(refused)->reason, "job_capacity_exhausted");
  std::vector<std::uint64_t> keys;
  for (int session = 0; session < 8; ++session) {
    keys.push_back(binding.open_audio());
    binding.audio(keys.back(), voiced(1'024, 67));
  }
  ASSERT_TRUE(binding.settle());
  const auto submitted = jobs_of(speech, JobClass::VadFrames);
  ASSERT_EQ(submitted.size(), 9U);
  for (std::size_t index = 0; index < keys.size(); ++index) {
    EXPECT_EQ(submitted[index + 1].identity().session_key, keys[index]);
  }
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  for (const auto key : keys) {
    EXPECT_EQ(binding.live_state(key), State::Active);
  }
}

// The backend counts a job open until it reports it released, after its
// result, and so does the voice-activity lane.
TEST(SpeechVoiceDetection, AVoiceActivityJobHoldsItsSlotUntilTheBackendReleasesIt) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  speech.fixture->hold_job_releases();
  std::vector<std::uint64_t> keys;
  for (int session = 0; session < 8; ++session) {
    keys.push_back(binding.open_audio());
    binding.audio(keys.back(), voiced(1'024, 99));
  }
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.strategy->stats().jobs_completed, 8U);
  for (const auto key : keys) {
    binding.audio(key, voiced(1'024, 100));
  }
  keys.push_back(binding.open_audio());
  binding.audio(keys.back(), voiced(1'024, 101));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(jobs_of(speech, JobClass::VadFrames).size(), 8U);

  speech.fixture->let_job_releases_go();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(jobs_of(speech, JobClass::VadFrames).size(), 17U);
  EXPECT_EQ(speech.strategy->stats().jobs_failed, 0U);
  for (const auto key : keys) {
    EXPECT_EQ(binding.live_state(key), State::Active);
  }
}

TEST(SpeechVoiceDetection, ALostBackendsUnreleasedJobsKeepNoSlotsTaken) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  speech.fixture->hold_jobs(JobClass::VadFrames);
  std::vector<std::uint64_t> keys;
  for (int session = 0; session < 8; ++session) {
    keys.push_back(binding.open_audio());
    binding.audio(keys.back(), voiced(1'024, 103));
  }
  ASSERT_TRUE(binding.settle());
  speech.fixture->lose_backend();
  for (const auto key : keys) {
    ASSERT_TRUE(binding.ends_as(key, State::Failed));
  }
  const auto next = binding.open_audio();
  binding.audio(next, voiced(1'024, 104));
  ASSERT_TRUE(binding.ends_as(next, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(next)->reason, "backend_unavailable");
}

TEST(SpeechVoiceDetection, ASessionThatEndedWhileWaitingForTheVoiceActivityLaneIsPassedOver) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  speech.fixture->hold_jobs(JobClass::VadFrames);
  for (int session = 0; session < 8; ++session) {
    binding.audio(binding.open_audio(), voiced(1'024, 69));
  }
  const auto waiting = binding.open_audio();
  binding.audio(waiting, voiced(1'024, 70));
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(jobs_of(speech, JobClass::VadFrames).size(), 8U);
  // Its release refused, the ended session stays known to the dispatch.
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(waiting, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  const auto submitted = jobs_of(speech, JobClass::VadFrames);
  EXPECT_EQ(submitted.size(), 8U);
  for (const auto& request : submitted) {
    EXPECT_NE(request.identity().session_key, waiting);
  }
}

// A frame the stream accepted before the session's cancel and handed over
// after it: the ended session keeps none of it.
TEST(SpeechVoiceDetection, AFrameHandedOverAfterItsSessionEndedReturnsItsCredit) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes late = voiced(1'024, 102);
  ASSERT_TRUE(binding.manager->accept_input(key, late.size()).has_value());
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(queue_depth(binding, key), 1U);
  binding.dispatch().audio(key, late);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(queue_depth(binding, key), 0U);
  EXPECT_TRUE(jobs_of(speech, JobClass::VadFrames).empty());
}

TEST(SpeechVoiceDetection, AudioToASessionThatTakesTextFailsIt) {
  Binding binding(&speech_dispatch_detecting_voice);
  const auto text_key = binding.open_text();
  binding.dispatch().audio(text_key, voiced(1'024, 71));
  ASSERT_TRUE(binding.ends_as(text_key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(text_key)->reason, "wrong_input_kind");
}

TEST(SpeechVoiceDetection, AFailedVoiceActivityJobFailsItsSession) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const auto other = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  speech.fixture->fail_next_job(
      Error::make(Error::Code::Internal, "voice activity failed", "vad_runner_error"));
  binding.audio(key, voiced(1'024, 27));
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "vad_runner_error");
  EXPECT_EQ(binding.live_state(other), State::Active);

  // A lost backend ends every session it served.
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(other, voiced(1'024, 28));
  const auto idle = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  speech.fixture->lose_backend();
  ASSERT_TRUE(binding.ends_as(other, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(other)->reason, "backend_reset");
  ASSERT_TRUE(binding.ends_as(idle, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(idle)->reason, "backend_reset");
}

TEST(SpeechVoiceDetection, AVoiceActivityJobPastItsDeadlineFailsItsSession) {
  Binding binding(&detecting_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(key, voiced(1'024, 29));
  ASSERT_TRUE(binding.settle());
  // Two frames, 64 ms of audio: 1 s and 32 ms.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1032ms - 1ns));
  EXPECT_EQ(binding.live_state(key), State::Active);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(binding.live_state(key), State::Failed);
  // Its release waits for the job, which its work ends.
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "deadline_missed");
}

TEST(SpeechVoiceDetection, AReleaseIsBoundFromTheDeadlineOfItsVoiceActivityJob) {
  Binding binding(&detecting_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(key, voiced(1'024, 30));
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  // The job's deadline is 1,032 ms away, so the bound 3,032 ms.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 3032ms - 1ns));
  EXPECT_TRUE(speech.stalls().empty());
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(speech.stalls().size(), 1U);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SpeechVoiceDetection, StopCancelsTheVoiceActivityJobOfASessionWhoseReleaseWasRefused) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(key, voiced(1'024, 31));
  ASSERT_TRUE(binding.settle());
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_TRUE(speech.bridge->cancelled().empty());
  binding.dispatch().stop();
  const auto submitted = jobs_of(speech, JobClass::VadFrames);
  ASSERT_EQ(submitted.size(), 1U);
  EXPECT_EQ(speech.bridge->cancelled(), std::vector{submitted.front().identity().job_id});
  speech.release_jobs();
}

TEST(SpeechVoiceDetection, AWorkerDrainScoresOnlyWhatAFinalizeEnds) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(key, voiced(4'800, 32));
  ASSERT_TRUE(binding.settle());
  const Error shutdown =
      Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown");
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, shutdown).has_value());
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());

  const auto finalized = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  const Bytes audio = voiced(4'800, 33);
  binding.audio(finalized, audio);
  binding.finalize(finalized, 3);
  ASSERT_TRUE(binding.manager->apply(finalized, Event::Drain, shutdown).has_value());
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(finalized, EndpointReason::ClientFinalize, 1, 0, audio, 3));
  EXPECT_TRUE(binding.ends_as(finalized, State::Closed));
  EXPECT_EQ(jobs_of(speech, JobClass::SttDecode).size(), 1U);

  // A whole utterance scored after the drain began, its final unread
  // holding the drain open, ends nothing.
  const auto unread = binding.open_audio();
  binding.audio(unread, voiced(4'800, 36));
  binding.finalize(unread, 4);
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(unread) == State::Active; }));
  speech.fixture->hold_jobs(JobClass::VadFrames);
  // Within the session's credit, which nothing returns while the lane is held.
  ASSERT_TRUE(binding.audio_in_long_frames(unread, join(voiced(8'000, 37), quiet(6'000, 38))));
  ASSERT_TRUE(binding.manager->apply(unread, Event::Drain, shutdown).has_value());
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(unread, EndpointReason::ClientFinalize, 1, 0, voiced(4'800, 36), 4));
  EXPECT_TRUE(binding.ends_as(unread, State::Closed));
  EXPECT_FALSE(binding.output(unread)->take().has_value());
  EXPECT_EQ(jobs_of(speech, JobClass::SttDecode).size(), 2U);
}

/// Half as fast as the steady clock, so a timed wait for one of its points
/// ends short of it.
class HalfSpeedClock final : public SchedulerClock {
 public:
  [[nodiscard]] TimePoint now() const noexcept override {
    return origin_ + (std::chrono::steady_clock::now() - origin_) / 2;
  }

 private:
  TimePoint origin_ = std::chrono::steady_clock::now();
};

std::unique_ptr<DispatchUnderTest> detecting_on_a_half_speed_clock(
    const SchedulerClock& /*clock*/) {
  static const HalfSpeedClock clock;
  SpeechDispatchOptions options;
  options.job_deadline_base = 20ms;
  options.release_bound = 20ms;
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, std::move(options),
                                           VoiceDetection::On);
}

// Nobody moves the clock or gives notice: the thread waits for the
// voice-activity job's deadline, and then for the release's bound.
TEST(SpeechVoiceDetection, AVoiceActivityJobsDeadlineIsReachedWithoutNotice) {
  Binding binding(&detecting_on_a_half_speed_clock);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(key, voiced(1'024, 39));
  ASSERT_TRUE(speech.stalled(30s));
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "deadline_missed");
}

// Partial transcripts: the open utterance's audio so far, decoded once the
// accumulation and then each stride of it was scored.

/// A decode: its session, and its window's first sample and end.
using Decode = std::tuple<std::uint64_t, std::uint64_t, std::uint64_t>;

std::vector<Decode> decodes_of(const SpeechUnderTest& speech) {
  std::vector<Decode> decodes;
  for (const auto& request : jobs_of(speech, JobClass::SttDecode)) {
    const auto& audio = std::get<AudioFrames>(request.payload());
    decodes.emplace_back(request.identity().session_key, audio.start_sample,
                         audio.start_sample + audio.pcm.byte_size / 2);
  }
  return decodes;
}

/// What the dispatch's decode of [first, end) of a session's `audio` yields.
std::string text_of(const SpeechUnderTest& speech, const Bytes& audio, std::uint64_t first,
                    std::uint64_t end) {
  return speech.transcript(first, samples_of(audio, first, end)).front().text;
}

void expect_partial(Binding& binding, std::uint64_t key, std::uint64_t utterance_id,
                    std::uint32_t revision, const std::string& text) {
  const auto partial = binding.read_as<PartialTranscriptOutput>(key);
  ASSERT_TRUE(partial.has_value());
  EXPECT_EQ(partial->utterance_id, utterance_id);
  EXPECT_EQ(partial->revision, revision);
  EXPECT_EQ(partial->text, text);
}

/// Answers the nth decode with `answer(n)`, from one, and voice activity as
/// the fixture does.
FixtureJobBridge::ResultFunction answering_decodes(
    std::function<Result<JobResult>(std::uint64_t, const JobRequest&, BufferManager&)> answer) {
  auto decodes = std::make_shared<std::uint64_t>(0);
  return [decodes, answer = std::move(answer)](const JobRequest& request,
                                               BufferManager& buffers) -> Result<JobResult> {
    if (request.job_class() != JobClass::SttDecode) {
      return fixture_job_result(request, buffers);
    }
    return answer(++*decodes, request, buffers);
  };
}

Result<JobResult> transcript_reading(std::string text) {
  return JobResult::create(TranscriptResult{std::move(text), {}, {}});
}

std::unique_ptr<DispatchUnderTest> detecting_with_set_texts(const SchedulerClock& clock) {
  auto answer = [](std::uint64_t decode, const JobRequest& request, BufferManager& buffers) {
    switch (decode) {
      case 1:
        return transcript_reading(" \t ");
      case 2:
      case 3:
        return transcript_reading("hello");
      case 4:
        return transcript_reading("hello world");
      default:
        return result_following_input(request, buffers);
    }
  };
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{},
                                           answering_decodes(answer), BufferManagerConfig{},
                                           SpeechDispatchOptions{}, VoiceDetection::On);
}

/// The backend fails the first, second and fifth decode.
Result<JobResult> failing_some_decodes(std::uint64_t decode, const JobRequest& request,
                                       BufferManager& buffers) {
  if (decode == 1 || decode == 2 || decode == 5) {
    return unexpected(Error::make(Error::Code::Internal, "inference failed", "inference_failed"));
  }
  return result_following_input(request, buffers);
}

struct PartialFailures {
  std::mutex mu;
  std::vector<Error> reported;
};

PartialFailures& partial_failures() {
  static PartialFailures failures;
  return failures;
}

std::unique_ptr<DispatchUnderTest> detecting_with_failing_partials(const SchedulerClock& clock) {
  SpeechDispatchOptions options;
  options.on_partial_failed = [](const Error& cause) {
    const std::lock_guard guard(partial_failures().mu);
    partial_failures().reported.push_back(cause);
  };
  return std::make_unique<SpeechUnderTest>(
      clock, FixtureJobBridgeOptions{}, answering_decodes(failing_some_decodes),
      BufferManagerConfig{}, std::move(options), VoiceDetection::On);
}

std::unique_ptr<DispatchUnderTest> detecting_with_failing_partials_unreported(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(
      clock, FixtureJobBridgeOptions{}, answering_decodes(failing_some_decodes),
      BufferManagerConfig{}, SpeechDispatchOptions{}, VoiceDetection::On);
}

TEST(SpeechPartialTranscripts, ComeAtTheAccumulationThenEachStrideAndRestartEachUtterance) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes audio =
      join(join(voiced(15 * kVadFrame, 70), quiet(10 * kVadFrame, 71)), voiced(10 * kVadFrame, 72));
  // Speech from sample 0: its first 320 ms are the first window.
  binding.audio(key, samples_of(audio, 0, 9 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  EXPECT_TRUE(decodes_of(speech).empty());
  binding.audio(key, samples_of(audio, 9 * kVadFrame, 10 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{key, 0, 10 * kVadFrame}}));
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 1, 1, text_of(speech, audio, 0, 10 * kVadFrame)));

  // A frame short of the 160 ms stride, then the stride: the utterance so far.
  binding.audio(key, samples_of(audio, 10 * kVadFrame, 14 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech).size(), 1U);
  binding.audio(key, samples_of(audio, 14 * kVadFrame, 15 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech).back(), (Decode{key, 0, 15 * kVadFrame}));
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 1, 2, text_of(speech, audio, 0, 15 * kVadFrame)));

  // Scored at once with its endpoint, the silence starts no partial.
  binding.audio(key, samples_of(audio, 15 * kVadFrame, 25 * kVadFrame));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::Silence, 1, 0,
                                               samples_of(audio, 0, 25 * kVadFrame), 0));
  binding.audio(key, samples_of(audio, 25 * kVadFrame, 35 * kVadFrame));
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 2, 1, text_of(speech, audio, 25 * kVadFrame, 35 * kVadFrame)));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{key, 0, 10 * kVadFrame},
                                                     {key, 0, 15 * kVadFrame},
                                                     {key, 0, 25 * kVadFrame},
                                                     {key, 25 * kVadFrame, 35 * kVadFrame}}));
}

TEST(SpeechPartialTranscripts, TheAccumulationCountsFromTheFirstSpeechFrameNotThePadding) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes audio = join(quiet(20 * kVadFrame, 73), voiced(10 * kVadFrame, 74));
  ASSERT_TRUE(binding.audio_in_long_frames(key, samples_of(audio, 0, 29 * kVadFrame)));
  ASSERT_TRUE(binding.settle());
  // 320 ms past the onset's padded start, but not past its first speech frame.
  EXPECT_TRUE(decodes_of(speech).empty());
  binding.audio(key, samples_of(audio, 29 * kVadFrame, 30 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  const std::uint64_t onset = 20 * kVadFrame - 2'560;
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{key, onset, 30 * kVadFrame}}));
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 1, 1, text_of(speech, audio, onset, 30 * kVadFrame)));
}

// Split mid-frame at 480,000, the next utterance's speech starts there: 320 ms
// past it falls inside frame 947, so the frame ending at 485,376 makes it due.
TEST(SpeechPartialTranscripts, AfterASplitTheNextUtterancesCountFromTheSplit) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const std::uint64_t scored = 948 * kVadFrame;
  const Bytes audio = voiced(scored, 97);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0,
                                               samples_of(audio, 0, kLongest), 0));
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 2, 1, text_of(speech, audio, kLongest, scored)));
  ASSERT_TRUE(binding.settle());
  std::vector<Decode> after_split;
  for (const auto& decode : decodes_of(speech)) {
    if (std::get<1>(decode) == kLongest) {
      after_split.push_back(decode);
    }
  }
  EXPECT_EQ(after_split, (std::vector<Decode>{{key, kLongest, scored}}));
}

TEST(SpeechPartialTranscripts, StridesPassedWhileOneRunsGiveOneDecodeOfTheLatestAudio) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes audio = voiced(25 * kVadFrame, 75);
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(key, samples_of(audio, 0, 10 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.audio_in_long_frames(key, samples_of(audio, 10 * kVadFrame, 25 * kVadFrame)));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech).size(), 1U);
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech),
            (std::vector<Decode>{{key, 0, 10 * kVadFrame}, {key, 0, 25 * kVadFrame}}));
  // The second took the place of the first, unsent.
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 1, 2, text_of(speech, audio, 0, 25 * kVadFrame)));
}

TEST(SpeechPartialTranscripts, OneWhoseUtteranceEndedWhileItRanIsDropped) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes audio = join(voiced(10 * kVadFrame, 76), quiet(10 * kVadFrame, 77));
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(key, samples_of(audio, 0, 10 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  binding.audio(key, samples_of(audio, 10 * kVadFrame, 20 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  const auto endpoint = binding.read(key);
  ASSERT_TRUE(endpoint.has_value());
  EXPECT_TRUE(std::holds_alternative<EndpointDetectedOutput>(*endpoint));
  const auto transcript = binding.read(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_TRUE(std::holds_alternative<FinalTranscriptOutput>(*transcript));
  EXPECT_FALSE(binding.output(key)->take().has_value());
  EXPECT_EQ(decodes_of(speech),
            (std::vector<Decode>{{key, 0, 10 * kVadFrame}, {key, 0, 20 * kVadFrame}}));
}

TEST(SpeechPartialTranscripts, ADecodeAnEndpointWaitsForGoesBeforeAnotherSessionsPartial) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto first = binding.open_audio();
  const auto second = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(first, voiced(10 * kVadFrame, 78));
  ASSERT_TRUE(binding.settle());
  binding.audio(second, voiced(10 * kVadFrame, 79));
  binding.audio(first, quiet(10 * kVadFrame, 80));
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{first, 0, 10 * kVadFrame},
                                                     {first, 0, 20 * kVadFrame},
                                                     {second, 0, 10 * kVadFrame}}));
}

TEST(SpeechPartialTranscripts, ASessionWhosePartialEndedGoesBehindThoseWaiting) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto first = binding.open_audio();
  const auto second = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(first, voiced(10 * kVadFrame, 81));
  ASSERT_TRUE(binding.settle());
  // Due again while its first runs, then the other session is due.
  binding.audio(first, voiced(5 * kVadFrame, 82));
  ASSERT_TRUE(binding.settle());
  binding.audio(second, voiced(10 * kVadFrame, 83));
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{first, 0, 10 * kVadFrame},
                                                     {second, 0, 10 * kVadFrame},
                                                     {first, 0, 15 * kVadFrame}}));
}

// More audio for a session already waiting on the lane keeps its one place.
TEST(SpeechPartialTranscripts, ASessionDueAgainWhileWaitingKeepsOnePlace) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto first = binding.open_audio();
  const auto second = binding.open_audio();
  // The first session's partial ends but keeps the lane while others queue.
  speech.fixture->hold_job_releases();
  binding.audio(first, voiced(10 * kVadFrame, 98));
  ASSERT_TRUE(binding.settle());
  binding.audio(second, voiced(10 * kVadFrame, 99));
  ASSERT_TRUE(binding.settle());
  binding.audio(second, voiced(5 * kVadFrame, 100));
  ASSERT_TRUE(binding.settle());
  binding.audio(first, voiced(5 * kVadFrame, 101));
  ASSERT_TRUE(binding.settle());
  speech.fixture->hold_jobs(JobClass::SttDecode);
  speech.fixture->let_job_releases_go();
  ASSERT_TRUE(binding.settle());
  // Due again while its decode runs: it queues once that ends, behind the first.
  binding.audio(second, voiced(5 * kVadFrame, 102));
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{first, 0, 10 * kVadFrame},
                                                     {second, 0, 15 * kVadFrame},
                                                     {first, 0, 15 * kVadFrame},
                                                     {second, 0, 20 * kVadFrame}}));
}

TEST(SpeechPartialTranscripts, NoneStartsOnceAFinalizeWaitsForItsFrames) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes audio = voiced(20 * kVadFrame, 84);
  speech.fixture->hold_jobs(JobClass::VadFrames);
  binding.audio(key, samples_of(audio, 0, 10 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  // Scored while the Finalize still waits for these frames.
  binding.audio(key, samples_of(audio, 10 * kVadFrame, 20 * kVadFrame));
  binding.finalize(key, 5);
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, audio, 5));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{key, 0, 20 * kVadFrame}}));
}

// A partial already waiting for the lane when the session starts draining
// does not start: the drain does not end the utterance, so nothing reads it.
TEST(SpeechPartialTranscripts, NoneStartsOnceTheSessionDrains) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto draining = binding.open_audio();
  const auto other = binding.open_audio();
  // Its first utterance's endpoint and final, unread, hold the drain open.
  binding.audio(draining, voiced(10 * kVadFrame, 105));
  ASSERT_TRUE(binding.settle());
  binding.audio(draining, quiet(10 * kVadFrame, 106));
  ASSERT_TRUE(binding.settle());
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(other, voiced(10 * kVadFrame, 107));
  ASSERT_TRUE(binding.settle());
  binding.audio(draining, voiced(10 * kVadFrame, 108));
  ASSERT_TRUE(binding.settle());
  const Error shutdown =
      Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown");
  ASSERT_TRUE(binding.manager->apply(draining, Event::Drain, shutdown).has_value());
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech), (std::vector<Decode>{{draining, 0, 10 * kVadFrame},
                                                     {draining, 0, 20 * kVadFrame},
                                                     {other, 0, 10 * kVadFrame}}));
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(draining).has_value());
  ASSERT_TRUE(binding.read_as<FinalTranscriptOutput>(draining).has_value());
  EXPECT_TRUE(binding.ends_as(draining, State::Closed));
}

TEST(SpeechPartialTranscripts, OnlyANonBlankChangedTextIsOfferedAndRevisesTheLast) {
  Binding binding(&detecting_with_set_texts);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  binding.audio(key, voiced(10 * kVadFrame, 85));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech).size(), 1U);
  EXPECT_FALSE(binding.read(key, 0ms).has_value());
  binding.audio(key, voiced(5 * kVadFrame, 86));
  ASSERT_NO_FATAL_FAILURE(expect_partial(binding, key, 1, 1, "hello"));
  binding.audio(key, voiced(5 * kVadFrame, 87));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(decodes_of(speech).size(), 3U);
  EXPECT_FALSE(binding.read(key, 0ms).has_value());
  binding.audio(key, voiced(5 * kVadFrame, 88));
  ASSERT_NO_FATAL_FAILURE(expect_partial(binding, key, 1, 2, "hello world"));
}

TEST(SpeechPartialTranscripts, OneTheBackendFailsIsDroppedAndReportedOnceAnUtterance) {
  {
    const std::lock_guard guard(partial_failures().mu);
    partial_failures().reported.clear();
  }
  Binding binding(&detecting_with_failing_partials);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes audio =
      join(join(voiced(20 * kVadFrame, 89), quiet(10 * kVadFrame, 90)), voiced(10 * kVadFrame, 91));
  binding.audio(key, samples_of(audio, 0, 10 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  binding.audio(key, samples_of(audio, 10 * kVadFrame, 15 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().partials_failed, 2U);
  binding.audio(key, samples_of(audio, 15 * kVadFrame, 20 * kVadFrame));
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 1, 1, text_of(speech, audio, 0, 20 * kVadFrame)));
  binding.audio(key, samples_of(audio, 20 * kVadFrame, 30 * kVadFrame));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::Silence, 1, 0,
                                               samples_of(audio, 0, 30 * kVadFrame), 0));
  // The next utterance's first partial fails: reported again.
  binding.audio(key, samples_of(audio, 30 * kVadFrame, 40 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  binding.finalize(key, 9);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 2, 30 * kVadFrame,
                           samples_of(audio, 30 * kVadFrame, 40 * kVadFrame), 9));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  const auto stats = speech.strategy->stats();
  EXPECT_EQ(stats.partials_failed, 3U);
  EXPECT_EQ(stats.jobs_failed, 3U);
  const std::lock_guard guard(partial_failures().mu);
  ASSERT_EQ(partial_failures().reported.size(), 2U);
  for (const auto& cause : partial_failures().reported) {
    EXPECT_EQ(cause.context, "inference_failed");
  }
}

TEST(SpeechPartialTranscripts, OneTheBackendFailsIsDroppedWithNobodyToReportTo) {
  Binding binding(&detecting_with_failing_partials_unreported);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  binding.audio(key, voiced(10 * kVadFrame, 92));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().partials_failed, 1U);
  EXPECT_EQ(binding.live_state(key), State::Active);
}

std::unique_ptr<DispatchUnderTest> detecting_with_refused_decodes(const SchedulerClock& clock) {
  auto answer = [](std::uint64_t /*decode*/, const JobRequest& /*request*/,
                   BufferManager& /*buffers*/) -> Result<JobResult> {
    return unexpected(
        Error::make(Error::Code::Unsupported, "language is not permitted", "job_not_permitted"));
  };
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{},
                                           answering_decodes(answer), BufferManagerConfig{},
                                           SpeechDispatchOptions{}, VoiceDetection::On);
}

// The backend would refuse the final too: the session fails at its first job.
TEST(SpeechPartialTranscripts, OneTheBackendRefusesFailsItsSession) {
  Binding binding(&detecting_with_refused_decodes);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  binding.audio(key, voiced(10 * kVadFrame, 103));
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_unsupported_capability");
  EXPECT_EQ(speech.strategy->stats().partials_failed, 0U);
}

// The final would meet the unload too: the session fails at the partial.
TEST(SpeechPartialTranscripts, OneEndedByTheBackendsUnloadFailsItsSession) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(key, voiced(10 * kVadFrame, 109));
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(decodes_of(speech).size(), 1U);
  speech.fixture->unload();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_unavailable");
  const auto stats = speech.strategy->stats();
  EXPECT_EQ(stats.partials_failed, 0U);
  EXPECT_EQ(stats.jobs_failed, 1U);
}

TEST(SpeechPartialTranscripts, OneEndedByTheBackendsResetEndsEverySession) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const auto idle = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(key, voiced(10 * kVadFrame, 104));
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(decodes_of(speech).size(), 1U);
  speech.fixture->lose_backend();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_reset");
  ASSERT_TRUE(binding.ends_as(idle, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(idle)->reason, "backend_reset");
}

TEST(SpeechPartialTranscripts, OnePastItsDeadlineFailsItsSession) {
  Binding binding(&detecting_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(key, voiced(10 * kVadFrame, 93));
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(decodes_of(speech).size(), 1U);
  // 320 ms of audio: 1 s and 160 ms.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1160ms - 1ns));
  EXPECT_EQ(binding.live_state(key), State::Active);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(binding.live_state(key), State::Failed);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "deadline_missed");
}

TEST(SpeechPartialTranscripts, ACancelWaitsForOneRunningAsForAnyJob) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.fixture->hold_jobs(JobClass::SttDecode);
  binding.audio(key, voiced(10 * kVadFrame, 94));
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(decodes_of(speech).size(), 1U);
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::CancelRequested);
  speech.release_jobs();
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// A drain waits for the delivery of the session's output, but a partial
// that gave way unsent is never delivered.
TEST(SpeechPartialTranscripts, ADrainCompletesAfterOneWasReplacedUnsent) {
  Binding binding(&speech_dispatch_detecting_voice);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes audio = voiced(15 * kVadFrame, 95);
  binding.audio(key, samples_of(audio, 0, 10 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  binding.audio(key, samples_of(audio, 10 * kVadFrame, 15 * kVadFrame));
  ASSERT_TRUE(binding.settle());
  ASSERT_NO_FATAL_FAILURE(
      expect_partial(binding, key, 1, 2, text_of(speech, audio, 0, 15 * kVadFrame)));
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_NO_FATAL_FAILURE(binding.expect_endpoint(key, EndpointReason::HalfClose, 1, 0, audio));
  ASSERT_TRUE(binding.settle());
  // The final, unread, still holds the drain open.
  EXPECT_EQ(binding.live_state(key), State::Draining);
  binding.expect_transcript(key, 1, 0, audio, 0);
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SpeechPartialTranscripts, ADrainCompletesAfterAFinalDroppedOneUnsent) {
  Binding binding(&speech_dispatch_detecting_voice);
  const auto key = binding.open_audio();
  const Bytes audio = voiced(10 * kVadFrame, 96);
  binding.audio(key, audio);
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_TRUE(binding.settle());
  const auto endpoint = binding.read(key);
  ASSERT_TRUE(endpoint.has_value());
  EXPECT_TRUE(std::holds_alternative<EndpointDetectedOutput>(*endpoint));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);
  const auto transcript = binding.read(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_TRUE(std::holds_alternative<FinalTranscriptOutput>(*transcript));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}
}  // namespace
}  // namespace tensorplate::testing
