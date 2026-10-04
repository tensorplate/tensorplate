// SPDX-License-Identifier: Apache-2.0
//
// Logical-session lifecycle on the real clock: a SessionManager with its
// timer thread, a stream, a backend on its own thread and a reading peer.

#include <gtest/gtest.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <thread>
#include <vector>

#include "tensorplate/scheduler/clock.hpp"

#include "serving/session/session_manager.hpp"
#include "synthetic_session_host.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;

constexpr std::uint64_t kGeneration = 5;

struct RealClockWorker {
  RealClockWorker()
      : manager(SessionManager::create(
                    SessionLimits::defaults(), kGeneration, clock,
                    [this](const auto& transition) { host.on_transition(transition); })
                    .value()) {
    host.attach(*manager);
  }
  ~RealClockWorker() { host.stop(); }
  RealClockWorker(const RealClockWorker&) = delete;
  RealClockWorker& operator=(const RealClockWorker&) = delete;

  SystemSchedulerClock clock;
  testing::SyntheticSessionHost host{clock, true};
  std::unique_ptr<SessionManager> manager;
};

template <typename Predicate>
bool eventually(Predicate&& holds) {
  const auto give_up = std::chrono::steady_clock::now() + 10s;
  while (!holds()) {
    if (std::chrono::steady_clock::now() > give_up) {
      return false;
    }
    std::this_thread::sleep_for(1ms);
  }
  return true;
}

TEST(SessionLifecycleIntegration, FinalizeThenHalfCloseDrainsAndClosesWithOneTerminal) {
  RealClockWorker worker;
  const auto opened =
      worker.manager->open(kGeneration, SessionBudgets::for_audio_input(32'000).value());
  ASSERT_TRUE(opened);
  const auto key = opened->session_key;
  ASSERT_TRUE(worker.manager->accept_input(key, 3'200));
  ASSERT_TRUE(worker.manager->release_audio_input(key, 1, 3'200));

  ASSERT_TRUE(worker.manager->apply(key, Event::Finalize));
  ASSERT_TRUE(eventually([&] {
    return worker.manager->state(key).value_or(LogicalSessionState::Failed) ==
           LogicalSessionState::Active;
  }));
  ASSERT_TRUE(worker.manager->accept_input(key, 3'200));
  ASSERT_TRUE(worker.manager->release_audio_input(key, 1, 3'200));

  ASSERT_TRUE(worker.manager->apply(key, Event::HalfClose));
  ASSERT_TRUE(worker.host.wait_released(1, 10s));
  ASSERT_TRUE(worker.host.wait_sent(key, Effect::EmitTerminal, 10s));
  const auto seen = worker.host.session(key);
  EXPECT_EQ(seen.state, LogicalSessionState::Closed);
  EXPECT_EQ(seen.terminal_cause, std::nullopt);
  EXPECT_EQ(seen.terminals_written, 1U);
  EXPECT_EQ(seen.count(Effect::EmitTerminal), 1U);
  EXPECT_EQ(seen.count(Effect::SuppressOutput), 0U);
  ASSERT_EQ(seen.sent.size(), 2U);
  EXPECT_EQ(seen.sent.front().lifecycle, Effect::EmitReady);
  EXPECT_EQ(seen.sent.back().kind, OutputKind::Terminal);
  EXPECT_EQ(worker.manager->held_slots(), 0U);
  const auto ended = worker.manager->tombstone(key);
  ASSERT_TRUE(ended);
  EXPECT_EQ(ended->state, LogicalSessionState::Closed);
}

// The acknowledgement must not wait for the backend: every cancel here is
// applied while the backend reports nothing, other sessions keep the manager
// busy, and the peer reads.
TEST(SessionLifecycleIntegration, CancelIsAcknowledgedWithinOneHundredMillisecondsAtP99) {
  constexpr std::size_t kRounds = 16;
  constexpr std::size_t kSessionsPerRound = 64;
  const auto budgets = SessionBudgets::for_text_input(48'000).value();
  RealClockWorker worker;
  std::vector<std::chrono::nanoseconds> latencies;
  latencies.reserve(kRounds * kSessionsPerRound);
  std::size_t released = 0;

  for (std::size_t round = 0; round < kRounds; ++round) {
    SCOPED_TRACE(round);
    std::vector<ManagedSessionTransition> sessions;
    for (std::size_t index = 0; index < 2 * kSessionsPerRound; ++index) {
      auto opened = worker.manager->open(kGeneration, budgets);
      ASSERT_TRUE(opened);
      sessions.push_back(std::move(*opened));
    }
    // The second half of the sessions is the load: pings and task output.
    std::atomic<bool> stop_load{false};
    std::thread load([&] {
      while (!stop_load.load()) {
        for (std::size_t index = kSessionsPerRound; index < sessions.size(); ++index) {
          (void)worker.manager->apply(sessions[index].session_key, Event::Ping);
          OutputItem chunk{OutputKind::Audio, 960, 0, std::make_unique<OutputBody>()};
          (void)sessions[index].output->offer(chunk, worker.clock.now());
        }
        worker.host.output_written();
      }
    });

    worker.host.block_backend(true);
    std::vector<std::chrono::steady_clock::time_point> cancelled_at(kSessionsPerRound);
    for (std::size_t index = 0; index < kSessionsPerRound; ++index) {
      OutputItem pending{OutputKind::Audio, 960, 0, std::make_unique<OutputBody>()};
      ASSERT_EQ(sessions[index].output->offer(pending, worker.clock.now()).value(),
                OutputOffer::Queued);
      cancelled_at[index] = std::chrono::steady_clock::now();
      const auto cancelled = worker.manager->apply(sessions[index].session_key, Event::Cancel);
      ASSERT_TRUE(cancelled);
      EXPECT_TRUE(cancelled->effects.contains(Effect::EmitCancelAccepted));
      OutputItem late{OutputKind::Audio, 960, 0, std::make_unique<OutputBody>()};
      EXPECT_EQ(sessions[index].output->offer(late, worker.clock.now()).value(),
                OutputOffer::Suppressed);
    }
    for (std::size_t index = 0; index < kSessionsPerRound; ++index) {
      const auto key = sessions[index].session_key;
      ASSERT_TRUE(worker.host.wait_sent(key, Effect::EmitCancelAccepted, 10s));
      latencies.push_back(*worker.host.session(key).cancel_accepted_at - cancelled_at[index]);
      EXPECT_EQ(worker.manager->state(key).value(), LogicalSessionState::CancelRequested);
    }
    EXPECT_EQ(worker.manager->held_slots(), sessions.size());

    stop_load.store(true);
    load.join();
    worker.host.block_backend(false);
    for (std::size_t index = kSessionsPerRound; index < sessions.size(); ++index) {
      (void)worker.manager->apply(sessions[index].session_key, Event::Cancel);
    }
    released += sessions.size();
    ASSERT_TRUE(worker.host.wait_released(released, 10s));
  }

  ASSERT_EQ(latencies.size(), kRounds * kSessionsPerRound);
  std::sort(latencies.begin(), latencies.end());
  const auto p99 = latencies[(latencies.size() * 99 + 99) / 100 - 1];
  const auto micros = [](std::chrono::nanoseconds value) {
    return std::chrono::duration_cast<std::chrono::microseconds>(value).count();
  };
  RecordProperty("cancel_accepted_p50_us", std::to_string(micros(latencies[latencies.size() / 2])));
  RecordProperty("cancel_accepted_p99_us", std::to_string(micros(p99)));
  RecordProperty("cancel_accepted_max_us", std::to_string(micros(latencies.back())));
  EXPECT_LE(p99, 100ms);
  EXPECT_EQ(worker.manager->held_slots(), 0U);
}
}  // namespace
}  // namespace tensorplate::serving
