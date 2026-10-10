// SPDX-License-Identifier: Apache-2.0
#pragma once

// A transport binding with no wire, and what it needs of the dispatch it
// drives. The cases written against it hold for every SessionDispatch.

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
#include <string_view>
#include <utility>
#include <variant>
#include <vector>

#include "fake_scheduler_clock.hpp"
#include "serving/session/dispatch.hpp"
#include "serving/session/session_manager.hpp"

namespace tensorplate::testing {

/// The dispatch a ReferenceBinding drives, and what its cases must know of
/// it: what it serves, what it produces for given input, and how to hold it.
class DispatchUnderTest {
 public:
  DispatchUnderTest() = default;
  DispatchUnderTest(const DispatchUnderTest&) = delete;
  DispatchUnderTest& operator=(const DispatchUnderTest&) = delete;
  virtual ~DispatchUnderTest() = default;

  [[nodiscard]] virtual serving::SessionDispatch& dispatch() = 0;
  /// A request of each input kind that the dispatch grants.
  [[nodiscard]] virtual serving::SessionRequest audio_request() const = 0;
  [[nodiscard]] virtual serving::SessionRequest text_request() const = 0;
  /// The segments of the final of an utterance that holds `audio` and starts
  /// at `first_sample` of the session's input. None for no audio.
  [[nodiscard]] virtual std::vector<serving::TranscriptSegment> transcript(
      std::uint64_t first_sample, std::span<const std::byte> audio) const = 0;
  /// The PCM of the whole segment synthesized from `text`.
  [[nodiscard]] virtual std::vector<std::byte> speech(std::string_view text) const = 0;
  /// True once everything handed over so far was worked through, as far as
  /// held jobs allow.
  [[nodiscard]] virtual bool settled(std::chrono::milliseconds patience) = 0;
  /// Sessions the dispatch still keeps state for.
  [[nodiscard]] virtual std::size_t held_sessions() const = 0;
  /// From now until release_jobs(), a job that has started does not
  /// complete.
  virtual void hold_jobs() = 0;
  virtual void release_jobs() = 0;
};

using DispatchFactory = std::unique_ptr<DispatchUnderTest> (*)(const SchedulerClock&);

/// `bytes` of input that differ from one `seed` to the next.
[[nodiscard]] inline std::vector<std::byte> patterned_bytes(std::size_t bytes, unsigned seed) {
  std::vector<std::byte> out(bytes);
  for (std::size_t index = 0; index < bytes; ++index) {
    out[index] = static_cast<std::byte>((seed * 131U + index * 7U) & 0xffU);
  }
  return out;
}

/// What a transport binding does around a dispatch: it drives the manager,
/// keeps the binding's side of SessionDispatch and reads each queue as the
/// peer would. Helpers that assert are called through ASSERT_NO_FATAL_FAILURE.
class ReferenceBinding {
 public:
  using Effect = serving::LogicalSessionEffect;
  using Event = serving::LogicalSessionEvent;
  using State = serving::LogicalSessionState;

  static constexpr std::uint64_t kGeneration = 5;
  static constexpr std::chrono::seconds kPatience{5};

  explicit ReferenceBinding(DispatchFactory factory)
      : under_test(factory(clock)),
        manager(
            serving::SessionManager::create(serving::SessionLimits::defaults(), kGeneration, clock,
                                            [this](const auto& transition) { sink(transition); })
                .value()) {
    dispatch().attach(*manager);
  }
  ~ReferenceBinding() {
    under_test->release_jobs();
    dispatch().stop();
  }
  ReferenceBinding(const ReferenceBinding&) = delete;
  ReferenceBinding& operator=(const ReferenceBinding&) = delete;

  [[nodiscard]] serving::SessionDispatch& dispatch() { return under_test->dispatch(); }

  std::uint64_t open(const serving::SessionRequest& request) {
    const auto terms = dispatch().negotiate(request).value();
    const auto budgets = serving::budgets_for(terms);
    const std::uint64_t key =
        manager
            ->open(kGeneration, budgets.value(),
                   [&](std::uint64_t opened) { return dispatch().open_session(opened, terms); })
            .value()
            .session_key;
    const std::lock_guard guard(mu);
    streams[key].terms = terms;
    return key;
  }
  std::uint64_t open_audio() { return open(under_test->audio_request()); }
  std::uint64_t open_text() { return open(under_test->text_request()); }

  /// One frame as a stream takes it: checked, accepted, handed over.
  void audio(std::uint64_t key, std::span<const std::byte> frame) {
    bool after_short_frame = false;
    {
      const std::lock_guard guard(mu);
      Stream& stream = streams[key];
      after_short_frame = stream.short_frame_sent;
      const auto& format = stream.terms.audio_format;
      const auto smallest = format.bytes_per_second() *
                            static_cast<std::uint64_t>(stream.terms.min_frame.count()) / 1000;
      stream.short_frame_sent = frame.size() < smallest;
    }
    // Only a Finalize or a half-close may follow a short frame.
    if (after_short_frame) {
      (void)manager->apply(key, Event::Fail,
                           Error::make(Error::Code::ConfigInvalid,
                                       "audio after a frame shorter than the smallest granted",
                                       "short_frame_not_last"));
      return;
    }
    const auto accepted = manager->accept_input(key, frame.size());
    if (accepted && accepted->effects.contains(Effect::AcceptInput)) {
      dispatch().audio(key, frame);
    }
  }

  /// Sends `audio` in frames of `frame_bytes`, each once the credit the sink
  /// last showed has room for it. False if credit stops returning.
  [[nodiscard]] bool audio_paced(std::uint64_t key, std::span<const std::byte> audio_bytes,
                                 std::size_t frame_bytes) {
    while (!audio_bytes.empty()) {
      const auto frame = audio_bytes.first(std::min(frame_bytes, audio_bytes.size()));
      const bool room = eventually([&] {
        const std::lock_guard guard(mu);
        return streams[key].credit.available() >= frame.size();
      });
      if (!room) {
        return false;
      }
      audio(key, frame);
      audio_bytes = audio_bytes.subspan(frame.size());
    }
    return true;
  }

  void text(std::uint64_t key, std::uint64_t segment_id, std::string text) {
    const auto accepted = manager->accept_input(key, text.size());
    if (accepted && accepted->effects.contains(Effect::AcceptInput)) {
      dispatch().text_segment(key, segment_id, std::move(text));
    }
  }

  void finalize(std::uint64_t key, std::uint64_t client_sequence) {
    ends_utterance(key);
    const auto applied = manager->apply(key, Event::Finalize);
    if (applied && applied->effects.contains(Effect::StartFinalize)) {
      dispatch().finalize(key, client_sequence);
    }
  }

  [[nodiscard]] bool half_close(std::uint64_t key) {
    ends_utterance(key);
    return manager->apply(key, Event::HalfClose).has_value();
  }

  std::shared_ptr<serving::BoundedOutputQueue> output(std::uint64_t key) {
    const std::lock_guard guard(mu);
    return streams.at(key).output;
  }

  serving::SessionTerms terms(std::uint64_t key) {
    const std::lock_guard guard(mu);
    return streams.at(key).terms;
  }

  /// Every transition the sink received for `key`, in order.
  std::vector<serving::ManagedSessionTransition> transitions(std::uint64_t key) {
    const std::lock_guard guard(mu);
    return streams[key].sunk;
  }

  /// How many credit reports a stream would have written for `key`.
  std::uint64_t credit_reports(std::uint64_t key) {
    const std::lock_guard guard(mu);
    return streams[key].credit_reports;
  }

  /// The next task output the peer reads, confirmed delivered; none if the
  /// stream stays silent for `patience`.
  std::optional<serving::TaskOutput> read(std::uint64_t key,
                                          std::chrono::milliseconds patience = kPatience) {
    const auto queue = output(key);
    std::optional<serving::SequencedOutput> message;
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
    dispatch().output_delivered(key, message->item.kind);
    auto* body = dynamic_cast<serving::TaskOutputBody*>(message->item.body.get());
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

  /// Reads an utterance's endpoint and final and checks both against what
  /// the dispatch yields for `audio`, the utterance's whole input.
  void expect_final(std::uint64_t key, serving::EndpointReason reason, std::uint64_t utterance_id,
                    std::uint64_t first_sample, std::span<const std::byte> audio_bytes,
                    std::uint64_t finalize_sequence) {
    const auto endpoint = read_as<serving::EndpointDetectedOutput>(key);
    ASSERT_TRUE(endpoint.has_value());
    const auto transcript = read_as<serving::FinalTranscriptOutput>(key);
    ASSERT_TRUE(transcript.has_value());
    const std::uint64_t end_sample =
        first_sample + audio_bytes.size() / terms(key).audio_format.bytes_per_sample();
    EXPECT_EQ(endpoint->reason, reason);
    EXPECT_EQ(endpoint->utterance_id, utterance_id);
    EXPECT_EQ(endpoint->end_sample_offset, end_sample);
    EXPECT_EQ(transcript->utterance_id, utterance_id);
    EXPECT_EQ(transcript->end_sample_offset, end_sample);
    EXPECT_EQ(transcript->finalize_sequence, finalize_sequence);
    const auto expected = under_test->transcript(first_sample, audio_bytes);
    ASSERT_EQ(transcript->segments.size(), expected.size());
    for (std::size_t index = 0; index < expected.size(); ++index) {
      EXPECT_EQ(transcript->segments[index].text, expected[index].text);
      EXPECT_EQ(transcript->segments[index].start_us, expected[index].start_us);
      EXPECT_EQ(transcript->segments[index].end_us, expected[index].end_us);
    }
  }

  /// Reads a whole segment and checks that its chunks are contiguous, hold
  /// kAudioChunkDuration each but the last, and carry the speech of `text`.
  void expect_segment(std::uint64_t key, std::uint64_t segment_id, std::string_view text) {
    const auto format = terms(key).audio_format;
    const std::size_t chunk_bytes =
        format.bytes_per_second() *
        static_cast<std::uint64_t>(serving::kAudioChunkDuration.count()) / 1000;
    const auto speech = under_test->speech(text);
    std::size_t offset = 0;
    std::uint32_t chunks = 0;
    while (offset < speech.size()) {
      const auto chunk = read_as<serving::AudioChunkOutput>(key);
      ASSERT_TRUE(chunk.has_value());
      EXPECT_EQ(chunk->segment_id, segment_id);
      EXPECT_EQ(chunk->chunk_index, chunks);
      EXPECT_EQ(chunk->segment_sample_offset, offset / format.bytes_per_sample());
      const std::size_t expected = std::min(chunk_bytes, speech.size() - offset);
      ASSERT_EQ(chunk->pcm.size(), expected);
      ASSERT_TRUE(std::equal(chunk->pcm.begin(), chunk->pcm.end(), speech.begin() + offset))
          << "chunk " << chunks << " does not carry the segment's speech";
      offset += expected;
      ++chunks;
    }
    const auto completed = read_as<serving::SegmentCompletedOutput>(key);
    ASSERT_TRUE(completed.has_value());
    EXPECT_EQ(completed->segment_id, segment_id);
    EXPECT_EQ(completed->total_samples, speech.size() / format.bytes_per_sample());
    EXPECT_EQ(completed->chunk_count, chunks);
  }

  [[nodiscard]] bool settle() { return under_test->settled(kPatience); }
  std::size_t held_sessions() const { return under_test->held_sessions(); }

  /// The state of a session that still holds its slot.
  std::optional<State> live_state(std::uint64_t key) const {
    const auto state = manager->state(key);
    return state ? std::optional{*state} : std::nullopt;
  }

  bool ends_as(std::uint64_t key, State state) {
    return eventually([&] {
      const auto tombstone = manager->tombstone(key);
      return tombstone && tombstone->state == state;
    });
  }

  /// Waits for `done`, which is tried again after every sink call and every
  /// queued output. `done` runs unlocked: it may call the manager.
  template <typename Predicate>
  bool eventually(Predicate&& done, std::chrono::milliseconds patience = kPatience) {
    const auto deadline = std::chrono::steady_clock::now() + patience;
    for (;;) {
      std::uint64_t seen = 0;
      {
        const std::lock_guard guard(wake->mu);
        seen = wake->changes;
      }
      if (done()) {
        return true;
      }
      std::unique_lock lock(wake->mu);
      if (!wake->cv.wait_until(lock, deadline, [&] { return wake->changes != seen; })) {
        lock.unlock();
        return done();
      }
    }
  }

 private:
  /// What a waiting test looks at has changed. Shared with each queue's
  /// consumer, which may outlive a stream.
  struct Wake {
    void changed() {
      {
        const std::lock_guard guard(mu);
        ++changes;
      }
      cv.notify_all();
    }
    std::mutex mu;
    std::condition_variable cv;
    std::uint64_t changes = 0;
  };
  struct Stream {
    serving::SessionTerms terms;
    std::shared_ptr<serving::BoundedOutputQueue> output;
    std::vector<serving::ManagedSessionTransition> sunk;
    /// The credit last reported to the peer.
    serving::LogicalSessionUsage credit;
    std::uint64_t credit_reports = 0;
    bool short_frame_sent = false;
  };

  void sink(const serving::ManagedSessionTransition& transition) {
    dispatch().on_transition(transition);
    bool first = false;
    {
      const std::lock_guard guard(mu);
      Stream& stream = streams[transition.session_key];
      first = stream.output == nullptr;
      stream.output = transition.output;
      stream.sunk.push_back(transition);
      // Credit is reported from here only, and only while input is taken.
      if (transition.state == State::Active || transition.state == State::Finalizing) {
        stream.credit = transition.status.input_credit_bytes;
        ++stream.credit_reports;
      }
    }
    if (first) {
      EXPECT_TRUE(transition.output->set_consumer([shared = wake] { shared->changed(); }));
    }
    wake->changed();
  }

  void ends_utterance(std::uint64_t key) {
    const std::lock_guard guard(mu);
    streams[key].short_frame_sent = false;
  }

  std::shared_ptr<Wake> wake = std::make_shared<Wake>();
  std::mutex mu;
  std::map<std::uint64_t, Stream> streams;

 public:
  FakeSchedulerClock clock;
  std::unique_ptr<DispatchUnderTest> under_test;
  // Last, so that what its sink uses outlives it.
  std::unique_ptr<serving::SessionManager> manager;
};

/// The cases every SessionDispatch passes. A test binary instantiates the
/// suite once per dispatch it builds.
class SessionDispatchContract : public ::testing::TestWithParam<DispatchFactory> {};
}  // namespace tensorplate::testing
