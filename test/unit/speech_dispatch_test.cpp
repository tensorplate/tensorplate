// SPDX-License-Identifier: Apache-2.0
#include "serving/session/speech_dispatch.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <variant>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

#include "fake_scheduler_clock.hpp"
#include "fixture_job_bridge.hpp"
#include "serving/session/dispatch.hpp"
#include "session_dispatch_binding.hpp"
#include "speech_dispatch_binding.hpp"

// What the dispatch contract's cases leave to the speech dispatch: its jobs,
// its one lane, what it makes of each outcome a bridge may give, and where
// every buffer goes.

namespace tensorplate::testing {
namespace {
using namespace std::chrono_literals;
using namespace tensorplate::serving;
using Binding = ReferenceBinding;
using Event = LogicalSessionEvent;
using State = LogicalSessionState;
using Bytes = std::vector<std::byte>;

/// 20 ms of 16 kHz mono 16-bit PCM.
constexpr std::size_t kFrameBytes = 640;

JobRequest foreign_synthesis(std::uint64_t job_id) {
  return JobRequest::create({job_id, 999, SpeechUnderTest::kGeneration}, JobClass::TtsSynthesis,
                            TextSegment{"hi"}, JobOptions{"en", "voice-a", 1000})
      .value();
}

/// An utterance of one frame, ended by a Finalize: one decode.
void one_utterance(Binding& binding, std::uint64_t key) {
  binding.audio(key, patterned_bytes(kFrameBytes, 1));
  binding.finalize(key, 1);
}

std::string refusal(const SessionDispatch& dispatch, const SessionRequest& request) {
  const auto refused = dispatch.negotiate(request);
  if (refused.has_value()) {
    return "<granted>";
  }
  EXPECT_EQ(refused.error().code, Error::Code::Unsupported);
  return refused.error().context.value_or("");
}

template <typename Value>
std::string context_of(const Result<Value>& result) {
  return result.has_value() ? "<success>" : result.error().context.value_or("");
}

TEST(SpeechJobDispatch, AnUtteranceIsOneDecodeAndASegmentOneSynthesis) {
  Binding binding(&speech_dispatch_with_fixture_results);
  SpeechUnderTest& speech = speech_of(binding);
  const auto audio_key = binding.open_audio();
  const Bytes audio = patterned_bytes(3 * kFrameBytes, 1);
  binding.audio(audio_key, audio);
  binding.finalize(audio_key, 4);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(audio_key).has_value());
  const auto transcript = binding.read_as<FinalTranscriptOutput>(audio_key);
  ASSERT_TRUE(transcript.has_value());
  ASSERT_EQ(transcript->segments.size(), 1U);
  EXPECT_EQ(transcript->segments.front().text, "hello");
  EXPECT_EQ(transcript->segments.front().start_us, 0U);
  EXPECT_EQ(transcript->segments.front().end_us, 60'000U);
  EXPECT_EQ(transcript->finalize_sequence, 4U);
  binding.audio(audio_key, patterned_bytes(kFrameBytes, 2));
  binding.finalize(audio_key, 5);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(audio_key).has_value());
  ASSERT_TRUE(binding.read_as<FinalTranscriptOutput>(audio_key).has_value());

  const auto text_key = binding.open_text();
  binding.text(text_key, 7, "hi");
  const auto chunk = binding.read_as<AudioChunkOutput>(text_key);
  ASSERT_TRUE(chunk.has_value());
  EXPECT_EQ(chunk->pcm, Bytes(2 * kFixtureSpeechBytesPerTextByte, std::byte{0}));
  const auto completed = binding.read_as<SegmentCompletedOutput>(text_key);
  ASSERT_TRUE(completed.has_value());
  EXPECT_EQ(completed->total_samples, kFixtureSpeechBytesPerTextByte);
  EXPECT_EQ(completed->chunk_count, 1U);
  ASSERT_TRUE(binding.settle());

  const auto submitted = speech.bridge->submitted();
  ASSERT_EQ(submitted.size(), 3U);
  EXPECT_EQ(submitted[0].job_class(), JobClass::SttDecode);
  EXPECT_EQ(submitted[0].identity().session_key, audio_key);
  EXPECT_EQ(submitted[0].identity().generation, SpeechUnderTest::kGeneration);
  EXPECT_EQ(submitted[0].options(), (JobOptions{"en", "", 0}));
  EXPECT_EQ(submitted[0].progress_limit(), 0U);
  const auto& first = std::get<AudioFrames>(submitted[0].payload());
  EXPECT_EQ(first.pcm.byte_offset, 0U);
  EXPECT_EQ(first.pcm.byte_size, audio.size());
  EXPECT_EQ(first.start_sample, 0U);
  // The session's timeline goes on across utterances.
  EXPECT_EQ(std::get<AudioFrames>(submitted[1].payload()).start_sample, audio.size() / 2);
  EXPECT_NE(submitted[1].identity().job_id, submitted[0].identity().job_id);
  EXPECT_EQ(submitted[2].job_class(), JobClass::TtsSynthesis);
  EXPECT_EQ(submitted[2].identity().session_key, text_key);
  EXPECT_EQ(submitted[2].options(), (JobOptions{"en-US", "voice-a", 1000}));
  EXPECT_EQ(submitted[2].progress_limit(), 0U);
  EXPECT_EQ(std::get<TextSegment>(submitted[2].payload()).text, "hi");

  // An utterance's buffer went back at its job's release, the speech's once cut.
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.bridge->health_calls(), 0U);
  const auto stats = speech.strategy->stats();
  EXPECT_EQ(stats.jobs_submitted, 3U);
  EXPECT_EQ(stats.jobs_completed, 3U);
  EXPECT_EQ(stats.jobs_failed, 0U);
  EXPECT_EQ(stats.jobs_discarded, 0U);
}

TEST(SpeechJobDispatch, CreateRefusesWhatItCannotServeThrough) {
  const FakeSchedulerClock clock;
  const auto buffers = BufferManager::create({}).value();
  EXPECT_EQ(context_of(SpeechJobDispatch::create(nullptr, *buffers, clock)), "job_bridge_null");
  const auto bridge = std::make_shared<FixtureJobBridge>(*buffers);
  EXPECT_EQ(context_of(SpeechJobDispatch::create(bridge, *buffers, clock, {0})), "generation_zero");
  // The refusals above left the bridge its sink to give.
  const auto first = SpeechJobDispatch::create(bridge, *buffers, clock);
  ASSERT_TRUE(first.has_value());
  EXPECT_EQ(context_of(SpeechJobDispatch::create(bridge, *buffers, clock)),
            "event_sink_already_set");
}

TEST(SpeechJobDispatch, NegotiateRefusesWhatAJobRequestCouldNotCarry) {
  Binding binding(&speech_dispatch);
  const SessionDispatch& dispatch = binding.dispatch();
  const auto audio_request = binding.under_test->audio_request();
  const auto text_request = binding.under_test->text_request();

  const auto audio = dispatch.negotiate(audio_request);
  ASSERT_TRUE(audio.has_value());
  EXPECT_EQ(audio->audio_format, audio_request.input_format);
  EXPECT_EQ(audio->min_frame, 20ms);
  EXPECT_EQ(audio->max_frame, 320ms);
  EXPECT_EQ(audio->max_utterance, 30s);
  const auto text = dispatch.negotiate(text_request);
  ASSERT_TRUE(text.has_value());
  EXPECT_EQ(text->audio_format, (StreamAudioFormat{StreamAudioEncoding::PcmS16Le, 24'000, 1}));
  EXPECT_EQ(text->voice, "voice-a");
  EXPECT_EQ(text->max_segment_text_bytes, 4'096U);
  EXPECT_EQ(text->max_segment_audio, 30s);
  EXPECT_EQ(text->max_synthesis_text_bytes, 12'288U);
  EXPECT_EQ(text->max_synthesis_audio, 90s);

  auto request = audio_request;
  request.input_format = {StreamAudioEncoding::Mulaw, 8'000, 1};
  EXPECT_EQ(refusal(dispatch, request), "unsupported_audio_format");
  request = audio_request;
  request.language = "";
  EXPECT_EQ(refusal(dispatch, request), "unsupported_language");
  request = text_request;
  request.language = "a language tag that is too long";
  EXPECT_EQ(refusal(dispatch, request), "unsupported_language");
  request = text_request;
  request.voice = "";
  EXPECT_EQ(refusal(dispatch, request), "unsupported_voice");
  request.voice = "a voice";
  EXPECT_EQ(refusal(dispatch, request), "unsupported_voice");
  // Any tag and any voice a job can carry is granted: the backend decides.
  request.language = "ar";
  request.voice = "af_heart";
  EXPECT_EQ(refusal(dispatch, request), "<granted>");
  // A session that takes audio names no voice.
  request = audio_request;
  request.voice = "";
  EXPECT_EQ(refusal(dispatch, request), "<granted>");

  const FakeSchedulerClock clock;
  const auto buffers = BufferManager::create({}).value();
  const auto text_only = SpeechJobDispatch::create(std::make_shared<FixtureJobBridge>(*buffers),
                                                   *buffers, clock, {1, false, true})
                             .value();
  EXPECT_EQ(refusal(*text_only, audio_request), "unsupported_input_kind");
  EXPECT_EQ(refusal(*text_only, text_request), "<granted>");
  const auto audio_only = SpeechJobDispatch::create(std::make_shared<FixtureJobBridge>(*buffers),
                                                    *buffers, clock, {1, true, false})
                              .value();
  EXPECT_EQ(refusal(*audio_only, text_request), "unsupported_input_kind");
  EXPECT_EQ(refusal(*audio_only, audio_request), "<granted>");
}

TEST(SpeechJobDispatch, EverySessionOpenedIsReleasedThroughTheBridgeOnce) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto idle = binding.open_audio();
  const auto worked = binding.open_text();
  binding.text(worked, 1, "hi");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(worked, 1, "hi"));
  for (const auto key : {idle, worked}) {
    ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
    EXPECT_TRUE(binding.ends_as(key, State::Closed));
  }

  // A session that was never opened here is not the bridge's to release.
  const auto terms = binding.dispatch().negotiate(binding.under_test->audio_request()).value();
  const auto refused = binding.manager->open(
      Binding::kGeneration, budgets_for(terms).value(), [](std::uint64_t) -> Result<void> {
        return unexpected(
            Error::make(Error::Code::Unavailable, "backend is gone", "backend_unavailable"));
      });
  ASSERT_FALSE(refused.has_value());
  const std::uint64_t never_opened = worked + 1;
  EXPECT_TRUE(binding.ends_as(never_opened, State::Failed));

  const auto open_at_stop = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->releases(open_at_stop), 0U);
  binding.dispatch().stop();
  binding.dispatch().stop();
  EXPECT_EQ(speech.bridge->releases(idle), 1U);
  EXPECT_EQ(speech.bridge->releases(worked), 1U);
  EXPECT_EQ(speech.bridge->releases(never_opened), 0U);
  EXPECT_EQ(speech.bridge->releases(open_at_stop), 1U);
}

TEST(SpeechJobDispatch, AReleaseTheBridgeRefusesLeavesTheSlotHeld) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.manager->held_slots(), 1U);
  EXPECT_FALSE(binding.manager->tombstone(key).has_value());
  EXPECT_EQ(speech.bridge->releases(key), 1U);
  EXPECT_EQ(speech.strategy->stats().releases_refused, 1U);
}

/// Another submitter fills the backend, so the session's one job finds no
/// room: refused by submit() itself or ended by `failed`, as the backend
/// chooses.
void capacity_fails_the_session(DispatchFactory factory) {
  Binding binding(factory);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  speech.hold_jobs();
  for (std::uint64_t job = 0; job < 1 + FixtureJobBridge::kWaitingJobs; ++job) {
    ASSERT_TRUE(speech.fixture->submit(foreign_synthesis(1000 + job)).has_value());
  }
  one_utterance(binding, key);
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  const auto tombstone = binding.manager->tombstone(key);
  EXPECT_EQ(tombstone->code, Error::Code::ResourceExhausted);
  EXPECT_EQ(tombstone->reason, "job_capacity_exhausted");
  EXPECT_EQ(binding.manager->held_slots(), 0U);

  // The other submitter's results reach this sink too: each is dropped, and
  // its audio released, as a late result of a job of its own would be.
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.strategy->stats().jobs_completed, 0U);
}

TEST(SpeechJobDispatch, CapacityRefusedByAFailedJobFailsTheSession) {
  ASSERT_NO_FATAL_FAILURE(capacity_fails_the_session(&speech_dispatch));
}

TEST(SpeechJobDispatch, CapacityRefusedBySubmitFailsTheSessionTheSameWay) {
  ASSERT_NO_FATAL_FAILURE(capacity_fails_the_session(&speech_dispatch_refusing_at_submit));
}

/// A session is cancelled while its job runs. The backend ends the job as
/// cancelled, or lets it complete: either way its result is dropped, and the
/// lane is the next session's only once the job has ended.
void a_cancelled_job_keeps_the_lane_until_it_ends(DispatchFactory factory) {
  Binding binding(factory);
  SpeechUnderTest& speech = speech_of(binding);
  const auto cancelled = binding.open_text();
  const auto next = binding.open_text();
  speech.hold_jobs();
  binding.text(cancelled, 1, "hello");
  ASSERT_TRUE(binding.settle());
  binding.text(next, 1, "hi");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  ASSERT_TRUE(binding.manager->apply(cancelled, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  EXPECT_EQ(speech.bridge->releases(cancelled), 1U);

  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(next, 1, "hi"));
  EXPECT_TRUE(binding.ends_as(cancelled, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(cancelled)->take().has_value());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.strategy->stats().jobs_discarded, 1U);
  EXPECT_EQ(speech.strategy->stats().jobs_completed, 1U);
}

TEST(SpeechJobDispatch, ACancelledJobThatFailsKeepsTheLaneUntilItEnds) {
  ASSERT_NO_FATAL_FAILURE(a_cancelled_job_keeps_the_lane_until_it_ends(&speech_dispatch));
}

TEST(SpeechJobDispatch, ACancelledJobThatCompletesIsDroppedAndItsAudioReleased) {
  ASSERT_NO_FATAL_FAILURE(
      a_cancelled_job_keeps_the_lane_until_it_ends(&speech_dispatch_with_late_cancels));
}

TEST(SpeechJobDispatch, SessionsAreServedInTheOrderTheirWorkBecameReady) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto first = binding.open_text();
  const auto second = binding.open_audio();
  const auto third = binding.open_text();
  speech.hold_jobs();
  binding.text(first, 1, "one");
  ASSERT_TRUE(binding.settle());
  one_utterance(binding, second);
  ASSERT_TRUE(binding.settle());
  binding.text(third, 1, "three");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);

  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(third, 1, "three"));
  const auto submitted = speech.bridge->submitted();
  ASSERT_EQ(submitted.size(), 3U);
  EXPECT_EQ(submitted[0].identity().session_key, first);
  EXPECT_EQ(submitted[1].identity().session_key, second);
  EXPECT_EQ(submitted[2].identity().session_key, third);
}

TEST(SpeechJobDispatch, ABackendLostUnderAJobFailsItsSessionAsBackendReset) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.hold_jobs();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.settle());
  speech.fixture->lose_backend();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_reset");
  // The lost backend holds nothing: its refusal of the release is a release.
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  EXPECT_EQ(speech.bridge->releases(key), 1U);
  ASSERT_TRUE(binding.settle());
  // The job is released only by the unload, and its input is kept until then.
  EXPECT_EQ(speech.buffers_in_use(), 1U);
  speech.fixture->unload();
  EXPECT_EQ(speech.buffers_in_use(), 0U);
}

TEST(SpeechJobDispatch, ABackendLostWithNoJobOpenIsLearnedAtTheNextSubmit) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  speech.fixture->lose_backend();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);

  one_utterance(binding, key);
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_unavailable");
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.strategy->stats().jobs_submitted, 0U);
}

TEST(SpeechJobDispatch, AnUnloadUnderAJobFailsItsSessionAsBackendUnavailable) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  speech.fixture->unload();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->code, Error::Code::Unavailable);
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_unavailable");
  EXPECT_EQ(binding.manager->held_slots(), 0U);
}

TEST(SpeechJobDispatch, AJobTheBackendRefusesOrFailsEndsItsSessionWithTheStreamsReason) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto refused = binding.open_text();
  speech.fixture->fail_next_job(
      Error::make(Error::Code::Unsupported, "voice is not permitted", "job_not_permitted"));
  binding.text(refused, 1, "hello");
  ASSERT_TRUE(binding.ends_as(refused, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(refused)->code, Error::Code::Unsupported);
  EXPECT_EQ(binding.manager->tombstone(refused)->reason, "backend_unsupported_capability");

  // Any other failure keeps the backend's own code and reason.
  const auto failed = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  speech.fixture->fail_next_job(
      Error::make(Error::Code::Timeout, "the decode ran out of time", "decode_timeout"));
  one_utterance(binding, failed);
  ASSERT_TRUE(binding.ends_as(failed, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(failed)->code, Error::Code::Timeout);
  EXPECT_EQ(binding.manager->tombstone(failed)->reason, "decode_timeout");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.strategy->stats().jobs_failed, 2U);
}

TEST(SpeechJobDispatch, BackpressureKeepsTheSpeechInItsBufferAndAnAbortReleasesIt) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  const auto queue = binding.output(key);
  const std::size_t chunk = 960;
  const std::string text(queue->pcm_usage().limit() / 160 + 64, 'x');
  ASSERT_LE(text.size(), binding.terms(key).max_segment_text_bytes);
  binding.text(key, 1, text);
  ASSERT_TRUE(binding.eventually([&] { return queue->pcm_usage().available() < chunk; }));
  ASSERT_TRUE(binding.settle());
  // What the queue has no room for is still in the backend's buffer.
  EXPECT_EQ(speech.buffers_in_use(), 1U);
  const auto queued = queue->pcm_usage().used();
  // A delivery that made no room for a chunk changes nothing.
  binding.dispatch().output_delivered(key, OutputKind::Control);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(queue->pcm_usage().used(), queued);

  // A reader that makes no progress is ended through the manager.
  ASSERT_TRUE(binding.manager
                  ->apply(key, Event::Abort,
                          Error::make(Error::Code::ResourceExhausted,
                                      "output made no delivery progress", "slow_consumer"))
                  .has_value());
  ASSERT_TRUE(binding.eventually([&] { return binding.manager->held_slots() == 0; }));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.bridge->releases(key), 1U);
}

TEST(SpeechJobDispatch, AJobsInputIsReleasedWhenTheBackendReportsItAlsoAfterStop) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto decoding = binding.open_audio();
  const auto idle = binding.open_text();
  speech.hold_jobs();
  one_utterance(binding, decoding);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 1U);
  ASSERT_TRUE(binding.manager->apply(decoding, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  // Cancelled, and the backend still reads the utterance.
  EXPECT_EQ(speech.buffers_in_use(), 1U);

  binding.dispatch().stop();
  // Released at its cancel, so not again at the stop; the idle one there.
  EXPECT_EQ(speech.bridge->releases(decoding), 1U);
  EXPECT_EQ(speech.bridge->releases(idle), 1U);
  speech.release_jobs();
  ASSERT_TRUE(speech.fixture->settled(Binding::kPatience));
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  // Nothing is reported after the stop: both slots end with the manager.
  EXPECT_EQ(binding.manager->held_slots(), 2U);
}

TEST(SpeechJobDispatch, SpeechThatArrivesAfterStopIsReleased) {
  Binding binding(&speech_dispatch_with_late_cancels);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  binding.dispatch().stop();
  EXPECT_EQ(speech.bridge->releases(key), 1U);
  // The backend lets the job complete: its audio reaches a stopped dispatch.
  speech.release_jobs();
  ASSERT_TRUE(speech.fixture->settled(Binding::kPatience));
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

Result<JobResult> speech_in_a_buffer_nobody_holds(const JobRequest& /*request*/,
                                                  BufferManager& buffers) {
  const auto buffer = buffers.allocate(320).value();
  (void)buffers.release(buffer);
  return JobResult::create(AudioChunkResult{kJobOutputAudioFormat, PcmWindow{buffer, 0, 320}, 0});
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_with_unreadable_speech(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{},
                                           speech_in_a_buffer_nobody_holds);
}

TEST(SpeechJobDispatch, SpeechInABufferThatCannotBeReadFailsTheSession) {
  Binding binding(&speech_dispatch_with_unreadable_speech);
  const auto key = binding.open_text();
  binding.text(key, 1, "hi");
  EXPECT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}
}  // namespace
}  // namespace tensorplate::testing
