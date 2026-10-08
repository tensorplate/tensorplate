// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#include "fake_scheduler_clock.hpp"
#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"
#include "serving/session/synthetic_dispatch.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;
using State = LogicalSessionState;
using Fake = SyntheticSessionDispatch;

constexpr std::uint64_t kGeneration = 5;
constexpr StreamAudioFormat kPcm16k{StreamAudioEncoding::PcmS16Le, 16'000, 1};
constexpr StreamAudioFormat kPcm24k{StreamAudioEncoding::PcmS16Le, 24'000, 1};
/// 20 ms of kPcm16k: 320 samples.
constexpr std::size_t kFrameBytes = 640;
constexpr std::size_t kChunkBytes = std::size_t{Fake::kChunkSamples} * 2;
constexpr auto kPatience = 5s;

SessionRequest audio_request() {
  return {SessionInputKind::Audio, "en", "", kPcm16k};
}
SessionRequest text_request() {
  return {SessionInputKind::Text, "en-US", "voice-a", {}};
}

/// What a transport binding does around a dispatch, with no wire: it drives
/// the manager, forwards as SessionDispatch asks, and reads each queue as the
/// peer would.
struct Binding {
  Binding()
      : dispatch(clock),
        manager(SessionManager::create(SessionLimits::defaults(), kGeneration, clock,
                                       [this](const auto& transition) { sink(transition); })
                    .value()) {
    dispatch.attach(*manager);
  }
  ~Binding() { dispatch.stop(); }
  Binding(const Binding&) = delete;
  Binding& operator=(const Binding&) = delete;

  void sink(const ManagedSessionTransition& transition) {
    dispatch.on_transition(transition);
    bool first = false;
    {
      const std::lock_guard guard(mu);
      first = outputs.emplace(transition.session_key, transition.output).second;
      sunk[transition.session_key].push_back(transition);
    }
    if (first) {
      EXPECT_TRUE(transition.output->set_consumer([this] { changed(); }));
    }
    changed();
  }

  /// Something a waiting test may be looking for has happened.
  void changed() {
    {
      const std::lock_guard guard(mu);
      ++changes;
    }
    cv.notify_all();
  }

  std::uint64_t open(const SessionRequest& request) {
    const auto terms = dispatch.negotiate(request).value();
    const auto budgets = budgets_for(terms);
    return manager
        ->open(kGeneration, budgets.value(),
               [&](std::uint64_t key) { return dispatch.open_session(key, terms); })
        .value()
        .session_key;
  }

  void audio(std::uint64_t key, std::size_t bytes = kFrameBytes) {
    const auto accepted = manager->accept_input(key, bytes);
    if (accepted && accepted->effects.contains(Effect::AcceptInput)) {
      const std::vector<std::byte> frame(bytes);
      dispatch.audio(key, frame);
    }
  }

  void text(std::uint64_t key, std::uint64_t segment_id, std::string text) {
    const auto accepted = manager->accept_input(key, text.size());
    if (accepted && accepted->effects.contains(Effect::AcceptInput)) {
      dispatch.text_segment(key, segment_id, std::move(text));
    }
  }

  void finalize(std::uint64_t key, std::uint64_t client_sequence) {
    const auto applied = manager->apply(key, Event::Finalize);
    if (applied && applied->effects.contains(Effect::StartFinalize)) {
      dispatch.finalize(key, client_sequence);
    }
  }

  std::shared_ptr<BoundedOutputQueue> output(std::uint64_t key) {
    const std::lock_guard guard(mu);
    return outputs.at(key);
  }

  /// The next task output the peer reads, confirmed delivered; none if the
  /// stream stays silent for `patience`.
  std::optional<TaskOutput> read(std::uint64_t key,
                                 std::chrono::milliseconds patience = kPatience) {
    const auto queue = output(key);
    std::optional<SequencedOutput> message;
    const bool taken = eventually(
        [&] {
          message = queue->take();
          return message.has_value();
        },
        patience);
    if (!taken) {
      return std::nullopt;
    }
    EXPECT_TRUE(queue->delivered(message->sequence, clock.now()));
    dispatch.output_delivered(key, message->item.kind);
    auto* body = dynamic_cast<TaskOutputBody*>(message->item.body.get());
    if (body == nullptr) {
      ADD_FAILURE() << "an item that is not task output";
      return std::nullopt;
    }
    return std::move(body->output);
  }

  template <typename Output>
  std::optional<Output> read_as(std::uint64_t key) {
    auto next = read(key);
    if (!next || !std::holds_alternative<Output>(*next)) {
      ADD_FAILURE() << "expected another kind of output";
      return std::nullopt;
    }
    return std::get<Output>(std::move(*next));
  }

  /// An utterance's endpoint and then its final, which must agree.
  std::optional<FinalTranscriptOutput> read_final(std::uint64_t key, EndpointReason reason) {
    const auto endpoint = read_as<EndpointDetectedOutput>(key);
    auto transcript = read_as<FinalTranscriptOutput>(key);
    if (!endpoint || !transcript) {
      return std::nullopt;
    }
    EXPECT_EQ(endpoint->reason, reason);
    EXPECT_EQ(endpoint->utterance_id, transcript->utterance_id);
    EXPECT_EQ(endpoint->end_sample_offset, transcript->end_sample_offset);
    return transcript;
  }

  /// True once the dispatch has finished everything handed to it so far. It
  /// works through one queue in order, so an exchange on a session of its
  /// own that completes afterwards proves it; the further rounds cover what
  /// the dispatch's own reports queued behind the first.
  [[nodiscard]] bool settle() {
    if (barrier == 0) {
      barrier = open(audio_request());
    }
    for (int round = 0; round < 3; ++round) {
      finalize(barrier, 0);
      if (!read(barrier) || !read(barrier)) {
        return false;
      }
    }
    return true;
  }

  /// The state of a session that still holds its slot.
  std::optional<State> live_state(std::uint64_t key) const {
    const auto state = manager->state(key);
    return state ? std::optional{*state} : std::nullopt;
  }

  /// Every transition the sink received for `key`, in order.
  std::vector<ManagedSessionTransition> transitions(std::uint64_t key) {
    const std::lock_guard guard(mu);
    return sunk[key];
  }

  /// Sessions the dispatch holds besides the barrier's.
  std::size_t held_sessions() const { return dispatch.held_sessions() - (barrier == 0 ? 0U : 1U); }

  /// Waits for `done`, which is tried again after every sink call and every
  /// queued output. `done` runs unlocked: it may call the manager.
  template <typename Predicate>
  bool eventually(Predicate&& done, std::chrono::milliseconds patience = kPatience) {
    const auto deadline = std::chrono::steady_clock::now() + patience;
    for (;;) {
      std::uint64_t seen = 0;
      {
        const std::lock_guard guard(mu);
        seen = changes;
      }
      if (done()) {
        return true;
      }
      std::unique_lock lock(mu);
      if (!cv.wait_until(lock, deadline, [&] { return changes != seen; })) {
        lock.unlock();
        return done();
      }
    }
  }

  bool ends_as(std::uint64_t key, State state) {
    return eventually([&] {
      const auto tombstone = manager->tombstone(key);
      return tombstone && tombstone->state == state;
    });
  }

  testing::FakeSchedulerClock clock;
  std::mutex mu;
  std::map<std::uint64_t, std::shared_ptr<BoundedOutputQueue>> outputs;
  std::map<std::uint64_t, std::vector<ManagedSessionTransition>> sunk;
  std::condition_variable cv;
  std::uint64_t changes = 0;
  std::uint64_t barrier = 0;
  Fake dispatch;
  // Last, so that what its sink uses outlives it.
  std::unique_ptr<SessionManager> manager;
};

Error worker_shutdown() {
  return Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown");
}

std::string text_of(std::size_t bytes) {
  return std::string(bytes, 'a');
}

/// Reads a whole segment and checks its chunks are contiguous, sized and
/// filled as the fake defines.
void expect_segment(Binding& binding, std::uint64_t key, std::uint64_t segment_id,
                    std::uint64_t total_samples) {
  std::uint64_t samples = 0;
  std::uint32_t chunks = 0;
  while (samples < total_samples) {
    const auto chunk = binding.read_as<AudioChunkOutput>(key);
    ASSERT_TRUE(chunk.has_value());
    EXPECT_EQ(chunk->segment_id, segment_id);
    EXPECT_EQ(chunk->chunk_index, chunks);
    EXPECT_EQ(chunk->segment_sample_offset, samples);
    const std::uint64_t expected =
        std::min<std::uint64_t>(Fake::kChunkSamples, total_samples - samples);
    ASSERT_EQ(chunk->pcm.size(), expected * 2);
    const auto first = static_cast<std::uint16_t>(Fake::sample_at(samples));
    const auto last = static_cast<std::uint16_t>(Fake::sample_at(samples + expected - 1));
    EXPECT_EQ(chunk->pcm.front(), static_cast<std::byte>(first & 0xffU));
    EXPECT_EQ(chunk->pcm.at(1), static_cast<std::byte>(first >> 8U));
    EXPECT_EQ(chunk->pcm.at(chunk->pcm.size() - 2), static_cast<std::byte>(last & 0xffU));
    EXPECT_EQ(chunk->pcm.back(), static_cast<std::byte>(last >> 8U));
    samples += expected;
    ++chunks;
  }
  const auto completed = binding.read_as<SegmentCompletedOutput>(key);
  ASSERT_TRUE(completed.has_value());
  EXPECT_EQ(completed->segment_id, segment_id);
  EXPECT_EQ(completed->total_samples, total_samples);
  EXPECT_EQ(completed->chunk_count, chunks);
}

TEST(SessionDispatch, AudioThenFinalizeYieldsOneFinalPerUtterance) {
  Binding binding;
  const auto key = binding.open(audio_request());
  for (int frame = 0; frame < 3; ++frame) {
    binding.audio(key);
  }
  binding.finalize(key, 9);

  const auto first = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(first.has_value());
  EXPECT_EQ(first->utterance_id, 1U);
  EXPECT_EQ(first->finalize_sequence, 9U);
  EXPECT_EQ(first->end_sample_offset, 960U);
  ASSERT_EQ(first->segments.size(), 1U);
  EXPECT_EQ(first->segments.front().text, Fake::kTranscript);
  EXPECT_EQ(first->segments.front().start_us, 0U);
  EXPECT_EQ(first->segments.front().end_us, 60'000U);
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(binding.manager->status(key).value().input_credit_bytes.used(), 0U);

  binding.audio(key);
  binding.finalize(key, 12);
  const auto second = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(second.has_value());
  EXPECT_EQ(second->utterance_id, 2U);
  EXPECT_EQ(second->finalize_sequence, 12U);
  EXPECT_EQ(second->end_sample_offset, 1'280U);
  ASSERT_EQ(second->segments.size(), 1U);
  EXPECT_EQ(second->segments.front().start_us, 60'000U);
  EXPECT_EQ(second->segments.front().end_us, 80'000U);
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST(SessionDispatch, FinalizationIsReportedBeforeItsAnswerIsOffered) {
  Binding binding;
  const auto key = binding.open(audio_request());
  const auto queue = binding.output(key);
  // A lifecycle message the peer has not read leaves a result no room.
  OutputItem unread{
      OutputKind::Control,
      SessionBudgets::kOutputMetadataBytes - SessionBudgets::kOutputControlReserveBytes, 0,
      std::make_unique<OutputBody>()};
  ASSERT_EQ(queue->offer(unread, binding.clock.now()).value(), OutputOffer::Queued);
  binding.audio(key);
  binding.finalize(key, 6);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Active);
  EXPECT_EQ(queue->metadata_usage().used(), unread.bytes);

  // Room made by a delivery that is not task output is room all the same.
  const auto lifecycle = queue->take();
  ASSERT_TRUE(lifecycle.has_value());
  ASSERT_TRUE(queue->delivered(lifecycle->sequence, binding.clock.now()));
  binding.dispatch.output_delivered(key, OutputKind::Control);
  const auto transcript = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->finalize_sequence, 6U);
}

TEST(SessionDispatch, SegmentIntervalIsRoundedOutward) {
  Binding binding;
  const auto key = binding.open(audio_request());
  // 321 samples end at 20,062.5 ms.
  binding.audio(key, 642);
  binding.finalize(key, 1);
  const auto first = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(first.has_value());
  ASSERT_EQ(first->segments.size(), 1U);
  EXPECT_EQ(first->segments.front().end_us, 20'063U);
  binding.audio(key);
  binding.finalize(key, 2);
  const auto second = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(second.has_value());
  ASSERT_EQ(second->segments.size(), 1U);
  EXPECT_EQ(second->segments.front().start_us, 20'062U);
}

TEST(SessionDispatch, FinalizeWithoutAudioYieldsFinalWithoutSegments) {
  Binding binding;
  const auto key = binding.open(audio_request());
  binding.finalize(key, 2);
  const auto transcript = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->utterance_id, 1U);
  EXPECT_TRUE(transcript->segments.empty());
  EXPECT_EQ(transcript->end_sample_offset, 0U);
}

TEST(SessionDispatch, TextSegmentYieldsChunksThenCompletion) {
  Binding binding;
  const auto key = binding.open(text_request());
  // 11 bytes: 880 samples, one whole chunk and a shorter last one.
  binding.text(key, 7, "hello world");
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 7, 880));
  binding.text(key, 8, "hi");
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 8, 160));
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST(SessionDispatch, SegmentKeepsItsSlotUntilItsOutputWasDelivered) {
  Binding binding;
  const auto key = binding.open(text_request());
  binding.text(key, 1, "hello world");
  // The delivery of a lifecycle message is not the segment's.
  for (int message = 0; message < 3; ++message) {
    binding.dispatch.output_delivered(key, OutputKind::Control);
  }
  ASSERT_TRUE(binding.settle());
  // Synthesized and queued, not read: still the active segment.
  EXPECT_EQ(binding.output(key)->pcm_usage().used(), 1'760U);
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 1U);

  ASSERT_TRUE(binding.read_as<AudioChunkOutput>(key).has_value());
  ASSERT_TRUE(binding.read_as<AudioChunkOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 1U);
  ASSERT_TRUE(binding.read_as<SegmentCompletedOutput>(key).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 0U);
}

TEST(SessionDispatch, OutputWaitsForRoomAndNothingIsLost) {
  Binding binding;
  const auto key = binding.open(text_request());
  // 1,000 bytes: 80,000 samples, 160,000 bytes of PCM against a budget of
  // 96,000, in 167 chunks.
  binding.text(key, 3, text_of(1'000));
  binding.text(key, 4, "next");
  ASSERT_TRUE(binding.settle());
  const auto usage = binding.output(key)->pcm_usage();
  EXPECT_EQ(usage.limit(), 96'000U);
  EXPECT_EQ(usage.used(), 100 * kChunkBytes);
  // The second segment waits behind the one being delivered.
  EXPECT_EQ(binding.manager->status(key).value().input_queue_depth, 2U);

  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 3, 80'000));
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 4, 320));
}

TEST(SessionDispatch, TextFinalizeIsAnsweredOnceAcceptedSegmentsWereDelivered) {
  Binding binding;
  const auto key = binding.open(text_request());
  binding.text(key, 1, "hi");
  binding.text(key, 2, "hello world");
  binding.finalize(key, 4);
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 1, 160));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 2, 880));
  const auto answer = binding.read_as<SynthesisCompletedOutput>(key);
  ASSERT_TRUE(answer.has_value());
  EXPECT_EQ(answer->segment_count, 2U);
  EXPECT_EQ(answer->total_samples, 1'040U);
  EXPECT_EQ(answer->finalize_sequence, 4U);
  EXPECT_EQ(binding.live_state(key), State::Active);

  // The counts start again after each answer.
  binding.text(key, 3, "hi");
  binding.finalize(key, 7);
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 3, 160));
  const auto next = binding.read_as<SynthesisCompletedOutput>(key);
  ASSERT_TRUE(next.has_value());
  EXPECT_EQ(next->segment_count, 1U);
  EXPECT_EQ(next->total_samples, 160U);
  EXPECT_EQ(next->finalize_sequence, 7U);
}

// The binding sends its credit updates from what the sink shows, so a credit
// return has to arrive there after the acceptance it follows.
TEST(SessionDispatch, CreditReturnsAndCompletedFinalizationsReachTheSinkInOrder) {
  Binding binding;
  const auto audio_key = binding.open(audio_request());
  binding.audio(audio_key);
  ASSERT_TRUE(binding.settle());
  auto seen = binding.transitions(audio_key);
  ASSERT_EQ(seen.size(), 3U);
  EXPECT_EQ(seen.at(1).event, Event::Data);
  EXPECT_EQ(seen.at(1).status.input_credit_bytes.used(), kFrameBytes);
  EXPECT_FALSE(seen.at(2).event.has_value());
  EXPECT_EQ(seen.at(2).status.input_credit_bytes.used(), 0U);

  const auto text_key = binding.open(text_request());
  binding.text(text_key, 1, "hi");
  // The segment has started before the Finalize is applied.
  ASSERT_TRUE(binding.settle());
  binding.finalize(text_key, 3);
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, text_key, 1, 160));
  ASSERT_TRUE(binding.read_as<SynthesisCompletedOutput>(text_key).has_value());
  seen = binding.transitions(text_key);
  // Ready, the segment, its start, the Finalize, the segment finishing and
  // the finalization completing.
  ASSERT_EQ(seen.size(), 6U);
  EXPECT_FALSE(seen.at(2).event.has_value());
  EXPECT_EQ(seen.at(2).status.input_queue_depth, 1U);
  EXPECT_EQ(seen.at(2).status.input_credit_bytes.used(), 0U);
  EXPECT_EQ(seen.at(3).event, Event::Finalize);
  EXPECT_FALSE(seen.at(4).event.has_value());
  EXPECT_EQ(seen.at(4).status.input_queue_depth, 0U);
  EXPECT_EQ(seen.at(5).event, Event::FinalizeCompleted);
  EXPECT_EQ(seen.at(5).state, State::Active);
}

TEST(SessionDispatch, HalfCloseFinalizesTheOpenUtteranceThenCloses) {
  Binding binding;
  const auto key = binding.open(audio_request());
  binding.audio(key);
  ASSERT_TRUE(binding.manager->apply(key, Event::HalfClose).has_value());
  ASSERT_TRUE(binding.settle());
  // The drain is not complete while its output is unread.
  EXPECT_EQ(binding.live_state(key), State::Draining);

  const auto transcript = binding.read_final(key, EndpointReason::HalfClose);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->finalize_sequence, 0U);
  EXPECT_EQ(transcript->end_sample_offset, 320U);
  ASSERT_EQ(transcript->segments.size(), 1U);
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SessionDispatch, HalfCloseWithNothingOpenClosesWithoutOutput) {
  Binding binding;
  const auto key = binding.open(audio_request());
  binding.audio(key);
  binding.finalize(key, 3);
  ASSERT_TRUE(binding.read_final(key, EndpointReason::ClientFinalize).has_value());
  ASSERT_TRUE(binding.manager->apply(key, Event::HalfClose).has_value());
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST(SessionDispatch, HalfCloseCompletesAcceptedSegmentsThenCloses) {
  Binding binding;
  const auto key = binding.open(text_request());
  binding.text(key, 1, "hi");
  binding.text(key, 2, "hello world");
  ASSERT_TRUE(binding.manager->apply(key, Event::HalfClose).has_value());
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 1, 160));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 2, 880));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SessionDispatch, WorkerDrainLeavesTheOpenUtteranceUntranscribed) {
  Binding binding;
  const auto key = binding.open(audio_request());
  binding.audio(key);
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

// A drain the worker starts can reach the dispatch before a Finalize or an
// input the manager had already accepted is handed over.
TEST(SessionDispatch, DrainWaitsForAFinalizeTheManagerAccepted) {
  Binding binding;
  const auto key = binding.open(audio_request());
  binding.audio(key);
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);
  EXPECT_FALSE(binding.output(key)->take().has_value());

  binding.dispatch.finalize(key, 9);
  const auto transcript = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->utterance_id, 1U);
  EXPECT_EQ(transcript->finalize_sequence, 9U);
  EXPECT_EQ(transcript->segments.size(), 1U);
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST(SessionDispatch, HalfCloseDoesNotOvertakeAnAcceptedFinalize) {
  Binding binding;
  const auto key = binding.open(audio_request());
  binding.audio(key);
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_TRUE(binding.manager->apply(key, Event::HalfClose).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_FALSE(binding.output(key)->take().has_value());

  binding.dispatch.finalize(key, 9);
  const auto transcript = binding.read_final(key, EndpointReason::ClientFinalize);
  ASSERT_TRUE(transcript.has_value());
  EXPECT_EQ(transcript->finalize_sequence, 9U);
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST(SessionDispatch, DrainWaitsForATextFinalizeTheManagerAccepted) {
  Binding binding;
  const auto key = binding.open(text_request());
  binding.text(key, 1, "hi");
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 1, 160));
  ASSERT_TRUE(binding.manager->apply(key, Event::Finalize).has_value());
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);

  binding.dispatch.finalize(key, 9);
  const auto answer = binding.read_as<SynthesisCompletedOutput>(key);
  ASSERT_TRUE(answer.has_value());
  EXPECT_EQ(answer->segment_count, 1U);
  EXPECT_EQ(answer->finalize_sequence, 9U);
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

// Only a client's Finalize is handed over: the finalization an automatic
// endpoint starts must not keep a drain waiting for one.
TEST(SessionDispatch, AnAutomaticEndpointOwesNoHandOver) {
  Binding binding;
  const auto key = binding.open(audio_request());
  ASSERT_TRUE(binding.manager->apply(key, Event::AutomaticEndpoint).has_value());
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SessionDispatch, DrainWaitsForInputTheManagerAccepted) {
  Binding binding;
  const auto key = binding.open(text_request());
  const auto accepted = binding.manager->accept_input(key, 2);
  ASSERT_TRUE(accepted.has_value());
  ASSERT_TRUE(accepted->effects.contains(Effect::AcceptInput));
  ASSERT_TRUE(binding.manager->apply(key, Event::Drain, worker_shutdown()).has_value());
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.live_state(key), State::Draining);

  binding.dispatch.text_segment(key, 5, "hi");
  ASSERT_NO_FATAL_FAILURE(expect_segment(binding, key, 5, 160));
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
}

TEST(SessionDispatch, CancelIsReleasedAndTheSessionForgotten) {
  Binding binding;
  const auto key = binding.open(text_request());
  binding.text(key, 1, text_of(1'000));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 1U);
  ASSERT_TRUE(binding.manager->apply(key, Event::Cancel).has_value());
  EXPECT_TRUE(binding.ends_as(key, State::Closed));
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
  // What raced the end does not bring the session back.
  binding.dispatch.text_segment(key, 2, "late");
  binding.dispatch.finalize(key, 8);
  binding.dispatch.output_delivered(key, OutputKind::Audio);
  ASSERT_TRUE(binding.settle());
  EXPECT_EQ(binding.held_sessions(), 0U);
  EXPECT_FALSE(binding.output(key)->take().has_value());
}

TEST(SessionDispatch, CleanupIsAcknowledgedForASessionThatNeverOpenedHere) {
  Binding binding;
  const auto budgets = SessionBudgets::for_audio_input(kPcm16k.bytes_per_second()).value();
  const auto refused =
      binding.manager->open(kGeneration, budgets, [](std::uint64_t) -> Result<void> {
        return unexpected(
            Error::make(Error::Code::Unavailable, "backend is gone", "backend_unavailable"));
      });
  ASSERT_FALSE(refused.has_value());
  // Key 1: the slot is returned only by the dispatch's acknowledgement.
  EXPECT_TRUE(binding.ends_as(1, State::Failed));
  EXPECT_EQ(binding.manager->held_slots(), 0U);
  EXPECT_EQ(binding.held_sessions(), 0U);
}

TEST(SessionDispatch, InputOfTheWrongKindFailsTheSession) {
  Binding binding;
  const auto text_key = binding.open(text_request());
  const std::vector<std::byte> frame(kFrameBytes);
  binding.dispatch.audio(text_key, frame);
  ASSERT_TRUE(binding.ends_as(text_key, State::Failed));
  EXPECT_EQ(binding.manager->tombstone(text_key)->reason, "wrong_input_kind");

  const auto audio_key = binding.open(audio_request());
  binding.dispatch.text_segment(audio_key, 1, "hi");
  EXPECT_TRUE(binding.ends_as(audio_key, State::Failed));
}

TEST(SessionDispatch, NegotiateGrantsTermsOrRefusesBeforeAdmission) {
  Binding binding;
  const auto audio = binding.dispatch.negotiate(audio_request());
  ASSERT_TRUE(audio.has_value());
  EXPECT_EQ(audio->input_kind, SessionInputKind::Audio);
  EXPECT_EQ(audio->audio_format, kPcm16k);
  EXPECT_EQ(audio->language, "en");
  EXPECT_EQ(audio->min_frame, 20ms);
  EXPECT_EQ(audio->max_frame, 320ms);
  EXPECT_EQ(audio->max_utterance, 30s);

  const auto text = binding.dispatch.negotiate(text_request());
  ASSERT_TRUE(text.has_value());
  EXPECT_EQ(text->audio_format, kPcm24k);
  EXPECT_EQ(text->voice, "voice-a");
  EXPECT_EQ(text->max_segment_text_bytes, 4'096U);
  EXPECT_EQ(text->max_segment_audio, 30s);
  EXPECT_EQ(text->max_synthesis_text_bytes, 12'288U);
  EXPECT_EQ(text->max_synthesis_audio, 90s);
  EXPECT_EQ(budgets_for(*text).value(), SessionBudgets::for_text_input(48'000).value());
  EXPECT_EQ(budgets_for(*audio).value(), SessionBudgets::for_audio_input(32'000).value());

  auto telephony = audio_request();
  telephony.input_format = {StreamAudioEncoding::Mulaw, 8'000, 1};
  const auto refused = binding.dispatch.negotiate(telephony);
  ASSERT_FALSE(refused.has_value());
  EXPECT_EQ(refused.error().code, Error::Code::Unsupported);
  EXPECT_EQ(refused.error().context, "unsupported_audio_format");
  EXPECT_EQ(binding.manager->held_slots(), 0U);
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

TEST(SessionDispatch, StopEndsReportsAndMayBeRepeated) {
  Binding binding;
  const auto key = binding.open(audio_request());
  ASSERT_TRUE(binding.settle());
  std::thread other([&] { binding.dispatch.stop(); });
  binding.dispatch.stop();
  other.join();
  binding.dispatch.stop();
  // No thread is left to report or offer.
  binding.finalize(key, 1);
  EXPECT_FALSE(binding.output(key)->take().has_value());
  EXPECT_EQ(binding.live_state(key), State::Finalizing);
}

// A reader, a cancel and the dispatch's own reports race; every session must
// still end closed with its slot returned.
TEST(SessionDispatch, CancelRacingDeliveryAlwaysReleases) {
  for (int round = 0; round < 40; ++round) {
    Binding binding;
    const auto key = binding.open(text_request());
    binding.text(key, 1, text_of(1'000));
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
}  // namespace
}  // namespace tensorplate::serving
