// SPDX-License-Identifier: Apache-2.0
//
// The stream listener's wait for a call's first message: no transport, no
// clock.

#include "serving/stream/stream_handshake.hpp"

#include <gtest/gtest.h>

#include <atomic>
#include <chrono>
#include <cstddef>
#include <memory>
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

}  // namespace
