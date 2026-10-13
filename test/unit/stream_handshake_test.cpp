// SPDX-License-Identifier: Apache-2.0
//
// The stream listener's wait for a call's first message and its places for
// refused calls: no transport, no clock.

#include "serving/stream/stream_handshake.hpp"

#include <gtest/gtest.h>

#include <atomic>
#include <chrono>
#include <cstddef>
#include <memory>
#include <optional>
#include <thread>
#include <vector>

#include "tensorplate/scheduler/clock.hpp"

namespace {

using namespace std::chrono_literals;
using tensorplate::SchedulerClock;
using tensorplate::SystemSchedulerClock;
using tensorplate::serving::stream::CallOutput;
using tensorplate::serving::stream::CallWriter;
using tensorplate::serving::stream::OpenHandshakes;
using tensorplate::serving::stream::RefusedCalls;
using tensorplate::stream::v1::ServerEvent;

class IdleWriter final : public CallWriter {
 public:
  void start_write(const ServerEvent& /*event*/) override {}
  void finish(const grpc::Status& /*status*/) override {}
  void cancel() override {}
};

/// Calls that exist only to be waited on.
struct Calls {
  [[nodiscard]] OpenHandshakes::Call make() { return std::make_shared<CallOutput>(writer, clock); }

  SystemSchedulerClock clock;
  IdleWriter writer;
};

using Taken = std::vector<OpenHandshakes::Call>;
constexpr SchedulerClock::TimePoint kBegan{std::chrono::hours{1}};

/// Begins a wait whose first read is of no interest.
OpenHandshakes::Begun begin(OpenHandshakes& handshakes, const OpenHandshakes::Call& call,
                            SchedulerClock::TimePoint now = kBegan) {
  return handshakes.begin(call, now, [] {});
}

TEST(OpenHandshakes, ACallIsOverdueFiveSecondsAfterItBeganAndNotBefore) {
  Calls calls;
  OpenHandshakes handshakes;
  const auto call = calls.make();
  ASSERT_EQ(begin(handshakes, call, kBegan), OpenHandshakes::Begun::Yes);
  EXPECT_EQ(handshakes.next_due(), kBegan + 5s);
  EXPECT_TRUE(handshakes.take_overdue(kBegan + 5s - 1ns).empty());
  EXPECT_EQ(handshakes.size(), 1U);
  EXPECT_EQ(handshakes.take_overdue(kBegan + 5s), (Taken{call}));
  EXPECT_EQ(handshakes.size(), 0U);
  EXPECT_FALSE(handshakes.next_due().has_value());
}

TEST(OpenHandshakes, ThirtyTwoCallsAreWaitedOnAndTheNextIsNotUntilOneLeaves) {
  Calls calls;
  OpenHandshakes handshakes;
  Taken waited;
  for (int i = 0; i < 32; ++i) {
    waited.push_back(calls.make());
    ASSERT_EQ(begin(handshakes, waited.back(), kBegan), OpenHandshakes::Begun::Yes) << i;
  }
  EXPECT_EQ(begin(handshakes, calls.make(), kBegan), OpenHandshakes::Begun::Full);
  EXPECT_EQ(handshakes.size(), 32U);
  EXPECT_TRUE(handshakes.end(*waited.front()));
  EXPECT_EQ(begin(handshakes, calls.make(), kBegan), OpenHandshakes::Begun::Yes);
}

// A bound refuses what it takes, so it must never find a call whose first
// read is still to start, and a call given no place must start none.
TEST(OpenHandshakes, TheFirstReadStartsWithTheWaitAndOnlyWithIt) {
  Calls calls;
  OpenHandshakes handshakes;
  int reads = 0;
  ASSERT_EQ(handshakes.begin(calls.make(), kBegan, [&] { ++reads; }), OpenHandshakes::Begun::Yes);
  EXPECT_EQ(reads, 1);
  for (int i = 1; i < 32; ++i) {
    ASSERT_EQ(begin(handshakes, calls.make()), OpenHandshakes::Begun::Yes);
  }
  EXPECT_EQ(handshakes.begin(calls.make(), kBegan, [&] { ++reads; }), OpenHandshakes::Begun::Full);
  EXPECT_EQ(reads, 1) << "a call with no place started a read";
  (void)handshakes.close();
  EXPECT_EQ(handshakes.begin(calls.make(), kBegan, [&] { ++reads; }),
            OpenHandshakes::Begun::Closed);
  EXPECT_EQ(reads, 1) << "a call that arrived after the close started a read";
}

// The read starts inside the wait's own lock: a bound that asks while it
// starts is kept out until it has, and then finds the call.
TEST(OpenHandshakes, ABoundCannotTakeACallWhoseFirstReadIsStillStarting) {
  Calls calls;
  OpenHandshakes handshakes;
  std::thread bound;
  std::atomic<bool> asking{false};
  // Not atomic on purpose: only the wait's lock orders its write and its
  // read, and ThreadSanitizer reports the pair if the lock does not.
  bool read_started = false;
  bool taken_before_the_read = false;
  std::size_t taken = 0;
  ASSERT_EQ(handshakes.begin(calls.make(), kBegan,
                             [&] {
                               bound = std::thread([&] {
                                 asking = true;
                                 taken = handshakes.take_overdue(kBegan + 5s).size();
                                 taken_before_the_read = !read_started;
                               });
                               while (!asking) {
                                 std::this_thread::yield();
                               }
                               read_started = true;
                             }),
            OpenHandshakes::Begun::Yes);
  bound.join();
  EXPECT_EQ(taken, 1U);
  EXPECT_FALSE(taken_before_the_read);
}

TEST(OpenHandshakes, AReaderEndsItsOwnCallsWaitAndNoOther) {
  Calls calls;
  OpenHandshakes handshakes;
  const Taken begun{calls.make(), calls.make(), calls.make()};
  for (const auto& call : begun) {
    ASSERT_EQ(begin(handshakes, call), OpenHandshakes::Begun::Yes);
  }
  EXPECT_TRUE(handshakes.end(*begun[1]));
  EXPECT_FALSE(handshakes.end(*begun[1]));
  EXPECT_EQ(handshakes.take_overdue(kBegan + 5s), (Taken{begun[0], begun[2]}));
}

TEST(OpenHandshakes, ACallLeavesOnceWhetherItsReaderOrABoundTakesIt) {
  Calls calls;
  OpenHandshakes handshakes;
  const auto read = calls.make();
  const auto overdue = calls.make();
  ASSERT_EQ(begin(handshakes, read, kBegan), OpenHandshakes::Begun::Yes);
  ASSERT_EQ(begin(handshakes, overdue, kBegan), OpenHandshakes::Begun::Yes);
  EXPECT_TRUE(handshakes.end(*read));
  EXPECT_FALSE(handshakes.end(*read));
  EXPECT_EQ(handshakes.take_overdue(kBegan + 1h), (Taken{overdue})) << "the call read was taken";
  EXPECT_FALSE(handshakes.end(*overdue)) << "a call a bound took is not its reader's to answer";
}

TEST(OpenHandshakes, OverdueCallsAreTakenInTheOrderTheyBeganAndTheRestKept) {
  Calls calls;
  OpenHandshakes handshakes;
  const Taken begun{calls.make(), calls.make(), calls.make()};
  ASSERT_EQ(begin(handshakes, begun[0], kBegan), OpenHandshakes::Begun::Yes);
  ASSERT_EQ(begin(handshakes, begun[1], kBegan + 1s), OpenHandshakes::Begun::Yes);
  ASSERT_EQ(begin(handshakes, begun[2], kBegan + 2s), OpenHandshakes::Begun::Yes);
  EXPECT_EQ(handshakes.next_due(), kBegan + 5s);
  EXPECT_EQ(handshakes.take_overdue(kBegan + 6s), (Taken{begun[0], begun[1]}));
  EXPECT_EQ(handshakes.next_due(), kBegan + 7s);
  EXPECT_TRUE(handshakes.end(*begun[2]));
}

TEST(OpenHandshakes, ClosingTakesEveryCallAndWaitsOnNoneAfter) {
  Calls calls;
  OpenHandshakes handshakes;
  const Taken begun{calls.make(), calls.make()};
  ASSERT_EQ(begin(handshakes, begun[0], kBegan), OpenHandshakes::Begun::Yes);
  ASSERT_EQ(begin(handshakes, begun[1], kBegan), OpenHandshakes::Begun::Yes);
  EXPECT_EQ(handshakes.close(), begun);
  EXPECT_EQ(handshakes.size(), 0U);
  EXPECT_FALSE(handshakes.end(*begun[0]));
  EXPECT_EQ(begin(handshakes, calls.make(), kBegan), OpenHandshakes::Begun::Closed);
  EXPECT_TRUE(handshakes.close().empty());
}

// Readers, the timeout and the close race over the same calls: each call
// that was waited on is answered by exactly one of them.
TEST(OpenHandshakes, EachCallIsAnsweredOnceWhenReadersAndBoundsRace) {
  constexpr int kReaders = 4;
  constexpr int kEach = 400;
  Calls calls;
  OpenHandshakes handshakes;
  std::atomic<int> waited{0};
  std::atomic<int> answered{0};
  std::atomic<bool> readers_done{false};
  std::thread bound([&] {
    while (!readers_done) {
      answered += static_cast<int>(handshakes.take_overdue(kBegan + 5s).size());
      std::this_thread::yield();
    }
  });
  std::vector<std::thread> readers;
  for (int reader = 0; reader < kReaders; ++reader) {
    readers.emplace_back([&] {
      for (int i = 0; i < kEach; ++i) {
        const auto call = calls.make();
        // Every other call is overdue the instant it begins.
        if (begin(handshakes, call, i % 2 == 0 ? kBegan : kBegan + 1s) !=
            OpenHandshakes::Begun::Yes) {
          continue;
        }
        ++waited;
        if (handshakes.end(*call)) {
          ++answered;
        }
      }
    });
  }
  for (auto& reader : readers) {
    reader.join();
  }
  readers_done = true;
  bound.join();
  answered += static_cast<int>(handshakes.close().size());
  EXPECT_EQ(answered, waited);
  EXPECT_GT(waited, 0);
}

/// The transport's side of a refused call, with gRPC's rules for a reactor:
/// it is finished once, and nothing reaches it once the call is done.
class EndRecorder final : public CallWriter {
 public:
  void start_write(const ServerEvent& /*event*/) override {
    ADD_FAILURE() << "a call that was never admitted is sent no event";
  }
  void finish(const grpc::Status& status) override {
    EXPECT_FALSE(done) << "finished after the call was done";
    EXPECT_FALSE(finished.has_value()) << "finished twice";
    finished = status.error_code();
  }
  void cancel() override {
    EXPECT_FALSE(done) << "cancelled after the call was done";
    ++cancels;
  }

  std::optional<grpc::StatusCode> finished;
  int cancels = 0;
  bool done = false;
};

/// One call whose peer does not read: it is done when the test says so, and
/// gRPC ends no call that its reactor has not finished.
struct UnreadCall {
  void refuse(RefusedCalls& places) {
    out->refuse(places, {grpc::StatusCode::UNAVAILABLE, "open refused"});
  }
  void end() {
    ASSERT_TRUE(writer.finished.has_value()) << "a call is done only once it was finished";
    writer.done = true;
    out->detach();
  }
  [[nodiscard]] bool kept_its_status() const {
    return writer.finished == grpc::StatusCode::UNAVAILABLE && writer.cancels == 0;
  }
  [[nodiscard]] bool was_cancelled() const {
    return writer.finished == grpc::StatusCode::CANCELLED && writer.cancels == 1;
  }

  SystemSchedulerClock clock;
  EndRecorder writer;
  std::shared_ptr<CallOutput> out = std::make_shared<CallOutput>(writer, clock);
};

/// `count` calls, each refused and none done.
std::vector<std::unique_ptr<UnreadCall>> refused_and_held(RefusedCalls& places, int count) {
  std::vector<std::unique_ptr<UnreadCall>> held;
  for (int i = 0; i < count; ++i) {
    held.push_back(std::make_unique<UnreadCall>());
    held.back()->refuse(places);
  }
  return held;
}

TEST(RefusedCalls, TheThirtySecondCallKeepsItsStatusAndTheNextIsCancelled) {
  RefusedCalls places;
  const auto held = refused_and_held(places, 32);
  for (const auto& call : held) {
    EXPECT_TRUE(call->kept_its_status());
  }
  EXPECT_EQ(places.size(), 32U);
  UnreadCall next;
  next.refuse(places);
  EXPECT_TRUE(next.was_cancelled()) << "a thirty-third refused call was kept";
  EXPECT_EQ(places.size(), 32U);
}

TEST(RefusedCalls, ACallThatEndsFreesItsPlaceAndACancelledCallHeldNone) {
  RefusedCalls places;
  const auto held = refused_and_held(places, 32);
  UnreadCall cancelled;
  cancelled.refuse(places);
  ASSERT_TRUE(cancelled.was_cancelled());
  cancelled.end();
  EXPECT_EQ(places.size(), 32U) << "a call that held no place gave one back";
  UnreadCall still_none;
  still_none.refuse(places);
  EXPECT_TRUE(still_none.was_cancelled());

  held.front()->end();
  EXPECT_EQ(places.size(), 31U);
  UnreadCall next;
  next.refuse(places);
  EXPECT_TRUE(next.kept_its_status()) << "the place of a call that ended was not free";
  EXPECT_EQ(places.size(), 32U);
  // Once only: the place went with the call.
  held.front()->out->detach();
  EXPECT_EQ(places.size(), 32U);
}

// A call its client cancelled while a bound was about to refuse it: it has
// ended, so the refusal neither takes a place nor cancels.
TEST(RefusedCalls, ARefusalOfACallThatIsEndingAlreadyTakesNoPlaceAndCancelsNothing) {
  RefusedCalls places;
  UnreadCall ending;
  ending.out->finish(grpc::Status::CANCELLED);
  ending.refuse(places);
  EXPECT_EQ(places.size(), 0U);
  EXPECT_EQ(ending.writer.finished, grpc::StatusCode::CANCELLED);

  RefusedCalls none{0};
  UnreadCall refused;
  refused.refuse(places);
  refused.refuse(none);
  EXPECT_TRUE(refused.kept_its_status()) << "a second refusal cancelled a call that had a place";
  EXPECT_EQ(places.size(), 1U);
}

TEST(RefusedCalls, ARefusalAfterTheWriterIsGoneDoesNothing) {
  RefusedCalls places;
  UnreadCall gone;
  gone.out->detach();
  gone.refuse(places);
  EXPECT_EQ(places.size(), 0U) << "a place was taken that no end gives back";
  EXPECT_FALSE(gone.writer.finished.has_value());
  EXPECT_EQ(gone.writer.cancels, 0);
}

// Refusals from the reactions, the watch thread and a closing admission run
// together: the places are never exceeded and none is lost.
TEST(RefusedCalls, ExactlyThirtyTwoOfSixtyFourCallsRefusedAtOnceKeepTheirStatus) {
  constexpr int kThreads = 8;
  constexpr int kEach = 8;
  RefusedCalls places;
  std::vector<std::unique_ptr<UnreadCall>> calls;
  for (int i = 0; i < kThreads * kEach; ++i) {
    calls.push_back(std::make_unique<UnreadCall>());
  }
  std::vector<std::thread> refusers;
  for (int thread = 0; thread < kThreads; ++thread) {
    refusers.emplace_back([&, thread] {
      for (int i = 0; i < kEach; ++i) {
        calls[static_cast<std::size_t>(thread * kEach + i)]->refuse(places);
      }
    });
  }
  for (auto& refuser : refusers) {
    refuser.join();
  }
  int kept = 0;
  int cancelled = 0;
  for (const auto& call : calls) {
    kept += call->kept_its_status() ? 1 : 0;
    cancelled += call->was_cancelled() ? 1 : 0;
  }
  EXPECT_EQ(kept, 32);
  EXPECT_EQ(cancelled, 32);
  EXPECT_EQ(places.size(), 32U);
  for (const auto& call : calls) {
    call->end();
  }
  EXPECT_EQ(places.size(), 0U);
}

}  // namespace
