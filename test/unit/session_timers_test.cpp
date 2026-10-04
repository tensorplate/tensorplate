// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <chrono>

#include "fake_scheduler_clock.hpp"
#include "serving/session/timers.hpp"
#include "session_lifecycle_fixture.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;

TEST(SessionTimers, HeartbeatCadenceAndLivenessUseInjectedClock) {
  testing::FakeSchedulerClock clock;
  SessionTimers timers(SessionLimits::defaults(), SessionInputKind::Audio, clock.now());
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
  SessionTimers timers(SessionLimits::defaults(), SessionInputKind::Audio, clock.now());
  clock.advance(20s);
  timers.on_client_activity(LogicalSessionEvent::StatusRequest, clock.now());
  clock.advance(10s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active), SessionExpiry::Heartbeat);
}

TEST(SessionTimers, IdleExpiryIsIndependentOfHeartbeat) {
  const auto limits = SessionLimits::create(2s, 1s, 5s, 10s, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  SessionTimers timers(*limits, SessionInputKind::Audio, clock.now());
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
  SessionTimers timers(*limits, SessionInputKind::Audio, clock.now());
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
  SessionTimers timers(*limits, SessionInputKind::Audio, clock.now());
  clock.advance(9s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active), SessionExpiry::Idle);
}

TEST(SessionTimers, StalledOutputExpiresAfterTheNoProgressLimit) {
  testing::FakeSchedulerClock clock;
  SessionTimers timers(SessionLimits::defaults(), SessionInputKind::Audio, clock.now());
  const auto stalled_since = clock.now() + 1s;
  EXPECT_EQ(timers.next_deadline(LogicalSessionState::Active, stalled_since), stalled_since + 5s);
  EXPECT_EQ(timers.next_deadline(LogicalSessionState::Draining, stalled_since), stalled_since + 5s);
  EXPECT_FALSE(timers.next_deadline(LogicalSessionState::CancelRequested, stalled_since));
  clock.advance(5'999ms);
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::Active, stalled_since));
  clock.advance(1ms);
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::Active));
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active, stalled_since),
            SessionExpiry::SlowConsumer);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Draining, stalled_since),
            SessionExpiry::SlowConsumer);
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::CancelRequested, stalled_since));
  EXPECT_FALSE(timers.expired(clock.now(), LogicalSessionState::Failed, stalled_since));
}

TEST(SessionTimers, EqualDeadlinesReportDurationThenStalledOutputThenHeartbeat) {
  const auto limits = SessionLimits::create(60s, 1s, 5s, 5s, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  SessionTimers timers(*limits, SessionInputKind::Audio, clock.now());
  const auto opened_at = clock.now();
  clock.advance(5s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Active, opened_at),
            SessionExpiry::MaxDuration);
  const auto longer = SessionLimits::create(60s, 1s, 5s, 6s, 1);
  ASSERT_TRUE(longer);
  SessionTimers stalled(*longer, SessionInputKind::Audio, opened_at);
  EXPECT_EQ(stalled.expired(clock.now(), LogicalSessionState::Active, opened_at),
            SessionExpiry::SlowConsumer);
  EXPECT_EQ(stalled.expired(clock.now(), LogicalSessionState::Active), SessionExpiry::Heartbeat);
}

TEST(SessionTimers, FinalizeDeadlinesMatchFixture) {
  const auto deadlines = testing::load_session_lifecycle_fixture().at("finalize_deadlines");
  EXPECT_EQ(finalize_deadline(SessionInputKind::Audio), testing::fixture_ms(deadlines, "audio_ms"));
  EXPECT_EQ(finalize_deadline(SessionInputKind::Text), testing::fixture_ms(deadlines, "text_ms"));
}

TEST(SessionTimers, FinalizeDeadlineRunsFromTheFirstFinalizationOwed) {
  using State = LogicalSessionState;
  using Event = LogicalSessionEvent;
  testing::FakeSchedulerClock clock;
  SessionTimers timers(SessionLimits::defaults(), SessionInputKind::Audio, clock.now());
  clock.advance(1s);
  const auto started = clock.now();
  timers.on_transition(State::Active, Event::AutomaticEndpoint, State::Finalizing, started);
  EXPECT_EQ(timers.next_deadline(State::Finalizing), started + 10s);
  clock.advance(4s);
  timers.on_transition(State::Finalizing, Event::AutomaticEndpoint, State::Finalizing, clock.now());
  timers.on_transition(State::Finalizing, Event::Finalize, State::Finalizing, clock.now());
  timers.on_transition(State::Finalizing, Event::HalfClose, State::Draining, clock.now());
  timers.on_transition(State::Draining, Event::FinalizeCompleted, State::Draining, clock.now());
  EXPECT_EQ(timers.next_deadline(State::Draining), started + 10s);
  clock.advance(5'999ms);
  EXPECT_FALSE(timers.expired(clock.now(), State::Draining));
  clock.advance(1ms);
  EXPECT_EQ(timers.expired(clock.now(), State::Draining), SessionExpiry::FinalizeDeadline);
}

TEST(SessionTimers, FinalizeDeadlineStopsOnceTheSessionIsActiveAgain) {
  using State = LogicalSessionState;
  using Event = LogicalSessionEvent;
  testing::FakeSchedulerClock clock;
  SessionTimers timers(SessionLimits::defaults(), SessionInputKind::Audio, clock.now());
  const auto liveness = clock.now() + 30s;
  EXPECT_EQ(timers.next_deadline(State::Active), liveness);
  timers.on_transition(State::Active, Event::Finalize, State::Finalizing, clock.now());
  clock.advance(9s);
  timers.on_transition(State::Finalizing, Event::FinalizeCompleted, State::Active, clock.now());
  EXPECT_EQ(timers.next_deadline(State::Active), liveness);
  clock.advance(1s);
  EXPECT_FALSE(timers.expired(clock.now(), State::Active));
  timers.on_transition(State::Active, Event::Finalize, State::Finalizing, clock.now());
  EXPECT_EQ(timers.next_deadline(State::Finalizing), clock.now() + 10s);
}

TEST(SessionTimers, CompletedDrainStopsTheFinalizeDeadlineForGood) {
  using State = LogicalSessionState;
  using Event = LogicalSessionEvent;
  testing::FakeSchedulerClock clock;
  SessionTimers timers(SessionLimits::defaults(), SessionInputKind::Text, clock.now());
  const auto duration = clock.now() + 60min;
  timers.on_transition(State::Active, Event::Drain, State::Draining, clock.now());
  EXPECT_EQ(timers.next_deadline(State::Draining), clock.now() + 120s);
  clock.advance(119s);
  timers.on_transition(State::Draining, Event::DrainCompleted, State::Draining, clock.now());
  EXPECT_EQ(timers.next_deadline(State::Draining), duration);
  timers.on_transition(State::Draining, Event::Ping, State::Draining, clock.now());
  timers.on_transition(State::Draining, Event::HalfClose, State::Draining, clock.now());
  clock.advance(1s);
  EXPECT_FALSE(timers.expired(clock.now(), State::Draining));
  EXPECT_EQ(timers.next_deadline(State::Draining), duration);
}

TEST(SessionTimers, EqualDeadlinesReportDurationThenFinalizeThenStalledOutput) {
  const auto limits = SessionLimits::create(60s, 1s, 5s, 10s, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  const auto opened_at = clock.now();
  SessionTimers timers(*limits, SessionInputKind::Audio, opened_at);
  timers.on_transition(LogicalSessionState::Active, LogicalSessionEvent::HalfClose,
                       LogicalSessionState::Draining, opened_at);
  clock.advance(10s);
  EXPECT_EQ(timers.expired(clock.now(), LogicalSessionState::Draining, opened_at + 5s),
            SessionExpiry::MaxDuration);
  const auto longer = SessionLimits::create(60s, 1s, 5s, 11s, 1);
  ASSERT_TRUE(longer);
  SessionTimers draining(*longer, SessionInputKind::Audio, opened_at);
  draining.on_transition(LogicalSessionState::Active, LogicalSessionEvent::HalfClose,
                         LogicalSessionState::Draining, opened_at);
  EXPECT_EQ(draining.expired(clock.now(), LogicalSessionState::Draining, opened_at + 5s),
            SessionExpiry::FinalizeDeadline);
  EXPECT_EQ(draining.expired(clock.now(), LogicalSessionState::Draining, opened_at + 4s),
            SessionExpiry::SlowConsumer);
}
}  // namespace
}  // namespace tensorplate::serving
