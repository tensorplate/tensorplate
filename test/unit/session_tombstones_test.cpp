// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <chrono>
#include <cstdint>
#include <string>

#include "fake_scheduler_clock.hpp"
#include "serving/session/tombstones.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;

TEST(SessionTombstones, KeepsTheNewestThousandAndTwentyFour) {
  const testing::FakeSchedulerClock clock;
  SessionTombstones tombstones;
  for (std::uint64_t key = 1; key <= SessionTombstones::kCapacity + 1; ++key) {
    tombstones.record(key, 5, LogicalSessionState::Closed, std::nullopt, clock.now());
  }
  EXPECT_EQ(tombstones.size(), 1024U);
  EXPECT_FALSE(tombstones.find(1, clock.now()));
  EXPECT_TRUE(tombstones.find(2, clock.now()));
  EXPECT_TRUE(tombstones.find(1025, clock.now()));
}

TEST(SessionTombstones, ExpireAfterSixtySeconds) {
  testing::FakeSchedulerClock clock;
  SessionTombstones tombstones;
  tombstones.record(1, 5, LogicalSessionState::Closed, std::nullopt, clock.now());
  clock.advance(30s);
  tombstones.record(2, 5, LogicalSessionState::Failed, std::nullopt, clock.now());
  clock.advance(29'999ms);
  EXPECT_TRUE(tombstones.find(1, clock.now()));
  clock.advance(1ms);
  EXPECT_FALSE(tombstones.find(1, clock.now()));
  EXPECT_TRUE(tombstones.find(2, clock.now()));
  EXPECT_EQ(tombstones.size(), 2U);
  tombstones.record(3, 5, LogicalSessionState::Closed, std::nullopt, clock.now());
  EXPECT_EQ(tombstones.size(), 2U);
  clock.advance(30s);
  EXPECT_FALSE(tombstones.find(2, clock.now()));
  EXPECT_TRUE(tombstones.find(3, clock.now()));
}

TEST(SessionTombstones, KeepTheEndCauseWithinSixtyFourBytes) {
  const testing::FakeSchedulerClock clock;
  SessionTombstones tombstones;
  tombstones.record(1, 7, LogicalSessionState::Closed, std::nullopt, clock.now());
  tombstones.record(2, 7, LogicalSessionState::Failed,
                    Error::make(Error::Code::ResourceExhausted, "a message that is not kept",
                                "input_credit_exceeded"),
                    clock.now());
  tombstones.record(3, 7, LogicalSessionState::Failed,
                    Error::make(Error::Code::Internal, "no context"), clock.now());
  // 63 ASCII bytes, then a two-byte character straddling the 64-byte cut.
  const std::string straddling = std::string(63, 'a') + "\xC3\xA9" + "tail";
  tombstones.record(4, 7, LogicalSessionState::Failed,
                    Error::make(Error::Code::Internal, "m", straddling), clock.now());
  tombstones.record(5, 7, LogicalSessionState::Failed,
                    Error::make(Error::Code::Internal, "m", std::string(100, 'b')), clock.now());
  tombstones.record(6, 7, LogicalSessionState::Failed,
                    Error::make(Error::Code::Internal, "m", std::string(64, 'c')), clock.now());

  const auto closed = tombstones.find(1, clock.now());
  ASSERT_TRUE(closed);
  EXPECT_EQ(closed->generation, 7U);
  EXPECT_EQ(closed->state, LogicalSessionState::Closed);
  EXPECT_FALSE(closed->code);
  EXPECT_TRUE(closed->reason.empty());
  EXPECT_EQ(closed->ended_at, clock.now());

  const auto failed = tombstones.find(2, clock.now());
  ASSERT_TRUE(failed);
  EXPECT_EQ(failed->state, LogicalSessionState::Failed);
  EXPECT_EQ(failed->code, Error::Code::ResourceExhausted);
  EXPECT_EQ(failed->reason, "input_credit_exceeded");

  EXPECT_EQ(tombstones.find(3, clock.now())->code, Error::Code::Internal);
  EXPECT_TRUE(tombstones.find(3, clock.now())->reason.empty());
  EXPECT_EQ(tombstones.find(4, clock.now())->reason, std::string(63, 'a'));
  EXPECT_EQ(tombstones.find(5, clock.now())->reason, std::string(64, 'b'));
  EXPECT_EQ(tombstones.find(6, clock.now())->reason, std::string(64, 'c'));
}
}  // namespace
}  // namespace tensorplate::serving
