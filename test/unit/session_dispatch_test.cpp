// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest-spi.h>
#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <future>
#include <memory>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"
#include "serving/session/timers.hpp"
#include "session_dispatch_binding.hpp"
#include "speech_dispatch_binding.hpp"
#include "synthetic_session_dispatch.hpp"

namespace tensorplate::testing {
namespace {
using namespace std::chrono_literals;
using namespace tensorplate::serving;
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;
using State = LogicalSessionState;
using Fake = SyntheticSessionDispatch;
using Binding = ReferenceBinding;
using Bytes = std::vector<std::byte>;

constexpr StreamAudioFormat kPcm16k{StreamAudioEncoding::PcmS16Le, 16'000, 1};
constexpr StreamAudioFormat kPcm24k{StreamAudioEncoding::PcmS16Le, 24'000, 1};
/// 20 ms of kPcm16k, the synthetic dispatch's shortest frame.
constexpr std::size_t kFrameBytes = 640;

/// The synthetic dispatch, as the cases know it.
class SyntheticUnderTest final : public DispatchUnderTest {
 public:
  explicit SyntheticUnderTest(const SchedulerClock& clock) : fake(clock) {}

  SessionDispatch& dispatch() override { return fake; }
  SessionRequest audio_request() const override {
    return {SessionInputKind::Audio, "en", "", kPcm16k};
  }
  SessionRequest text_request() const override {
    return {SessionInputKind::Text, "en-US", "voice-a", {}};
  }
  std::vector<TranscriptSegment> transcript(std::uint64_t first_sample,
                                            std::span<const std::byte> audio) const override {
    if (audio.empty()) {
      return {};
    }
    // 62.5 us a sample, the start rounded down and the end up.
    const std::uint64_t end_sample = first_sample + audio.size() / 2;
    return {{first_sample * 125 / 2, (end_sample * 125 + 1) / 2, Fake::transcript_of(audio)}};
  }
  Bytes speech(std::string_view text) const override {
    Bytes pcm;
    for (std::uint64_t index = 0; index < text.size() * Fake::kSamplesPerTextByte; ++index) {
      const auto sample = static_cast<std::uint16_t>(Fake::sample_of(text, index));
      pcm.push_back(static_cast<std::byte>(sample & 0xffU));
      pcm.push_back(static_cast<std::byte>(sample >> 8U));
    }
    return pcm;
  }
  bool settled(std::chrono::milliseconds patience) override { return fake.wait_idle(patience); }
  std::size_t held_sessions() const override { return fake.held_sessions(); }
  void hold_jobs() override { fake.pause(Fake::Gate::InJob); }
  void release_jobs() override { fake.resume(); }

  Fake fake;
};

std::unique_ptr<DispatchUnderTest> synthetic(const SchedulerClock& clock) {
  return std::make_unique<SyntheticUnderTest>(clock);
}

Fake& fake_of(Binding& binding) {
  return static_cast<SyntheticUnderTest&>(*binding.under_test).fake;
}

Error worker_shutdown() {
  return Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown");
}

std::string text_of(std::size_t bytes) {
  std::string text(bytes, 'a');
  for (std::size_t index = 0; index < bytes; ++index) {
    text[index] = static_cast<char>('a' + index % 26);
  }
  return text;
}

std::size_t audio_bytes(const SessionTerms& terms, std::chrono::milliseconds duration) {
  return terms.audio_format.bytes_per_second() * static_cast<std::uint64_t>(duration.count()) /
         1000;
}

std::size_t chunk_bytes(const SessionTerms& terms) {
  return audio_bytes(terms, kAudioChunkDuration);
}

/// What an audio session's terms grant, in bytes.
struct AudioSizes {
  std::size_t sample = 0;
  std::size_t shortest_frame = 0;
  std::size_t longest_frame = 0;
  std::size_t longest_utterance = 0;
  /// A frame shorter than the shortest granted.
  std::size_t short_frame = 0;
  /// The frames voice activity is scored in, the profile's frame of input
  /// samples as the dispatch submits it; none without voice detection.
  std::size_t scored_frame = 0;
  /// A frame that is speech to a dispatch detecting voice, as long as its
  /// shortest speech run; the shortest frame without voice detection.
  std::size_t spoken_frame = 0;
};

/// Bytes of input from the start of an utterance until all of `bytes` is
/// scored: whole scored frames when the dispatch detects voice.
std::size_t scored_length(const AudioSizes& sizes, std::size_t bytes) {
  if (sizes.scored_frame == 0) {
    return bytes;
  }
  return (bytes + sizes.scored_frame - 1) / sizes.scored_frame * sizes.scored_frame;
}

AudioSizes sizes_of(Binding& binding, std::uint64_t key) {
  const auto terms = binding.terms(key);
  AudioSizes sizes;
  sizes.sample = terms.audio_format.bytes_per_sample();
  sizes.shortest_frame = audio_bytes(terms, terms.min_frame);
  sizes.longest_frame = audio_bytes(terms, terms.max_frame);
  sizes.longest_utterance = audio_bytes(terms, terms.max_utterance);
  sizes.short_frame = sizes.shortest_frame / 2 / sizes.sample * sizes.sample;
  sizes.spoken_frame = sizes.shortest_frame;
  if (const auto voice = binding.under_test->voice_detection()) {
    sizes.scored_frame = voice->frame_samples * sizes.sample;
    sizes.spoken_frame =
        std::max(sizes.shortest_frame, scored_length(sizes, audio_bytes(terms, voice->min_speech)));
  }
  return sizes;
}

bool detects_voice(Binding& binding) {
  return binding.under_test->voice_detection().has_value();
}

/// Records what `key`'s output queue holds when its next finalization is
/// reported.
void record_at_completion(Binding& binding, std::uint64_t key, std::atomic<std::uint64_t>& queued) {
  binding.before_next(key, Event::FinalizeCompleted, [&binding, key, &queued] {
    queued = binding.output(key)->metadata_usage().used();
  });
}

/// What an utterance's undelivered endpoint, and its final when
/// `with_final`, hold of the output queue's metadata budget.
std::uint64_t utterance_charge(Binding& binding, std::uint64_t first_sample,
                               std::span<const std::byte> audio, bool with_final) {
  std::uint64_t bytes = make_task_output_item(EndpointDetectedOutput{}).bytes;
  if (with_final) {
    FinalTranscriptOutput transcript;
    transcript.segments = binding.under_test->transcript(first_sample, audio);
    bytes += make_task_output_item(std::move(transcript)).bytes;
  }
  return bytes;
}

/// Text whose speech is more than the session's PCM budget holds at once.
std::string overflowing_text(Binding& binding, std::uint64_t key) {
  const auto terms = binding.terms(key);
  const std::size_t budget = binding.output(key)->pcm_usage().limit() + chunk_bytes(terms);
  std::size_t bytes = 16;
  while (bytes < terms.max_segment_text_bytes &&
         binding.under_test->speech(text_of(bytes)).size() <= budget) {
    bytes *= 2;
  }
  return text_of(std::min<std::size_t>(bytes, terms.max_segment_text_bytes));
}

/// The first transition at or after `from` that `matches`; the size if none.
template <typename Matches>
std::size_t first_from(const std::vector<ManagedSessionTransition>& seen, std::size_t from,
                       Matches&& matches) {
  const auto found = std::find_if(
      seen.begin() + static_cast<std::ptrdiff_t>(std::min(from, seen.size())), seen.end(), matches);
  return static_cast<std::size_t>(found - seen.begin());
}

bool shown(Binding& binding, std::uint64_t key, Event event) {
  const auto seen = binding.transitions(key);
  return first_from(seen, 0, [&](const auto& moved) { return moved.event == event; }) !=
         seen.size();
}

/// Whether `calls` return while the dispatch is held. If they do not,
/// `unblock` lets the test end.
template <typename Calls, typename Unblock>
bool returns_promptly(Calls&& calls, Unblock&& unblock) {
  std::promise<void> returned;
  const auto done = returned.get_future();
  std::thread caller([&] {
    calls();
    returned.set_value();
  });
  const bool prompt = done.wait_for(Binding::kPatience) == std::future_status::ready;
  if (!prompt) {
    unblock();
  }
  caller.join();
  return prompt;
}

enum class Takeover : std::uint8_t {
  BeforeTheJobEnds,
  AfterTheJobEnds,
  AsTheFinalizeIsApplied,
  Racing
};

/// An utterance the duration limit ended is held as a job, and a client's
/// Finalize meets that job's end in each order a binding can produce.
void finalize_takes_over(const DispatchCase& dispatch, Takeover order, int round) {
  Binding binding(dispatch);
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const std::size_t longest = sizes.longest_utterance;
  const Bytes audio = patterned_bytes(longest + sizes.shortest_frame, 5);
  const auto first = std::span{audio}.first(longest);
  const auto second = std::span{audio}.subspan(longest);
  // Detecting voice, the first utterance completes while the Finalize's
  // frames still wait to be scored: the finalization is reported once the
  // Finalize's endpoint was queued, before its final. Released as the
  // Finalize is applied, the job may be seen to end before the Finalize.
  const bool pinned = detects_voice(binding) &&
                      (order == Takeover::BeforeTheJobEnds || order == Takeover::AfterTheJobEnds);
  std::atomic<std::uint64_t> queued_at_completion{0};
  if (pinned) {
    record_at_completion(binding, key, queued_at_completion);
  }
  binding.under_test->hold_jobs();
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Finalizing; }));
  ASSERT_TRUE(binding.settle());

  if (order == Takeover::BeforeTheJobEnds) {
    binding.finalize(key, 7);
    ASSERT_TRUE(binding.settle());
    binding.under_test->release_jobs();
  } else if (order == Takeover::AfterTheJobEnds) {
    ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
    ASSERT_TRUE(binding.settle());
    binding.under_test->release_jobs();
  } else if (order == Takeover::AsTheFinalizeIsApplied) {
    // Released inside the manager call that applies the Finalize: the job's
    // report cannot be applied before the Finalize, and the dispatch may
    // begin it before it has recorded the Finalize.
    binding.before_next(key, Event::Finalize, [&] { binding.under_test->release_jobs(); });
    binding.finalize(key, 7);
  } else {
    std::thread releaser([&] { binding.under_test->release_jobs(); });
    std::this_thread::sleep_for(std::chrono::microseconds{round * 20});
    binding.finalize(key, 7);
    releaser.join();
  }
  const std::uint64_t second_endpoint =
      pinned ? utterance_charge(binding, longest / sizes.sample, second, false) : 0;
  if (pinned && order == Takeover::BeforeTheJobEnds) {
    ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));
    EXPECT_EQ(queued_at_completion, utterance_charge(binding, 0, first, true) + second_endpoint);
  }
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0, first, 0));
  if (order == Takeover::AfterTheJobEnds) {
    // The completion is not reported while the Finalize the dispatch has
    // recorded is still to be handed over.
    ASSERT_TRUE(binding.settle());
    ASSERT_EQ(binding.live_state(key), State::Finalizing);
    binding.dispatch().finalize(key, 7);
    if (pinned) {
      // The first utterance's endpoint and final were read.
      ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));
      EXPECT_EQ(queued_at_completion, second_endpoint);
    }
  }
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               longest / sizes.sample, second, 7));
  ASSERT_TRUE(binding.settle());
  ASSERT_EQ(binding.live_state(key), State::Active);
}

TEST_P(SessionDispatchContract, AudioThenFinalizeYieldsOneFinalPerUtterance) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const Bytes first = patterned_bytes(3 * sizes.spoken_frame, 1);
  for (std::size_t frame = 0; frame < 3; ++frame) {
    binding.audio(key, std::span{first}.subspan(frame * sizes.spoken_frame, sizes.spoken_frame));
  }
  binding.finalize(key, 9);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, first, 9));
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(binding.manager->status(key).value().input_credit_bytes.used(), 0U);

  const Bytes second = patterned_bytes(sizes.spoken_frame, 2);
  binding.audio(key, second);
  binding.finalize(key, 12);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               first.size() / sizes.sample, second, 12));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, FinalizationIsReportedBeforeItsAnswerIsOffered) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const auto queue = binding.output(key);
  // A lifecycle message the peer has not read leaves a result no room.
  OutputItem unread{
      OutputKind::Control,
      SessionBudgets::kOutputMetadataBytes - SessionBudgets::kOutputControlReserveBytes, 0,
      std::make_unique<OutputBody>()};
  ASSERT_EQ(queue->offer(unread, binding.clock.now()).value(), OutputOffer::Queued);
  std::uint64_t queued_at_completion = 0;
  binding.before_next(key, Event::FinalizeCompleted,
                      [&] { queued_at_completion = queue->metadata_usage().used(); });
  const AudioSizes sizes = sizes_of(binding, key);
  const Bytes frame = patterned_bytes(sizes.spoken_frame, 1);
  binding.audio(key, frame);
  binding.finalize(key, 6);
  if (detects_voice(binding)) {
    // Detecting voice, the dispatch offers the endpoint before it decodes,
    // and reports the finalization once the endpoint was queued.
    ASSERT_TRUE(binding.settle());
    EXPECT_EQ(binding.live_state(key), State::Finalizing);
  } else {
    ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));
    ASSERT_TRUE(binding.settle());
  }
  EXPECT_EQ(queue->metadata_usage().used(), unread.bytes);

  // Room made by a delivery that is not task output is room all the same.
  const auto lifecycle = queue->take();
  ASSERT_TRUE(lifecycle.has_value());
  ASSERT_TRUE(queue->delivered(lifecycle->sequence, binding.clock.now()));
  binding.dispatch().output_delivered(key, OutputKind::Control);
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));
  ASSERT_TRUE(binding.settle());
  // When the report was applied, the final was not yet queued.
  const OutputItem endpoint = make_task_output_item(
      EndpointDetectedOutput{1, frame.size() / sizes.sample, EndpointReason::ClientFinalize});
  EXPECT_EQ(queued_at_completion, detects_voice(binding) ? endpoint.bytes : unread.bytes);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, frame, 6));
}

TEST_P(SessionDispatchContract, SegmentIntervalFollowsTheUtteranceSamples) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  // One sample more than a frame of whole milliseconds: at the usual rates
  // the utterance does not end on a whole microsecond.
  const Bytes first = patterned_bytes(sizes.spoken_frame + sizes.sample, 1);
  binding.audio(key, first);
  binding.finalize(key, 1);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, first, 1));
  const Bytes second = patterned_bytes(sizes.spoken_frame, 2);
  binding.audio(key, second);
  binding.finalize(key, 2);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               first.size() / sizes.sample, second, 2));
}

TEST_P(SessionDispatchContract, FinalizeWithoutAudioYieldsFinalWithoutSegments) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  binding.finalize(key, 2);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, {}, 2));
}

TEST_P(SessionDispatchContract, TextSegmentYieldsChunksThenCompletion) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  binding.text(key, 7, "hello world");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 7, "hello world"));
  binding.text(key, 8, "hi");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 8, "hi"));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, SegmentKeepsItsSlotUntilItsOutputWasDelivered) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  const std::size_t speech = binding.under_test->speech("hello world").size();
  const std::size_t chunks = (speech - 1) / chunk_bytes(binding.terms(key)) + 1;
  binding.text(key, 1, "hello world");
  // The delivery of a lifecycle message is not the segment's.
  for (int message = 0; message < 3; ++message) {
    binding.dispatch().output_delivered(key, OutputKind::Control);
  }
  ASSERT_TRUE(
      binding.eventually([&] { return binding.output(key)->pcm_usage().used() == speech; }));
  ASSERT_TRUE(binding.settle());
  // Synthesized and queued, not read: still the active segment.
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 1U);

  for (std::size_t chunk = 0; chunk < chunks; ++chunk) {
    ASSERT_TRUE(binding.read_as<AudioChunkOutput>(key).has_value());
  }
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 1U);
  ASSERT_TRUE(binding.read_as<SegmentCompletedOutput>(key).has_value());
  EXPECT_TRUE(binding.eventually(
      [&] { return binding.manager->status(key).value().input_queue_depth == 0; }));
}

TEST_P(SessionDispatchContract, OutputWaitsForRoomAndNothingIsLost) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  const std::string longer = overflowing_text(binding, key);
  const std::size_t chunk = chunk_bytes(binding.terms(key));
  // More speech than the PCM budget holds.
  ASSERT_GT(binding.under_test->speech(longer).size(),
            binding.output(key)->pcm_usage().limit() + chunk);
  binding.text(key, 3, longer);
  binding.text(key, 4, "next");
  ASSERT_TRUE(binding.eventually([&] {
    const auto usage = binding.output(key)->pcm_usage();
    return usage.available() < chunk;
  }));
  ASSERT_TRUE(binding.settle());
  // The second segment waits behind the one being delivered.
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 2U);

  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 3, longer));
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 4, "next"));
}

TEST_P(SessionDispatchContract, TextFinalizeIsAnsweredOnceAcceptedSegmentsWereDelivered) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  const std::size_t sample = binding.terms(key).audio_format.bytes_per_sample();
  const std::uint64_t short_samples = binding.under_test->speech("hi").size() / sample;
  const std::uint64_t long_samples = binding.under_test->speech("hello world").size() / sample;
  binding.text(key, 1, "hi");
  binding.text(key, 2, "hello world");
  binding.finalize(key, 4);
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, "hi"));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 2, "hello world"));
  const auto answer = binding.read_as<SynthesisCompletedOutput>(key);
  ASSERT_TRUE(answer.has_value());
  EXPECT_EQ(answer->segment_count, 2U);
  EXPECT_EQ(answer->total_samples, short_samples + long_samples);
  EXPECT_EQ(answer->finalize_sequence, 4U);
  EXPECT_EQ(binding.live_state(key), State::Active);

  // The counts start again after each answer.
  binding.text(key, 3, "hi");
  binding.finalize(key, 7);
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 3, "hi"));
  const auto next = binding.read_as<SynthesisCompletedOutput>(key);
  ASSERT_TRUE(next.has_value());
  EXPECT_EQ(next->segment_count, 1U);
  EXPECT_EQ(next->total_samples, short_samples);
  EXPECT_EQ(next->finalize_sequence, 7U);
}

// The binding sends its credit updates from what the sink shows, so a credit
// return has to arrive there after the acceptance it follows.
TEST_P(SessionDispatchContract, CreditReturnsAndCompletedFinalizationsReachTheSinkInOrder) {
  Binding binding(GetParam());
  const auto no_event = [](const auto& moved) { return !moved.event.has_value(); };
  const auto audio_key = binding.open_audio();
  const std::size_t frame_bytes = sizes_of(binding, audio_key).spoken_frame;
  binding.audio(audio_key, patterned_bytes(frame_bytes, 1));
  const auto returned_after_data = [&] {
    const auto seen = binding.transitions(audio_key);
    const std::size_t data =
        first_from(seen, 0, [](const auto& moved) { return moved.event == Event::Data; });
    if (data == seen.size() || seen[data].status.input_credit_bytes.used() != frame_bytes) {
      return false;
    }
    return first_from(seen, data + 1, [&](const auto& moved) {
             return no_event(moved) && moved.status.input_credit_bytes.used() == 0;
           }) != seen.size();
  };
  ASSERT_TRUE(binding.eventually(returned_after_data));

  const auto text_key = binding.open_text();
  binding.text(text_key, 1, "hi");
  const auto started = [&](const auto& moved) {
    return no_event(moved) && moved.status.input_queue_depth == 1 &&
           moved.status.input_credit_bytes.used() == 0;
  };
  // The segment has started before the Finalize is applied.
  ASSERT_TRUE(binding.eventually([&] {
    const auto seen = binding.transitions(text_key);
    return first_from(seen, 0, started) != seen.size();
  }));
  binding.finalize(text_key, 3);
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(text_key, 1, "hi"));
  ASSERT_TRUE(binding.read_as<SynthesisCompletedOutput>(text_key).has_value());
  const auto seen = binding.transitions(text_key);
  // The segment's start, the Finalize, the segment finishing and the
  // finalization completing, in that order.
  std::size_t at = first_from(seen, 0, started);
  at = first_from(seen, at + 1, [](const auto& moved) { return moved.event == Event::Finalize; });
  at = first_from(seen, at + 1, [&](const auto& moved) {
    return no_event(moved) && moved.status.input_queue_depth == 0;
  });
  at = first_from(seen, at + 1, [](const auto& moved) {
    return moved.event == Event::FinalizeCompleted && moved.state == State::Active;
  });
  EXPECT_LT(at, seen.size());
}

TEST_P(SessionDispatchContract, CreditReturnedOnceInputStoppedIsNotReported) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(sizes_of(binding, key).spoken_frame, 1);
  const auto accepted = binding.manager->accept_input(key, frame.size());
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::AcceptInput));
  ASSERT_TRUE(binding.half_close(key));
  const auto reports = binding.credit_reports(key);
  const auto shown = binding.transitions(key).size();

  binding.dispatch().audio(key, frame);
  const auto credit_returned = [](const auto& moved) {
    return !moved.event.has_value() && moved.status.input_credit_bytes.used() == 0;
  };
  ASSERT_TRUE(binding.eventually([&] {
    const auto seen = binding.transitions(key);
    return first_from(seen, shown, credit_returned) != seen.size();
  }));
  const auto seen = binding.transitions(key);
  EXPECT_EQ(seen.at(first_from(seen, shown, credit_returned)).state, State::Draining);
  EXPECT_EQ(binding.credit_reports(key), reports);
}

TEST_P(SessionDispatchContract, HalfCloseFinalizesTheOpenUtteranceThenCloses) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(sizes_of(binding, key).spoken_frame, 1);
  binding.audio(key, frame);
  ASSERT_TRUE(binding.half_close(key));
  // Input that races a drain is not taken, so it is not handed over.
  binding.audio(key, patterned_bytes(frame.size(), 2));
  ASSERT_TRUE(
      binding.eventually([&] { return binding.output(key)->metadata_usage().used() != 0; }));
  ASSERT_TRUE(binding.settle());
  // The drain is not complete while its output is unread.
  EXPECT_EQ(binding.live_state(key), State::Draining);

  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::HalfClose, 1, 0, frame, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST_P(SessionDispatchContract, HalfCloseWithNothingOpenClosesWithoutOutput) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(sizes_of(binding, key).spoken_frame, 1);
  binding.audio(key, frame);
  binding.finalize(key, 3);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, frame, 3));
  ASSERT_TRUE(binding.half_close(key));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, HalfCloseCompletesAcceptedSegmentsThenCloses) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  binding.text(key, 1, "hi");
  binding.text(key, 2, "hello world");
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, "hi"));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 2, "hello world"));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST_P(SessionDispatchContract, WorkerDrainLeavesTheOpenUtteranceUntranscribed) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  binding.audio(key, patterned_bytes(sizes_of(binding, key).spoken_frame, 1));
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// A drain the worker starts can reach the dispatch before a Finalize or an
// input the manager had already accepted is handed over.
TEST_P(SessionDispatchContract, DrainWaitsForAFinalizeTheManagerAccepted) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(sizes_of(binding, key).spoken_frame, 1);
  binding.audio(key, frame);
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);
  EXPECT_FALSE(binding.output(key)->take().has_value());

  binding.dispatch().finalize(key, 9);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, frame, 9));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, HalfCloseDoesNotOvertakeAnAcceptedFinalize) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(sizes_of(binding, key).spoken_frame, 1);
  binding.audio(key, frame);
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());

  binding.dispatch().finalize(key, 9);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, frame, 9));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, HalfCloseDoesNotOvertakeAnAcceptedFrame) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const std::size_t frame_bytes = sizes_of(binding, key).spoken_frame;
  const Bytes audio = patterned_bytes(2 * frame_bytes, 1);
  binding.audio(key, std::span{audio}.first(frame_bytes));
  const auto accepted = binding.manager->accept_input(key, frame_bytes);
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::AcceptInput));
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);
  EXPECT_EQ(binding.output(key)->last_sequence(), 0U);
  EXPECT_EQ(binding.output(key)->metadata_usage().used(), 0U);

  binding.dispatch().audio(key, std::span{audio}.subspan(frame_bytes));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::HalfClose, 1, 0, audio, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, DrainWaitsForATextFinalizeTheManagerAccepted) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  binding.text(key, 1, "hi");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, "hi"));
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);

  binding.dispatch().finalize(key, 9);
  const auto answer = binding.read_as<SynthesisCompletedOutput>(key);
  ASSERT_TRUE(answer.has_value());
  EXPECT_EQ(answer->segment_count, 1U);
  EXPECT_EQ(answer->finalize_sequence, 9U);
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST_P(SessionDispatchContract, DrainWaitsForInputTheManagerAccepted) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  const auto accepted = binding.manager->accept_input(key, 2);
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::AcceptInput));
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);

  binding.dispatch().text_segment(key, 5, "hi");
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 5, "hi"));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST_P(SessionDispatchContract, TheLongestUtteranceIsEndedByAnAutomaticEndpoint) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const std::size_t longest = sizes.longest_utterance;
  // Whole frames, one of which straddles the limit: what is over starts the
  // next utterance. A first frame of the shortest moves the frame boundaries
  // off the limit where they would meet it.
  const std::size_t lead = longest % sizes.longest_frame == 0 ? sizes.shortest_frame : 0;
  ASSERT_NE((longest - lead) % sizes.longest_frame, 0U);
  const std::size_t whole = ((longest - lead) / sizes.longest_frame + 1) * sizes.longest_frame;
  const Bytes audio = patterned_bytes(lead + whole, 3);
  if (lead != 0) {
    binding.audio(key, std::span{audio}.first(lead));
  }
  ASSERT_TRUE(binding.audio_paced(key, std::span{audio}.subspan(lead), sizes.longest_frame));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0,
                                               std::span{audio}.first(longest), 0));
  EXPECT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));

  binding.finalize(key, 4);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               longest / sizes.sample,
                                               std::span{audio}.subspan(longest), 4));
  EXPECT_EQ(binding.live_state(key), State::Active);
}

// The finalization an automatic endpoint starts is the dispatch's own: no
// hand-over follows it, so a drain must not wait for one. Detecting voice,
// the dispatch reaches the limit once the frame that passes it was scored.
TEST_P(SessionDispatchContract, AnAutomaticEndpointOwesNoHandOver) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const std::size_t longest = sizes.longest_utterance;
  const Bytes audio = patterned_bytes(scored_length(sizes, longest), 3);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_TRUE(binding.eventually([&] { return shown(binding, key, Event::AutomaticEndpoint); }));
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0,
                                               std::span{audio}.first(longest), 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// An automatic endpoint raised while the finalization of an earlier one is
// running joins it: the two utterances are completed by one report.
// Detecting voice, the audio after an endpoint is scored only once its
// utterance's final is out: two endpoints are raised together when the frame
// that passes the limit also ends a silence run.
TEST_P(SessionDispatchContract, AnEndpointDuringAnEndpointsFinalizationJoinsIt) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const std::size_t longest = sizes.longest_utterance;
  const auto count = [&](Event event) {
    const auto seen = binding.transitions(key);
    return std::count_if(seen.begin(), seen.end(),
                         [&](const auto& moved) { return moved.event == event; });
  };
  binding.under_test->hold_jobs();
  if (const auto voice = binding.under_test->voice_detection()) {
    const std::size_t scored = scored_length(sizes, longest);
    const std::size_t quiet =
        scored_length(sizes, audio_bytes(binding.terms(key), voice->silence_endpoint));
    Bytes audio = patterned_bytes(scored - quiet, 6);
    audio.resize(scored + sizes.longest_frame, std::byte{0});
    ASSERT_TRUE(binding.audio_in_long_frames(key, std::span{audio}.first(scored)));
    ASSERT_TRUE(binding.audio_paced(key, std::span{audio}.subspan(scored), sizes.longest_frame));
    ASSERT_TRUE(binding.eventually([&] { return count(Event::AutomaticEndpoint) == 2; }));
    ASSERT_TRUE(binding.settle());
    EXPECT_EQ(binding.live_state(key), State::Finalizing);
    EXPECT_EQ(binding.manager->status(key).value().input_credit_bytes.used(), sizes.longest_frame);

    // Both endpoints are offered at detection; the second utterance lies
    // wholly inside the silence run.
    const auto second = std::span{audio}.subspan(longest, scored - longest);
    ASSERT_NO_FATAL_FAILURE(binding.expect_endpoint(key, EndpointReason::DurationLimit, 1, 0,
                                                    std::span{audio}.first(longest)));
    ASSERT_NO_FATAL_FAILURE(
        binding.expect_endpoint(key, EndpointReason::Silence, 2, longest / sizes.sample, second));
    binding.under_test->release_jobs();
    ASSERT_NO_FATAL_FAILURE(
        binding.expect_transcript(key, 1, 0, std::span{audio}.first(longest), 0));
    ASSERT_NO_FATAL_FAILURE(
        binding.expect_transcript_of(key, 2, longest / sizes.sample, second, 0, {}));
  } else {
    const Bytes audio = patterned_bytes(2 * longest, 6);
    ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
    ASSERT_TRUE(binding.eventually([&] { return count(Event::AutomaticEndpoint) >= 2; }));
    ASSERT_TRUE(binding.settle());
    EXPECT_EQ(binding.live_state(key), State::Finalizing);

    binding.under_test->release_jobs();
    ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0,
                                                 std::span{audio}.first(longest), 0));
    ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 2,
                                                 longest / sizes.sample,
                                                 std::span{audio}.subspan(longest), 0));
  }
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(count(Event::FinalizeCompleted), 1);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// The last frame of the longest utterance and the client's Finalize are
// worked on in either order: the endpoint joins the Finalize's finalization
// or the Finalize takes the endpoint's over. Detecting voice, the Finalize
// waits until that frame was scored, and ends the audio after the limit.
TEST_P(SessionDispatchContract, AFinalizeRightAfterTheLongestUtteranceIsCompletedOnce) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const std::size_t longest = sizes.longest_utterance;
  const Bytes audio = patterned_bytes(scored_length(sizes, longest), 8);
  const auto first = std::span{audio}.first(longest);
  const auto rest = std::span{audio}.subspan(longest);
  const bool voice = detects_voice(binding);
  std::atomic<std::uint64_t> queued_at_completion{0};
  if (voice) {
    // Held, the first utterance completes after the Finalize was handed
    // over and while its frames still wait to be scored: the finalization
    // is reported once the Finalize's endpoint was queued, before its final.
    record_at_completion(binding, key, queued_at_completion);
    binding.under_test->hold_jobs();
  }
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.finalize(key, 7);
  if (voice) {
    ASSERT_TRUE(binding.settle());
    binding.under_test->release_jobs();
    ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Active; }));
    EXPECT_EQ(queued_at_completion,
              utterance_charge(binding, 0, first, true) +
                  utterance_charge(binding, longest / sizes.sample, rest, false));
  }
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0, first, 0));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               longest / sizes.sample, rest, 7));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// The manager counts a Finalize that arrives during an automatic endpoint's
// finalization as the same one, so it is completed once, whichever of the
// Finalize and the first utterance's job the dispatch sees first.
TEST_P(SessionDispatchContract, AFinalizeHandedOverDuringAnEndpointsJobIsCompletedOnce) {
  ASSERT_NO_FATAL_FAILURE(finalize_takes_over(GetParam(), Takeover::BeforeTheJobEnds, 0));
}

TEST_P(SessionDispatchContract, AFinalizeHandedOverAfterAnEndpointsJobIsCompletedOnce) {
  ASSERT_NO_FATAL_FAILURE(finalize_takes_over(GetParam(), Takeover::AfterTheJobEnds, 0));
}

// The job ends inside the manager call that applies the Finalize. The
// synthetic dispatch then reports before it has recorded the Finalize;
// another dispatch may take either order.
TEST_P(SessionDispatchContract, AFinalizeAppliedAsAnEndpointsJobEndsIsCompletedOnce) {
  ASSERT_NO_FATAL_FAILURE(finalize_takes_over(GetParam(), Takeover::AsTheFinalizeIsApplied, 0));
}

TEST_P(SessionDispatchContract, AFinalizeRacingAnEndpointsJobIsCompletedOnce) {
  for (int round = 0; round < 8; ++round) {
    ASSERT_NO_FATAL_FAILURE(finalize_takes_over(GetParam(), Takeover::Racing, round))
        << "round " << round;
  }
}

// Only the client's own Finalize or half-close lets a frame be shorter than
// the smallest granted: an endpoint the dispatch raised does not.
TEST_P(SessionDispatchContract, AudioAfterAShortFrameFailsDespiteAnAutomaticEndpoint) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  ASSERT_NE(sizes.short_frame, 0U);
  const Bytes short_frame = patterned_bytes(sizes.short_frame, 1);
  binding.audio(key, short_frame);
  binding.finalize(key, 1);
  if (detects_voice(binding)) {
    // Shorter than the shortest speech run: silence.
    ASSERT_NO_FATAL_FAILURE(
        binding.expect_silent_final(key, EndpointReason::ClientFinalize, 1, 0, short_frame, 1));
  } else {
    ASSERT_NO_FATAL_FAILURE(
        binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, short_frame, 1));
  }
  EXPECT_EQ(binding.live_state(key), State::Active);

  // A short frame completes the longest utterance, which the dispatch ends.
  const std::size_t longest = sizes.longest_utterance;
  const Bytes audio = patterned_bytes(scored_length(sizes, longest), 3);
  const std::size_t lead = audio.size() - sizes.short_frame;
  ASSERT_TRUE(binding.audio_in_long_frames(key, std::span{audio}.first(lead)));
  ASSERT_TRUE(binding.audio_paced(key, std::span{audio}.subspan(lead), sizes.short_frame));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 2,
                                               short_frame.size() / sizes.sample,
                                               std::span{audio}.first(longest), 0));
  binding.audio(key, patterned_bytes(sizes.shortest_frame, 4));
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "short_frame_not_last");
  EXPECT_EQ(binding.manager->held_slots(), 0U);
}

TEST_P(SessionDispatchContract, AShortFrameIsValidBeforeAHalfClose) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  ASSERT_NE(sizes.short_frame, 0U);
  const Bytes audio = patterned_bytes(sizes.spoken_frame + sizes.short_frame, 1);
  binding.audio(key, std::span{audio}.first(sizes.spoken_frame));
  binding.audio(key, std::span{audio}.subspan(sizes.spoken_frame));
  ASSERT_TRUE(binding.half_close(key));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::HalfClose, 1, 0, audio, 0));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

// A Finalize repeated while one is running changes nothing, so the binding
// has nothing to hand over for it.
TEST_P(SessionDispatchContract, ARepeatedFinalizeIsNotHandedOver) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(sizes_of(binding, key).spoken_frame, 1);
  binding.under_test->hold_jobs();
  binding.audio(key, frame);
  binding.finalize(key, 1);
  binding.finalize(key, 2);
  ASSERT_TRUE(binding.settle());
  binding.under_test->release_jobs();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, frame, 1));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, CancelIsReleasedAndTheSessionForgotten) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  binding.text(key, 1, overflowing_text(binding, key));
  ASSERT_TRUE(binding.eventually([&] { return binding.held_sessions() == 1; }));
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
  // What raced the end does not bring the session back.
  binding.dispatch().text_segment(key, 2, "late");
  binding.dispatch().finalize(key, 8);
  binding.dispatch().output_delivered(key, OutputKind::Audio);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, CallsForASessionNeverOpenedAreIgnored) {
  Binding binding(GetParam());
  const auto opened = binding.open_audio();
  const std::uint64_t unknown = opened + 100;
  binding.dispatch().audio(unknown, patterned_bytes(sizes_of(binding, opened).shortest_frame, 1));
  binding.dispatch().text_segment(unknown, 1, "hi");
  binding.dispatch().finalize(unknown, 3);
  binding.dispatch().output_delivered(unknown, OutputKind::Result);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 1U);
  EXPECT_EQ(binding.manager->held_slots(), 1U);
  // The session that was opened is not disturbed.
  binding.finalize(opened, 2);
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(opened, EndpointReason::ClientFinalize, 1, 0, {}, 2));
}

// The binding's lifecycle messages and the dispatch's output enter one queue
// from two threads, each waking the stream's writer.
TEST_P(SessionDispatchContract, LifecycleMessagesAndTaskOutputShareTheQueue) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  const std::string text = overflowing_text(binding, key);
  const std::size_t chunk = chunk_bytes(binding.terms(key));
  constexpr std::uint64_t kMessages = 64;
  binding.text(key, 1, text);
  // The dispatch holds a chunk the queue had no room for, so each read below
  // makes it offer while the other thread offers too.
  ASSERT_TRUE(
      binding.eventually([&] { return binding.output(key)->pcm_usage().available() < chunk; }));
  bool queued = true;
  std::thread lifecycle([&] {
    for (std::uint64_t message = 0; message < kMessages; ++message) {
      queued = queued && binding.lifecycle(key, 16);
      std::this_thread::yield();
    }
  });
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, text));
  lifecycle.join();
  ASSERT_TRUE(queued);
  // What is left is lifecycle only, and a wake-up that finds nothing ends it.
  EXPECT_FALSE(binding.read(key, 50ms).has_value());
  EXPECT_EQ(binding.lifecycle_read(key), kMessages);
}

TEST_P(SessionDispatchContract, CancelWithAJobHeldIsReleasedOnceTheJobEnds) {
  Binding binding(GetParam());
  const auto text_key = binding.open_text();
  const auto audio_key = binding.open_audio();
  binding.under_test->hold_jobs();
  binding.text(text_key, 1, "hello world");
  binding.audio(audio_key, patterned_bytes(sizes_of(binding, audio_key).spoken_frame, 1));
  binding.finalize(audio_key, 2);
  ASSERT_TRUE(binding.settle());
  ASSERT_TRUE(binding.manager->apply(text_key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.manager->apply(audio_key, Event::Cancel).has_value());
  ASSERT_TRUE(binding.settle());

  binding.under_test->release_jobs();
  EXPECT_TRUE(binding.ends_as(text_key, State::Closed));
  EXPECT_TRUE(binding.ends_as(audio_key, State::Closed));
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
  EXPECT_FALSE(binding.output(text_key)->take().has_value());
  EXPECT_FALSE(binding.output(audio_key)->take().has_value());
}

TEST_P(SessionDispatchContract, CallsReturnWhileAJobIsHeld) {
  Binding binding(GetParam());
  const auto text_key = binding.open_text();
  const auto audio_key = binding.open_audio();
  const Bytes frame = patterned_bytes(sizes_of(binding, audio_key).spoken_frame, 1);
  binding.under_test->hold_jobs();
  binding.text(text_key, 1, "hello");
  ASSERT_TRUE(binding.settle());

  const bool prompt = returns_promptly(
      [&] {
        binding.text(text_key, 2, "hi");
        binding.finalize(text_key, 3);
        binding.dispatch().output_delivered(text_key, OutputKind::Control);
        binding.audio(audio_key, frame);
        binding.finalize(audio_key, 4);
        (void)binding.dispatch().negotiate(binding.under_test->text_request());
      },
      [&] { binding.under_test->release_jobs(); });
  ASSERT_TRUE(prompt);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(text_key)->take().has_value());
  if (detects_voice(binding)) {
    // Detecting voice, the endpoint is offered before the decode ends.
    ASSERT_NO_FATAL_FAILURE(
        binding.expect_endpoint(audio_key, EndpointReason::ClientFinalize, 1, 0, frame));
  }
  EXPECT_FALSE(binding.output(audio_key)->take().has_value());

  binding.under_test->release_jobs();
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(text_key, 1, "hello"));
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(text_key, 2, "hi"));
  const auto answer = binding.read_as<SynthesisCompletedOutput>(text_key);
  ASSERT_TRUE(answer.has_value());
  EXPECT_EQ(answer->finalize_sequence, 3U);
  if (!detects_voice(binding)) {
    ASSERT_NO_FATAL_FAILURE(
        binding.expect_endpoint(audio_key, EndpointReason::ClientFinalize, 1, 0, frame));
  }
  ASSERT_NO_FATAL_FAILURE(binding.expect_transcript(audio_key, 1, 0, frame, 4));
}

TEST_P(SessionDispatchContract, ATimeoutFromTheTimerThreadIsCleanedUp) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  binding.audio(key, patterned_bytes(sizes_of(binding, key).spoken_frame, 1));
  ASSERT_TRUE(binding.eventually([&] { return binding.held_sessions() == 1; }));
  // No client is heard and no call is made: only the timer acts.
  binding.clock.advance(SessionLimits::defaults().idle_timeout() + 1s);
  binding.manager->notify_clock_advanced();
  ASSERT_TRUE(binding.eventually([&] { return binding.manager->tombstone(key).has_value(); }));
  EXPECT_EQ(binding.manager->tombstone(key)->code, Error::Code::Timeout);
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
}

TEST_P(SessionDispatchContract, AHandOverThatNeverArrivesEndsByTheFinalizeDeadline) {
  Binding binding(GetParam());
  const auto key = binding.open_audio();
  binding.audio(key, patterned_bytes(sizes_of(binding, key).spoken_frame, 1));
  const auto accepted = binding.manager->apply(key, Event::Finalize);
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::StartFinalize));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);
  EXPECT_FALSE(binding.output(key)->take().has_value());

  binding.clock.advance(finalize_deadline(SessionInputKind::Audio) + 1s);
  binding.manager->notify_clock_advanced();
  ASSERT_TRUE(binding.eventually([&] { return binding.manager->tombstone(key).has_value(); }));
  EXPECT_EQ(binding.manager->tombstone(key)->reason, "finalize_timeout");
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, AFailedSessionWithOutputHeldIsReleased) {
  Binding binding(GetParam());
  const auto key = binding.open_text();
  const std::size_t chunk = chunk_bytes(binding.terms(key));
  binding.text(key, 1, overflowing_text(binding, key));
  // The queue is full and the dispatch keeps what it could not offer.
  ASSERT_TRUE(
      binding.eventually([&] { return binding.output(key)->pcm_usage().available() < chunk; }));
  ASSERT_TRUE(binding.settle());
  const auto failed = binding.manager->apply(
      key, Event::Fail, Error::make(Error::Code::Internal, "stream broke", "stream_broken"));
  ASSERT_TRUE(failed.has_value());
  ASSERT_TRUE(binding.ends_as(key, State::Failed));
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
  // Suppressed, and what the dispatch held is not offered afterwards.
  EXPECT_FALSE(binding.output(key)->take().has_value());
  binding.dispatch().output_delivered(key, OutputKind::Audio);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST_P(SessionDispatchContract, CleanupIsAcknowledgedForASessionThatNeverOpenedHere) {
  Binding binding(GetParam());
  const auto terms = binding.dispatch().negotiate(binding.under_test->audio_request()).value();
  const auto refused = binding.manager->open(
      Binding::kGeneration, budgets_for(terms).value(), [](std::uint64_t) -> Result<void> {
        return unexpected(
            Error::make(Error::Code::Unavailable, "backend is gone", "backend_unavailable"));
      });
  ASSERT_FALSE(refused.has_value());
  // Key 1: the slot is returned only by the dispatch's acknowledgement.
  EXPECT_TRUE(binding.ends_as(1, State::Failed));
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  EXPECT_EQ(binding.held_sessions(), 0U);
}

TEST_P(SessionDispatchContract, InputOfTheWrongKindFailsTheSession) {
  Binding binding(GetParam());
  const auto text_key = binding.open_text();
  const auto audio_key = binding.open_audio();
  binding.dispatch().audio(text_key,
                           patterned_bytes(sizes_of(binding, audio_key).shortest_frame, 1));
  EXPECT_TRUE(binding.ends_as(text_key, State::Failed));

  binding.dispatch().text_segment(audio_key, 1, "hi");
  EXPECT_TRUE(binding.ends_as(audio_key, State::Failed));
}

TEST_P(SessionDispatchContract, StopEndsReportsAndCallsAfterItAreIgnored) {
  Binding binding(GetParam());
  const auto audio_key = binding.open_audio();
  const auto text_key = binding.open_text();
  ASSERT_TRUE(binding.settle());
  std::thread other([&] { binding.dispatch().stop(); });
  binding.dispatch().stop();
  other.join();
  binding.dispatch().stop();

  // No thread is left to report or offer, and every call still returns.
  binding.audio(audio_key, patterned_bytes(sizes_of(binding, audio_key).spoken_frame, 1));
  binding.finalize(audio_key, 1);
  binding.text(text_key, 1, "hi");
  binding.dispatch().output_delivered(text_key, OutputKind::Audio);
  ASSERT_TRUE(binding.manager->apply(text_key, Event::Cancel).has_value());
  EXPECT_TRUE(binding.dispatch().negotiate(binding.under_test->audio_request()).has_value());
  EXPECT_FALSE(binding.output(audio_key)->take().has_value());
  EXPECT_FALSE(binding.output(text_key)->take().has_value());
  EXPECT_EQ(binding.live_state(audio_key), State::Finalizing);
  // The cancelled session keeps its slot: nobody acknowledges its cleanup.
  EXPECT_EQ(binding.manager->held_slots(), 2U);
}

// A reader, a cancel and the dispatch's own reports race; every session must
// still end closed with its slot returned.
TEST_P(SessionDispatchContract, CancelRacingDeliveryAlwaysReleases) {
  for (int round = 0; round < 40; ++round) {
    Binding binding(GetParam());
    const auto key = binding.open_text();
    binding.text(key, 1, overflowing_text(binding, key));
    std::thread reader([&] {
      for (int reads = 0; reads < 20 + round * 4; ++reads) {
        if (!binding.read(key, 50ms)) {
          return;
        }
      }
    });
    std::thread canceller([&] {
      std::this_thread::sleep_for(std::chrono::microseconds{round * 50});
      (void)binding.manager->apply(key, Event::Cancel);
    });
    reader.join();
    canceller.join();
    ASSERT_TRUE(binding.ends_as(key, State::Closed)) << "round " << round;
    ASSERT_EQ(binding.manager->held_slots(), 0U);
  }
}

INSTANTIATE_TEST_SUITE_P(Synthetic, SessionDispatchContract,
                         ::testing::Values(DispatchCase{"synthetic", &synthetic}),
                         SessionDispatchContract::name_of);

// The dispatch that runs backend jobs, over the fixture backend in each of
// the outcomes a backend may choose between, and once with the bridge's
// callbacks run before the call that caused them returns.
INSTANTIATE_TEST_SUITE_P(
    Speech, SessionDispatchContract,
    ::testing::Values(
        DispatchCase{"speech", &speech_dispatch},
        DispatchCase{"speech_refusing_at_submit", &speech_dispatch_refusing_at_submit},
        DispatchCase{"speech_with_late_cancels", &speech_dispatch_with_late_cancels},
        DispatchCase{"speech_with_callbacks_first", &speech_dispatch_with_callbacks_first},
        DispatchCase{"speech_detecting_voice", &speech_dispatch_detecting_voice}),
    SessionDispatchContract::name_of);

TEST(SyntheticSessionDispatch, NegotiateGrantsTermsOrRefusesBeforeAdmission) {
  Binding binding(&synthetic);
  const auto audio = binding.dispatch().negotiate(binding.under_test->audio_request());
  ASSERT_TRUE(audio.has_value());
  EXPECT_EQ(audio->input_kind, SessionInputKind::Audio);
  EXPECT_EQ(audio->audio_format, kPcm16k);
  EXPECT_EQ(audio->language, "en");
  EXPECT_EQ(audio->min_frame, 20ms);
  EXPECT_EQ(audio->max_frame, 320ms);
  EXPECT_EQ(audio->max_utterance, 30s);

  const auto text = binding.dispatch().negotiate(binding.under_test->text_request());
  ASSERT_TRUE(text.has_value());
  EXPECT_EQ(text->audio_format, kPcm24k);
  EXPECT_EQ(text->voice, "voice-a");
  EXPECT_EQ(text->max_segment_text_bytes, 4'096U);
  EXPECT_EQ(text->max_segment_audio, 30s);
  EXPECT_EQ(text->max_synthesis_text_bytes, 12'288U);
  EXPECT_EQ(text->max_synthesis_audio, 90s);
  EXPECT_EQ(budgets_for(*text).value(), SessionBudgets::for_text_input(48'000).value());
  EXPECT_EQ(budgets_for(*audio).value(), SessionBudgets::for_audio_input(32'000).value());

  const auto refusal = [&](const SessionRequest& request) -> std::string {
    const auto refused = binding.dispatch().negotiate(request);
    EXPECT_FALSE(refused.has_value());
    if (refused.has_value()) {
      return std::string{};
    }
    EXPECT_EQ(refused.error().code, Error::Code::Unsupported);
    return refused.error().context.value_or("");
  };
  auto telephony = binding.under_test->audio_request();
  telephony.input_format = {StreamAudioEncoding::Mulaw, 8'000, 1};
  EXPECT_EQ(refusal(telephony), "unsupported_audio_format");
  auto other_language = binding.under_test->audio_request();
  other_language.language = "fr";
  EXPECT_EQ(refusal(other_language), "unsupported_language");
  other_language = binding.under_test->text_request();
  other_language.language = "";
  EXPECT_EQ(refusal(other_language), "unsupported_language");
  auto other_voice = binding.under_test->text_request();
  other_voice.voice = "voice-z";
  EXPECT_EQ(refusal(other_voice), "unsupported_voice");
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  for (const auto language : Fake::kLanguages) {
    for (const auto voice : Fake::kVoices) {
      const SessionRequest served{
          SessionInputKind::Text, std::string{language}, std::string{voice}, {}};
      EXPECT_TRUE(binding.dispatch().negotiate(served).has_value());
    }
  }
}

TEST(SyntheticSessionDispatch, OutputIsAFunctionOfTheBytesHandedOver) {
  const Bytes one = patterned_bytes(kFrameBytes, 1);
  Bytes other = one;
  other[100] ^= std::byte{1};
  EXPECT_EQ(Fake::transcript_of(one).rfind("pcm-640-", 0), 0U);
  EXPECT_NE(Fake::transcript_of(one), Fake::transcript_of(other));
  EXPECT_NE(Fake::transcript_of(one), Fake::transcript_of(std::span{one}.first(kFrameBytes - 2)));
  EXPECT_EQ(Fake::sample_of("ab", 0), 'a' * 128);
  EXPECT_EQ(Fake::sample_of("ab", 79), 'a' * 128 + 79);
  EXPECT_EQ(Fake::sample_of("ab", 80), 'b' * 128);
  EXPECT_EQ(Fake::sample_of("\xff", 79), 32'719);

  Binding binding(&synthetic);
  const auto key = binding.open_audio();
  binding.audio(key, one);
  binding.finalize(key, 1);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  const auto first = binding.read_as<FinalTranscriptOutput>(key);
  binding.audio(key, other);
  binding.finalize(key, 2);
  ASSERT_TRUE(binding.read_as<EndpointDetectedOutput>(key).has_value());
  const auto second = binding.read_as<FinalTranscriptOutput>(key);
  ASSERT_TRUE(first && second);
  ASSERT_EQ(first->segments.size(), 1U);
  ASSERT_EQ(second->segments.size(), 1U);
  EXPECT_EQ(first->segments.front().text, Fake::transcript_of(one));
  EXPECT_EQ(second->segments.front().text, Fake::transcript_of(other));
}

TEST(SyntheticSessionDispatch, AParkedThreadKeepsInputAcceptedAndItsCreditOwed) {
  Binding binding(&synthetic);
  Fake& fake = fake_of(binding);
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(kFrameBytes, 1);
  ASSERT_TRUE(binding.settle());
  fake.pause(Fake::Gate::BeforeWork);
  // Only a hand-over parks the thread: a session still opens and is released.
  const auto other = binding.open_text();
  ASSERT_TRUE(binding.manager->apply(other, Event::Cancel).has_value());
  EXPECT_TRUE(binding.ends_as(other, State::Closed));

  const bool prompt = returns_promptly(
      [&] {
        binding.audio(key, frame);
        binding.finalize(key, 5);
        fake.output_delivered(key, OutputKind::Control);
      },
      [&] { fake.resume(); });
  ASSERT_TRUE(prompt);
  EXPECT_FALSE(fake.wait_idle(50ms));
  const auto status = binding.manager->status(key).value();
  EXPECT_EQ(status.state, State::Finalizing);
  EXPECT_EQ(status.input_queue_depth, 1U);
  EXPECT_EQ(status.input_credit_bytes.used(), kFrameBytes);
  EXPECT_FALSE(binding.output(key)->take().has_value());

  fake.resume();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, frame, 5));
  EXPECT_EQ(binding.manager->status(key).value().input_credit_bytes.used(), 0U);

  // A Finalize is a hand-over too.
  fake.pause(Fake::Gate::BeforeWork);
  binding.finalize(key, 6);
  EXPECT_FALSE(fake.wait_idle(50ms));
  EXPECT_FALSE(binding.output(key)->take().has_value());
  fake.resume();
  ASSERT_NO_FATAL_FAILURE(
      binding.expect_final(key, EndpointReason::ClientFinalize, 2, kFrameBytes / 2, {}, 6));
}

// A dispatch is shown a Finalize after the manager applied it, so the
// completion it applies for an endpoint's finalization may already have
// answered a Finalize it has not seen.
TEST(SyntheticSessionDispatch, ACompletionAppliedBeforeAFinalizeWasShownAnswersIt) {
  Binding binding(&synthetic);
  Fake& fake = fake_of(binding);
  const auto key = binding.open_audio();
  const std::size_t longest = sizes_of(binding, key).longest_utterance;
  const Bytes audio = patterned_bytes(longest + kFrameBytes, 5);
  fake.pause(Fake::Gate::InJob);
  ASSERT_TRUE(binding.audio_in_long_frames(key, std::span{audio}.first(longest)));
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Finalizing; }));
  ASSERT_TRUE(binding.settle());

  // Parked at the frame, the thread ends the first utterance's job from
  // that frame's step, before it is shown the Finalize queued behind it.
  fake.pause(Fake::Gate::BeforeWork);
  binding.audio(key, std::span{audio}.subspan(longest));
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  fake.resume();
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0,
                                               std::span{audio}.first(longest), 0));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);

  binding.dispatch().finalize(key, 7);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2, longest / 2,
                                               std::span{audio}.subspan(longest), 7));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
}

// The binding's hook runs before the dispatch is shown the transition: a job
// released from it is reported before the Finalize is recorded, so the
// session is active again although the Finalize was not handed over.
TEST(SyntheticSessionDispatch, AJobReleasedFromTheSinkHookIsReportedBeforeTheFinalizeIsRecorded) {
  Binding binding(&synthetic);
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const Bytes audio = patterned_bytes(sizes.longest_utterance, 5);
  binding.under_test->hold_jobs();
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  ASSERT_TRUE(binding.eventually([&] { return binding.live_state(key) == State::Finalizing; }));
  ASSERT_TRUE(binding.settle());

  binding.before_next(key, Event::Finalize, [&] { binding.under_test->release_jobs(); });
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0, audio, 0));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);

  binding.dispatch().finalize(key, 7);
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               audio.size() / sizes.sample, {}, 7));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
}

// The last frame of the longest utterance is worked on after the manager
// applied the client's Finalize, so the endpoint it raises joins that
// finalization.
TEST(SyntheticSessionDispatch, AnEndpointRaisedDuringAFinalizesFinalizationJoinsIt) {
  Binding binding(&synthetic);
  Fake& fake = fake_of(binding);
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const Bytes audio = patterned_bytes(sizes.longest_utterance, 9);
  const std::size_t head = audio.size() - kFrameBytes;
  ASSERT_TRUE(binding.audio_in_long_frames(key, std::span{audio}.first(head)));
  ASSERT_TRUE(binding.settle());
  fake.pause(Fake::Gate::BeforeWork);
  binding.audio(key, std::span{audio}.subspan(head));
  binding.finalize(key, 7);
  EXPECT_FALSE(shown(binding, key, Event::AutomaticEndpoint));
  fake.resume();

  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::DurationLimit, 1, 0, audio, 0));
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 2,
                                               audio.size() / sizes.sample, {}, 7));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  const auto seen = binding.transitions(key);
  const std::size_t asked =
      first_from(seen, 0, [](const auto& moved) { return moved.event == Event::Finalize; });
  const std::size_t joined = first_from(
      seen, asked + 1, [](const auto& moved) { return moved.event == Event::AutomaticEndpoint; });
  ASSERT_LT(joined, seen.size());
  EXPECT_EQ(seen[joined].state, State::Finalizing);
}

// What the binding's own pacing leaves for a last frame must not be a short
// frame in the middle of the audio.
TEST(SyntheticSessionDispatch, LongFramesEndOnAFrameTheTermsGrant) {
  Binding binding(&synthetic);
  const auto key = binding.open_audio();
  const AudioSizes sizes = sizes_of(binding, key);
  const Bytes audio = patterned_bytes(2 * sizes.longest_frame + sizes.short_frame, 1);
  const Bytes more = patterned_bytes(kFrameBytes, 2);
  ASSERT_TRUE(binding.audio_in_long_frames(key, audio));
  binding.audio(key, more);
  binding.finalize(key, 1);
  Bytes all = audio;
  all.insert(all.end(), more.begin(), more.end());
  ASSERT_NO_FATAL_FAILURE(binding.expect_final(key, EndpointReason::ClientFinalize, 1, 0, all, 1));
}

// EXPECT_NONFATAL_FAILURE takes a statement that names no local.
Binding* stray_binding = nullptr;
std::uint64_t stray_key = 0;
void read_the_stray_item() {
  (void)stray_binding->read(stray_key, 50ms);
}

// A control item the binding did not queue itself is not passed over.
TEST(SyntheticSessionDispatch, ReadFailsOnAControlItemTheBindingDidNotQueue) {
  Binding binding(&synthetic);
  const auto key = binding.open_text();
  ASSERT_TRUE(binding.lifecycle(key, 16));
  OutputItem stray{OutputKind::Control, 16, 0, std::make_unique<OutputBody>()};
  ASSERT_EQ(binding.output(key)->offer(stray, binding.clock.now()).value(), OutputOffer::Queued);
  stray_binding = &binding;
  stray_key = key;
  EXPECT_NONFATAL_FAILURE(read_the_stray_item(), "an item that is not task output");
  stray_binding = nullptr;
  EXPECT_EQ(binding.lifecycle_read(key), 1U);
}

TEST(SyntheticSessionDispatch, InputOfTheWrongKindIsRefusedAsSuch) {
  Binding binding(&synthetic);
  const auto text_key = binding.open_text();
  binding.dispatch().audio(text_key, patterned_bytes(kFrameBytes, 1));
  ASSERT_TRUE(binding.ends_as(text_key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(text_key)->reason, "wrong_input_kind");

  const auto audio_key = binding.open_audio();
  binding.dispatch().text_segment(audio_key, 1, "hi");
  ASSERT_TRUE(binding.ends_as(audio_key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(audio_key)->reason, "wrong_input_kind");
}

// wait_idle() is what gives a negative assertion after settle() its meaning:
// a command taken off the queue and still being handled is not idle.
TEST(SyntheticSessionDispatch, ACommandBeingHandledIsNotIdle) {
  Binding binding(&synthetic);
  Fake& fake = fake_of(binding);
  const auto key = binding.open_audio();
  const Bytes frame = patterned_bytes(kFrameBytes, 1);
  ASSERT_TRUE(binding.settle());
  const auto accepted = binding.manager->accept_input(key, frame.size());
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::AcceptInput));
  ASSERT_TRUE(binding.settle());

  // The initialize hook holds the manager's serialized section, which the
  // frame's credit return waits for.
  const auto terms = fake.negotiate(binding.under_test->text_request()).value();
  bool idle_inside = true;
  const auto opened = binding.manager->open(Binding::kGeneration, budgets_for(terms).value(),
                                            [&](std::uint64_t other) {
                                              fake.audio(key, frame);
                                              idle_inside = fake.wait_idle(50ms);
                                              return fake.open_session(other, terms);
                                            });
  ASSERT_TRUE(opened.has_value());
  EXPECT_FALSE(idle_inside);
  EXPECT_TRUE(fake.wait_idle(Binding::kPatience));
  EXPECT_EQ(binding.manager->status(key).value().input_credit_bytes.used(), 0U);
}

TEST(SyntheticSessionDispatch, StopWakesAWaitForIdle) {
  Binding binding(&synthetic);
  Fake& fake = fake_of(binding);
  const auto key = binding.open_audio();
  ASSERT_TRUE(binding.settle());
  fake.pause(Fake::Gate::BeforeWork);
  binding.audio(key, patterned_bytes(kFrameBytes, 1));
  ASSERT_FALSE(fake.wait_idle(50ms));

  std::promise<bool> idle;
  auto woken = idle.get_future();
  std::thread waiter([&] { idle.set_value(fake.wait_idle(2 * Binding::kPatience)); });
  // The waiter is, as far as a test can tell, asleep on the parked command.
  std::this_thread::sleep_for(50ms);
  fake.stop();
  const bool prompt = woken.wait_for(Binding::kPatience) == std::future_status::ready;
  waiter.join();
  ASSERT_TRUE(prompt);
  EXPECT_TRUE(woken.get());
}

TEST(SyntheticSessionDispatch, ResumeKeepsTheOrderOfWhatWasHeld) {
  Binding binding(&synthetic);
  Fake& fake = fake_of(binding);
  const auto key = binding.open_text();
  ASSERT_TRUE(binding.settle());
  fake.pause(Fake::Gate::BeforeWork);
  binding.text(key, 1, "first");
  binding.text(key, 2, "second");
  binding.finalize(key, 9);
  EXPECT_FALSE(fake.wait_idle(50ms));
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 2U);

  fake.resume();
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 1, "first"));
  ASSERT_NO_FATAL_FAILURE(binding.expect_segment(key, 2, "second"));
  const auto answer = binding.read_as<SynthesisCompletedOutput>(key);
  ASSERT_TRUE(answer.has_value());
  EXPECT_EQ(answer->segment_count, 2U);
  EXPECT_EQ(answer->finalize_sequence, 9U);
}

TEST(SyntheticSessionDispatch, AHeldJobKeepsItsSessionsCleanupUnanswered) {
  Binding binding(&synthetic);
  Fake& fake = fake_of(binding);
  const auto text_key = binding.open_text();
  const auto audio_key = binding.open_audio();
  const auto idle_key = binding.open_text();
  fake.pause(Fake::Gate::InJob);
  binding.text(text_key, 1, "hello world");
  binding.audio(audio_key, patterned_bytes(kFrameBytes, 1));
  binding.finalize(audio_key, 2);
  ASSERT_TRUE(binding.eventually(
      [&] { return binding.manager->status(text_key).value().input_credit_bytes.used() == 0; }));
  ASSERT_TRUE(binding.settle());
  // The segment and the utterance have started and yield nothing.
  EXPECT_EQ(binding.manager->status(text_key).value().input_queue_depth, 1U);
  EXPECT_FALSE(binding.output(text_key)->take().has_value());
  EXPECT_FALSE(binding.output(audio_key)->take().has_value());

  for (const auto key : {text_key, audio_key, idle_key}) {
    ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  }
  // A session with no job is released at once; the other two wait.
  EXPECT_TRUE(binding.ends_as(idle_key, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.manager->held_slots(), 2U);
  EXPECT_EQ(binding.held_sessions(), 2U);
  EXPECT_FALSE(binding.manager->tombstone(text_key).has_value());
  EXPECT_FALSE(binding.manager->tombstone(audio_key).has_value());

  fake.resume();
  EXPECT_TRUE(binding.ends_as(text_key, State::Closed));
  EXPECT_TRUE(binding.ends_as(audio_key, State::Closed));
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  EXPECT_FALSE(binding.output(text_key)->take().has_value());
  EXPECT_FALSE(binding.output(audio_key)->take().has_value());
}

TEST(SessionDispatch, TaskOutputItemsAreChargedByKind) {
  AudioChunkOutput chunk;
  chunk.pcm.resize(960);
  const auto audio = make_task_output_item(std::move(chunk));
  EXPECT_EQ(audio.kind, OutputKind::Audio);
  EXPECT_EQ(audio.bytes, 960U);
  EXPECT_EQ(audio.replace_key, 0U);

  FinalTranscriptOutput transcript;
  transcript.utterance_id = 4;
  transcript.segments = {{0, 1, "hello"}, {1, 2, "you"}};
  const auto final_item = make_task_output_item(std::move(transcript));
  EXPECT_EQ(final_item.kind, OutputKind::Result);
  EXPECT_EQ(final_item.bytes, kTaskOutputMessageUnits + 2 * kTaskOutputSegmentUnits + 8);
  EXPECT_EQ(final_item.replace_key, 4U);

  const auto completed = make_task_output_item(SegmentCompletedOutput{1, 880, 2});
  EXPECT_EQ(completed.kind, OutputKind::Result);
  EXPECT_EQ(completed.bytes, kTaskOutputMessageUnits);
  ASSERT_NE(dynamic_cast<TaskOutputBody*>(completed.body.get()), nullptr);

  const auto endpoint = make_task_output_item(EndpointDetectedOutput{4, 320, {}});
  EXPECT_EQ(endpoint.kind, OutputKind::Result);
  EXPECT_EQ(endpoint.bytes, kTaskOutputMessageUnits);
  // Only a final takes the place of its utterance's unsent partial.
  EXPECT_EQ(endpoint.replace_key, 0U);
  const auto answer = make_task_output_item(SynthesisCompletedOutput{1, 160, 7});
  EXPECT_EQ(answer.kind, OutputKind::Result);
  EXPECT_EQ(answer.bytes, kTaskOutputMessageUnits);
}
}  // namespace
}  // namespace tensorplate::testing
