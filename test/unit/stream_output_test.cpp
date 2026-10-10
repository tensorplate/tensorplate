// SPDX-License-Identifier: Apache-2.0
//
// The stream listener's write path against a recording writer: no transport.

#include "serving/stream/stream_output.hpp"

#include <gtest/gtest.h>

#include <condition_variable>
#include <cstdint>
#include <deque>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "serving/session/credits.hpp"
#include "serving/session/output_queue.hpp"

namespace {

using tensorplate::SchedulerClock;
using tensorplate::serving::BoundedOutputQueue;
using tensorplate::serving::OutputBody;
using tensorplate::serving::OutputItem;
using tensorplate::serving::OutputKind;
using tensorplate::serving::OutputOffer;
using tensorplate::serving::SessionBudgets;
using tensorplate::serving::stream::CallOutput;
using tensorplate::serving::stream::CallWriter;
using tensorplate::serving::stream::WireEvent;
using tensorplate::stream::v1::ServerEvent;

/// A clock that runs a hook once, inside the next now().
class HookClock final : public SchedulerClock {
 public:
  [[nodiscard]] TimePoint now() const noexcept override {
    if (auto hook = std::exchange(next_now, nullptr)) {
      hook();
    }
    return std::chrono::steady_clock::now();
  }
  mutable std::function<void()> next_now;
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
  std::function<void()> on_write;
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
  Path() {
    out->bind(queue, [this](OutputKind kind) {
      {
        const std::lock_guard guard(mu);
        told.push_back(kind);
      }
      if (auto hook = std::exchange(next_told, nullptr)) {
        hook();
      }
    });
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
  [[nodiscard]] std::size_t told_count() const {
    const std::lock_guard guard(mu);
    return told.size();
  }

  HookClock clock;
  RecordingWriter writer;
  std::shared_ptr<BoundedOutputQueue> queue =
      std::make_shared<BoundedOutputQueue>(SessionBudgets::for_audio_input(32'000).value());
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

TEST(StreamOutput, NoWriteStartsBetweenAWriteEndingAndItsConfirmation) {
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
  path.next_told = [&] {
    path.offer(OutputKind::Control);
    written_inside = path.writer.sequences().size();
  };
  path.out->write_done(true);
  EXPECT_EQ(written_inside, 1U);
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

TEST(StreamOutput, ABodyTheStreamDidNotQueueEndsTheCallAsInternal) {
  struct Foreign final : OutputBody {};
  Path path;
  OutputItem foreign{OutputKind::Control, 1, 0, std::make_unique<Foreign>()};
  ASSERT_EQ(path.queue->offer(foreign, std::chrono::steady_clock::now()).value(),
            OutputOffer::Queued);
  EXPECT_EQ(path.writer.codes(), (Codes{grpc::StatusCode::INTERNAL}));
  EXPECT_TRUE(path.writer.sequences().empty());
}

TEST(StreamOutput, ProducersAndATransportThreadKeepEveryOrder) {
  constexpr int kProducers = 4;
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
  // Answers each write from a thread that started none of them.
  std::thread transport([&] {
    int answered = 0;
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
  transport_cv.notify_one();
  transport.join();

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
