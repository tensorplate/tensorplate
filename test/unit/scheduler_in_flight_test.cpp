// SPDX-License-Identifier: Apache-2.0
//
// Logical and physical in-flight accounting of the FIFO scheduler.
//
// A dispatched request counts logically until its completion or an accepted
// cancel, and physically until the executor reports it released, which in
// this interface is its on_completion. These tests pin the counts at every
// transition, and pin that dispatch, the in-flight gate and cancel behave as
// before: the gate still counts logical requests.

#include <gtest/gtest.h>

#include <memory>
#include <string>

#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

struct Harness {
  std::unique_ptr<FakeSchedulerClock> clock = std::make_unique<FakeSchedulerClock>();
  std::unique_ptr<InferScheduler> scheduler;

  explicit Harness(std::size_t in_flight_capacity) {
    SchedulerConfig config;
    config.queue_capacity = 8;
    config.in_flight_capacity = in_flight_capacity;
    SchedulerRuntimeHooks hooks;
    hooks.clock = clock.get();
    scheduler = make_scheduler(config, hooks).value();
  }

  void admit(const std::string& id) const {
    ASSERT_TRUE(scheduler->admit(make_scheduler_request(make_infer_request(id), *clock)));
  }

  [[nodiscard]] std::string dispatch() const {
    auto next = scheduler->next();
    EXPECT_TRUE(next.has_value());
    return next.has_value() ? next->request_id() : std::string{};
  }
};

// The identities the header documents for every snapshot.
void expect_documented_identities(const SchedulerMetrics& m) {
  EXPECT_EQ(m.in_flight, m.in_flight_logical);
  EXPECT_GE(m.in_flight_physical, m.in_flight_logical + m.in_flight_physical_cancelled);
  EXPECT_GE(m.in_flight_physical_high_water, m.in_flight_physical);
  EXPECT_GE(m.in_flight_high_water, m.in_flight_logical);
}

void expect_counts(const InferScheduler& scheduler, std::size_t logical, std::size_t physical,
                   std::size_t cancelled) {
  const auto m = scheduler.metrics();
  EXPECT_EQ(m.in_flight, logical);
  EXPECT_EQ(m.in_flight_logical, logical);
  EXPECT_EQ(m.in_flight_physical, physical);
  EXPECT_EQ(m.in_flight_physical_cancelled, cancelled);
  expect_documented_identities(m);
}

TEST(SchedulerInFlight, DispatchCountsLogicallyAndPhysically) {
  Harness h{2};
  h.admit("a");
  h.admit("b");
  expect_counts(*h.scheduler, 0, 0, 0);  // queued requests count in neither
  EXPECT_EQ(h.dispatch(), "a");
  expect_counts(*h.scheduler, 1, 1, 0);
  EXPECT_EQ(h.dispatch(), "b");
  expect_counts(*h.scheduler, 2, 2, 0);
  EXPECT_EQ(h.scheduler->metrics().in_flight_physical_high_water, 2U);
}

TEST(SchedulerInFlight, CompletionEndsBothCountsAndKeepsTheHighWater) {
  Harness h{2};
  h.admit("a");
  h.admit("b");
  (void)h.dispatch();
  (void)h.dispatch();
  ASSERT_TRUE(h.scheduler->on_completion("a", CompletionStatus::Success));
  expect_counts(*h.scheduler, 1, 1, 0);
  ASSERT_TRUE(h.scheduler->on_completion("b", CompletionStatus::Failure, Error::Code::Internal));
  expect_counts(*h.scheduler, 0, 0, 0);
  EXPECT_EQ(h.scheduler->metrics().in_flight_physical_high_water, 2U);

  // A later, smaller dispatch does not lower the high-water mark.
  h.admit("c");
  EXPECT_EQ(h.dispatch(), "c");
  expect_counts(*h.scheduler, 1, 1, 0);
  EXPECT_EQ(h.scheduler->metrics().in_flight_physical_high_water, 2U);
}

TEST(SchedulerInFlight, InFlightCancelKeepsThePhysicalCountUntilCompletion) {
  Harness h{1};
  h.admit("a");
  (void)h.dispatch();
  ASSERT_TRUE(h.scheduler->cancel("a", CancellationReason::StaleSequence));
  expect_counts(*h.scheduler, 0, 1, 1);
  EXPECT_EQ(h.scheduler->metrics().cancelled_in_flight, 1U);

  // The completion that follows is still refused as before, and ends the
  // physical count.
  auto late = h.scheduler->on_completion("a", CompletionStatus::Success);
  ASSERT_FALSE(late);
  EXPECT_EQ(late.error().code, Error::Code::Internal);
  expect_counts(*h.scheduler, 0, 0, 0);
  EXPECT_EQ(h.scheduler->metrics().completed_success, 0U);

  // A duplicate completion changes nothing further.
  EXPECT_FALSE(h.scheduler->on_completion("a", CompletionStatus::Success));
  expect_counts(*h.scheduler, 0, 0, 0);
}

TEST(SchedulerInFlight, GateStillCountsLogicalRequests) {
  // Behaviour this change keeps: an in-flight cancel frees the gate at
  // once, so a second request dispatches while the first may still run.
  // The physical count and its high-water mark make that visible.
  Harness h{1};
  h.admit("a");
  h.admit("b");
  EXPECT_EQ(h.dispatch(), "a");
  EXPECT_FALSE(h.scheduler->next().has_value());
  ASSERT_TRUE(h.scheduler->cancel("a", CancellationReason::ClientRequest));
  EXPECT_EQ(h.dispatch(), "b");
  expect_counts(*h.scheduler, 1, 2, 1);
  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.in_flight_high_water, 1U);
  EXPECT_EQ(m.in_flight_physical_high_water, 2U);

  EXPECT_FALSE(h.scheduler->on_completion("a", CompletionStatus::Success));
  expect_counts(*h.scheduler, 1, 1, 0);
  ASSERT_TRUE(h.scheduler->on_completion("b", CompletionStatus::Success));
  expect_counts(*h.scheduler, 0, 0, 0);
}

TEST(SchedulerInFlight, QueuedCancelNeverCountsPhysically) {
  Harness h{1};
  h.admit("a");
  ASSERT_TRUE(h.scheduler->cancel("a", CancellationReason::ClientRequest));
  expect_counts(*h.scheduler, 0, 0, 0);
  EXPECT_EQ(h.scheduler->metrics().cancelled_queued, 1U);
  EXPECT_EQ(h.scheduler->metrics().in_flight_physical_high_water, 0U);
}

TEST(SchedulerInFlight, ShutdownKeepsDispatchedRequestsPhysical) {
  Harness h{2};
  h.admit("a");
  h.admit("b");
  h.admit("c");
  (void)h.dispatch();
  (void)h.dispatch();
  EXPECT_EQ(h.scheduler->shutdown(), 3U);
  expect_counts(*h.scheduler, 0, 2, 2);
  EXPECT_FALSE(h.scheduler->on_completion("a", CompletionStatus::Success));
  expect_counts(*h.scheduler, 0, 1, 1);
  EXPECT_FALSE(h.scheduler->on_completion("b", CompletionStatus::Success));
  expect_counts(*h.scheduler, 0, 0, 0);
}

}  // namespace
