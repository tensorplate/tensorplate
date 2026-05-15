// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F02-T01 / T02: FIFO scheduler ordering, capacity, and
// in-flight accounting coverage. Verifies the v0.1.0 default
// scheduler behavior:
//
//   - Admitted requests dispatch in arrival order.
//   - Queue capacity is enforced before accepting new work.
//   - In-flight count is incremented on dispatch, not on enqueue.
//   - In-flight capacity gates dispatch even when the queue is full.
//   - Metrics snapshot reflects depth, in-flight, and high-water.
//   - Mock executor code holds the scheduler through the abstract
//     interface and never references the FIFO concrete type.

#include <gtest/gtest.h>

#include <chrono>
#include <memory>
#include <utility>
#include <vector>

#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

// Helper to build a scheduler with a fake clock and our recording sink.
struct SchedulerHarness {
  std::unique_ptr<FakeSchedulerClock> clock = std::make_unique<FakeSchedulerClock>();
  RecordingSchedulerEventSink sink;
  std::unique_ptr<InferScheduler> scheduler;

  SchedulerHarness(std::size_t queue_capacity, std::size_t in_flight_capacity) {
    SchedulerConfig config;
    config.queue_capacity = queue_capacity;
    config.in_flight_capacity = in_flight_capacity;
    SchedulerRuntimeHooks hooks;
    hooks.clock = clock.get();
    hooks.event_sink = &sink;
    scheduler = make_scheduler(config, hooks).value();
  }
};

TEST(SchedulerFifo, EmptyQueueReturnsNullopt) {
  SchedulerHarness h{/*queue=*/4, /*in_flight=*/1};
  EXPECT_FALSE(h.scheduler->next().has_value());
}

TEST(SchedulerFifo, SingleAdmitDispatchOrder) {
  SchedulerHarness h{/*queue=*/4, /*in_flight=*/1};
  auto envelope = make_scheduler_request(make_infer_request("req-a"), *h.clock);
  ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));

  auto next = h.scheduler->next();
  ASSERT_TRUE(next.has_value());
  EXPECT_EQ(next->request_id(), "req-a");
}

TEST(SchedulerFifo, MultipleAdmitsDispatchInArrivalOrder) {
  SchedulerHarness h{/*queue=*/8, /*in_flight=*/8};
  for (auto id : {"a", "b", "c", "d"}) {
    auto envelope = make_scheduler_request(make_infer_request(id), *h.clock);
    ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));
  }

  std::vector<std::string> dispatch_order;
  while (auto n = h.scheduler->next()) {
    dispatch_order.push_back(n->request_id());
  }
  ASSERT_EQ(dispatch_order.size(), 4u);
  EXPECT_EQ(dispatch_order[0], "a");
  EXPECT_EQ(dispatch_order[1], "b");
  EXPECT_EQ(dispatch_order[2], "c");
  EXPECT_EQ(dispatch_order[3], "d");
}

TEST(SchedulerFifo, QueueCapacityRejectsWithOverloadError) {
  SchedulerHarness h{/*queue=*/2, /*in_flight=*/4};
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("b"), *h.clock)));
  auto rejected = h.scheduler->admit(make_scheduler_request(make_infer_request("c"), *h.clock));
  ASSERT_FALSE(rejected);
  EXPECT_EQ(rejected.error().code, Error::Code::OOMError);

  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.queue_depth, 2u);
  EXPECT_EQ(m.admitted_total, 2u);
  EXPECT_EQ(m.admission_rejected_overload, 1u);
}

TEST(SchedulerFifo, DuplicateQueuedRequestIdIsRejected) {
  SchedulerHarness h{/*queue=*/4, /*in_flight=*/1};
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("dup"), *h.clock)));

  auto duplicate = h.scheduler->admit(make_scheduler_request(make_infer_request("dup"), *h.clock));
  ASSERT_FALSE(duplicate);
  EXPECT_EQ(duplicate.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 1u);

  auto next = h.scheduler->next();
  ASSERT_TRUE(next.has_value());
  EXPECT_EQ(next->request_id(), "dup");
  ASSERT_TRUE(h.scheduler->on_completion("dup", CompletionStatus::Success, std::nullopt));
}

TEST(SchedulerFifo, DuplicateInFlightRequestIdIsRejected) {
  SchedulerHarness h{/*queue=*/4, /*in_flight=*/1};
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("dup"), *h.clock)));
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());

  auto duplicate = h.scheduler->admit(make_scheduler_request(make_infer_request("dup"), *h.clock));
  ASSERT_FALSE(duplicate);
  EXPECT_EQ(duplicate.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(h.scheduler->metrics().in_flight, 1u);

  ASSERT_TRUE(h.scheduler->on_completion("dup", CompletionStatus::Success, std::nullopt));
  EXPECT_EQ(h.scheduler->metrics().completed_success, 1u);
}

TEST(SchedulerFifo, InFlightCapacityGatesDispatch) {
  SchedulerHarness h{/*queue=*/8, /*in_flight=*/2};
  for (auto id : {"a", "b", "c"}) {
    ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request(id), *h.clock)));
  }

  // Two dispatches should succeed (in_flight_capacity = 2).
  auto first = h.scheduler->next();
  auto second = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  ASSERT_TRUE(second.has_value());

  // Third dispatch is gated by in_flight_capacity until completion.
  EXPECT_FALSE(h.scheduler->next().has_value());

  ASSERT_TRUE(
      h.scheduler->on_completion(first->request_id(), CompletionStatus::Success, std::nullopt));
  // Now slot is free.
  auto third = h.scheduler->next();
  ASSERT_TRUE(third.has_value());
  EXPECT_EQ(third->request_id(), "c");
}

TEST(SchedulerFifo, InFlightIncrementsOnDispatchNotEnqueue) {
  SchedulerHarness h{/*queue=*/4, /*in_flight=*/4};
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("b"), *h.clock)));

  EXPECT_EQ(h.scheduler->metrics().in_flight, 0u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 2u);

  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  EXPECT_EQ(h.scheduler->metrics().in_flight, 1u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 1u);

  auto second = h.scheduler->next();
  ASSERT_TRUE(second.has_value());
  EXPECT_EQ(h.scheduler->metrics().in_flight, 2u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 0u);
}

TEST(SchedulerFifo, MetricsHighWaterIsRetained) {
  SchedulerHarness h{/*queue=*/8, /*in_flight=*/8};
  for (auto id : {"a", "b", "c", "d"}) {
    ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request(id), *h.clock)));
  }
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 4u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth_high_water, 4u);

  // Drain and assert the high-water did not regress.
  while (auto n = h.scheduler->next()) {
    ASSERT_TRUE(
        h.scheduler->on_completion(n->request_id(), CompletionStatus::Success, std::nullopt));
  }
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 0u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth_high_water, 4u);
}

TEST(SchedulerFifo, DispatchPopulatesWaitTime) {
  SchedulerHarness h{/*queue=*/4, /*in_flight=*/1};
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  h.clock->advance_ms(std::chrono::milliseconds{12});

  auto next = h.scheduler->next();
  ASSERT_TRUE(next.has_value());
  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.wait_time_samples, 1u);
  EXPECT_GE(m.wait_time_max,
            std::chrono::duration_cast<SchedulerClock::Duration>(std::chrono::milliseconds{12}));
}

TEST(SchedulerFifo, AdmittedEventCarriesPolicyAndBackend) {
  SchedulerHarness h{/*queue=*/2, /*in_flight=*/1};
  auto envelope =
      SchedulerRequest{make_infer_request("a"), "tensorrt", "model-x", {}, h.clock->now()};
  ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));
  const auto events = h.sink.events();
  ASSERT_GE(events.size(), 1u);
  EXPECT_EQ(events.back().kind, SchedulerEventKind::Admitted);
  EXPECT_EQ(events.back().policy, "fifo");
  EXPECT_EQ(events.back().backend_name, "tensorrt");
}

TEST(SchedulerFifo, MockExecutorOnlyHoldsAbstractInterface) {
  // This test deliberately uses InferScheduler* (not FifoScheduler*) to
  // assert the abstraction boundary. If a future change accidentally
  // moves a FIFO-only API to a non-virtual public method, this test
  // will not compile (it only references the interface).
  SchedulerHarness h{/*queue=*/2, /*in_flight=*/1};
  InferScheduler* sched = h.scheduler.get();
  ASSERT_TRUE(sched->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(sched->next().has_value());
  ASSERT_TRUE(sched->on_completion("a", CompletionStatus::Success, std::nullopt));
  EXPECT_EQ(sched->metrics().completed_success, 1u);
}

TEST(SchedulerFifo, RegisteredUnderFifoKey) {
  // Uniqueness of the policy key is the public guarantee for V01-E07.
  EXPECT_TRUE(SchedulerPolicyRegistry::global().is_registered("fifo"));
  SchedulerConfig config;
  config.policy = "fifo";
  auto sched = make_scheduler(config);
  ASSERT_TRUE(sched);
  EXPECT_EQ(sched.value()->policy_name(), "fifo");
}

TEST(SchedulerFifo, MetricsExposeStateWithoutLeakingContainers) {
  SchedulerHarness h{/*queue=*/4, /*in_flight=*/2};
  for (auto id : {"a", "b"}) {
    ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request(id), *h.clock)));
  }
  // The snapshot is a value type; it must not depend on internal state.
  auto m1 = h.scheduler->metrics();
  ASSERT_TRUE(h.scheduler->next().has_value());
  // Second snapshot reflects the dispatch but the first is stable.
  auto m2 = h.scheduler->metrics();
  EXPECT_EQ(m1.queue_depth, 2u);
  EXPECT_EQ(m2.queue_depth, 1u);
  EXPECT_EQ(m2.in_flight, 1u);
}

TEST(SchedulerFifo, CompletionAfterCapacityFreesQueueSlot) {
  SchedulerHarness h{/*queue=*/2, /*in_flight=*/1};
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("b"), *h.clock)));

  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  ASSERT_TRUE(
      h.scheduler->on_completion(first->request_id(), CompletionStatus::Success, std::nullopt));

  // Queue has slot for one more.
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("c"), *h.clock)));
  auto second = h.scheduler->next();
  ASSERT_TRUE(second.has_value());
  EXPECT_EQ(second->request_id(), "b");
}

}  // namespace
