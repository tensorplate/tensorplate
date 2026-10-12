// SPDX-License-Identifier: Apache-2.0
//
// The stream listener's write path against a recording writer: no transport.

#include "serving/stream/stream_output.hpp"

#include <gtest/gtest.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <functional>
#include <future>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "serving/session/credits.hpp"
#include "serving/session/dispatch.hpp"
#include "serving/session/output_queue.hpp"

namespace {

using tensorplate::SchedulerClock;
using tensorplate::serving::BoundedOutputQueue;
using tensorplate::serving::OutputBody;
using tensorplate::serving::OutputItem;
using tensorplate::serving::OutputKind;
using tensorplate::serving::OutputOffer;
using tensorplate::serving::SessionBudgets;
using tensorplate::serving::StreamAudioEncoding;
using tensorplate::serving::StreamAudioFormat;
using tensorplate::serving::TaskOutput;
using tensorplate::serving::stream::CallOutput;
using tensorplate::serving::stream::CallWriter;
using tensorplate::serving::stream::lifecycle_item;
using tensorplate::serving::stream::WireEvent;
using tensorplate::stream::v1::ServerEvent;

/// The format of a session's chunks: 24 kHz 16-bit mono.
constexpr StreamAudioFormat kFormat{StreamAudioEncoding::PcmS16Le, 24'000, 1};

/// A clock that runs a hook once, inside the next now(), and reads `fixed`
/// when a test set it.
class HookClock final : public SchedulerClock {
 public:
  [[nodiscard]] TimePoint now() const noexcept override {
    if (auto hook = std::exchange(next_now, nullptr)) {
      hook();
    }
    return fixed.value_or(std::chrono::steady_clock::now());
  }
  mutable std::function<void()> next_now;
  std::optional<TimePoint> fixed;
};

class RecordingWriter final : public CallWriter {
 public:
  void start_write(const ServerEvent& event) override {
    {
      const std::lock_guard guard(mu);
      written.push_back(event);
    }
    if (on_write) {
      on_write();
    }
  }
  void finish(const grpc::Status& status) override {
    const std::lock_guard guard(mu);
    finished.push_back(status);
  }
  void cancel() override {
    {
      const std::lock_guard guard(mu);
      ++cancels;
    }
    if (on_cancel) {
      on_cancel();
    }
  }
  [[nodiscard]] int cancelled() const {
    const std::lock_guard guard(mu);
    return cancels;
  }
  [[nodiscard]] std::vector<std::uint64_t> sequences() const {
    const std::lock_guard guard(mu);
    std::vector<std::uint64_t> out;
    for (const auto& event : written) {
      out.push_back(event.sequence());
    }
    return out;
  }
  [[nodiscard]] std::vector<grpc::StatusCode> codes() const {
    const std::lock_guard guard(mu);
    std::vector<grpc::StatusCode> out;
    for (const auto& status : finished) {
      out.push_back(status.error_code());
    }
    return out;
  }

  mutable std::mutex mu;
  std::vector<ServerEvent> written;
  std::vector<grpc::Status> finished;
  int cancels = 0;
  std::function<void()> on_write;
  std::function<void()> on_cancel;
};

OutputItem item(OutputKind kind, const std::string& tag = "t") {
  ServerEvent event;
  if (kind == OutputKind::Terminal) {
    event.mutable_session_closed();
  } else {
    event.mutable_ready()->set_session_id(tag);
  }
  const auto bytes = std::max<std::uint64_t>(1, event.ByteSizeLong());
  return OutputItem{kind, bytes, 0, std::make_unique<WireEvent>(std::move(event))};
}

/// One call's write path with its queue, as the listener wires them.
struct Path {
  explicit Path(const SessionBudgets& budgets = SessionBudgets::for_audio_input(32'000).value())
      : queue(std::make_shared<BoundedOutputQueue>(budgets)) {
    out->bind(
        queue,
        [this](OutputKind kind) {
          {
            const std::lock_guard guard(mu);
            told.push_back(kind);
          }
          if (auto hook = std::exchange(next_told, nullptr)) {
            hook();
          }
        },
        kFormat);
    EXPECT_TRUE(queue->set_consumer([weak = std::weak_ptr<CallOutput>(out)] {
      if (const auto shared = weak.lock()) {
        shared->pump();
      }
    }));
  }

  OutputOffer offer(OutputKind kind, const std::string& tag = "t") {
    auto queued = item(kind, tag);
    return queue->offer(queued, std::chrono::steady_clock::now()).value();
  }
  /// Queues task output as a dispatch does.
  OutputOffer offer(TaskOutput output) {
    auto queued = tensorplate::serving::make_task_output_item(std::move(output));
    return queue->offer(queued, std::chrono::steady_clock::now()).value();
  }
  OutputOffer offer(ServerEvent event) {
    auto queued = lifecycle_item(std::move(event));
    return queue->offer(queued, std::chrono::steady_clock::now()).value();
  }
  [[nodiscard]] std::size_t told_count() const {
    const std::lock_guard guard(mu);
    return told.size();
  }

  HookClock clock;
  RecordingWriter writer;
  std::shared_ptr<BoundedOutputQueue> queue;
  std::shared_ptr<CallOutput> out = std::make_shared<CallOutput>(writer, clock);
  mutable std::mutex mu;
  std::vector<OutputKind> told;
  std::function<void()> next_told;
};

using Sequences = std::vector<std::uint64_t>;
using Codes = std::vector<grpc::StatusCode>;

TEST(StreamOutput, WritesInSequenceWithOneWriteInFlight) {
  Path path;
  for (int i = 0; i < 3; ++i) {
    EXPECT_EQ(path.offer(OutputKind::Control), OutputOffer::Queued);
  }
  EXPECT_EQ(path.writer.sequences(), (Sequences{1}));
  path.out->write_done(true);
  EXPECT_EQ(path.writer.sequences(), (Sequences{1, 2}));
  path.out->write_done(true);
  path.out->write_done(true);
  EXPECT_EQ(path.writer.sequences(), (Sequences{1, 2, 3}));
  EXPECT_EQ(path.told_count(), 3U);
  EXPECT_EQ(path.queue->metadata_usage().used(), 0U);
  EXPECT_TRUE(path.writer.codes().empty());
}

TEST(StreamOutput, AnOfferMadeWhileAWriteIsAnsweredStartsNoWriteBeforeItsConfirmation) {
  Path path;
  path.offer(OutputKind::Control);
  std::size_t written_inside = 0;
  path.clock.next_now = [&] {
    path.offer(OutputKind::Control);
    written_inside = path.writer.sequences().size();
  };
  path.out->write_done(true);
  EXPECT_EQ(written_inside, 1U) << "the second write began before the first was confirmed";
  EXPECT_EQ(path.writer.sequences(), (Sequences{1, 2}));
  path.out->write_done(true);
  // Both were confirmed in order: nothing is left charged or stalled.
  EXPECT_EQ(path.queue->metadata_usage().used(), 0U);
  EXPECT_FALSE(path.queue->stalled_since().has_value());
  EXPECT_EQ(path.told_count(), 2U);
}

TEST(StreamOutput, NoWriteStartsBeforeADeliveryWasTold) {
  Path path;
  path.offer(OutputKind::Control);
  std::size_t written_inside = 0;
  std::uint64_t charged_when_told = 1;
  path.next_told = [&] {
    charged_when_told = path.queue->metadata_usage().used();
    path.offer(OutputKind::Control);
    written_inside = path.writer.sequences().size();
  };
  path.out->write_done(true);
  EXPECT_EQ(charged_when_told, 0U) << "the delivery was told before the queue confirmed it";
  EXPECT_EQ(written_inside, 1U) << "the second write began before the first was told";
  EXPECT_EQ(path.writer.sequences(), (Sequences{1, 2}));
}

TEST(StreamOutput, TheTerminalEventEndsTheCallOnceWithItsEndStatus) {
  Path path;
  path.out->set_end_status(grpc::Status(grpc::StatusCode::UNAVAILABLE, "ended"));
  path.offer(OutputKind::Terminal);
  EXPECT_TRUE(path.writer.codes().empty()) << "the terminal event is written first";
  path.out->write_done(true);
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::UNAVAILABLE}));
  path.out->finish(grpc::Status::CANCELLED);
  path.out->pump();
  EXPECT_EQ(path.writer.codes().size(), 1U);
  EXPECT_EQ(path.told, (std::vector<OutputKind>{OutputKind::Terminal}));
}

TEST(StreamOutput, FinishWaitsForTheWriteInFlightAndTheFirstStatusWins) {
  Path path;
  path.offer(OutputKind::Control);
  path.offer(OutputKind::Control);
  path.out->finish(grpc::Status(grpc::StatusCode::INVALID_ARGUMENT, "first"));
  path.out->finish(grpc::Status(grpc::StatusCode::INTERNAL, "second"));
  EXPECT_TRUE(path.writer.codes().empty());
  path.out->write_done(true);
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::INVALID_ARGUMENT}));
  EXPECT_EQ(path.writer.sequences(), (Sequences{1})) << "nothing is sent once the call is ending";
}

TEST(StreamOutput, FinishWithNothingInFlightEndsTheCallAtOnce) {
  Path path;
  path.out->finish(grpc::Status(grpc::StatusCode::FAILED_PRECONDITION, "refused"));
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::FAILED_PRECONDITION}));
  EXPECT_EQ(path.offer(OutputKind::Control), OutputOffer::Queued);
  EXPECT_TRUE(path.writer.sequences().empty());
}

TEST(StreamOutput, AFailedWriteEndsTheCallCancelledAndConfirmsNothing) {
  Path path;
  path.offer(OutputKind::Control);
  path.offer(OutputKind::Control);
  path.out->write_done(false);
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::CANCELLED}));
  EXPECT_EQ(path.told_count(), 0U);
  EXPECT_GT(path.queue->metadata_usage().used(), 0U);
  EXPECT_EQ(path.writer.sequences(), (Sequences{1}));
}

TEST(StreamOutput, NothingIsWrittenOrFinishedAfterDetach) {
  Path path;
  path.out->detach();
  path.offer(OutputKind::Control);
  path.out->finish(grpc::Status::CANCELLED);
  EXPECT_TRUE(path.writer.sequences().empty());
  EXPECT_TRUE(path.writer.codes().empty());
}

constexpr auto kLimit = SessionBudgets::kOutputNoProgressTimeout;
const SchedulerClock::TimePoint kEnded{std::chrono::hours{1}};

TEST(StreamOutput, ACallIsNotWatchedBeforeItsSessionEnded) {
  Path path;
  path.offer(OutputKind::Control);
  EXPECT_FALSE(path.out->cancel_if_stalled(kEnded + std::chrono::hours{1}).has_value());
  EXPECT_EQ(path.writer.cancelled(), 0);
}

TEST(StreamOutput, AnEndedCallWhoseWriteNeverCompletesIsCancelledOnceAtTheLimit) {
  Path path;
  path.offer(OutputKind::Control);
  path.out->session_ended(kEnded);
  path.offer(OutputKind::Terminal);
  EXPECT_EQ(path.out->cancel_if_stalled(kEnded), kEnded + kLimit);
  EXPECT_EQ(path.out->cancel_if_stalled(kEnded + kLimit - std::chrono::nanoseconds{1}),
            kEnded + kLimit);
  EXPECT_EQ(path.writer.cancelled(), 0) << "the call has the whole limit";
  EXPECT_FALSE(path.out->cancel_if_stalled(kEnded + kLimit).has_value());
  EXPECT_EQ(path.writer.cancelled(), 1);
  // Nothing is left to watch, and a second cancel is never sent.
  EXPECT_FALSE(path.out->cancel_if_stalled(kEnded + 2 * kLimit).has_value());
  EXPECT_EQ(path.writer.cancelled(), 1);
  // The transport answers the write it gave up, which ends the call.
  path.out->write_done(false);
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::CANCELLED}));
  EXPECT_EQ(path.writer.sequences(), (Sequences{1})) << "the terminal event was never written";
}

TEST(StreamOutput, AWriteThatCompletesAfterTheEndGivesTheCallTheLimitAgain) {
  Path path;
  path.offer(OutputKind::Control);
  path.out->session_ended(kEnded);
  path.offer(OutputKind::Terminal);
  path.clock.fixed = kEnded + kLimit - std::chrono::seconds{1};
  path.out->write_done(true);
  EXPECT_EQ(path.out->cancel_if_stalled(kEnded + kLimit), *path.clock.fixed + kLimit);
  EXPECT_EQ(path.writer.cancelled(), 0);
  EXPECT_FALSE(path.out->cancel_if_stalled(*path.clock.fixed + kLimit).has_value());
  EXPECT_EQ(path.writer.cancelled(), 1);
}

TEST(StreamOutput, TheEndOfASessionIsCountedFromOnce) {
  Path path;
  path.offer(OutputKind::Control);
  path.out->session_ended(kEnded);
  path.out->session_ended(kEnded + kLimit);
  EXPECT_EQ(path.out->cancel_if_stalled(kEnded), kEnded + kLimit);
}

TEST(StreamOutput, AFailedWriteDoesNotGiveAnEndedCallTheLimitAgain) {
  Path path;
  path.offer(OutputKind::Control);
  path.out->session_ended(kEnded);
  path.clock.fixed = kEnded + std::chrono::seconds{1};
  path.out->write_done(false);
  EXPECT_EQ(path.out->cancel_if_stalled(kEnded), kEnded + kLimit);
}

// A finished call is watched until its writer is gone: the transport may not
// be able to send its status either.
TEST(StreamOutput, AnEndedCallIsWatchedPastItsFinishUntilItsWriterIsGone) {
  Path path;
  path.out->set_end_status(grpc::Status(grpc::StatusCode::UNAVAILABLE, "ended"));
  path.out->session_ended(kEnded);
  path.offer(OutputKind::Terminal);
  path.clock.fixed = kEnded;
  path.out->write_done(true);
  ASSERT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::UNAVAILABLE}));
  EXPECT_EQ(path.out->cancel_if_stalled(kEnded), kEnded + kLimit);
  EXPECT_FALSE(path.out->cancel_if_stalled(kEnded + kLimit).has_value());
  EXPECT_EQ(path.writer.cancelled(), 1);
}

TEST(StreamOutput, ACallWhoseWriterIsGoneIsNeverCancelled) {
  Path path;
  path.offer(OutputKind::Control);
  path.out->session_ended(kEnded);
  path.out->detach();
  EXPECT_FALSE(path.out->cancel_if_stalled(kEnded + kLimit).has_value());
  EXPECT_EQ(path.writer.cancelled(), 0);
}

// The reactor is destroyed when detach() returns: it must not return while a
// cancel is still using it.
TEST(StreamOutput, DetachWaitsForACancelInProgress) {
  Path path;
  path.offer(OutputKind::Control);
  path.out->session_ended(kEnded);
  std::promise<void> detaching;
  std::atomic<bool> detached{false};
  bool detached_during_cancel = false;
  std::thread detacher;
  path.writer.on_cancel = [&] {
    detacher = std::thread([&] {
      detaching.set_value();
      path.out->detach();
      detached = true;
    });
    detaching.get_future().wait();
    // The reactions of a cancelled call run while the cancel is in progress.
    path.out->write_done(false);
    // Bounded by its count, not by a clock: a detach() that did not wait
    // would have returned long before the last yield.
    for (int spin = 0; spin < 100'000 && !detached; ++spin) {
      std::this_thread::yield();
    }
    detached_during_cancel = detached;
  };
  EXPECT_FALSE(path.out->cancel_if_stalled(kEnded + kLimit).has_value());
  detacher.join();
  EXPECT_FALSE(detached_during_cancel);
  EXPECT_EQ(path.writer.cancelled(), 1);
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::CANCELLED}));
}

TEST(StreamOutput, ABodyTheStreamDidNotQueueEndsTheCallAsInternal) {
  struct Foreign final : OutputBody {};
  Path path;
  OutputItem foreign{OutputKind::Control, 1, 0, std::make_unique<Foreign>()};
  ASSERT_EQ(path.queue->offer(foreign, std::chrono::steady_clock::now()).value(),
            OutputOffer::Queued);
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::INTERNAL}));
  EXPECT_TRUE(path.writer.sequences().empty());
}

TEST(StreamOutput, TaskOutputIsEncodedWhenItIsTakenInTheSessionsFormat) {
  Path path{SessionBudgets::for_text_input(48'000).value()};
  tensorplate::serving::AudioChunkOutput chunk;
  chunk.segment_id = 4;
  chunk.chunk_index = 2;
  chunk.segment_sample_offset = 960;
  chunk.pcm = {std::byte{1}, std::byte{2}, std::byte{3}, std::byte{4}};
  ASSERT_EQ(path.offer(chunk), OutputOffer::Queued);
  ASSERT_EQ(path.writer.written.size(), 1U);
  const auto& event = path.writer.written.front();
  ASSERT_TRUE(event.has_audio_chunk());
  EXPECT_EQ(event.sequence(), 1U);
  EXPECT_EQ(event.audio_chunk().segment_id(), 4U);
  EXPECT_EQ(event.audio_chunk().chunk_index(), 2U);
  EXPECT_EQ(event.audio_chunk().segment_sample_offset(), 960U);
  EXPECT_EQ(event.audio_chunk().data(), std::string("\1\2\3\4"));
  EXPECT_EQ(event.audio_chunk().format().sample_rate_hz(), 24'000U);
  EXPECT_EQ(event.audio_chunk().format().channels(), 1U);
  EXPECT_EQ(event.audio_chunk().format().encoding(),
            tensorplate::stream::v1::AUDIO_ENCODING_PCM_S16LE);
  path.out->write_done(true);
  EXPECT_EQ(path.told, (std::vector<OutputKind>{OutputKind::Audio}));
}

TEST(StreamOutput, SessionClosedLeavesWithTheTotalsOfWhatWasTakenBeforeIt) {
  Path path{SessionBudgets::for_text_input(48'000).value()};
  tensorplate::serving::AudioChunkOutput chunk;
  chunk.pcm.assign(960, std::byte{0});
  ASSERT_EQ(path.offer(chunk), OutputOffer::Queued);
  chunk.pcm.assign(160, std::byte{0});
  ASSERT_EQ(path.offer(chunk), OutputOffer::Queued);
  ASSERT_EQ(path.offer(tensorplate::serving::SegmentCompletedOutput{1, 560, 2}),
            OutputOffer::Queued);
  ASSERT_EQ(path.offer(tensorplate::serving::FinalTranscriptOutput{}), OutputOffer::Queued);
  ASSERT_EQ(path.offer(tensorplate::serving::EndpointDetectedOutput{}), OutputOffer::Queued);
  ASSERT_EQ(path.offer(tensorplate::serving::SynthesisCompletedOutput{}), OutputOffer::Queued);
  // What the sink knows is already in the event; the write path adds its own.
  ServerEvent terminal;
  terminal.mutable_session_closed()->mutable_totals()->set_input_text_bytes(13);
  ASSERT_EQ(path.offer(terminal), OutputOffer::Queued);
  for (int written = 0; written < 6; ++written) {
    path.out->write_done(true);
  }
  ASSERT_EQ(path.writer.written.size(), 7U);
  ASSERT_TRUE(path.writer.written.back().has_session_closed());
  const auto& totals = path.writer.written.back().session_closed().totals();
  EXPECT_EQ(totals.output_audio_samples(), 560U);
  EXPECT_EQ(totals.segments_completed(), 1U);
  EXPECT_EQ(totals.utterances_completed(), 1U);
  EXPECT_EQ(totals.input_text_bytes(), 13U);
}

// A stream the peer does not read holds one unsent Pong, one Accepted and one
// status reply, the newest of each, so replies never use up the part of the
// budget kept for lifecycle messages.
TEST(StreamOutput, KeyedRepliesReplaceTheirUnsentInstanceAndAreNeverRefused) {
  Path path;
  // Never answered: everything queued behind this write stays unsent.
  ASSERT_EQ(path.offer(OutputKind::Control), OutputOffer::Queued);
  tensorplate::serving::FinalTranscriptOutput transcript;
  transcript.segments.push_back({0, 1, std::string(400, 't')});
  std::size_t results = 0;
  while (path.offer(transcript) == OutputOffer::Queued) {
    ++results;
  }
  ASSERT_GT(results, 0U) << "task output never filled its share of the budget";

  constexpr std::uint64_t kRounds = 400;
  std::map<OutputOffer, int> answers;
  for (std::uint64_t round = 1; round <= kRounds; ++round) {
    ServerEvent pong;
    pong.mutable_pong();
    ServerEvent accepted;
    accepted.mutable_accepted()->set_accepted_sequence(round);
    ServerEvent status;
    status.mutable_status()->set_input_queue_depth(static_cast<std::uint32_t>(round));
    for (auto* reply : {&pong, &accepted, &status}) {
      ++answers[path.offer(*reply)];
    }
  }
  EXPECT_EQ(answers[OutputOffer::Queued], 3);
  EXPECT_EQ(answers[OutputOffer::Replaced], static_cast<int>(3 * kRounds - 3));
  EXPECT_EQ(answers.size(), 2U) << "a reply was refused";

  for (std::size_t written = 0; written < results + 4; ++written) {
    path.out->write_done(true);
  }
  int replies = 0;
  for (const auto& event : path.writer.written) {
    if (event.has_accepted()) {
      ++replies;
      EXPECT_EQ(event.accepted().accepted_sequence(), kRounds);
    } else if (event.has_status()) {
      ++replies;
      EXPECT_EQ(event.status().input_queue_depth(), kRounds);
    } else if (event.has_pong()) {
      ++replies;
    }
  }
  EXPECT_EQ(replies, 3);
}

TEST(StreamOutput, ProducersAndTransportThreadsKeepEveryOrder) {
  constexpr int kProducers = 4;
  constexpr int kTransports = 3;
  constexpr int kEach = 250;
  Path path;
  std::mutex transport_mu;
  std::condition_variable transport_cv;
  int started = 0;
  bool done = false;
  path.writer.on_write = [&] {
    {
      const std::lock_guard guard(transport_mu);
      ++started;
    }
    transport_cv.notify_one();
  };
  // Each write is answered by whichever of these threads takes it, so two
  // answers overlap whenever a write starts before the last one was settled.
  int answered = 0;
  std::vector<std::thread> transports;
  for (int transport = 0; transport < kTransports; ++transport) {
    transports.emplace_back([&] {
      std::unique_lock lock(transport_mu);
      for (;;) {
        transport_cv.wait(lock, [&] { return done || started > answered; });
        if (started == answered) {
          return;
        }
        ++answered;
        lock.unlock();
        path.out->write_done(true);
        lock.lock();
      }
    });
  }
  std::vector<std::thread> producers;
  for (int producer = 0; producer < kProducers; ++producer) {
    producers.emplace_back([&path, producer] {
      for (int index = 0; index < kEach; ++index) {
        const auto tag = std::to_string(producer) + ":" + std::to_string(index);
        while (path.offer(OutputKind::Control, tag) != OutputOffer::Queued) {
          std::this_thread::yield();
        }
      }
    });
  }
  for (auto& producer : producers) {
    producer.join();
  }
  path.out->set_end_status(grpc::Status(grpc::StatusCode::DEADLINE_EXCEEDED, "ended"));
  EXPECT_EQ(path.offer(OutputKind::Terminal), OutputOffer::Queued);
  const std::size_t total = kProducers * kEach + 1;
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds{60};
  while (path.writer.codes().empty() && std::chrono::steady_clock::now() < deadline) {
    std::this_thread::sleep_for(std::chrono::milliseconds{1});
  }
  {
    const std::lock_guard guard(transport_mu);
    done = true;
  }
  transport_cv.notify_all();
  for (auto& transport : transports) {
    transport.join();
  }

  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::DEADLINE_EXCEEDED}));
  ASSERT_EQ(path.writer.written.size(), total);
  std::map<std::string, int> next_index;
  for (std::size_t at = 0; at < total; ++at) {
    const auto& event = path.writer.written[at];
    ASSERT_EQ(event.sequence(), at + 1) << "writes left in sequence";
    if (at + 1 == total) {
      EXPECT_TRUE(event.has_session_closed());
      break;
    }
    const auto& tag = event.ready().session_id();
    const auto colon = tag.find(':');
    const auto producer = tag.substr(0, colon);
    EXPECT_EQ(std::stoi(tag.substr(colon + 1)), next_index[producer]++) << "producer " << producer;
  }
  EXPECT_EQ(path.told_count(), total);
  EXPECT_EQ(path.told.back(), OutputKind::Terminal);
  EXPECT_EQ(path.queue->metadata_usage().used(), 0U);
}

}  // namespace
