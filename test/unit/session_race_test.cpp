// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <functional>
#include <future>
#include <memory>
#include <random>
#include <thread>
#include <vector>

#include "fake_scheduler_clock.hpp"
#include "serving/session/session_manager.hpp"
#include "synthetic_session_host.hpp"

namespace tensorplate::serving {
namespace {
using namespace std::chrono_literals;
using testing::SyntheticSessionHost;
using Effect = LogicalSessionEffect;
using Event = LogicalSessionEvent;

constexpr std::uint64_t kGeneration = 5;
constexpr std::uint32_t kRounds = 200;

/// A manager with its timer thread on the fake clock, and the stream, backend
/// and peer around it.
struct SyntheticWorker {
  explicit SyntheticWorker(bool start_peer = false)
      : host(clock, start_peer),
        manager(SessionManager::create(
                    SessionLimits::defaults(), kGeneration, clock,
                    [this](const auto& transition) { host.on_transition(transition); })
                    .value()) {
    host.attach(*manager);
  }
  ~SyntheticWorker() { host.stop(); }
  SyntheticWorker(const SyntheticWorker&) = delete;
  SyntheticWorker& operator=(const SyntheticWorker&) = delete;

  testing::FakeSchedulerClock clock;
  SyntheticSessionHost host;
  std::unique_ptr<SessionManager> manager;
};

using Actor = std::function<void(SyntheticWorker&, std::uint64_t)>;

Actor apply_twice(Event event, const char* reason = nullptr) {
  return [event, reason](SyntheticWorker& worker, std::uint64_t key) {
    for (int repeat = 0; repeat < 2; ++repeat) {
      (void)worker.manager->apply(
          key, event,
          reason != nullptr ? std::optional{Error::make(Error::Code::Unavailable, reason, reason)}
                            : std::nullopt);
    }
  };
}

void submit(SyntheticWorker& worker, std::uint64_t key) {
  for (int chunk = 0; chunk < 20; ++chunk) {
    const auto accepted = worker.manager->accept_input(key, 320);
    if (accepted && accepted->effects.contains(Effect::AcceptInput)) {
      (void)worker.manager->release_audio_input(key, 1, 320);
    }
  }
}

void worker_drain(SyntheticWorker& worker, std::uint64_t /*key*/) {
  (void)worker.manager->stop_admission_and_drain(
      Error::make(Error::Code::Unavailable, "worker shutting down", "worker_shutdown"));
}

void reap(SyntheticWorker& worker, std::uint64_t /*key*/) {
  worker.clock.advance(31s);
  worker.manager->notify_clock_advanced();
}

void stale_generation(SyntheticWorker& worker, std::uint64_t key) {
  const auto checked = worker.manager->check_generation(key, kGeneration + 1);
  EXPECT_FALSE(checked);
}

/// Offers task output while the session ends; an answer never goes back from
/// closed, nor from suppressed to queued.
void produce(SyntheticWorker& worker, std::uint64_t key) {
  const auto output = worker.host.session(key).output;
  bool suppressed = false;
  bool closed = false;
  for (int chunk = 0; chunk < 100; ++chunk) {
    OutputItem item{OutputKind::Audio, 960, 0, std::make_unique<OutputBody>()};
    const auto offered = output->offer(item, worker.clock.now());
    if (!offered) {
      ADD_FAILURE() << "offer refused";
      return;
    }
    EXPECT_FALSE(closed && *offered != OutputOffer::Closed);
    EXPECT_FALSE(suppressed && *offered != OutputOffer::Suppressed &&
                 *offered != OutputOffer::Closed);
    suppressed = suppressed || *offered == OutputOffer::Suppressed;
    closed = closed || *offered == OutputOffer::Closed;
    worker.host.output_written();
  }
}

void expect_one_terminal_outcome(const SyntheticSessionHost::Session& seen) {
  EXPECT_EQ(seen.count(Effect::EmitReady), 1U);
  EXPECT_EQ(seen.count(Effect::RequestCleanup), 1U);
  EXPECT_EQ(seen.count(Effect::ReleaseSlot), 1U);
  EXPECT_EQ(seen.count(Effect::EmitTerminal), 1U);
  EXPECT_EQ(seen.terminals_written, 1U);
  EXPECT_LE(seen.count(Effect::SuppressOutput), 1U);
  EXPECT_LE(seen.count(Effect::EmitCancelAccepted), 1U);
  EXPECT_LE(seen.count(Effect::StartDrain), 1U);
  EXPECT_TRUE(is_terminal(seen.state));

  const auto is_kind = [](OutputKind kind) {
    return [kind](const SyntheticSessionHost::Sent& sent) { return sent.kind == kind; };
  };
  ASSERT_FALSE(seen.sent.empty());
  EXPECT_EQ(std::count_if(seen.sent.begin(), seen.sent.end(), is_kind(OutputKind::Terminal)), 1);
  EXPECT_EQ(seen.sent.back().kind, OutputKind::Terminal);
  const auto accepted = std::find_if(seen.sent.begin(), seen.sent.end(), [](const auto& sent) {
    return sent.lifecycle == Effect::EmitCancelAccepted;
  });
  EXPECT_EQ(std::count_if(accepted, seen.sent.end(), is_kind(OutputKind::Audio)), 0);
}

/// Starts every actor on one session, each after its own short delay so the
/// rounds interleave differently, lets the backend release the session and
/// checks what its stream and peer saw.
void run_rounds(const SessionBudgets& budgets, const std::vector<Actor>& actors,
                bool start_peer = false) {
  for (std::uint32_t round = 0; round < kRounds; ++round) {
    SCOPED_TRACE(round);
    SyntheticWorker worker(start_peer);
    const auto opened = worker.manager->open(kGeneration, budgets);
    ASSERT_TRUE(opened);
    const auto key = opened->session_key;
    // Ready is read, so no output is pending when the clock jumps.
    worker.host.read_all();
    std::minstd_rand delays(round + 1);
    std::promise<void> start;
    const auto started = start.get_future().share();
    std::vector<std::thread> threads;
    threads.reserve(actors.size());
    for (const auto& actor : actors) {
      threads.emplace_back([&worker, &actor, started, key, yields = delays() % 64] {
        started.wait();
        for (auto yield = yields; yield != 0; --yield) {
          std::this_thread::yield();
        }
        actor(worker, key);
      });
    }
    start.set_value();
    for (auto& thread : threads) {
      thread.join();
    }
    // Ends a session the actors left open.
    (void)worker.manager->apply(key, Event::Cancel);
    ASSERT_TRUE(worker.host.wait_released(1, 10s));
    worker.host.read_all();
    expect_one_terminal_outcome(worker.host.session(key));
    EXPECT_EQ(worker.manager->held_slots(), 0U);
    OutputItem late{OutputKind::Control, 16, 0, std::make_unique<OutputBody>()};
    EXPECT_EQ(opened->output->offer(late, worker.clock.now()).value(), OutputOffer::Closed);
    EXPECT_FALSE(opened->output->take());
    if (::testing::Test::HasFailure()) {
      return;
    }
  }
}

const SessionBudgets kAudioIn = SessionBudgets::for_audio_input(32'000).value();
const SessionBudgets kTextIn = SessionBudgets::for_text_input(48'000).value();

TEST(SessionRaces, SubmitCancelDrainAndReapEndInOneTerminalOutcome) {
  run_rounds(kAudioIn, {submit, apply_twice(Event::Cancel), worker_drain, reap});
}

TEST(SessionRaces, HalfCloseCancelAndReapEndInOneTerminalOutcome) {
  run_rounds(kAudioIn, {submit, apply_twice(Event::HalfClose), apply_twice(Event::Cancel), reap});
}

TEST(SessionRaces, DisconnectStaleGenerationAndBackendFailureEndInOneTerminalOutcome) {
  run_rounds(kAudioIn,
             {submit, apply_twice(Event::Abort, "transport_closed"), stale_generation,
              apply_twice(Event::BackendReset, "backend_reset"), apply_twice(Event::HalfClose)});
}

TEST(SessionRaces, NoOutputFollowsTheTerminalOutcomeWhileProducersAndThePeerRun) {
  run_rounds(kTextIn,
             {produce, produce, apply_twice(Event::Cancel), apply_twice(Event::HalfClose),
              apply_twice(Event::BackendReset, "backend_reset")},
             true);
}

TEST(SessionRaces, DrainOfManySessionsRacingTheirCancelsReleasesEverySlotOnce) {
  constexpr std::size_t kSessions = 32;
  for (std::uint32_t round = 0; round < kRounds / 10; ++round) {
    SCOPED_TRACE(round);
    SyntheticWorker worker;
    std::vector<std::uint64_t> keys;
    for (std::size_t index = 0; index < kSessions; ++index) {
      const auto opened = worker.manager->open(kGeneration, kAudioIn);
      ASSERT_TRUE(opened);
      keys.push_back(opened->session_key);
    }
    worker.host.read_all();
    std::thread drainer([&worker] { worker_drain(worker, 0); });
    std::thread canceller([&worker, &keys] {
      for (const auto key : keys) {
        (void)worker.manager->apply(key, Event::Cancel);
      }
    });
    std::thread reaper([&worker] { reap(worker, 0); });
    drainer.join();
    canceller.join();
    reaper.join();
    ASSERT_TRUE(worker.host.wait_released(kSessions, 10s));
    worker.host.read_all();
    for (const auto key : keys) {
      SCOPED_TRACE(key);
      expect_one_terminal_outcome(worker.host.session(key));
    }
    EXPECT_EQ(worker.manager->held_slots(), 0U);
    EXPECT_FALSE(worker.manager->admission_open());
  }
}
}  // namespace
}  // namespace tensorplate::serving
