// SPDX-License-Identifier: Apache-2.0
#include "serving/session/session_manager.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <cstdint>
#include <future>
#include <thread>
#include <type_traits>
#include <vector>

#include "tensorplate/scheduler/scheduler_request.hpp"

#include "fake_scheduler_clock.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
static_assert(
    std::is_same_v<decltype(ManagedSessionTransition{}.session_key), SchedulerRequest::SessionKey>);

TEST(SessionManager, CountCapHoldsUntilPhysicalRelease) {
  const auto limits = SessionLimits::create(60s, 10s, 30s, 60min, 2);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      *limits, 5, clock, [&](const auto& transition) { observed.push_back(transition); }, {},
      false);
  ASSERT_TRUE(manager);
  const auto first = (*manager)->open(5);
  const auto second = (*manager)->open(5);
  ASSERT_TRUE(first);
  ASSERT_TRUE(second);
  EXPECT_NE(first->session_key, second->session_key);
  EXPECT_TRUE(first->effects.contains(LogicalSessionEffect::EmitReady));
  EXPECT_EQ((*manager)->held_slots(), 2U);
  const auto cause_missing = (*manager)->apply(first->session_key, LogicalSessionEvent::Abort);
  ASSERT_FALSE(cause_missing);
  EXPECT_EQ(cause_missing.error().context, "missing_session_cause");
  EXPECT_EQ((*manager)->state(first->session_key).value(), LogicalSessionState::Active);
  const auto rejected = (*manager)->open(5);
  ASSERT_FALSE(rejected);
  EXPECT_EQ(rejected.error().code, Error::Code::ResourceExhausted);
  EXPECT_EQ(rejected.error().context, "session_count_limit");

  const auto cancelled = (*manager)->apply(first->session_key, LogicalSessionEvent::Cancel);
  ASSERT_TRUE(cancelled);
  EXPECT_EQ(cancelled->state, LogicalSessionState::CancelRequested);
  EXPECT_TRUE(cancelled->effects.contains(LogicalSessionEffect::SuppressOutput));
  EXPECT_TRUE(cancelled->effects.contains(LogicalSessionEffect::EmitCancelAccepted));
  EXPECT_TRUE(cancelled->effects.contains(LogicalSessionEffect::RequestCleanup));
  EXPECT_EQ((*manager)->held_slots(), 2U);
  EXPECT_FALSE((*manager)->open(5));

  const auto released =
      (*manager)->apply(first->session_key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(released);
  EXPECT_EQ(released->state, LogicalSessionState::Closed);
  EXPECT_TRUE(released->effects.contains(LogicalSessionEffect::ReleaseSlot));
  EXPECT_TRUE(released->effects.contains(LogicalSessionEffect::EmitTerminal));
  EXPECT_EQ((*manager)->held_slots(), 1U);
  EXPECT_TRUE((*manager)->open(5));
  EXPECT_EQ(observed.back().state, LogicalSessionState::Active);
}

TEST(SessionManager, AdmissionHookAndGenerationRejectWithoutTakingSlot) {
  const auto limits = SessionLimits::create(60s, 10s, 30s, 60min, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  bool allow = false;
  auto manager = SessionManager::create(
      *limits, 7, clock, [](const auto&) {},
      [&]() -> Result<void> {
        if (!allow) {
          return unexpected(
              Error::make(Error::Code::ResourceExhausted, "quota unavailable", "memory_quota"));
        }
        return {};
      },
      false);
  ASSERT_TRUE(manager);
  const auto stale = (*manager)->open(6);
  ASSERT_FALSE(stale);
  EXPECT_EQ(stale.error().context, "stale_generation");
  EXPECT_EQ((*manager)->held_slots(), 0U);
  const auto memory_rejected = (*manager)->open(7);
  ASSERT_FALSE(memory_rejected);
  EXPECT_EQ(memory_rejected.error().context, "memory_quota");
  EXPECT_EQ((*manager)->held_slots(), 0U);
  allow = true;
  EXPECT_TRUE((*manager)->open(7));
  EXPECT_EQ((*manager)->held_slots(), 1U);
}

TEST(SessionManager, InitializationFailureNeverEmitsReadyAndRetainsCleanupSlot) {
  const auto limits = SessionLimits::create(60s, 10s, 30s, 60min, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      *limits, 5, clock, [&](const auto& transition) { observed.push_back(transition); }, {},
      false);
  ASSERT_TRUE(manager);
  const auto failed_open = (*manager)->open(5, [](std::uint64_t) -> Result<void> {
    return unexpected(
        Error::make(Error::Code::ConfigInvalid, "initialization failed", "initialization_failed"));
  });
  ASSERT_FALSE(failed_open);
  EXPECT_EQ(failed_open.error().context, "initialization_failed");
  ASSERT_EQ(observed.size(), 1U);
  EXPECT_EQ(observed[0].state, LogicalSessionState::Failed);
  EXPECT_FALSE(observed[0].effects.contains(LogicalSessionEffect::EmitReady));
  EXPECT_TRUE(observed[0].effects.contains(LogicalSessionEffect::RequestCleanup));
  EXPECT_TRUE(observed[0].effects.contains(LogicalSessionEffect::EmitTerminal));
  EXPECT_EQ((*manager)->held_slots(), 1U);
  EXPECT_FALSE((*manager)->open(5));
  EXPECT_TRUE((*manager)->apply(observed[0].session_key, LogicalSessionEvent::ReleaseAcknowledged));
  EXPECT_TRUE((*manager)->open(5));
}

TEST(SessionManager, InitializationPastDeadlineNeverEmitsReady) {
  const auto limits = SessionLimits::create(60min, 1s, 60min, 2s, 1);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      *limits, 5, clock, [&](const auto& transition) { observed.push_back(transition); }, {},
      false);
  ASSERT_TRUE(manager);
  const auto late_open = (*manager)->open(5, [&](std::uint64_t) -> Result<void> {
    clock.advance(2s);
    return {};
  });
  ASSERT_FALSE(late_open);
  EXPECT_EQ(late_open.error().code, Error::Code::Timeout);
  EXPECT_EQ(late_open.error().context, "max_duration");
  ASSERT_EQ(observed.size(), 1U);
  EXPECT_EQ(observed[0].state, LogicalSessionState::Failed);
  EXPECT_FALSE(observed[0].effects.contains(LogicalSessionEffect::EmitReady));
  EXPECT_TRUE(observed[0].effects.contains(LogicalSessionEffect::EmitTerminal));
  EXPECT_EQ((*manager)->held_slots(), 1U);
  EXPECT_TRUE((*manager)->apply(observed[0].session_key, LogicalSessionEvent::ReleaseAcknowledged));
  EXPECT_EQ((*manager)->held_slots(), 0U);
}

TEST(SessionManager, HeartbeatTimeoutUsesFakeClockAndRetainsCleanupSlot) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5);
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  clock.advance(10s);
  EXPECT_TRUE((*manager)->heartbeat_due(key).value());
  const auto pong = (*manager)->apply(key, LogicalSessionEvent::Ping);
  ASSERT_TRUE(pong);
  EXPECT_FALSE((*manager)->heartbeat_due(key).value());
  clock.advance(29s);
  EXPECT_TRUE((*manager)->sweep_due().empty());
  clock.advance(1s);
  const auto expired = (*manager)->sweep_due();
  ASSERT_EQ(expired.size(), 1U);
  EXPECT_EQ(expired[0].state, LogicalSessionState::CancelRequested);
  ASSERT_TRUE(expired[0].cause);
  EXPECT_EQ(expired[0].cause->context, "heartbeat_timeout");
  EXPECT_EQ((*manager)->held_slots(), 1U);
  EXPECT_TRUE((*manager)->sweep_due().empty());
  EXPECT_TRUE((*manager)->apply(key, LogicalSessionEvent::ReleaseAcknowledged));
  EXPECT_EQ((*manager)->held_slots(), 0U);
  EXPECT_FALSE((*manager)->state(key));
  EXPECT_EQ(observed.back().state, LogicalSessionState::Closed);
}

TEST(SessionManager, LatePingCannotReviveAnExpiredSession) {
  testing::FakeSchedulerClock clock;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock, [](const auto&) {}, {}, false);
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5);
  ASSERT_TRUE(opened);
  clock.advance(30s);
  const auto late_ping = (*manager)->apply(opened->session_key, LogicalSessionEvent::Ping);
  ASSERT_FALSE(late_ping);
  EXPECT_EQ(late_ping.error().code, Error::Code::Timeout);
  EXPECT_EQ(late_ping.error().context, "heartbeat_timeout");
  EXPECT_EQ((*manager)->state(opened->session_key).value(), LogicalSessionState::CancelRequested);
  EXPECT_TRUE((*manager)->sweep_due().empty());
}

TEST(SessionManager, IdleAndMaximumDurationCanExpireIndependently) {
  const auto limits = SessionLimits::create(2s, 1s, 5s, 8s, 2);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  auto manager = SessionManager::create(
      *limits, 5, clock, [](const auto&) {}, {}, false);
  ASSERT_TRUE(manager);
  const auto idle_key = (*manager)->open(5)->session_key;
  clock.advance(2s);
  const auto idle = (*manager)->sweep_due();
  ASSERT_EQ(idle.size(), 1U);
  EXPECT_EQ(idle[0].cause->context, "idle_timeout");
  EXPECT_TRUE((*manager)->apply(idle_key, LogicalSessionEvent::ReleaseAcknowledged));

  const auto long_limits = SessionLimits::create(20s, 1s, 30s, 8s, 1);
  ASSERT_TRUE(long_limits);
  auto duration_manager = SessionManager::create(
      *long_limits, 5, clock, [](const auto&) {}, {}, false);
  ASSERT_TRUE(duration_manager);
  const auto duration_key = (*duration_manager)->open(5)->session_key;
  clock.advance(6s);
  EXPECT_TRUE((*duration_manager)->apply(duration_key, LogicalSessionEvent::Ping));
  clock.advance(2s);
  const auto maximum = (*duration_manager)->sweep_due();
  ASSERT_EQ(maximum.size(), 1U);
  EXPECT_EQ(maximum[0].cause->context, "max_duration");
}

TEST(SessionManager, DrainStopsAdmissionAndWaitsForRelease) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5);
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  const auto draining = (*manager)->stop_admission_and_drain(
      Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown"));
  ASSERT_EQ(draining.size(), 1U);
  EXPECT_TRUE(draining[0].effects.contains(LogicalSessionEffect::StartDrain));
  EXPECT_EQ(draining[0].state, LogicalSessionState::Draining);
  EXPECT_FALSE((*manager)->heartbeat_due(key).value());
  EXPECT_FALSE((*manager)->admission_open());
  EXPECT_EQ((*manager)->open(5).error().context, "admission_closed");
  const auto ignored_input = (*manager)->apply(key, LogicalSessionEvent::Data);
  ASSERT_TRUE(ignored_input);
  EXPECT_TRUE(ignored_input->effects.empty());
  EXPECT_EQ((*manager)->state(key).value(), LogicalSessionState::Draining);
  clock.advance(31s);
  EXPECT_TRUE((*manager)->sweep_due().empty());
  EXPECT_EQ((*manager)->held_slots(), 1U);
  const auto drain_done = (*manager)->apply(key, LogicalSessionEvent::DrainCompleted);
  ASSERT_TRUE(drain_done);
  EXPECT_TRUE(drain_done->effects.contains(LogicalSessionEffect::RequestCleanup));
  EXPECT_EQ((*manager)->held_slots(), 1U);
  const auto closed = (*manager)->apply(key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(closed);
  EXPECT_EQ(closed->state, LogicalSessionState::Closed);
  EXPECT_EQ(closed->cause->context, "worker_shutdown");
  EXPECT_EQ((*manager)->held_slots(), 0U);
  EXPECT_EQ(observed.back().state, LogicalSessionState::Closed);
}

TEST(SessionManager, OwnerReportsAfterDeadlinePreserveTimeoutAndReleaseSlot) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(manager);
  const auto first = (*manager)->open(5);
  ASSERT_TRUE(first);
  EXPECT_TRUE((*manager)->apply(first->session_key, LogicalSessionEvent::Drain));
  clock.advance(60min);
  const auto late_drain =
      (*manager)->apply(first->session_key, LogicalSessionEvent::DrainCompleted);
  ASSERT_TRUE(late_drain);
  EXPECT_EQ(late_drain->state, LogicalSessionState::CancelRequested);
  ASSERT_TRUE(late_drain->cause);
  EXPECT_EQ(late_drain->cause->context, "max_duration");
  EXPECT_EQ((*manager)->held_slots(), 1U);
  const auto first_release =
      (*manager)->apply(first->session_key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(first_release);
  EXPECT_EQ(first_release->state, LogicalSessionState::Closed);
  EXPECT_EQ(first_release->cause->context, "max_duration");
  EXPECT_EQ((*manager)->held_slots(), 0U);

  auto second_manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(second_manager);
  const auto second = (*second_manager)->open(5);
  ASSERT_TRUE(second);
  EXPECT_TRUE((*second_manager)->apply(second->session_key, LogicalSessionEvent::Drain));
  EXPECT_TRUE((*second_manager)->apply(second->session_key, LogicalSessionEvent::DrainCompleted));
  const auto before = observed.size();
  clock.advance(60min);
  const auto late_release =
      (*second_manager)->apply(second->session_key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(late_release);
  EXPECT_EQ(late_release->state, LogicalSessionState::Closed);
  ASSERT_TRUE(late_release->cause);
  EXPECT_EQ(late_release->cause->context, "max_duration");
  EXPECT_EQ((*second_manager)->held_slots(), 0U);
  ASSERT_EQ(observed.size(), before + 2);
  EXPECT_EQ(observed[before].state, LogicalSessionState::CancelRequested);
  EXPECT_EQ(observed[before + 1].state, LogicalSessionState::Closed);
}

TEST(SessionManager, ExpiredEarlyReleaseAckDoesNotFreeSlot) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5);
  ASSERT_TRUE(opened);
  clock.advance(30s);
  const auto early_release =
      (*manager)->apply(opened->session_key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_FALSE(early_release);
  EXPECT_EQ(early_release.error().context, "unexpected_report");
  EXPECT_EQ((*manager)->state(opened->session_key).value(), LogicalSessionState::CancelRequested);
  EXPECT_EQ((*manager)->held_slots(), 1U);
  ASSERT_EQ(observed.size(), 2U);
  EXPECT_TRUE(observed.back().effects.contains(LogicalSessionEffect::RequestCleanup));
  EXPECT_FALSE(observed.back().effects.contains(LogicalSessionEffect::ReleaseSlot));
  const auto released =
      (*manager)->apply(opened->session_key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(released);
  EXPECT_EQ((*manager)->held_slots(), 0U);
}

TEST(SessionManager, RefusedClientEventFailsAndStillNeedsRelease) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(manager);
  const auto key = (*manager)->open(5)->session_key;
  auto pure_machine = LogicalSessionMachine::open(5, 5).value();
  ASSERT_TRUE(pure_machine.apply(LogicalSessionEvent::Admitted));
  const auto expected_refusal = pure_machine.apply(LogicalSessionEvent::Open);
  ASSERT_FALSE(expected_refusal);
  const auto refused = (*manager)->apply(key, LogicalSessionEvent::Open);
  ASSERT_FALSE(refused);
  EXPECT_EQ(refused.error(), expected_refusal.error());
  EXPECT_EQ((*manager)->state(key).value(), LogicalSessionState::Failed);
  EXPECT_EQ((*manager)->held_slots(), 1U);
  ASSERT_TRUE(observed.back().cause);
  EXPECT_EQ(observed.back().cause->context, "illegal_transition");
  EXPECT_TRUE(observed.back().effects.contains(LogicalSessionEffect::EmitTerminal));
  const auto released = (*manager)->apply(key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(released);
  EXPECT_TRUE(released->effects.contains(LogicalSessionEffect::ReleaseSlot));
  EXPECT_FALSE(released->effects.contains(LogicalSessionEffect::EmitTerminal));
  EXPECT_EQ((*manager)->held_slots(), 0U);
}

TEST(SessionManager, DedicatedTimerThreadUsesInjectedClock) {
  testing::FakeSchedulerClock clock;
  std::promise<std::thread::id> expired_on_thread;
  auto observed_thread = expired_on_thread.get_future();
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock, [&](const ManagedSessionTransition& transition) {
        if (transition.state == LogicalSessionState::CancelRequested) {
          expired_on_thread.set_value(std::this_thread::get_id());
        }
      });
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5);
  ASSERT_TRUE(opened);
  clock.advance(30s);
  (*manager)->notify_clock_advanced();
  ASSERT_EQ(observed_thread.wait_for(1s), std::future_status::ready);
  EXPECT_NE(observed_thread.get(), std::this_thread::get_id());
  EXPECT_EQ((*manager)->state(opened->session_key).value(), LogicalSessionState::CancelRequested);
  EXPECT_EQ((*manager)->held_slots(), 1U);
}
}  // namespace
}  // namespace tensorplate::serving
