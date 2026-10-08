// SPDX-License-Identifier: Apache-2.0
#include "serving/session/session_manager.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <cstdint>
#include <future>
#include <memory>
#include <mutex>
#include <optional>
#include <thread>
#include <type_traits>
#include <utility>
#include <vector>

#include "tensorplate/scheduler/scheduler_request.hpp"

#include "fake_scheduler_clock.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
static_assert(
    std::is_same_v<decltype(ManagedSessionTransition{}.session_key), SchedulerRequest::SessionKey>);

const SessionBudgets kBudgets = SessionBudgets::for_audio_input(32'000).value();

TEST(SessionManager, CountCapHoldsUntilPhysicalRelease) {
  const auto limits = SessionLimits::create(60s, 10s, 30s, 60min, 2);
  ASSERT_TRUE(limits);
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      *limits, 5, clock, [&](const auto& transition) { observed.push_back(transition); }, {},
      false);
  ASSERT_TRUE(manager);
  const auto first = (*manager)->open(5, kBudgets);
  const auto second = (*manager)->open(5, kBudgets);
  ASSERT_TRUE(first);
  ASSERT_TRUE(second);
  EXPECT_NE(first->session_key, second->session_key);
  EXPECT_TRUE(first->effects.contains(LogicalSessionEffect::EmitReady));
  EXPECT_EQ((*manager)->held_slots(), 2U);
  const auto cause_missing = (*manager)->apply(first->session_key, LogicalSessionEvent::Abort);
  ASSERT_FALSE(cause_missing);
  EXPECT_EQ(cause_missing.error().context, "missing_session_cause");
  EXPECT_EQ((*manager)->state(first->session_key).value(), LogicalSessionState::Active);
  const auto rejected = (*manager)->open(5, kBudgets);
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
  EXPECT_FALSE((*manager)->open(5, kBudgets));

  const auto released =
      (*manager)->apply(first->session_key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(released);
  EXPECT_EQ(released->state, LogicalSessionState::Closed);
  EXPECT_TRUE(released->effects.contains(LogicalSessionEffect::ReleaseSlot));
  EXPECT_TRUE(released->effects.contains(LogicalSessionEffect::EmitTerminal));
  EXPECT_EQ((*manager)->held_slots(), 1U);
  EXPECT_TRUE((*manager)->open(5, kBudgets));
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
  const auto stale = (*manager)->open(6, kBudgets);
  ASSERT_FALSE(stale);
  EXPECT_EQ(stale.error().context, "stale_generation");
  EXPECT_EQ((*manager)->held_slots(), 0U);
  const auto memory_rejected = (*manager)->open(7, kBudgets);
  ASSERT_FALSE(memory_rejected);
  EXPECT_EQ(memory_rejected.error().context, "memory_quota");
  EXPECT_EQ((*manager)->held_slots(), 0U);
  allow = true;
  EXPECT_TRUE((*manager)->open(7, kBudgets));
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
  const auto failed_open = (*manager)->open(5, kBudgets, [](std::uint64_t) -> Result<void> {
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
  EXPECT_FALSE((*manager)->open(5, kBudgets));
  EXPECT_TRUE((*manager)->apply(observed[0].session_key, LogicalSessionEvent::ReleaseAcknowledged));
  EXPECT_TRUE((*manager)->open(5, kBudgets));
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
  const auto late_open = (*manager)->open(5, kBudgets, [&](std::uint64_t) -> Result<void> {
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
  const auto opened = (*manager)->open(5, kBudgets);
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
  const auto opened = (*manager)->open(5, kBudgets);
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
  const auto idle_key = (*manager)->open(5, kBudgets)->session_key;
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
  const auto duration_key = (*duration_manager)->open(5, kBudgets)->session_key;
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
  const auto opened = (*manager)->open(5, SessionBudgets::for_text_input(48'000).value());
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  const auto draining = (*manager)->stop_admission_and_drain(
      Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown"));
  ASSERT_EQ(draining.size(), 1U);
  EXPECT_TRUE(draining[0].effects.contains(LogicalSessionEffect::StartDrain));
  EXPECT_EQ(draining[0].state, LogicalSessionState::Draining);
  EXPECT_FALSE((*manager)->heartbeat_due(key).value());
  EXPECT_FALSE((*manager)->admission_open());
  EXPECT_EQ((*manager)->open(5, kBudgets).error().context, "admission_closed");
  const auto ignored_input = (*manager)->accept_input(key, 640);
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
  const auto first = (*manager)->open(5, kBudgets);
  ASSERT_TRUE(first);
  EXPECT_TRUE((*manager)->apply(first->session_key, LogicalSessionEvent::Drain));
  clock.advance(60min);
  const auto late_drain =
      (*manager)->apply(first->session_key, LogicalSessionEvent::DrainCompleted);
  ASSERT_TRUE(late_drain);
  EXPECT_EQ(late_drain->state, LogicalSessionState::CancelRequested);
  ASSERT_TRUE(late_drain->cause);
  EXPECT_EQ(late_drain->cause->context, "finalize_timeout");
  EXPECT_EQ((*manager)->held_slots(), 1U);
  const auto first_release =
      (*manager)->apply(first->session_key, LogicalSessionEvent::ReleaseAcknowledged);
  ASSERT_TRUE(first_release);
  EXPECT_EQ(first_release->state, LogicalSessionState::Closed);
  EXPECT_EQ(first_release->cause->context, "finalize_timeout");
  EXPECT_EQ((*manager)->held_slots(), 0U);

  auto second_manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(second_manager);
  const auto second = (*second_manager)->open(5, kBudgets);
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
  const auto opened = (*manager)->open(5, kBudgets);
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
  const auto key = (*manager)->open(5, kBudgets)->session_key;
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
  const auto opened = (*manager)->open(5, kBudgets);
  ASSERT_TRUE(opened);
  clock.advance(30s);
  (*manager)->notify_clock_advanced();
  ASSERT_EQ(observed_thread.wait_for(1s), std::future_status::ready);
  EXPECT_NE(observed_thread.get(), std::this_thread::get_id());
  EXPECT_EQ((*manager)->state(opened->session_key).value(), LogicalSessionState::CancelRequested);
  EXPECT_EQ((*manager)->held_slots(), 1U);
}

struct FakeBody final : OutputBody {};

OutputItem output_item(OutputKind kind, std::uint64_t bytes) {
  return OutputItem{kind, bytes, 0, std::make_unique<FakeBody>()};
}

struct ManagerUnderTest {
  explicit ManagerUnderTest(SessionManager::TransitionSink extra = {})
      : manager(SessionManager::create(
                    SessionLimits::defaults(), 5, clock,
                    [this, extra = std::move(extra)](const ManagedSessionTransition& transition) {
                      observed.push_back(transition);
                      if (extra) {
                        extra(transition);
                      }
                    },
                    {}, false)
                    .value()) {}

  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  std::unique_ptr<SessionManager> manager;
};

TEST(SessionManager, InputWithinCreditIsAcceptedAndAboveItFailsTheSession) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto opened = manager.open(5, kBudgets);
  ASSERT_TRUE(opened);
  EXPECT_EQ(opened->status.input_credit_bytes.limit(), 32'000U);
  EXPECT_EQ(opened->status.output_metadata_bytes.limit(), 16'384U);
  const auto key = opened->session_key;

  const auto accepted = manager.accept_input(key, 32'000);
  ASSERT_TRUE(accepted);
  EXPECT_TRUE(accepted->effects.contains(LogicalSessionEffect::AcceptInput));
  EXPECT_EQ(accepted->status.state, LogicalSessionState::Active);
  EXPECT_EQ(accepted->status.input_queue_depth, 1U);
  EXPECT_EQ(accepted->status.input_credit_bytes.available(), 0U);

  const auto above = manager.accept_input(key, 2);
  ASSERT_FALSE(above);
  EXPECT_EQ(above.error().code, Error::Code::ResourceExhausted);
  EXPECT_EQ(above.error().context, "input_credit_exceeded");
  const auto& failed = under_test.observed.back();
  EXPECT_EQ(failed.state, LogicalSessionState::Failed);
  EXPECT_FALSE(failed.effects.contains(LogicalSessionEffect::AcceptInput));
  EXPECT_TRUE(failed.effects.contains(LogicalSessionEffect::EmitTerminal));
  ASSERT_TRUE(failed.cause);
  EXPECT_EQ(failed.cause->context, "input_credit_exceeded");
  EXPECT_EQ(failed.status.input_queue_depth, 1U);
  EXPECT_EQ(manager.held_slots(), 1U);
}

TEST(SessionManager, EmptyInputFailsTheSession) {
  ManagerUnderTest under_test;
  const auto key = under_test.manager->open(5, kBudgets)->session_key;
  const auto empty = under_test.manager->accept_input(key, 0);
  ASSERT_FALSE(empty);
  EXPECT_EQ(empty.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(empty.error().context, "empty_input");
  EXPECT_EQ(under_test.manager->state(key).value(), LogicalSessionState::Failed);
}

TEST(SessionManager, DataWithoutItsSizeIsRefusedAndChangesNothing) {
  ManagerUnderTest under_test;
  const auto key = under_test.manager->open(5, kBudgets)->session_key;
  const auto before = under_test.observed.size();
  const auto unsized = under_test.manager->apply(key, LogicalSessionEvent::Data);
  ASSERT_FALSE(unsized);
  EXPECT_EQ(unsized.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(unsized.error().context, "input_without_size");
  EXPECT_EQ(under_test.manager->state(key).value(), LogicalSessionState::Active);
  EXPECT_EQ(under_test.observed.size(), before);
}

TEST(SessionManager, InputTheStateDoesNotAcceptIsNotCharged) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto draining_key = manager.open(5, kBudgets)->session_key;
  ASSERT_TRUE(manager.apply(draining_key, LogicalSessionEvent::HalfClose));
  const auto ignored = manager.accept_input(draining_key, 64'000);
  ASSERT_TRUE(ignored);
  EXPECT_TRUE(ignored->effects.empty());
  EXPECT_EQ(manager.status(draining_key)->input_queue_depth, 0U);
  EXPECT_EQ(manager.status(draining_key)->input_credit_bytes.used(), 0U);

  const auto finalizing_key = manager.open(5, kBudgets)->session_key;
  ASSERT_TRUE(manager.apply(finalizing_key, LogicalSessionEvent::Finalize));
  const auto refused = manager.accept_input(finalizing_key, 640);
  ASSERT_FALSE(refused);
  EXPECT_EQ(refused.error().context, "illegal_transition");
  EXPECT_EQ(manager.status(finalizing_key)->state, LogicalSessionState::Failed);
  EXPECT_EQ(manager.status(finalizing_key)->input_credit_bytes.used(), 0U);
}

TEST(SessionManager, AudioCreditReturnsAsInputIsConsumed) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto key = manager.open(5, kBudgets)->session_key;
  ASSERT_TRUE(manager.accept_input(key, 16'000));
  ASSERT_TRUE(manager.accept_input(key, 16'000));
  const auto released = manager.release_audio_input(key, 1, 16'000);
  ASSERT_TRUE(released);
  EXPECT_EQ(released->input_queue_depth, 1U);
  EXPECT_EQ(released->input_credit_bytes.used(), 16'000U);
  EXPECT_EQ(manager.status(key).value(), *released);
  EXPECT_TRUE(manager.accept_input(key, 16'000));
}

TEST(SessionManager, TextCreditKeepsTwoWaitingSegmentsAndOneActive) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto budgets = SessionBudgets::for_text_input(48'000).value();
  const auto opened = manager.open(5, budgets);
  ASSERT_TRUE(opened);
  EXPECT_EQ(opened->status.output_pcm_bytes.limit(), 96'000U);
  const auto key = opened->session_key;
  ASSERT_TRUE(manager.accept_input(key, 100));
  ASSERT_TRUE(manager.accept_input(key, 200));
  const auto started = manager.start_text_segment(key);
  ASSERT_TRUE(started);
  EXPECT_EQ(started->input_queue_depth, 2U);
  EXPECT_EQ(started->input_credit_bytes.used(), 200U);
  ASSERT_TRUE(manager.accept_input(key, 300));
  EXPECT_EQ(manager.status(key)->input_queue_depth, 3U);
  const auto finished = manager.finish_text_segment(key);
  ASSERT_TRUE(finished);
  EXPECT_EQ(finished->input_queue_depth, 2U);

  const auto fourth = manager.accept_input(key, 1);
  ASSERT_FALSE(fourth);
  EXPECT_EQ(fourth.error().context, "input_credit_exceeded");
  EXPECT_EQ(manager.state(key).value(), LogicalSessionState::Failed);
}

TEST(SessionManager, CreditReleaseTheCreditRefusesFailsTheSession) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto key = manager.open(5, kBudgets)->session_key;
  ASSERT_TRUE(manager.accept_input(key, 640));
  const auto mismatch = manager.release_audio_input(key, 1, 641);
  ASSERT_FALSE(mismatch);
  EXPECT_EQ(mismatch.error().code, Error::Code::Internal);
  EXPECT_EQ(mismatch.error().context, "input_release_mismatch");
  EXPECT_EQ(under_test.observed.back().state, LogicalSessionState::Failed);
  EXPECT_EQ(under_test.observed.back().cause->context, "input_release_mismatch");
  EXPECT_EQ(manager.status(key)->input_credit_bytes.used(), 640U);

  const auto wrong_kind = manager.start_text_segment(key);
  ASSERT_FALSE(wrong_kind);
  EXPECT_EQ(wrong_kind.error().context, "wrong_input_kind");
  EXPECT_EQ(under_test.observed.back().cause->context, "input_release_mismatch");
  EXPECT_FALSE(manager.release_audio_input(key + 1, 1, 640));
}

TEST(SessionManager, UndeliveredOutputEndsTheSessionAfterFiveSecondsWithoutProgress) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  auto& clock = under_test.clock;
  const auto opened = manager.open(5, SessionBudgets::for_text_input(48'000).value());
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  auto& output = *opened->output;

  clock.advance(5s);
  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::Ping));
  EXPECT_TRUE(manager.sweep_due().empty());

  auto first = output_item(OutputKind::Audio, 960);
  auto second = output_item(OutputKind::Audio, 960);
  ASSERT_EQ(output.offer(first, clock.now()).value(), OutputOffer::Queued);
  ASSERT_EQ(output.offer(second, clock.now()).value(), OutputOffer::Queued);
  const auto sent = output.take();
  ASSERT_TRUE(sent);
  clock.advance(4s);
  ASSERT_TRUE(output.delivered(sent->sequence, clock.now()));
  clock.advance(4s);
  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::Ping));
  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::StatusRequest));
  clock.advance(999ms);
  EXPECT_TRUE(manager.sweep_due().empty());
  clock.advance(1ms);
  const auto stalled = manager.sweep_due();
  ASSERT_EQ(stalled.size(), 1U);
  EXPECT_EQ(stalled[0].state, LogicalSessionState::CancelRequested);
  EXPECT_TRUE(stalled[0].effects.contains(LogicalSessionEffect::SuppressOutput));
  ASSERT_TRUE(stalled[0].cause);
  EXPECT_EQ(stalled[0].cause->code, Error::Code::ResourceExhausted);
  EXPECT_EQ(stalled[0].cause->context, "slow_consumer");
  EXPECT_EQ(stalled[0].status.output_pcm_bytes.used(), 0U);
  EXPECT_FALSE(output.take());
  EXPECT_TRUE(manager.sweep_due().empty());
  EXPECT_EQ(manager.held_slots(), 1U);
}

TEST(SessionManager, LatePingCannotReviveASessionWhoseOutputStalled) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto opened = manager.open(5, SessionBudgets::for_text_input(48'000).value());
  ASSERT_TRUE(opened);
  auto chunk = output_item(OutputKind::Audio, 960);
  ASSERT_EQ(opened->output->offer(chunk, under_test.clock.now()).value(), OutputOffer::Queued);
  under_test.clock.advance(5s);
  const auto late_ping = manager.apply(opened->session_key, LogicalSessionEvent::Ping);
  ASSERT_FALSE(late_ping);
  EXPECT_EQ(late_ping.error().code, Error::Code::ResourceExhausted);
  EXPECT_EQ(late_ping.error().context, "slow_consumer");
  EXPECT_EQ(manager.state(opened->session_key).value(), LogicalSessionState::CancelRequested);
  EXPECT_TRUE(manager.sweep_due().empty());
}

TEST(SessionManager, DrainingSessionStillEndsWhenItsOutputStalls) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto opened = manager.open(5, SessionBudgets::for_text_input(48'000).value());
  ASSERT_TRUE(opened);
  ASSERT_TRUE(manager.apply(opened->session_key, LogicalSessionEvent::HalfClose));
  auto chunk = output_item(OutputKind::Audio, 960);
  ASSERT_EQ(opened->output->offer(chunk, under_test.clock.now()).value(), OutputOffer::Queued);
  under_test.clock.advance(4'999ms);
  EXPECT_TRUE(manager.sweep_due().empty());
  under_test.clock.advance(1ms);
  const auto stalled = manager.sweep_due();
  ASSERT_EQ(stalled.size(), 1U);
  EXPECT_EQ(stalled[0].cause->context, "slow_consumer");
}

TEST(SessionManager, OutputIsSuppressedBeforeTheSinkRunsAndOutlivesTheSlot) {
  std::vector<OutputOffer> task_offers;
  std::vector<OutputOffer> terminal_offers;
  SchedulerClock::TimePoint now;
  ManagerUnderTest under_test([&](const ManagedSessionTransition& transition) {
    if (transition.effects.contains(LogicalSessionEffect::SuppressOutput)) {
      auto late = output_item(OutputKind::Audio, 960);
      task_offers.push_back(transition.output->offer(late, now).value());
    }
    if (transition.effects.contains(LogicalSessionEffect::EmitTerminal)) {
      auto terminal = output_item(OutputKind::Terminal, 32);
      terminal_offers.push_back(transition.output->offer(terminal, now).value());
    }
  });
  auto& manager = *under_test.manager;
  now = under_test.clock.now();
  const auto opened = manager.open(5, SessionBudgets::for_text_input(48'000).value());
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  auto chunk = output_item(OutputKind::Audio, 960);
  ASSERT_EQ(opened->output->offer(chunk, now).value(), OutputOffer::Queued);

  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::Cancel));
  ASSERT_EQ(task_offers.size(), 1U);
  EXPECT_EQ(task_offers[0], OutputOffer::Suppressed);
  EXPECT_EQ(opened->output->pcm_usage().used(), 0U);

  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::ReleaseAcknowledged));
  EXPECT_EQ(manager.held_slots(), 0U);
  ASSERT_EQ(terminal_offers.size(), 1U);
  EXPECT_EQ(terminal_offers[0], OutputOffer::Queued);
  const auto terminal = opened->output->take();
  ASSERT_TRUE(terminal);
  EXPECT_EQ(terminal->item.kind, OutputKind::Terminal);
  auto after = output_item(OutputKind::Control, 4);
  EXPECT_EQ(opened->output->offer(after, now).value(), OutputOffer::Closed);
}

TEST(SessionManager, EndedSessionLeavesATombstoneForSixtySeconds) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto key = manager.open(5, kBudgets)->session_key;
  EXPECT_FALSE(manager.tombstone(key));
  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::Cancel));
  EXPECT_FALSE(manager.tombstone(key));
  under_test.clock.advance(3s);
  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::ReleaseAcknowledged));

  const auto tombstone = manager.tombstone(key);
  ASSERT_TRUE(tombstone);
  EXPECT_EQ(tombstone->session_key, key);
  EXPECT_EQ(tombstone->generation, 5U);
  EXPECT_EQ(tombstone->state, LogicalSessionState::Closed);
  EXPECT_EQ(tombstone->code, Error::Code::Cancelled);
  EXPECT_EQ(tombstone->reason, "client_cancelled");
  EXPECT_EQ(tombstone->ended_at, under_test.clock.now());

  const auto late = manager.apply(key, LogicalSessionEvent::FinalizeCompleted);
  ASSERT_FALSE(late);
  EXPECT_EQ(late.error().code, Error::Code::NotReady);
  EXPECT_EQ(late.error().context, "session_ended");
  EXPECT_EQ(manager.state(key).error().context, "session_ended");
  EXPECT_EQ(manager.status(key).error().context, "session_ended");
  EXPECT_EQ(manager.finish_text_segment(key).error().context, "session_ended");
  EXPECT_EQ(manager.state(key + 1).error().context, "unknown_session");

  under_test.clock.advance(59'999ms);
  EXPECT_TRUE(manager.tombstone(key));
  under_test.clock.advance(1ms);
  EXPECT_FALSE(manager.tombstone(key));
  EXPECT_EQ(manager.state(key).error().context, "unknown_session");
}

TEST(SessionManager, FailedSessionLeavesItsTombstoneOnlyAtRelease) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto key = manager.open(5, kBudgets)->session_key;
  ASSERT_FALSE(manager.accept_input(key, 32'001));
  EXPECT_FALSE(manager.tombstone(key));
  ASSERT_TRUE(manager.apply(key, LogicalSessionEvent::ReleaseAcknowledged));
  const auto tombstone = manager.tombstone(key);
  ASSERT_TRUE(tombstone);
  EXPECT_EQ(tombstone->state, LogicalSessionState::Failed);
  EXPECT_EQ(tombstone->code, Error::Code::ResourceExhausted);
  EXPECT_EQ(tombstone->reason, "input_credit_exceeded");
}

TEST(SessionManager, GenerationOfALaterMessageIsCheckedAgainstTheSession) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto unknown = manager.check_generation(99, 5);
  ASSERT_FALSE(unknown);
  EXPECT_EQ(unknown.error().context, "unknown_session");
  const auto opened = manager.open(5, kBudgets);
  ASSERT_TRUE(opened);
  EXPECT_TRUE(manager.check_generation(opened->session_key, 5));
  ASSERT_EQ(under_test.observed.size(), 1U);
  const auto stale = manager.check_generation(opened->session_key, 6);
  ASSERT_FALSE(stale);
  EXPECT_EQ(stale.error().code, Error::Code::NotReady);
  EXPECT_EQ(stale.error().context, "stale_generation");
  ASSERT_EQ(under_test.observed.size(), 2U);
  EXPECT_EQ(under_test.observed[1].state, LogicalSessionState::Failed);
  ASSERT_TRUE(under_test.observed[1].cause);
  EXPECT_EQ(under_test.observed[1].cause->context, "stale_generation");
  EXPECT_EQ(manager.held_slots(), 1U);
}

TEST(SessionManager, StaleGenerationOnAnExpiredSessionLeavesTheTimeoutAsItsCause) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto opened = manager.open(5, kBudgets);
  ASSERT_TRUE(opened);
  under_test.clock.advance(30s);
  const auto stale = manager.check_generation(opened->session_key, 4);
  ASSERT_FALSE(stale);
  EXPECT_EQ(stale.error().context, "stale_generation");
  ASSERT_EQ(under_test.observed.size(), 2U);
  EXPECT_EQ(under_test.observed[1].state, LogicalSessionState::CancelRequested);
  EXPECT_EQ(under_test.observed[1].cause.value().context, "heartbeat_timeout");
  EXPECT_TRUE(manager.sweep_due().empty());
}

TEST(SessionManager, WorkerDrainCutByItsDeadlineKeepsTheDrainCause) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto swept_session = manager.open(5, kBudgets);
  const auto messaged_session = manager.open(5, kBudgets);
  ASSERT_TRUE(swept_session);
  ASSERT_TRUE(messaged_session);
  const auto draining = manager.stop_admission_and_drain(
      Error::make(Error::Code::Unavailable, "deployment retiring", "deployment_retired"));
  ASSERT_EQ(draining.size(), 2U);
  under_test.clock.advance(10s);

  // A client message that finds the deadline passed is answered with the cause.
  const auto late = manager.apply(messaged_session->session_key, LogicalSessionEvent::Ping);
  ASSERT_FALSE(late);
  EXPECT_EQ(late.error().code, Error::Code::Unavailable);
  EXPECT_EQ(late.error().context, "deployment_retired");

  const auto swept = manager.sweep_due();
  ASSERT_EQ(swept.size(), 1U);
  EXPECT_EQ(swept[0].session_key, swept_session->session_key);
  for (const auto key : {swept_session->session_key, messaged_session->session_key}) {
    EXPECT_EQ(manager.state(key).value(), LogicalSessionState::CancelRequested);
    const auto closed = manager.apply(key, LogicalSessionEvent::ReleaseAcknowledged);
    ASSERT_TRUE(closed);
    ASSERT_TRUE(closed->cause);
    EXPECT_EQ(closed->cause->code, Error::Code::Unavailable);
    EXPECT_EQ(closed->cause->context, "deployment_retired");
  }
}

TEST(SessionManager, WorkerDrainEndedByAnotherLimitReportsThatLimit) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto opened = manager.open(5, SessionBudgets::for_text_input(48'000).value());
  ASSERT_TRUE(opened);
  ASSERT_EQ(manager
                .stop_admission_and_drain(Error::make(Error::Code::Unavailable,
                                                      "worker shutting down", "worker_shutdown"))
                .size(),
            1U);
  auto chunk = output_item(OutputKind::Audio, 960);
  ASSERT_EQ(opened->output->offer(chunk, under_test.clock.now()).value(), OutputOffer::Queued);
  under_test.clock.advance(5s);
  const auto stalled = manager.sweep_due();
  ASSERT_EQ(stalled.size(), 1U);
  EXPECT_EQ(stalled[0].cause.value().context, "slow_consumer");
}

TEST(SessionManager, ClientDrainCutByItsDeadlineEndsAsATimeout) {
  ManagerUnderTest under_test;
  auto& manager = *under_test.manager;
  const auto opened = manager.open(5, kBudgets);
  ASSERT_TRUE(opened);
  ASSERT_TRUE(manager.apply(opened->session_key, LogicalSessionEvent::HalfClose));
  under_test.clock.advance(10s);
  const auto cut = manager.sweep_due();
  ASSERT_EQ(cut.size(), 1U);
  EXPECT_EQ(cut[0].cause.value().code, Error::Code::Timeout);
  EXPECT_EQ(cut[0].cause.value().context, "finalize_timeout");
}

TEST(SessionManager, RestartedWorkerKnowsNothingOfEarlierSessions) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> replayed;
  std::shared_ptr<BoundedOutputQueue> earlier_output;
  std::uint64_t live_key = 0;
  std::uint64_t ended_key = 0;
  {
    ManagerUnderTest earlier;
    const auto live = earlier.manager->open(5, kBudgets);
    const auto ended = earlier.manager->open(5, kBudgets);
    ASSERT_TRUE(live);
    ASSERT_TRUE(ended);
    live_key = live->session_key;
    ended_key = ended->session_key;
    earlier_output = live->output;
    ASSERT_TRUE(earlier.manager->accept_input(live_key, 640));
    auto transcript = output_item(OutputKind::Result, 48);
    ASSERT_EQ(earlier_output->offer(transcript, earlier.clock.now()).value(), OutputOffer::Queued);
    ASSERT_TRUE(earlier.manager->apply(ended_key, LogicalSessionEvent::Cancel));
    ASSERT_TRUE(earlier.manager->apply(ended_key, LogicalSessionEvent::ReleaseAcknowledged));
    ASSERT_TRUE(earlier.manager->tombstone(ended_key));
  }
  auto restarted = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { replayed.push_back(transition); }, {}, false);
  ASSERT_TRUE(restarted);
  EXPECT_EQ((*restarted)->held_slots(), 0U);
  for (const auto key : {live_key, ended_key}) {
    EXPECT_EQ((*restarted)->state(key).error().context, "unknown_session");
    EXPECT_EQ((*restarted)->check_generation(key, 5).error().context, "unknown_session");
    EXPECT_EQ((*restarted)->apply(key, LogicalSessionEvent::Ping).error().context,
              "unknown_session");
    EXPECT_FALSE((*restarted)->tombstone(key));
  }
  EXPECT_TRUE(replayed.empty());

  // A new session may reuse an earlier key; it starts with nothing of it.
  const auto fresh = (*restarted)->open(5, kBudgets);
  ASSERT_TRUE(fresh);
  EXPECT_EQ(fresh->session_key, live_key);
  EXPECT_NE(fresh->output, earlier_output);
  EXPECT_EQ(fresh->status.input_queue_depth, 0U);
  EXPECT_EQ(fresh->status.output_metadata_bytes.used(), 0U);
  EXPECT_FALSE(fresh->output->take());
  ASSERT_EQ(replayed.size(), 1U);
  EXPECT_TRUE(replayed[0].effects.contains(LogicalSessionEffect::EmitReady));
}

TEST(SessionManager, TimerThreadEndsASessionAtItsFinalizeDeadline) {
  testing::FakeSchedulerClock clock;
  std::promise<Error> ended;
  auto cause = ended.get_future();
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock, [&](const ManagedSessionTransition& transition) {
        if (transition.state == LogicalSessionState::CancelRequested) {
          ended.set_value(transition.cause.value());
        }
      });
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5, kBudgets);
  ASSERT_TRUE(opened);
  ASSERT_TRUE((*manager)->apply(opened->session_key, LogicalSessionEvent::Finalize));
  clock.advance(10s);
  (*manager)->notify_clock_advanced();
  ASSERT_EQ(cause.wait_for(1s), std::future_status::ready);
  const auto error = cause.get();
  EXPECT_EQ(error.code, Error::Code::Timeout);
  EXPECT_EQ(error.context, "finalize_timeout");
}

TEST(SessionManager, TimerThreadEndsASessionWhoseOutputStalls) {
  testing::FakeSchedulerClock clock;
  std::promise<Error> ended;
  auto cause = ended.get_future();
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock, [&](const ManagedSessionTransition& transition) {
        if (transition.state == LogicalSessionState::CancelRequested) {
          ended.set_value(transition.cause.value());
        }
      });
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5, SessionBudgets::for_text_input(48'000).value());
  ASSERT_TRUE(opened);
  auto chunk = output_item(OutputKind::Audio, 960);
  ASSERT_EQ(opened->output->offer(chunk, clock.now()).value(), OutputOffer::Queued);
  clock.advance(5s);
  (*manager)->notify_clock_advanced();
  ASSERT_EQ(cause.wait_for(1s), std::future_status::ready);
  EXPECT_EQ(cause.get().context, "slow_consumer");
}

TEST(SessionManager, SinkSeesCreditReturnsAndReactivationInTheOrderApplied) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5, kBudgets);
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  ASSERT_TRUE((*manager)->accept_input(key, 640));
  ASSERT_TRUE((*manager)->release_audio_input(key, 1, 640));
  ASSERT_TRUE((*manager)->apply(key, LogicalSessionEvent::Finalize));
  ASSERT_TRUE((*manager)->apply(key, LogicalSessionEvent::FinalizeCompleted));

  ASSERT_EQ(observed.size(), 5U);
  EXPECT_EQ(observed[0].event, LogicalSessionEvent::Admitted);
  EXPECT_EQ(observed[1].event, LogicalSessionEvent::Data);
  EXPECT_EQ(observed[1].status.input_credit_bytes.used(), 640U);
  // A return of credit: no event, no effects, the status after it.
  EXPECT_FALSE(observed[2].event.has_value());
  EXPECT_TRUE(observed[2].effects.empty());
  EXPECT_EQ(observed[2].status.input_credit_bytes.used(), 0U);
  EXPECT_EQ(observed[3].event, LogicalSessionEvent::Finalize);
  EXPECT_EQ(observed[4].event, LogicalSessionEvent::FinalizeCompleted);
  EXPECT_TRUE(observed[4].effects.empty());
  EXPECT_EQ(observed[4].state, LogicalSessionState::Active);

  // A report that changes nothing is still not shown.
  ASSERT_TRUE((*manager)->apply(key, LogicalSessionEvent::Cancel));
  ASSERT_TRUE((*manager)->apply(key, LogicalSessionEvent::FinalizeCompleted));
  ASSERT_EQ(observed.size(), 6U);
  EXPECT_EQ(observed[5].event, LogicalSessionEvent::Cancel);
}

TEST(SessionManager, RefusedCreditReleaseShowsOnlyTheFailure) {
  testing::FakeSchedulerClock clock;
  std::vector<ManagedSessionTransition> observed;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const auto& transition) { observed.push_back(transition); }, {}, false);
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5, kBudgets);
  ASSERT_TRUE(opened);
  ASSERT_FALSE((*manager)->release_audio_input(opened->session_key, 1, 640));
  ASSERT_EQ(observed.size(), 2U);
  EXPECT_EQ(observed[1].event, LogicalSessionEvent::Fail);
  EXPECT_EQ(observed[1].state, LogicalSessionState::Failed);
}

// While the sink is busy with a credit return no other transition may reach
// it, or a status could arrive after a newer one.
TEST(SessionManager, CreditReturnIsShownInsideTheSerializedSection) {
  testing::FakeSchedulerClock clock;
  std::mutex mu;
  std::vector<std::optional<LogicalSessionEvent>> finished;
  std::promise<void> showing_credit;
  auto manager = SessionManager::create(
      SessionLimits::defaults(), 5, clock,
      [&](const ManagedSessionTransition& transition) {
        if (!transition.event) {
          showing_credit.set_value();
          std::this_thread::sleep_for(100ms);
        }
        const std::lock_guard guard(mu);
        finished.push_back(transition.event);
      },
      {}, false);
  ASSERT_TRUE(manager);
  const auto opened = (*manager)->open(5, kBudgets);
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  ASSERT_TRUE((*manager)->accept_input(key, 640));
  std::thread client([&] {
    if (showing_credit.get_future().wait_for(5s) == std::future_status::ready) {
      (void)(*manager)->apply(key, LogicalSessionEvent::Ping);
    }
  });
  ASSERT_TRUE((*manager)->release_audio_input(key, 1, 640));
  client.join();

  const std::lock_guard guard(mu);
  ASSERT_EQ(finished.size(), 4U);
  EXPECT_FALSE(finished[2].has_value());
  EXPECT_EQ(finished[3], LogicalSessionEvent::Ping);
}
}  // namespace
}  // namespace tensorplate::serving
