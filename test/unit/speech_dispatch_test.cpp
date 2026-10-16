// SPDX-License-Identifier: Apache-2.0
#include "serving/session/speech_dispatch.hpp"

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <future>
#include <memory>
#include <string>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

#include "fake_scheduler_clock.hpp"
#include "fixture_job_bridge.hpp"
#include "serving/session/dispatch.hpp"
#include "serving/session/timers.hpp"
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
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;
using State = LogicalSessionState;
using Bytes = std::vector<std::byte>;

/// 20 ms of 16 kHz mono 16-bit PCM.
constexpr std::size_t kFrameBytes = 640;

/// A job of a submitter other than the dispatch, on the same bridge.
JobRequest foreign_synthesis(std::uint64_t job_id, std::uint32_t progress_limit = 0) {
  return JobRequest::create({job_id, 999, SpeechUnderTest::kGeneration}, JobClass::TtsSynthesis,
                            TextSegment{"hi"}, JobOptions{"en", "voice-a", 1000}, progress_limit)
      .value();
}

/// An utterance of one frame, ended by a Finalize: one decode.
void one_utterance(Binding& binding, std::uint64_t key) {
  binding.audio(key, patterned_bytes(kFrameBytes, 1));
  binding.finalize(key, 1);
}

/// The audio of the longest utterance granted: the dispatch ends it itself.
Bytes utterance_at_the_limit(Binding& binding, std::uint64_t key, unsigned seed) {
  const auto terms = binding.terms(key);
  return patterned_bytes(static_cast<std::size_t>(terms.max_utterance.count()) *
                             terms.audio_format.bytes_per_second() / 1000,
                         seed);
}

std::string refusal(const SessionDispatch& dispatch, const SessionRequest& request) {
  const auto refused = dispatch.negotiate(request);
  if (refused.has_value()) {
    return "<granted>";
  }
  EXPECT_EQ(refused.error().code, Error::Code::Unsupported);
  return refused.error().context.value_or("");
}

SpeechDispatchOptions options_for(std::uint64_t generation, bool audio_input, bool text_input) {
  SpeechDispatchOptions options;
  options.generation = generation;
  options.audio_input = audio_input;
  options.text_input = text_input;
  return options;
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

// A bridge may run a job's callbacks before its submit returns. The job's
// input is held by then, so the `released` that overtakes the return finds it.
TEST(SpeechJobDispatch, AJobWhoseEventsOvertakeItsSubmitHasItsInputReleased) {
  Binding binding(&speech_dispatch_with_callbacks_first);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  ASSERT_TRUE(binding.read_as<FinalTranscriptOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.strategy->stats().jobs_completed, 1U);
}

TEST(SpeechJobDispatch, CreateRefusesWhatItCannotServeThrough) {
  const FakeSchedulerClock clock;
  const auto buffers = BufferManager::create({}).value();
  EXPECT_EQ(context_of(SpeechJobDispatch::create(nullptr, *buffers, clock)), "job_bridge_null");
  const auto bridge = std::make_shared<FixtureJobBridge>(*buffers);
  EXPECT_EQ(
      context_of(SpeechJobDispatch::create(bridge, *buffers, clock, options_for(0, true, true))),
      "generation_zero");
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
                                                   *buffers, clock, options_for(1, false, true))
                             .value();
  EXPECT_EQ(refusal(*text_only, audio_request), "unsupported_input_kind");
  EXPECT_EQ(refusal(*text_only, text_request), "<granted>");
  const auto audio_only = SpeechJobDispatch::create(std::make_shared<FixtureJobBridge>(*buffers),
                                                    *buffers, clock, options_for(1, true, false))
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

TEST(SpeechJobDispatch, AReleaseTheBridgeRefusesStillReleasesTheSpeechTheSessionHeld) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  const auto queue = binding.output(key);
  binding.text(key, 1, std::string(queue->pcm_usage().limit() / 160 + 64, 'x'));
  ASSERT_TRUE(binding.eventually([&] { return queue->pcm_usage().available() < 960; }));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 1U);
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.manager->held_slots(), 1U);
  EXPECT_EQ(speech.buffers_in_use(), 0U);
}

TEST(SpeechJobDispatch, ASessionThatEndedWhileItWaitedForTheLaneIsNotServed) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto running = binding.open_text();
  const auto waiting = binding.open_text();
  speech.hold_jobs();
  binding.text(running, 1, "one");
  ASSERT_TRUE(binding.settle());
  binding.text(waiting, 1, "two");
  ASSERT_TRUE(binding.settle());
  // Its release refused, the session stays, ended, where the lane finds it.
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(waiting, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(running, 1, "one"));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  EXPECT_EQ(speech.strategy->stats().sessions, 2U);
}

TEST(SpeechJobDispatch, WithoutVoiceDetectionADecodeWaitsForTheOutputBeforeIt) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const auto queue = binding.output(key);
  // A lifecycle message the peer has not read leaves the first answer no room.
  OutputItem unread{
      OutputKind::Control,
      SessionBudgets::kOutputMetadataBytes - SessionBudgets::kOutputControlReserveBytes, 0,
      std::make_unique<OutputBody>()};
  ASSERT_EQ(queue->offer(unread, binding.clock.now()).value(), OutputOffer::Queued);
  const Bytes first = patterned_bytes(kFrameBytes, 1);
  binding.audio(key, first);
  binding.finalize(key, 1);
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));
  const Bytes second = patterned_bytes(kFrameBytes, 2);
  binding.audio(key, second);
  binding.finalize(key, 2);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);

  const auto lifecycle = queue->take();
  ASSERT_TRUE(lifecycle.has_value());
  ASSERT_TRUE(queue->delivered(lifecycle->sequence, binding.clock.now()));
  binding.dispatch().output_delivered(key, OutputKind::Control);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, first, 1));
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 2, first.size() / 2, second, 2));
  EXPECT_EQ(speech.bridge->submitted().size(), 2U);
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

  // The job that found no room left the lane free.
  const auto next = binding.open_audio();
  one_utterance(binding, next);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(next).has_value());
  EXPECT_TRUE(binding.read_as<FinalTranscriptOutput>(next).has_value());
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

// The lane frees between a waiting session's failure and its cleanup: the
// session is still there, with its segment, and is not served.
TEST(SpeechJobDispatch, ASessionThatFailedWhileItWaitedForTheLaneIsNotServed) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto running = binding.open_text();
  const auto waiting = binding.open_text();
  speech.hold_jobs();
  binding.text(running, 1, "one");
  ASSERT_TRUE(binding.settle());
  binding.text(waiting, 1, "two");
  ASSERT_TRUE(binding.settle());

  // The initialize hook holds the manager's serialized section. Input of the
  // wrong kind parks the thread on its credit, and the running job's end
  // queues behind it, ahead of the cleanup the failure will ask for.
  const auto terms = binding.dispatch().negotiate(binding.under_test->audio_request()).value();
  bool ended_behind = false;
  const auto opened = binding.manager->open(
      Binding::kGeneration, budgets_for(terms).value(), [&](std::uint64_t other) {
        speech.strategy->audio(waiting, patterned_bytes(kFrameBytes, 1));
        speech.release_jobs();
        ended_behind = speech.fixture->settled(Binding::kPatience);
        return speech.strategy->open_session(other, terms);
      });
  ASSERT_TRUE(opened.has_value());
  ASSERT_TRUE(ended_behind);
  ASSERT_TRUE(binding.ends_as(waiting, State::Failed));
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(running, 1, "one"));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
}

/// The job's end reaches the dispatch ahead of its session's cancel, where the
/// cases above force it behind: the result is taken, the cancel then drops
/// what it yielded, no other job is submitted, and the session is released
/// once all the same.
void a_job_that_ends_ahead_of_its_sessions_cancel(Binding& binding, std::uint64_t key) {
  SpeechUnderTest& speech = speech_of(binding);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);
  bool ended_first = false;
  // Inside the cancel, before the dispatch is shown it.
  binding.before_next(key, Event::Cancel, [&] {
    speech.release_jobs();
    ended_first = speech.fixture->settled(Binding::kPatience);
  });
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(ended_first);
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
  EXPECT_EQ(speech.bridge->releases(key), 1U);
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.strategy->stats().jobs_completed, 1U);
  EXPECT_EQ(speech.strategy->stats().jobs_discarded, 0U);
}

TEST(SpeechJobDispatch, ASynthesisThatEndsAheadOfItsSessionsCancelIsTakenAndItsAudioReleased) {
  Binding binding(&speech_dispatch);
  const auto key = binding.open_text();
  speech_of(binding).hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_NO_FATAL_FAILURE(a_job_that_ends_ahead_of_its_sessions_cancel(binding, key));
}

TEST(SpeechJobDispatch, ADecodeThatEndsAheadOfItsSessionsCancelIsTakenAndItsFinalIsNotQueued) {
  Binding binding(&speech_dispatch);
  const auto key = binding.open_audio();
  speech_of(binding).hold_jobs();
  one_utterance(binding, key);
  ASSERT_NO_FATAL_FAILURE(a_job_that_ends_ahead_of_its_sessions_cancel(binding, key));
}

// The first utterance's final meets a queue the cancel has suppressed, which
// is where the dispatch learns that the session takes no more output: the
// utterance behind it is not submitted, though the cleanup is still to come.
TEST(SpeechJobDispatch, ADecodeThatEndsAheadOfItsSessionsCancelStartsNoJobForTheUtteranceBehindIt) {
  Binding binding(&speech_dispatch);
  const auto key = binding.open_audio();
  speech_of(binding).hold_jobs();
  // Ended by the limit: the session still takes the audio of a second one.
  ASSERT_TRUE(binding.audio_in_long_frames(key, utterance_at_the_limit(binding, key, 5)));
  one_utterance(binding, key);
  ASSERT_NO_FATAL_FAILURE(a_job_that_ends_ahead_of_its_sessions_cancel(binding, key));
}

TEST(SpeechJobDispatch, TheLongestReleaseWaitIsWhatACancelCostBehindTheJobInFlight) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto behind_its_job = binding.open_text();
  const auto idle = binding.open_audio();
  speech.hold_jobs();
  binding.text(behind_its_job, 1, "hello");
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.manager->apply(behind_its_job, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().longest_release_wait, 0ms);
  // By the manager's clock, which only the test moves.
  binding.clock.advance(70ms);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(behind_its_job, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().longest_release_wait, 70ms);

  // A shorter wait leaves the longest standing.
  ASSERT_TRUE(binding.manager->apply(idle, Event::Cancel).has_value());
  ASSERT_TRUE(binding.ends_as(idle, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().longest_release_wait, 70ms);
}

// A dispatch is shown a Finalize after the manager applied it, so the
// completion it applies for an endpoint's finalization may already have
// answered a Finalize it has not been shown.
TEST(SpeechJobDispatch, ACompletionAppliedBeforeAFinalizeWasShownAnswersIt) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const auto terms = binding.terms(key);
  const Bytes audio = patterned_bytes(static_cast<std::size_t>(terms.max_utterance.count()) *
                                          terms.audio_format.bytes_per_second() / 1000,
                                      5);
  speech.hold_jobs();
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Finalizing; }));
  ASSERT_TRUE(binding.settle());

  // The endpoint's job ends inside the Finalize, before the dispatch is shown it.
  bool ended_first = false;
  binding.before_next(key, Event::Finalize, [&] {
    speech.release_jobs();
    ended_first = speech.fixture->settled(Binding::kPatience);
  });
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_TRUE(ended_first);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0, audio, 0));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);

  binding.dispatch().finalize(key, 7);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               audio.size() / terms.audio_format.bytes_per_sample(),
                                               {}, 7));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
}

// A lifecycle message is delivered through the same queue and reported the
// same way; it is not the segment's output.
TEST(SpeechJobDispatch, OnlyDeliveredTaskOutputReturnsASegmentsActiveSlot) {
  Binding binding(&speech_dispatch);
  const auto key = binding.open_text();
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.lifecycle(key, 16));
  binding.text(key, 1, "hi");
  ASSERT_TRUE(binding.settle());
  // The speech went back to the pool when its last chunk was cut, unread.
  EXPECT_EQ(speech_of(binding).buffers_in_use(), 0U);
  const std::size_t chunks = (binding.under_test->speech("hi").size() + 959) / 960;
  for (std::size_t chunk = 0; chunk < chunks; ++chunk) {
    ASSERT_TRUE(binding.read_as<AudioChunkOutput>(key).has_value());
  }
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.lifecycle_read(key), 1U);
  // Its completion is still to be delivered.
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 1U);
  ASSERT_TRUE(binding.read_as<SegmentCompletedOutput>(key).has_value());
  EXPECT_TRUE(binding.eventually(
      [&] { return binding.manager->status(key).value().input_queue_depth == 0; }));
}

TEST(SpeechJobDispatch, AnotherSubmittersJobEndingLeavesTheLaneToTheJobInFlight) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto running = binding.open_text();
  const auto waiting = binding.open_text();
  speech.hold_jobs();
  binding.text(running, 1, "one");
  ASSERT_TRUE(binding.settle());
  binding.text(waiting, 1, "two");
  ASSERT_TRUE(binding.settle());
  // A waiting job ends at its cancel, while the dispatch's own is in flight.
  ASSERT_TRUE(speech.fixture->submit(foreign_synthesis(1000)).has_value());
  ASSERT_TRUE(speech.fixture->cancel(1000).has_value());
  ASSERT_TRUE(speech.fixture->release_session(999, SpeechUnderTest::kGeneration).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  EXPECT_EQ(speech.strategy->stats().jobs_discarded, 1U);
  EXPECT_EQ(speech.strategy->stats().sessions, 2U);

  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(running, 1, "one"));
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(waiting, 1, "two"));
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_over_a_backend_with_progress(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock,
                                           FixtureJobBridgeOptions{.progress_when_allowed = true});
}

// The dispatch's jobs ask for no progress, and a bridge delivers none that
// was not asked for. Another submitter's job may ask: its fragment reaches
// this sink as its result does, in a buffer of its own.
TEST(SpeechJobDispatch, AProgressFragmentIsDroppedAndItsAudioReleased) {
  Binding binding(&speech_dispatch_over_a_backend_with_progress);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(speech.fixture->submit(foreign_synthesis(1000, 1)).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_TRUE(speech.fixture->refusals().empty());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(speech.strategy->stats().jobs_discarded, 1U);

  // A job of the dispatch's own allows none, so it gets none.
  binding.text(key, 1, "hi");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, "hi"));
}

TEST(SpeechJobDispatch, ABackendLostWhileACancelWaitsForItsReleaseFailsTheSession) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::CancelRequested);

  speech.fixture->lose_backend();
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Failed; }));
  ASSERT_TRUE(binding.settle());
  // The release it asked for is confirmed only by the unload.
  EXPECT_EQ(binding.manager->held_slots(), 1U);
  speech.fixture->unload();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_reset");
  EXPECT_EQ(speech.bridge->releases(key), 1U);
}

// The job's failure reaches the dispatch ahead of the cancel, for a session
// the manager already holds as cancel-requested: it still ends as failed.
TEST(SpeechJobDispatch, ABackendLostAheadOfItsSessionsCancelFailsTheSession) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  bool failed_first = false;
  binding.before_next(key, Event::Cancel, [&] {
    speech.fixture->lose_backend();
    failed_first = speech.fixture->settled(Binding::kPatience);
  });
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(failed_first);
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_reset");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->releases(key), 1U);
  EXPECT_EQ(speech.strategy->stats().jobs_failed, 1U);
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

// The backend counts a job open until it reports it released, after its
// result: the lane is taken until then.
TEST(SpeechJobDispatch, AJobThatEndedHoldsTheLaneUntilTheBackendReleasesIt) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto first = binding.open_audio();
  const auto second = binding.open_audio();
  speech.fixture->hold_job_releases();
  one_utterance(binding, first);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(first, EndpointReason::ClientFinalize, 1, 0,
                                               patterned_bytes(kFrameBytes, 1), 1));
  one_utterance(binding, second);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);

  speech.fixture->let_job_releases_go();
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(second, EndpointReason::ClientFinalize, 1, 0,
                                               patterned_bytes(kFrameBytes, 1), 1));
  EXPECT_EQ(speech.bridge->submitted().size(), 2U);
}

// A session with more work goes behind the sessions that wait once its job
// ends: one job per session in turn, not the order the utterances ended in.
TEST(SpeechJobDispatch, ASessionWithSeveralUtterancesTakesItsJobsInTurn) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto busy = binding.open_audio();
  const auto other = binding.open_audio();
  const Bytes utterance = utterance_at_the_limit(binding, busy, 3);
  speech.hold_jobs();
  for (int ended = 0; ended < 3; ++ended) {
    ASSERT_TRUE(binding.audio_in_long_frames(busy, utterance));
  }
  ASSERT_TRUE(binding.settle());
  one_utterance(binding, other);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);

  speech.release_jobs();
  ASSERT_TRUE(binding.eventually([&] { return speech.bridge->submitted().size() == 4U; }));
  const auto submitted = speech.bridge->submitted();
  ASSERT_EQ(submitted.size(), 4U);
  EXPECT_EQ(submitted[0].identity().session_key, busy);
  EXPECT_EQ(submitted[1].identity().session_key, other);
  EXPECT_EQ(submitted[2].identity().session_key, busy);
  EXPECT_EQ(submitted[3].identity().session_key, busy);
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

// Its job is released only by the unload, but the lost backend refuses every
// submit: the next session learns of the loss at its first job.
TEST(SpeechJobDispatch, ALostBackendsUnreleasedJobKeepsNoSessionWaitingForTheLane) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.hold_jobs();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.settle());
  speech.fixture->lose_backend();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  const auto next = binding.open_text();
  binding.text(next, 1, "next");
  ASSERT_TRUE(binding.ends_as(next, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(next)->reason, "backend_unavailable");
}

// The loss ends every session the backend served: the one whose job runs,
// one whose job waits in the lane, one with nothing asked, and another
// session whose cancel still waits for the acknowledgement of its release.
TEST(SpeechJobDispatch, ABackendLostUnderAJobFailsEverySessionAsBackendReset) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto running = binding.open_audio();
  const auto waiting = binding.open_text();
  const auto idle = binding.open_audio();
  const auto cancelled = binding.open_text();
  speech.hold_jobs();
  one_utterance(binding, running);
  ASSERT_TRUE(binding.settle());
  binding.text(waiting, 1, "queued");
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);
  speech.bridge->hold_acknowledgements();
  ASSERT_TRUE(binding.manager->apply(cancelled, Event::Cancel).has_value());
  ASSERT_TRUE(binding.eventually([&] { return speech.bridge->releases(cancelled) == 1U; }));

  speech.fixture->lose_backend();
  for (const auto key : {running, waiting, idle}) {
    SCOPED_TRACE(key);
    ASSERT_TRUE(binding.ends_as(key, State::Failed));
    EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_reset");
    EXPECT_EQ(speech.bridge->releases(key), 1U);
  }
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(cancelled) == State::Failed; }));
  speech.bridge->let_acknowledgements_go();
  ASSERT_TRUE(binding.ends_as(cancelled, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(cancelled)->reason, "backend_reset");
  EXPECT_EQ(speech.bridge->releases(cancelled), 1U);
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  speech.fixture->unload();
  EXPECT_EQ(speech.buffers_in_use(), 0U);
}

// The loss arrives with the job of a session already cancelled: the sessions
// still served end with it all the same, and the queued job is never sent.
TEST(SpeechJobDispatch, ABackendLostUnderACancelledSessionsJobFailsTheSessionsStillServed) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto cancelled = binding.open_text();
  const auto waiting = binding.open_audio();
  const auto idle = binding.open_audio();
  speech.hold_jobs();
  binding.text(cancelled, 1, "held");
  ASSERT_TRUE(binding.settle());
  one_utterance(binding, waiting);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);
  ASSERT_TRUE(binding.manager->apply(cancelled, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(binding.live_state(cancelled), State::CancelRequested);

  speech.fixture->lose_backend();
  for (const auto key : {waiting, idle}) {
    SCOPED_TRACE(key);
    ASSERT_TRUE(binding.ends_as(key, State::Failed));
    EXPECT_EQ(binding.manager->tombstone(key)->reason, "backend_reset");
    EXPECT_EQ(speech.bridge->releases(key), 1U);
  }
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(cancelled) == State::Failed; }));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.manager->held_slots(), 1U);
  speech.fixture->unload();
  ASSERT_TRUE(binding.ends_as(cancelled, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(cancelled)->reason, "backend_reset");
  EXPECT_EQ(speech.bridge->releases(cancelled), 1U);
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
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
  // The release cancelled the job: the stop asks for no cancel of its own.
  EXPECT_TRUE(speech.bridge->cancelled().empty());
  speech.release_jobs();
  ASSERT_TRUE(speech.fixture->settled(Binding::kPatience));
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  // Nothing is reported after the stop: both slots end with the manager.
  EXPECT_EQ(binding.manager->held_slots(), 2U);
}

// A release the bridge refused cancelled nothing, so the session's job would
// run on under the unload that follows a stop.
TEST(SpeechJobDispatch, StopCancelsTheJobOfASessionWhoseReleaseTheBridgeRefused) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.hold_jobs();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.settle());
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_TRUE(speech.bridge->cancelled().empty());

  binding.dispatch().stop();
  const auto submitted = speech.bridge->submitted();
  ASSERT_EQ(submitted.size(), 1U);
  EXPECT_EQ(speech.bridge->cancelled(), std::vector{submitted.front().identity().job_id});
  // The refused release is not asked again.
  EXPECT_EQ(speech.bridge->releases(key), 1U);
  speech.release_jobs();
  ASSERT_TRUE(speech.fixture->settled(Binding::kPatience));
  EXPECT_EQ(speech.buffers_in_use(), 0U);
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
  EXPECT_TRUE(speech.bridge->cancelled().empty());
  // The backend lets the job complete: its audio reaches a stopped dispatch.
  speech.release_jobs();
  ASSERT_TRUE(speech.fixture->settled(Binding::kPatience));
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// negotiate() cannot know what the backend runs, so the session is granted
// and its first job decides.
TEST(SpeechJobDispatch, AJobClassTheBackendDoesNotRunFailsTheSessionAtItsFirstJob) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  speech.bridge->refuse_class(JobClass::TtsSynthesis);
  // The double answers as it refuses.
  EXPECT_FALSE(speech.bridge->runs(JobClass::TtsSynthesis));
  EXPECT_TRUE(speech.bridge->runs(JobClass::SttDecode));
  const auto refused = binding.open_text();
  binding.text(refused, 1, "hi");
  ASSERT_TRUE(binding.ends_as(refused, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(refused)->code, Error::Code::Unsupported);
  EXPECT_EQ(binding.manager->tombstone(refused)->reason, "backend_unsupported_capability");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().jobs_submitted, 0U);

  const auto served = binding.open_audio();
  one_utterance(binding, served);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(served).has_value());
  EXPECT_TRUE(binding.read_as<FinalTranscriptOutput>(served).has_value());
}

// The bindings' default result function scores voice activity as the fixture backend does.
TEST(ResultFollowingInput, ScoresVoiceActivityAsTheFixtureBackendDoes) {
  const auto buffers = BufferManager::create({}).value();
  const BufferRef pcm = buffers->allocate(2 * kFixtureVadFrameSamples).value();
  const auto held = buffers->data(pcm).value();
  std::fill(held.begin(), held.end(), std::byte{0});
  held[1] = std::byte{0x40};
  const JobRequest request =
      JobRequest::create(JobIdentity{1, 1, 1}, JobClass::VadFrames,
                         VadFrames{kJobInputAudioFormat, PcmWindow{pcm, 0, held.size()},
                                   kFixtureVadFrameSamples, 1, 1})
          .value();
  const JobResult scored = result_following_input(request, *buffers).value();
  EXPECT_EQ(std::get<VadResult>(scored.payload()).probabilities, std::vector<float>{0.5F});
}

Result<JobResult> nothing_heard(const JobRequest& /*request*/, BufferManager& /*buffers*/) {
  return JobResult::create(TranscriptResult{"", {}, {}});
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_that_hears_nothing(const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, nothing_heard);
}

TEST(SpeechJobDispatch, ADecodeWithoutTextYieldsAFinalWithoutSegments) {
  Binding binding(&speech_dispatch_that_hears_nothing);
  const auto key = binding.open_audio();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  const auto transcript = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_TRUE(transcript->segments.empty());
  EXPECT_EQ(transcript->finalize_sequence, 1U);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech_of(binding).strategy->stats().jobs_completed, 1U);
}

TEST(SpeechJobDispatch, UtterancesThatEndFasterThanTheyAreDecodedFailTheSession) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const auto terms = binding.terms(key);
  const Bytes utterance = patterned_bytes(static_cast<std::size_t>(terms.max_utterance.count()) *
                                              terms.audio_format.bytes_per_second() / 1000,
                                          3);
  speech.hold_jobs();
  // The one being decoded and two behind it.
  for (int ended = 0; ended < 3; ++ended) {
    ASSERT_TRUE(binding.audio_in_long_frames(key, utterance));
  }
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);

  (void)binding.audio_in_long_frames(key, utterance);
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Failed; }));
  // Its release waits behind the job the backend still holds.
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->code, Error::Code::ResourceExhausted);
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "utterance_backlog_exceeded");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  EXPECT_EQ(speech.buffers_in_use(), 0U);
}

// The bound is the same when a client's Finalize ends the fourth.
TEST(SpeechJobDispatch, AFourthUtteranceEndedByAFinalizeFailsTheSession) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes utterance = utterance_at_the_limit(binding, key, 3);
  speech.hold_jobs();
  for (int ended = 0; ended < 3; ++ended) {
    ASSERT_TRUE(binding.audio_in_long_frames(key, utterance));
  }
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);

  one_utterance(binding, key);
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Failed; }));
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->code, Error::Code::ResourceExhausted);
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "utterance_backlog_exceeded");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  EXPECT_EQ(speech.buffers_in_use(), 0U);
}

Result<JobResult> the_longest_speech_whatever_the_text(const JobRequest& /*request*/,
                                                       BufferManager& buffers) {
  const auto buffer = buffers.allocate(kMaxAudioChunkBytes);
  if (!buffer) {
    return unexpected(buffer.error());
  }
  const auto pcm = buffers.data(*buffer).value();
  std::fill(pcm.begin(), pcm.end(), std::byte{0});
  return JobResult::create(
      AudioChunkResult{kJobOutputAudioFormat, PcmWindow{*buffer, 0, kMaxAudioChunkBytes}, 0});
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_with_the_longest_speech(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{},
                                           the_longest_speech_whatever_the_text);
}

/// Reads one segment's chunks to its completion.
void read_segment(Binding& binding, std::uint64_t key) {
  for (;;) {
    const auto next = binding.read(key);
    ASSERT_TRUE(next.has_value());
    if (std::holds_alternative<SegmentCompletedOutput>(*next)) {
      return;
    }
    ASSERT_TRUE(std::holds_alternative<AudioChunkOutput>(*next));
  }
}

// Only the dispatch sees how much audio a text yields.
TEST(SpeechJobDispatch, SegmentsBetweenTwoFinalizesYieldNoMoreAudioThanWasGranted) {
  Binding binding(&speech_dispatch_with_the_longest_speech);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  ASSERT_EQ(binding.terms(key).max_synthesis_audio, 3 * binding.terms(key).max_segment_audio);
  // Exactly what was granted, twice: a Finalize starts the count again.
  std::uint64_t segment = 0;
  for (int round = 0; round < 2; ++round) {
    for (int sent = 0; sent < 3; ++sent) {
      binding.text(key, ++segment, "a");
      ASSERT_NO_FATAL_FAILURE(read_segment(binding, key));
    }
    binding.finalize(key, 9);
    ASSERT_TRUE(binding.read_as<SynthesisCompletedOutput>(key).has_value());
    ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));
  }
  for (int sent = 0; sent < 3; ++sent) {
    binding.text(key, ++segment, "a");
    ASSERT_NO_FATAL_FAILURE(read_segment(binding, key));
  }
  binding.text(key, ++segment, "a");
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->code, Error::Code::ResourceExhausted);
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "synthesis_audio_limit_exceeded");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

Result<JobResult> speech_behind_an_offset(const JobRequest& request, BufferManager& buffers) {
  constexpr std::size_t kOffset = 64;
  const auto speech = speech_following(std::get<TextSegment>(request.payload()).text);
  const auto buffer = buffers.allocate(kOffset + speech.size());
  if (!buffer) {
    return unexpected(buffer.error());
  }
  const auto pcm = buffers.data(*buffer).value();
  std::fill(pcm.begin(), pcm.end(), std::byte{0x55});
  std::copy(speech.begin(), speech.end(), pcm.begin() + kOffset);
  return JobResult::create(
      AudioChunkResult{kJobOutputAudioFormat, PcmWindow{*buffer, kOffset, speech.size()}, 0});
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_with_speech_behind_an_offset(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{},
                                           speech_behind_an_offset);
}

TEST(SpeechJobDispatch, SpeechIsCutFromTheResultsWindowOfItsBuffer) {
  Binding binding(&speech_dispatch_with_speech_behind_an_offset);
  const auto key = binding.open_text();
  binding.text(key, 1, "hello there");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, "hello there"));
}

TEST(SpeechJobDispatch, StopWakesAWaitForIdle) {
  const FakeSchedulerClock clock;
  SpeechUnderTest speech(clock);
  // No manager is attached, so the result stays queued and the wait asleep.
  ASSERT_TRUE(speech.fixture->submit(foreign_synthesis(1)).has_value());
  ASSERT_TRUE(speech.fixture->settled(Binding::kPatience));
  bool idle = false;
  // A wait nobody wakes outlasts the test's own time limit.
  std::thread waiter([&] { idle = speech.strategy->wait_idle(10min); });
  EXPECT_FALSE(speech.strategy->wait_idle(50ms));
  speech.strategy->stop();
  waiter.join();
  EXPECT_TRUE(idle);
}

TEST(SpeechJobDispatch, AResultStillQueuedAtStopIsDroppedAndItsAudioReleased) {
  const FakeSchedulerClock clock;
  SpeechUnderTest speech(clock);
  // No manager is attached, so nothing is taken off the queue.
  ASSERT_TRUE(speech.fixture->submit(foreign_synthesis(1)).has_value());
  ASSERT_TRUE(speech.fixture->settled(Binding::kPatience));
  EXPECT_EQ(speech.buffers_in_use(), 1U);
  speech.strategy->stop();
  EXPECT_EQ(speech.buffers_in_use(), 0U);
}

Result<JobResult> a_transcript_whatever_was_asked(const JobRequest& /*request*/,
                                                  BufferManager& /*buffers*/) {
  return JobResult::create(TranscriptResult{"hello", {}, {}});
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_with_transcripts_only(
    const SchedulerClock& clock) {
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{},
                                           a_transcript_whatever_was_asked);
}

// A bridge checks a result's kind against its job's class, so a result of the
// wrong kind never reaches a session as one.
TEST(SpeechJobDispatch, AResultOfTheWrongKindReachesTheDispatchAsAFailedJob) {
  Binding binding(&speech_dispatch_with_transcripts_only);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  binding.text(key, 1, "hi");
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->code, Error::Code::InferenceFailed);
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "result_kind_mismatch");
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().jobs_failed, 1U);
  EXPECT_EQ(speech.strategy->stats().jobs_completed, 0U);
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_with_a_small_pool(const SchedulerClock& clock) {
  BufferManagerConfig pool;
  pool.capacity_bytes = kFrameBytes;
  pool.max_buffer_bytes = 0;
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           pool);
}

TEST(SpeechJobDispatch, AnUtteranceThePoolHasNoBufferForFailsItsSessionAndSubmitsNothing) {
  Binding binding(&speech_dispatch_with_a_small_pool);
  SpeechUnderTest& speech = speech_of(binding);
  const auto fits = binding.open_audio();
  one_utterance(binding, fits);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(fits).has_value());
  ASSERT_TRUE(binding.read_as<FinalTranscriptOutput>(fits).has_value());

  const auto too_long = binding.open_audio();
  binding.audio(too_long, patterned_bytes(2 * kFrameBytes, 1));
  binding.finalize(too_long, 1);
  ASSERT_TRUE(binding.ends_as(too_long, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(too_long)->code, Error::Code::OOMError);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
  EXPECT_EQ(speech.buffers_in_use(), 0U);
  EXPECT_EQ(binding.live_state(fits), State::Active);
}

// settle() rests on wait_idle(): a command taken off the queue and still
// being handled is not idle.
TEST(SpeechJobDispatch, ACommandBeingHandledIsNotIdle) {
  Binding binding(&speech_dispatch);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(kFrameBytes, 1);
  ASSERT_TRUE(binding.settle());
  const auto accepted = binding.manager->accept_input(key, frame.size());
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::AcceptInput));
  ASSERT_TRUE(binding.settle());

  // The initialize hook holds the manager's serialized section, which the
  // frame's credit return waits for.
  const auto terms = binding.dispatch().negotiate(binding.under_test->text_request()).value();
  bool idle_inside = true;
  const auto opened = binding.manager->open(Binding::kGeneration, budgets_for(terms).value(),
                                            [&](std::uint64_t other) {
                                              speech.strategy->audio(key, frame);
                                              idle_inside = speech.strategy->wait_idle(50ms);
                                              return speech.strategy->open_session(other, terms);
                                            });
  ASSERT_TRUE(opened.has_value());
  EXPECT_FALSE(idle_inside);
  EXPECT_TRUE(speech.strategy->wait_idle(Binding::kPatience));
  EXPECT_EQ(binding.manager->status(key).value().input_credit_bytes.used(), 0U);
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
  SpeechUnderTest& speech = speech_of(binding);
  // The backend released the buffer itself: the dispatch's release is refused.
  speech.stale_results = 1;
  const auto key = binding.open_text();
  binding.text(key, 1, "hi");
  EXPECT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.buffers_in_use(), 0U);
}

/// Short enough for a session that sends nothing to outlive them: a job's
/// deadline is 1 s plus 0.5 s per second of its audio, a release's bound 2 s.
std::unique_ptr<DispatchUnderTest> speech_dispatch_with_short_bounds(const SchedulerClock& clock) {
  SpeechDispatchOptions options;
  options.job_deadline_base = 1s;
  options.job_deadline_per_audio_second = 500ms;
  options.release_bound = 2s;
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, std::move(options));
}

std::unique_ptr<DispatchUnderTest> speech_dispatch_with_long_jobs(const SchedulerClock& clock) {
  SpeechDispatchOptions options;
  options.job_deadline_base = 1h;
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, std::move(options));
}

// The finalize deadline counts from the Finalize, so the wait behind another
// session's job is part of it: a decode that never left the lane fails it.
TEST(SpeechJobDispatch, AFinalWaitingBehindAnotherSessionsJobKeepsItsDeadline) {
  Binding binding(&speech_dispatch_with_long_jobs);
  SpeechUnderTest& speech = speech_of(binding);
  const auto ahead = binding.open_text();
  const auto behind = binding.open_audio();
  speech.hold_jobs();
  binding.text(ahead, 1, "held");
  ASSERT_TRUE(binding.settle());
  one_utterance(binding, behind);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);

  binding.clock.advance(finalize_deadline(SessionInputKind::Audio));
  binding.manager->notify_clock_advanced();
  ASSERT_TRUE(binding.ends_as(behind, State::Closed));
  EXPECT_EQ(binding.manager->tombstone(behind)->reason, "finalize_timeout");
  EXPECT_EQ(binding.live_state(ahead), State::Active);
  speech.release_jobs();
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(ahead, 1, "held"));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.bridge->submitted().size(), 1U);
}

/// Moves the clock the manager and the dispatch share, and lets the dispatch
/// act on it.
void move_clock(Binding& binding, SchedulerClock::Duration by) {
  binding.clock.advance(by);
  speech_of(binding).strategy->notify_clock_advanced();
  ASSERT_TRUE(binding.settle());
}

TEST(SpeechJobDispatch, TheDeadlinesAndTheBoundAreTheProvisionalFigures) {
  const SpeechDispatchOptions options;
  EXPECT_EQ(options.job_deadline_base, 10s);
  EXPECT_EQ(options.job_deadline_per_audio_second, 500ms);
  EXPECT_EQ(options.release_bound, 10s);
  EXPECT_FALSE(options.on_stall);
}

// The runner does not interrupt a job: a cancel ends it when its work returns.
TEST(SpeechJobDispatch, ACancelledSessionWhoseJobNeverEndsIsReportedOnceItsReleasePassesItsBound) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.hold_jobs();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());

  // The job's deadline, 1 s and 10 ms after its submit, ends nothing more.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1010ms));
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
  EXPECT_EQ(binding.live_state(key), State::CancelRequested);
  // Only the bound is still due, and later: the thread waits for it.
  const auto waiting = speech.strategy->stats().steps;
  std::this_thread::sleep_for(200ms);
  EXPECT_EQ(speech.strategy->stats().steps, waiting);
  // The bound counts from that deadline, later than the release was asked.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 2s - 1ns));
  EXPECT_TRUE(speech.stalls().empty());
  EXPECT_EQ(speech.strategy->stats().releases_past_bound, 0U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  ASSERT_EQ(speech.stalls().size(), 1U);
  EXPECT_EQ(speech.stalls().front().code, Error::Code::Timeout);
  EXPECT_EQ(speech.stalls().front().context, "release_bound_exceeded");
  EXPECT_EQ(speech.strategy->stats().releases_past_bound, 1U);
  // Only the backend's acknowledgement frees the slot.
  EXPECT_EQ(binding.manager->held_slots(), 1U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1h));
  EXPECT_EQ(speech.stalls().size(), 1U);
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
  // Nothing is due any more: the thread waits, it does not turn.
  const auto steps = speech.strategy->stats().steps;
  std::this_thread::sleep_for(200ms);
  EXPECT_EQ(speech.strategy->stats().steps, steps);

  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "client_cancelled");
}

TEST(SpeechJobDispatch, ASynthesisStillRunningAtItsDeadlineFailsItsSession) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  // A synthesis is given the audio of the longest segment granted, 30 s.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 16s - 1ns));
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 0U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(binding.live_state(key), State::Failed);
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
  // Its release cancelled the job, which ends when its work returns.
  EXPECT_EQ(speech.bridge->releases(key), 1U);
  EXPECT_EQ(binding.manager->held_slots(), 1U);

  // A job that ends within the bound leaves no stall, and the lane free.
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  const auto tombstone = binding.manager->tombstone(key);
  EXPECT_EQ(tombstone->code, Error::Code::Timeout);
  EXPECT_EQ(tombstone->reason, "deadline_missed");
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1h));
  EXPECT_TRUE(speech.stalls().empty());
  const auto next = binding.open_text();
  binding.text(next, 1, "hi");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(next, 1, "hi"));
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
}

TEST(SpeechJobDispatch, AReleaseAskedAfterItsJobsDeadlineIsBoundFromTheRequest) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  // The clock moves between the failure at the deadline and the release it asks for.
  binding.before_next(key, Event::Fail, [&] { binding.clock.advance(5s); });
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 16s));
  ASSERT_EQ(binding.live_state(key), State::Failed);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 2s - 1ns));
  EXPECT_TRUE(speech.stalls().empty());
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(speech.stalls().size(), 1U);
}

// The job ends as its deadline passes: both reach the dispatch while its
// thread is held, in a step for another session.
TEST(SpeechJobDispatch, AJobsEndQueuedAsItsDeadlinePassesIsNotADeadlineMissed) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  const auto other = binding.open_audio();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  std::promise<void> entered;
  std::promise<void> go;
  const std::shared_future<void> gate = go.get_future().share();
  binding.before_next(other, Event::ReleaseAcknowledged, [&entered, gate] {
    entered.set_value();
    gate.wait();
  });
  auto held = entered.get_future();
  ASSERT_TRUE(binding.manager->apply(other, Event::Cancel).has_value());
  ASSERT_EQ(held.wait_for(Binding::kPatience), std::future_status::ready);
  binding.clock.advance(16s);
  speech.strategy->notify_clock_advanced();
  speech.release_jobs();
  const bool ended = speech.fixture->settled(Binding::kPatience);
  go.set_value();
  ASSERT_TRUE(ended);
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, "hello"));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 0U);
}

// A runner acknowledges a release in a frame of its own, after the session's
// job has ended: the bound fixed at the request keeps counting from that job's
// deadline.
TEST(SpeechJobDispatch, AReleaseWhoseJobEndsBeforeItIsAcknowledgedKeepsItsBound) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  speech.bridge->hold_acknowledgements();
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  // The job's deadline is 16 s away, so the bound 18 s.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 5s));
  speech.release_jobs();
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(speech.strategy->stats().jobs_discarded, 1U) << "the cancelled job's end";
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 13s - 1ns));
  EXPECT_TRUE(speech.stalls().empty());
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(speech.stalls().size(), 1U);
  speech.bridge->let_acknowledgements_go();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
}

// Once a bound is due, only what was queued by then goes ahead of its check,
// so a stream of commands cannot put it off: a job's end queued later is late.
TEST(SpeechJobDispatch, WhatArrivesAfterABoundFellDueDoesNotPutItsCheckOff) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_text();
  const auto other = binding.open_text();
  const auto third = binding.open_audio();
  speech.hold_jobs();
  binding.text(key, 1, "hello");
  ASSERT_TRUE(binding.settle());
  // The dispatch's thread waits in the bridge, releasing `other`.
  speech.bridge->hold_next_release();
  // Let go however the case ends: a hold left behind would stop the stop.
  const std::unique_ptr<CountingJobBridge, void (*)(CountingJobBridge*)> let_go{
      speech.bridge.get(), [](CountingJobBridge* bridge) { bridge->let_release_go(); }};
  ASSERT_TRUE(binding.manager->apply(other, Event::Cancel).has_value());
  ASSERT_TRUE(speech.bridge->release_held(Binding::kPatience));
  // The deadline passes and a Finalize of `third` is queued, whose completion
  // holds the thread again, in the manager's sink, after the check fell due.
  binding.clock.advance(16s);
  speech.strategy->notify_clock_advanced();
  std::promise<void> entered;
  std::promise<void> go;
  const std::shared_future<void> gate = go.get_future().share();
  binding.before_next(third, Event::FinalizeCompleted, [&entered, gate] {
    entered.set_value();
    gate.wait();
  });
  auto held = entered.get_future();
  binding.finalize(third, 1);
  speech.bridge->let_release_go();
  ASSERT_EQ(held.wait_for(Binding::kPatience), std::future_status::ready);
  speech.release_jobs();
  const bool ended = speech.fixture->settled(Binding::kPatience);
  go.set_value();
  ASSERT_TRUE(ended);
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "deadline_missed");
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
}

// A step that outlasts what was left of a bound leaves it due when the thread
// next waits: it is checked then, with no notice that the clock moved.
TEST(SpeechJobDispatch, ABoundDueWhenAStepEndsIsCheckedWithoutANotice) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.hold_jobs();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.settle());
  // The dispatch's thread waits in the bridge, releasing the cancelled session.
  speech.bridge->hold_next_release();
  const std::unique_ptr<CountingJobBridge, void (*)(CountingJobBridge*)> let_go{
      speech.bridge.get(), [](CountingJobBridge* bridge) { bridge->let_release_go(); }};
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(speech.bridge->release_held(Binding::kPatience));
  // Exactly onto the job's deadline, 1 s and 10 ms after its submit.
  binding.clock.advance(1010ms);
  speech.bridge->let_release_go();
  // The check is a step of its own after the release's: settle() may return
  // between the two, so the count is read until it moves.
  const auto give_up = std::chrono::steady_clock::now() + Binding::kPatience;
  while (speech.strategy->stats().jobs_past_deadline == 0 &&
         std::chrono::steady_clock::now() < give_up) {
    std::this_thread::sleep_for(1ms);
  }
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 2s - 1ns));
  EXPECT_TRUE(speech.stalls().empty());
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(speech.stalls().size(), 1U);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
}

/// Moved by hand, and counts how often it is read.
class CountedClock final : public SchedulerClock {
 public:
  [[nodiscard]] TimePoint now() const noexcept override {
    reads_.fetch_add(1);
    return moved.now();
  }
  [[nodiscard]] std::uint64_t reads() const noexcept { return reads_.load(); }

  FakeSchedulerClock moved;

 private:
  mutable std::atomic<std::uint64_t> reads_{0};
};

CountedClock& counted_clock() {
  static CountedClock clock;
  return clock;
}

/// The short bounds, on `counted_clock()`: the manager keeps the binding's.
std::unique_ptr<DispatchUnderTest> speech_dispatch_on_a_counted_clock(
    const SchedulerClock& /*clock*/) {
  return speech_dispatch_with_short_bounds(counted_clock());
}

/// Reads of `clock` over 200 ms, and the milliseconds measured around them.
/// The reads are counted inside the time measured, so a pause of the calling
/// thread cannot add wakes that the time leaves out.
std::pair<std::uint64_t, std::uint64_t> reads_over_a_while(const CountedClock& clock) {
  const auto from = std::chrono::steady_clock::now();
  const std::uint64_t before = clock.reads();
  std::this_thread::sleep_for(200ms);
  const std::uint64_t read = clock.reads() - before;
  const auto waited = std::chrono::duration_cast<std::chrono::milliseconds>(
      std::chrono::steady_clock::now() - from);
  return {read, static_cast<std::uint64_t>(waited.count())};
}

// The thread waits for its next bound until it is due, and 1 ms at least: a
// clock moved by hand is read twice a wake, and no more often than that allows.
TEST(SpeechJobDispatch, AClockMovedByHandIsReadNoMoreOftenThanItsNextBoundAllows) {
  Binding binding(&speech_dispatch_on_a_counted_clock);
  SpeechUnderTest& speech = speech_of(binding);
  CountedClock& clock = counted_clock();
  const auto move = [&](SchedulerClock::Duration by) {
    clock.moved.advance(by);
    speech.strategy->notify_clock_advanced();
    return binding.settle();
  };
  const auto key = binding.open_audio();
  speech.hold_jobs();
  one_utterance(binding, key);
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  // The job's deadline is 1 s and 10 ms away: a wake each 1010 ms at most.
  const auto [far, far_waited] = reads_over_a_while(clock);
  EXPECT_LE(far, 2 * (far_waited / 1010 + 1));
  // The release's bound is 3 s and 10 ms away: the clock stops 1 ns short,
  // and the thread wakes each 1 ms at most, without a step.
  ASSERT_TRUE(move(3010ms - 1ns));
  const auto steps = speech.strategy->stats().steps;
  const auto [near, near_waited] = reads_over_a_while(clock);
  EXPECT_EQ(speech.strategy->stats().steps, steps);
  EXPECT_LE(near, 3 * (near_waited + 2));
  // One wake at least in 200 ms, where a 1 ms floor gives about 200.
  EXPECT_GE(near, 2U);
  EXPECT_TRUE(speech.stalls().empty());
  ASSERT_TRUE(move(1ns));
  EXPECT_EQ(speech.stalls().size(), 1U);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Closed));
}

/// Half as fast as the steady clock: a timed wait for one of its points ends
/// short of it, as on any clock that runs slow.
class HalfSpeedClock final : public SchedulerClock {
 public:
  [[nodiscard]] TimePoint now() const noexcept override {
    return origin_ + (std::chrono::steady_clock::now() - origin_) / 2;
  }

 private:
  TimePoint origin_ = std::chrono::steady_clock::now();
};

/// A job's deadline is 20 ms and 0.5 s per second of its audio, a release's
/// bound 20 ms, on a clock that nobody moves and that gives no notice.
std::unique_ptr<DispatchUnderTest> speech_dispatch_on_a_half_speed_clock(
    const SchedulerClock& /*clock*/) {
  static const HalfSpeedClock clock;
  SpeechDispatchOptions options;
  options.job_deadline_base = 20ms;
  options.release_bound = 20ms;
  return std::make_unique<SpeechUnderTest>(clock, FixtureJobBridgeOptions{}, result_following_input,
                                           BufferManagerConfig{}, std::move(options));
}

TEST(SpeechJobDispatch, OnAClockSlowerThanItsWaitsTheThreadStillReachesEachBound) {
  Binding binding(&speech_dispatch_on_a_half_speed_clock);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.hold_jobs();
  one_utterance(binding, key);
  ASSERT_TRUE(speech.stalled(30s));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Failed);
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
  EXPECT_EQ(speech.strategy->stats().releases_past_bound, 1U);
  EXPECT_EQ(speech.stalls().front().context, "release_bound_exceeded");
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "deadline_missed");
}

// The bound of a release counts from the deadline of the job its own session
// owns: here not the first session opened, so its key is not the job's id.
TEST(SpeechJobDispatch, AReleaseIsBoundFromTheDeadlineOfTheJobItsSessionOwns) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  (void)binding.open_text();
  const auto owner = binding.open_text();
  speech.hold_jobs();
  binding.text(owner, 1, "hello");
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);
  ASSERT_NE(speech.bridge->submitted().front().identity().job_id, owner);
  ASSERT_TRUE(binding.manager->apply(owner, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  // The synthesis's deadline is 16 s away, so the bound 18 s.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 18s - 1ns));
  EXPECT_TRUE(speech.stalls().empty());
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 1U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(speech.stalls().size(), 1U);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(owner, State::Closed));
}

TEST(SpeechJobDispatch, ADecodesDeadlineGrowsWithItsAudio) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  const auto key = binding.open_audio();
  speech.hold_jobs();
  // Half a second of audio: 1 s and 250 ms.
  ASSERT_TRUE(binding.audio_in_long_frames(key, patterned_bytes(16'000, 1)));
  binding.finalize(key, 1);
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(speech.bridge->submitted().size(), 1U);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1250ms - 1ns));
  EXPECT_EQ(binding.live_state(key), State::Finalizing);
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(binding.live_state(key), State::Failed);
  speech.release_jobs();
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "deadline_missed");
}

TEST(SpeechJobDispatch, AReleaseTheBridgeRefusedIsReportedOnceItsBoundPasses) {
  Binding binding(&speech_dispatch_with_short_bounds);
  SpeechUnderTest& speech = speech_of(binding);
  // Another session's job is in flight throughout, its deadline far off.
  const auto busy = binding.open_text();
  speech.hold_jobs();
  binding.text(busy, 1, "hello");
  ASSERT_TRUE(binding.settle());
  const auto first = binding.open_audio();
  const auto second = binding.open_text();
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(first, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1s));
  speech.bridge->refuse_next_release();
  ASSERT_TRUE(binding.manager->apply(second, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());

  // Without its own job in flight a release's bound counts from its request.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1s - 1ns));
  EXPECT_TRUE(speech.stalls().empty());
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1ns));
  EXPECT_EQ(speech.stalls().size(), 1U);
  EXPECT_EQ(speech.strategy->stats().releases_past_bound, 1U);
  // The second release passes its bound too, and is not reported again.
  ASSERT_NO_FATAL_FAILURE(move_clock(binding, 1s));
  EXPECT_EQ(speech.strategy->stats().releases_past_bound, 2U);
  EXPECT_EQ(speech.stalls().size(), 1U);
  EXPECT_EQ(speech.strategy->stats().jobs_past_deadline, 0U);
  EXPECT_EQ(binding.manager->held_slots(), 3U);
}
}  // namespace
}  // namespace tensorplate::testing
