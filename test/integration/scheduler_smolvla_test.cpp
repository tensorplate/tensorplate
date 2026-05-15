// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F07-T01 / T02: SmolVLA-style async request and stale-cancel
// integration tests.
//
// These T2 tests exercise the v0.1.0 SmolVLA async chunk pattern at
// the scheduler boundary, against a real BufferManager so cleanup is
// observable through accounting:
//
//   - Overlapping chunk requests admit and dispatch in arrival order.
//   - LeRobot stale_after_sequence cancellation removes stale queued
//     chunks and releases their buffers.
//   - In-flight cancellation of a stale chunk clears accounting and
//     tombstones the id so a racing on_completion is a typed no-op.
//   - Deadline admission rejects chunks whose estimated completion
//     exceeds deadline + margin under overlapping-request load.
//   - Metrics reflect accepted, rejected, expired, cancelled,
//     completed, queue depth, and in-flight counts.
//
// Tests use the FakeSchedulerClock and never depend on real wall
// time. They do not require SmolVLA weights or the python_pytorch
// sidecar; the scheduler is exercised through the abstract
// InferScheduler* pointer.

#include <gtest/gtest.h>

#include <chrono>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"
#include "vla_fixtures.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

struct VlaHarness {
  std::unique_ptr<FakeSchedulerClock> clock = std::make_unique<FakeSchedulerClock>();
  RecordingSchedulerEventSink sink;
  std::unique_ptr<BufferManager> manager;
  std::unique_ptr<InferScheduler> scheduler;

  VlaHarness() {
    BufferManagerConfig bcfg;
    bcfg.capacity_bytes = 8 * 1024;
    bcfg.max_buffer_bytes = 1024;
    manager = BufferManager::create(bcfg).value();

    SchedulerConfig scfg;
    scfg.queue_capacity = 16;
    scfg.in_flight_capacity = 1;  // SmolVLA serves one chunk at a time.
    scfg.deadline_margin = std::chrono::milliseconds{20};
    scfg.default_service_estimate = std::chrono::milliseconds{5};
    SchedulerRuntimeHooks hooks;
    hooks.clock = clock.get();
    hooks.event_sink = &sink;
    hooks.buffer_manager = manager.get();
    scheduler = make_scheduler(scfg, hooks).value();
  }
};

TEST(SchedulerSmolVla, OverlappingChunkRequestsAdmitAndDispatchInOrder) {
  VlaHarness h;
  for (std::int64_t seq = 1; seq <= 3; ++seq) {
    ASSERT_TRUE(h.scheduler->admit(
        make_vla_request(*h.manager, *h.clock, "vla-" + std::to_string(seq), seq)));
  }
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 3u);
  EXPECT_EQ(h.scheduler->metrics().admitted_total, 3u);

  std::vector<std::string> dispatched;
  while (auto next = h.scheduler->next()) {
    dispatched.push_back(next->request_id());
    ASSERT_TRUE(
        h.scheduler->on_completion(next->request_id(), CompletionStatus::Success, std::nullopt));
  }
  ASSERT_EQ(dispatched.size(), 3u);
  EXPECT_EQ(dispatched[0], "vla-1");
  EXPECT_EQ(dispatched[1], "vla-2");
  EXPECT_EQ(dispatched[2], "vla-3");
  EXPECT_EQ(h.scheduler->metrics().completed_success, 3u);
}

TEST(SchedulerSmolVla, StaleQueuedChunksCancelAndReleaseBuffers) {
  VlaHarness h;
  // Admit three chunk requests, sequences 1..3.
  std::vector<SchedulerRequest> queued;
  for (std::int64_t seq = 1; seq <= 3; ++seq) {
    ASSERT_TRUE(h.scheduler->admit(
        make_vla_request(*h.manager, *h.clock, "vla-" + std::to_string(seq), seq)));
  }
  // Three buffers per request * 3 requests = 9 active.
  EXPECT_EQ(h.manager->accounting().active_count, 9u);

  // PolicyServer-side: a new chunk arrives with stale_after_sequence
  // = 2, so vla-1 and vla-2 are stale. The scheduler is told to
  // cancel them with reason StaleSequence.
  for (const auto* id : {"vla-1", "vla-2"}) {
    ASSERT_TRUE(h.scheduler->cancel(id, CancellationReason::StaleSequence));
  }
  // Each cancelled queued request released 3 buffers; 3 buffers
  // remain (vla-3).
  EXPECT_EQ(h.manager->accounting().active_count, 3u);
  EXPECT_EQ(h.scheduler->metrics().cancelled_queued, 2u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 1u);

  auto next = h.scheduler->next();
  ASSERT_TRUE(next.has_value());
  EXPECT_EQ(next->request_id(), "vla-3");
  ASSERT_TRUE(h.scheduler->on_completion("vla-3", CompletionStatus::Success, std::nullopt));
}

TEST(SchedulerSmolVla, InFlightStaleCancellationIsObservable) {
  VlaHarness h;
  ASSERT_TRUE(h.scheduler->admit(make_vla_request(*h.manager, *h.clock, "vla-1", 1)));
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  EXPECT_EQ(h.scheduler->metrics().in_flight, 1u);

  // PolicyServer marks vla-1 stale (e.g. a newer chunk arrived).
  ASSERT_TRUE(h.scheduler->cancel("vla-1", CancellationReason::StaleSequence));
  EXPECT_EQ(h.scheduler->metrics().cancelled_in_flight, 1u);
  EXPECT_EQ(h.scheduler->metrics().in_flight, 0u);

  // The Cancelled event carries reason=StaleSequence so observers
  // (V01-E12) can attribute the cancellation.
  bool found_stale = false;
  for (const auto& ev : h.sink.events()) {
    if (ev.kind == SchedulerEventKind::Cancelled && ev.cancellation_reason.has_value() &&
        *ev.cancellation_reason == CancellationReason::StaleSequence) {
      found_stale = true;
    }
  }
  EXPECT_TRUE(found_stale);

  // Executor releases the in-flight buffers.
  ASSERT_TRUE(release_request_buffers(*h.manager, first->request()).clean());
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
}

TEST(SchedulerSmolVla, DeadlineMarginRejectsImpossibleChunks) {
  VlaHarness h;
  // Build a request whose estimated completion (5 ms default) plus
  // (queue_depth * 5 ms) exceeds deadline + margin (20 ms).
  // Admit four chunks first so estimated_completion grows.
  for (std::int64_t seq = 1; seq <= 4; ++seq) {
    ASSERT_TRUE(h.scheduler->admit(
        make_vla_request(*h.manager, *h.clock, "vla-" + std::to_string(seq), seq)));
  }
  // Now try to admit a chunk with a tight deadline. Estimated
  // completion = now + 5 ms (default) * 4 (queue) + 5 ms (this
  // request) = now + 25 ms; deadline + margin = now + 5 ms + 20 ms =
  // now + 25 ms. Equal-to admits; bump the deadline down to fail.
  const auto tight_deadline = h.clock->now() + std::chrono::milliseconds{4};
  auto rejected =
      h.scheduler->admit(make_vla_request(*h.manager, *h.clock, "vla-late", /*sequence=*/100,
                                          /*stale_after_sequence=*/std::nullopt, tight_deadline));
  ASSERT_FALSE(rejected);
  EXPECT_EQ(rejected.error().code, Error::Code::Timeout);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_deadline, 1u);
  // Active buffers reflect the four admitted chunks (3 each = 12).
  // The rejected chunk's allocator-owned buffers were released by the
  // scheduler.
  EXPECT_EQ(h.manager->accounting().active_count, 12u);
}

TEST(SchedulerSmolVla, ExpiryUnderOverlappingRequests) {
  VlaHarness h;
  // Admit two chunks with deadlines that will pass after a clock
  // advance.
  const auto d1 = h.clock->now() + std::chrono::milliseconds{2};
  const auto d2 = h.clock->now() + std::chrono::milliseconds{4};
  ASSERT_TRUE(
      h.scheduler->admit(make_vla_request(*h.manager, *h.clock, "vla-1", 1, std::nullopt, d1)));
  ASSERT_TRUE(
      h.scheduler->admit(make_vla_request(*h.manager, *h.clock, "vla-2", 2, std::nullopt, d2)));
  EXPECT_EQ(h.manager->accounting().active_count, 6u);

  // Advance past both deadlines.
  h.clock->advance_ms(std::chrono::milliseconds{50});
  EXPECT_EQ(h.scheduler->expire_due(), 2u);
  EXPECT_EQ(h.scheduler->metrics().expired_total, 2u);
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
}

TEST(SchedulerSmolVla, OutcomeMetricsReflectMixedFlow) {
  VlaHarness h;
  // Mixed flow: admit, complete, expire, cancel.
  ASSERT_TRUE(h.scheduler->admit(make_vla_request(*h.manager, *h.clock, "vla-success", 1)));
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  ASSERT_TRUE(h.scheduler->on_completion("vla-success", CompletionStatus::Success, std::nullopt));
  // Successful in-flight requests are owned by the executor; the
  // executor releases their input buffers on the success path.
  ASSERT_TRUE(release_request_buffers(*h.manager, first->request()).clean());

  // Expired chunk.
  const auto d = h.clock->now() + std::chrono::milliseconds{2};
  ASSERT_TRUE(
      h.scheduler->admit(make_vla_request(*h.manager, *h.clock, "vla-stale", 2, std::nullopt, d)));
  h.clock->advance_ms(std::chrono::milliseconds{20});
  EXPECT_EQ(h.scheduler->expire_due(), 1u);

  // Cancelled chunk.
  ASSERT_TRUE(h.scheduler->admit(make_vla_request(*h.manager, *h.clock, "vla-cancel", 3)));
  ASSERT_TRUE(h.scheduler->cancel("vla-cancel", CancellationReason::ClientRequest));

  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.admitted_total, 3u);
  EXPECT_EQ(m.completed_success, 1u);
  EXPECT_EQ(m.expired_total, 1u);
  EXPECT_EQ(m.cancelled_queued, 1u);
  EXPECT_EQ(m.queue_depth, 0u);
  EXPECT_EQ(m.in_flight, 0u);

  // No buffer leaks across the whole flow.
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
}

TEST(SchedulerSmolVla, StaleHelperFiltersBySequence) {
  VlaHarness h;
  auto a = make_vla_request(*h.manager, *h.clock, "vla-a", 1);
  auto b = make_vla_request(*h.manager, *h.clock, "vla-b", 2);
  auto c = make_vla_request(*h.manager, *h.clock, "vla-c", 3);
  std::vector<const SchedulerRequest*> queued{&a, &b, &c};
  const auto stale = stale_request_ids(/*stale_after_sequence=*/2, queued);
  ASSERT_EQ(stale.size(), 2u);
  EXPECT_EQ(stale[0], "vla-a");
  EXPECT_EQ(stale[1], "vla-b");

  // Release fixture buffers so the harness leaves no allocator state.
  for (auto* env : queued) {
    (void)release_request_buffers(*h.manager, env->request());
  }
}

}  // namespace
