// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F03-T01 / T02: Deadline-aware admission and queued expiry.
//
// All deadline checks use the injected FakeSchedulerClock so behavior
// is deterministic. The scheduler:
//
//   - Rejects requests already past their deadline.
//   - Rejects requests whose estimated completion exceeds the
//     configured deadline + margin.
//   - Accepts requests whose estimated completion fits inside
//     deadline + margin.
//   - Sweeps the queue for expired requests on next() and on the
//     explicit expire_due() entry point.
//   - Records admission_rejected_deadline and expired counters.

#include <gtest/gtest.h>

#include <chrono>
#include <utility>

#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

// Helper to build a scheduler with custom deadline parameters.
struct DeadlineHarness {
  std::unique_ptr<FakeSchedulerClock> clock = std::make_unique<FakeSchedulerClock>();
  RecordingSchedulerEventSink sink;
  std::unique_ptr<InferScheduler> scheduler;

  DeadlineHarness(std::chrono::milliseconds margin,
                  std::chrono::milliseconds default_estimate = std::chrono::milliseconds{1},
                  std::size_t queue_capacity = 8, std::size_t in_flight_capacity = 4) {
    SchedulerConfig config;
    config.deadline_margin = margin;
    config.default_service_estimate = default_estimate;
    config.queue_capacity = queue_capacity;
    config.in_flight_capacity = in_flight_capacity;
    SchedulerRuntimeHooks hooks;
    hooks.clock = clock.get();
    hooks.event_sink = &sink;
    scheduler = make_scheduler(config, hooks).value();
  }
};

TEST(SchedulerDeadline, NoDeadlineIsAlwaysAdmitted) {
  DeadlineHarness h{std::chrono::milliseconds{0}};
  auto envelope = make_scheduler_request(make_infer_request("a"), *h.clock);
  EXPECT_TRUE(h.scheduler->admit(std::move(envelope)));
}

TEST(SchedulerDeadline, AlreadyPastDeadlineIsRejectedTimeout) {
  DeadlineHarness h{std::chrono::milliseconds{50}};
  // Set a deadline 1 ms before "now".
  const auto deadline = h.clock->now() - std::chrono::milliseconds{1};
  // InferRequest::create rejects already-expired deadlines, so build
  // by setting a future deadline first then advancing the clock past
  // it.
  const auto future_deadline = h.clock->now() + std::chrono::milliseconds{2};
  auto envelope = SchedulerRequest{
      make_infer_request("a", "endpoint-a", future_deadline), "mock", "model", {}, h.clock->now()};
  // Advance the clock past the deadline before admit().
  h.clock->advance_ms(std::chrono::milliseconds{10});
  auto rejected = h.scheduler->admit(std::move(envelope));
  ASSERT_FALSE(rejected);
  EXPECT_EQ(rejected.error().code, Error::Code::Timeout);
  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.admission_rejected_deadline, 1u);
  (void)deadline;
}

TEST(SchedulerDeadline, EstimateExceedsDeadlinePlusMarginRejects) {
  DeadlineHarness h{/*margin=*/std::chrono::milliseconds{2},
                    /*default_estimate=*/std::chrono::milliseconds{50}};
  // Deadline 5 ms in the future. Default service estimate is 50 ms.
  // queue_depth = 0, in_flight = 0, so estimated_completion = now + 50
  // ms, which exceeds deadline (5 ms) + margin (2 ms) = 7 ms.
  const auto deadline = h.clock->now() + std::chrono::milliseconds{5};
  auto envelope = SchedulerRequest{
      make_infer_request("a", "endpoint-a", deadline), "mock", "model", {}, h.clock->now()};
  auto rejected = h.scheduler->admit(std::move(envelope));
  ASSERT_FALSE(rejected);
  EXPECT_EQ(rejected.error().code, Error::Code::Timeout);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_deadline, 1u);
}

TEST(SchedulerDeadline, EstimateInsideDeadlinePlusMarginAccepts) {
  DeadlineHarness h{/*margin=*/std::chrono::milliseconds{20},
                    /*default_estimate=*/std::chrono::milliseconds{1}};
  const auto deadline = h.clock->now() + std::chrono::milliseconds{50};
  auto envelope = SchedulerRequest{
      make_infer_request("a", "endpoint-a", deadline), "mock", "model", {}, h.clock->now()};
  ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));
}

TEST(SchedulerDeadline, BoundaryAtDeadlinePlusMarginAccepts) {
  // estimated_completion = now + 5 ms; deadline + margin = (now + 3 ms)
  // + 2 ms = now + 5 ms. Equal-to is admitted.
  DeadlineHarness h{/*margin=*/std::chrono::milliseconds{2},
                    /*default_estimate=*/std::chrono::milliseconds{5}};
  const auto deadline = h.clock->now() + std::chrono::milliseconds{3};
  auto envelope = SchedulerRequest{
      make_infer_request("a", "endpoint-a", deadline), "mock", "model", {}, h.clock->now()};
  ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));
}

TEST(SchedulerDeadline, JustOverBoundaryRejects) {
  DeadlineHarness h{/*margin=*/std::chrono::milliseconds{2},
                    /*default_estimate=*/std::chrono::milliseconds{6}};
  // estimated_completion = now + 6 ms; deadline + margin = now + 5 ms.
  // 6 > 5: reject.
  const auto deadline = h.clock->now() + std::chrono::milliseconds{3};
  auto envelope = SchedulerRequest{
      make_infer_request("a", "endpoint-a", deadline), "mock", "model", {}, h.clock->now()};
  auto r = h.scheduler->admit(std::move(envelope));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Timeout);
}

TEST(SchedulerDeadline, PerRequestEstimateOverridesDefault) {
  DeadlineHarness h{/*margin=*/std::chrono::milliseconds{0},
                    /*default_estimate=*/std::chrono::milliseconds{100}};
  // Default estimate (100 ms) would reject; per-request estimate
  // (1 ms) is feasible.
  const auto deadline = h.clock->now() + std::chrono::milliseconds{10};
  ServiceEstimate est;
  est.estimated_service_time =
      std::chrono::duration_cast<SchedulerClock::Duration>(std::chrono::milliseconds{1});
  auto envelope = SchedulerRequest{make_infer_request("a", "endpoint-a", deadline), "mock", "model",
                                   est, h.clock->now()};
  ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));
}

TEST(SchedulerDeadline, QueuedExpiryOnExplicitSweep) {
  DeadlineHarness h{std::chrono::milliseconds{50}};
  const auto deadline = h.clock->now() + std::chrono::milliseconds{5};
  ASSERT_TRUE(h.scheduler->admit(SchedulerRequest{
      make_infer_request("a", "endpoint-a", deadline), "mock", "model", {}, h.clock->now()}));
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 1u);

  // Advance the clock past the deadline.
  h.clock->advance_ms(std::chrono::milliseconds{10});
  const auto removed = h.scheduler->expire_due();
  EXPECT_EQ(removed, 1u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 0u);
  EXPECT_EQ(h.scheduler->metrics().expired_total, 1u);

  // The expired event carries Timeout and the wait time.
  bool found = false;
  for (const auto& ev : h.sink.events()) {
    if (ev.kind == SchedulerEventKind::Expired) {
      found = true;
      ASSERT_TRUE(ev.error_code.has_value());
      EXPECT_EQ(*ev.error_code, Error::Code::Timeout);
      EXPECT_GT(ev.wait_time.count(), 0);
    }
  }
  EXPECT_TRUE(found);
}

TEST(SchedulerDeadline, QueuedExpirySweepsBeforeDispatch) {
  DeadlineHarness h{std::chrono::milliseconds{50}};
  const auto deadline = h.clock->now() + std::chrono::milliseconds{5};
  ASSERT_TRUE(h.scheduler->admit(SchedulerRequest{
      make_infer_request("a", "endpoint-a", deadline), "mock", "model", {}, h.clock->now()}));
  // Admit a second request with a later deadline so we have something
  // to dispatch after the sweep.
  ASSERT_TRUE(h.scheduler->admit(SchedulerRequest{
      make_infer_request("b", "endpoint-a", h.clock->now() + std::chrono::milliseconds{500}),
      "mock",
      "model",
      {},
      h.clock->now()}));

  h.clock->advance_ms(std::chrono::milliseconds{10});  // 'a' is now stale.
  auto next = h.scheduler->next();
  ASSERT_TRUE(next.has_value());
  EXPECT_EQ(next->request_id(), "b");  // 'a' was swept, not dispatched.
  EXPECT_EQ(h.scheduler->metrics().expired_total, 1u);
}

TEST(SchedulerDeadline, DeadlineUsesMonotonicTimeNotWallTime) {
  DeadlineHarness h{std::chrono::milliseconds{0},
                    /*default_estimate=*/std::chrono::milliseconds{1}};
  // Set a deadline using the fake clock's domain, then drive the
  // scheduler with the fake clock. The result must depend on the fake
  // (monotonic) clock alone, not on the system wall clock.
  const auto deadline = h.clock->now() + std::chrono::milliseconds{5};
  ASSERT_TRUE(h.scheduler->admit(SchedulerRequest{
      make_infer_request("a", "endpoint-a", deadline), "mock", "model", {}, h.clock->now()}));
  // Sleeping in real time has no effect; only fake-clock advance does.
  EXPECT_EQ(h.scheduler->expire_due(), 0u);
  h.clock->advance_ms(std::chrono::milliseconds{10});
  EXPECT_EQ(h.scheduler->expire_due(), 1u);
}

TEST(SchedulerDeadline, RejectionBuffersDoNotLeakWhenManagerIsWired) {
  // The scheduler must call release_request_buffers on rejection when
  // a buffer manager is wired into hooks. Use a real BufferManager
  // and assert active_count returns to zero after rejection.
  BufferManagerConfig bcfg;
  bcfg.capacity_bytes = 4 * 1024;
  bcfg.max_buffer_bytes = 1024;
  auto manager_r = BufferManager::create(bcfg);
  ASSERT_TRUE(manager_r) << "manager: " << manager_r.error().message;
  auto manager = std::move(manager_r).value();

  SchedulerConfig scfg;
  scfg.queue_capacity = 1;
  scfg.deadline_margin = std::chrono::milliseconds{0};
  scfg.default_service_estimate = std::chrono::milliseconds{1};
  auto clock = std::make_unique<FakeSchedulerClock>();
  SchedulerRuntimeHooks hooks;
  hooks.clock = clock.get();
  hooks.buffer_manager = manager.get();
  auto sched_r = make_scheduler(scfg, hooks);
  ASSERT_TRUE(sched_r) << "sched: " << sched_r.error().message;
  auto sched = std::move(sched_r).value();

  // Build a request with an *owned* buffer and a comfortably-future
  // deadline so InferRequest::create accepts it. Then advance the
  // fake clock past the deadline so the scheduler rejects it.
  const auto future_deadline = clock->now() + std::chrono::milliseconds{50};
  auto envelope = make_request_with_owned_buffer(*manager, *clock, "rejected-id",
                                                 /*byte_size=*/64, future_deadline);
  EXPECT_EQ(manager->accounting().active_count, 1u);
  clock->advance_ms(std::chrono::milliseconds{100});  // Past deadline.
  auto r = sched->admit(std::move(envelope));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Timeout);
  EXPECT_EQ(manager->accounting().active_count, 0u);
}

TEST(SchedulerDeadline, EstimateAccountsForQueueDepth) {
  DeadlineHarness h{/*margin=*/std::chrono::milliseconds{0},
                    /*default_estimate=*/std::chrono::milliseconds{5}};
  // Admit two head-of-line requests with generous deadlines.
  const auto far = h.clock->now() + std::chrono::milliseconds{500};
  ASSERT_TRUE(h.scheduler->admit(SchedulerRequest{
      make_infer_request("a", "endpoint", far), "mock", "model", {}, h.clock->now()}));
  ASSERT_TRUE(h.scheduler->admit(SchedulerRequest{
      make_infer_request("b", "endpoint", far), "mock", "model", {}, h.clock->now()}));

  // Now try to admit a request whose deadline is just barely later
  // than its own service estimate. The scheduler should reject because
  // queue_depth contributes to estimated completion: the request
  // would have to wait (5 ms * 2) + 5 ms = 15 ms, but its deadline is
  // only 12 ms out.
  const auto tight = h.clock->now() + std::chrono::milliseconds{12};
  auto rejected = h.scheduler->admit(SchedulerRequest{
      make_infer_request("c", "endpoint", tight), "mock", "model", {}, h.clock->now()});
  ASSERT_FALSE(rejected);
  EXPECT_EQ(rejected.error().code, Error::Code::Timeout);
}

}  // namespace
