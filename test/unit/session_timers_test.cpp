// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <chrono>

#include "fake_scheduler_clock.hpp"
#include "serving/session/timers.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;

TEST(SessionTimers, HeartbeatCadenceAndLivenessUseInjectedClock) {
  testing::FakeSchedulerClock clock;
  SessionTimers timers(SessionLimits::defaults(), clock.now());
  clock.advance(9999ms);
  EXPECT_FALSE(timers.heartbeat_due(clock.now()));
  clock.advance(1ms);
  EXPECT_TRUE(timers.heartbeat_due(clock.now()));
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::Active));
  timers.on_client_activity(LogicalSessionEvent::Ping, clock.now());
  EXPECT_FALSE(timers.heartbeat_due(clock.now()));
  clock.advance(29s);
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::Active));
  clock.advance(1s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active), SessionExpiry::Heartbeat);
}

TEST(SessionTimers, StatusDoesNotKeepHeartbeatAlive) {
  testing::FakeSchedulerClock clock;
  SessionTimers timers(SessionLimits::defaults(), clock.now());
  clock.advance(20s);
  timers.on_client_activity(LogicalSessionEvent::StatusRequest, clock.now());
  clock.advance(10s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active), SessionExpiry::Heartbeat);
}

TEST(SessionTimers, IdleExpiryIsIndependentOfHeartbeat) {
  const auto limits = SessionLimits::create(2s, 1s, 5s, 10s, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  SessionTimers timers(*limits, clock.now());
  clock.advance(1999ms);
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::Active));
  clock.advance(1ms);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active), SessionExpiry::Idle);
  EXPECT_EQ(timers.next_deadline(LogicalSessionState::Active), clock.now());
}

TEST(SessionTimers, DrainingSuspendsIdleAndHeartbeatButNotMaxDuration) {
  const auto limits = SessionLimits::create(2s, 1s, 3s, 5s, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  SessionTimers timers(*limits, clock.now());
  const auto absolute = clock.now() + 5s;
  EXPECT_EQ(timers.next_deadline(LogicalSessionState::Draining), absolute);
  clock.advance(4s);
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::Draining));
  EXPECT_EQ(timers.duration_remaining(clock.now()), 1s);
  clock.advance(1s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Draining), SessionExpiry::MaxDuration);
  EXPECT_EQ(timers.duration_remaining(clock.now()), SchedulerClock::Duration::zero());
  EXPECT_FALSE(timers.next_deadline(LogicalSessionState::CancelRequested));
}

TEST(SessionTimers, LargeClockJumpReportsEarliestExpiredDeadline) {
  const auto limits = SessionLimits::create(2s, 1s, 5s, 8s, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  SessionTimers timers(*limits, clock.now());
  clock.advance(9s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active), SessionExpiry::Idle);
}
}  // namespace
}  // namespace tensorplate::serving
