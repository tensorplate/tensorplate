// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F05-T01 / T02: Queue metrics and scheduler events.
//
// Verifies the local metrics shape and event emission documented in
// `protocol/schemas/scheduler_metrics.json` and
// `protocol/schemas/scheduler_event.json`. Wait-time durations use
// monotonic steady-clock nanoseconds; counter increments fire from the
// scheduler critical-path methods even when the event sink throws.

#include <chrono>
#include <memory>
#include <stdexcept>
#include <utility>

#include <gtest/gtest.h>

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

class ThrowingSchedulerSink final : public SchedulerEventSink {
 public:
  void on_event(const SchedulerEvent& /*event*/) override {
    ++calls_;
    throw std::runtime_error("scheduler event sink intentionally throwing");
  }
  [[nodiscard]] std::size_t calls() const noexcept { return calls_; }

 private:
  std::size_t calls_ = 0;
};

struct MetricsHarness {
  std::unique_ptr<FakeSchedulerClock> clock = std::make_unique<FakeSchedulerClock>();
  RecordingSchedulerEventSink sink;
  std::unique_ptr<InferScheduler> scheduler;

  MetricsHarness() {
    SchedulerConfig scfg;
    scfg.queue_capacity = 4;
    scfg.in_flight_capacity = 2;
    scfg.deadline_margin = std::chrono::milliseconds{500};
    SchedulerRuntimeHooks hooks;
    hooks.clock = clock.get();
    hooks.event_sink = &sink;
    scheduler = make_scheduler(scfg, hooks).value();
  }
};

TEST(SchedulerMetrics, SnapshotShapeOnEmptyScheduler) {
  MetricsHarness h;
  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.policy, "fifo");
  EXPECT_EQ(m.queue_depth, 0u);
  EXPECT_EQ(m.in_flight, 0u);
  EXPECT_EQ(m.admitted_total, 0u);
  EXPECT_EQ(m.completed_success, 0u);
  EXPECT_EQ(m.completed_failure, 0u);
  EXPECT_EQ(m.wait_time_samples, 0u);
  EXPECT_EQ(m.last_memory_severity, PressureSeverity::Normal);
  EXPECT_EQ(m.last_thermal_severity, PressureSeverity::Normal);
}

TEST(SchedulerMetrics, AdmitDispatchCompleteIncrementsCounters) {
  MetricsHarness h;
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("b"), *h.clock)));
  EXPECT_EQ(h.scheduler->metrics().admitted_total, 2u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 2u);

  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  EXPECT_EQ(h.scheduler->metrics().in_flight, 1u);

  ASSERT_TRUE(h.scheduler->on_completion("a", CompletionStatus::Success, std::nullopt));
  EXPECT_EQ(h.scheduler->metrics().completed_success, 1u);
  EXPECT_EQ(h.scheduler->metrics().in_flight, 0u);
}

TEST(SchedulerMetrics, RejectionCountersBreakOutByReason) {
  // Force overload + deadline + pressure rejections in one harness.
  MetricsHarness h;
  // Fill the queue (capacity = 4), then trigger overload.
  for (auto id : {"a", "b", "c", "d"}) {
    ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request(id), *h.clock)));
  }
  auto overload =
      h.scheduler->admit(make_scheduler_request(make_infer_request("overflow"), *h.clock));
  ASSERT_FALSE(overload);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_overload, 1u);

  // Complete all of them so we can run the deadline test.
  while (auto n = h.scheduler->next()) {
    ASSERT_TRUE(h.scheduler->on_completion(n->request_id(), CompletionStatus::Success,
                                           std::nullopt));
  }

  // Build a request that's already past its deadline against the
  // fake clock: build with future deadline, advance, then admit.
  const auto future_deadline = h.clock->now() + std::chrono::milliseconds{5};
  auto envelope = SchedulerRequest{make_infer_request("late", "endpoint", future_deadline),
                                    "mock", "model", {}, h.clock->now()};
  h.clock->advance_ms(std::chrono::milliseconds{50});
  auto deadline_rejected = h.scheduler->admit(std::move(envelope));
  ASSERT_FALSE(deadline_rejected);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_deadline, 1u);

  // Pressure rejection: drive critical memory and admit.
  PressureSignal sig{PressureSource::Memory, PressureSeverity::Critical, h.clock->now(),
                      "test"};
  h.scheduler->on_pressure(sig);
  auto pressure_rejected =
      h.scheduler->admit(make_scheduler_request(make_infer_request("pressed"), *h.clock));
  ASSERT_FALSE(pressure_rejected);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_pressure, 1u);
}

TEST(SchedulerMetrics, WaitTimeAggregatesAcrossDispatchAndCancel) {
  MetricsHarness h;
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("b"), *h.clock)));

  h.clock->advance_ms(std::chrono::milliseconds{5});
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());

  h.clock->advance_ms(std::chrono::milliseconds{20});
  ASSERT_TRUE(h.scheduler->cancel("b", CancellationReason::ClientRequest));

  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.wait_time_samples, 2u);
  EXPECT_GT(m.wait_time_max,
            std::chrono::duration_cast<SchedulerClock::Duration>(std::chrono::milliseconds{20}));
}

TEST(SchedulerMetrics, EventOrderForAdmitDispatchComplete) {
  MetricsHarness h;
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  auto next = h.scheduler->next();
  ASSERT_TRUE(next.has_value());
  ASSERT_TRUE(h.scheduler->on_completion("a", CompletionStatus::Success, std::nullopt));

  const auto events = h.sink.events();
  ASSERT_EQ(events.size(), 3u);
  EXPECT_EQ(events[0].kind, SchedulerEventKind::Admitted);
  EXPECT_EQ(events[1].kind, SchedulerEventKind::Dispatched);
  EXPECT_EQ(events[2].kind, SchedulerEventKind::Completed);
  ASSERT_TRUE(events[2].completion_status.has_value());
  EXPECT_EQ(*events[2].completion_status, CompletionStatus::Success);
  EXPECT_EQ(events[1].wait_time.count(), events[0].wait_time.count());
}

TEST(SchedulerMetrics, EventLabelsAreBounded) {
  MetricsHarness h;
  // Build with explicit endpoint / backend / model labels.
  auto envelope = SchedulerRequest{make_infer_request("a", "vision/detector"), "tensorrt",
                                    "model-detect", {}, h.clock->now()};
  ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));
  const auto events = h.sink.events();
  ASSERT_EQ(events.size(), 1u);
  EXPECT_EQ(events[0].endpoint, "vision/detector");
  EXPECT_EQ(events[0].backend_name, "tensorrt");
  EXPECT_EQ(events[0].policy, "fifo");
}

TEST(SchedulerMetrics, ThrowingSinkDoesNotBreakScheduler) {
  ThrowingSchedulerSink throwing;
  SchedulerConfig scfg;
  auto clock = std::make_unique<FakeSchedulerClock>();
  SchedulerRuntimeHooks hooks;
  hooks.event_sink = &throwing;
  hooks.clock = clock.get();
  auto sched = make_scheduler(scfg, hooks).value();

  // admit -> next -> on_completion all emit events; the throwing
  // sink must not affect counters.
  ASSERT_TRUE(sched->admit(make_scheduler_request(make_infer_request("a"), *clock)));
  ASSERT_TRUE(sched->next().has_value());
  ASSERT_TRUE(sched->on_completion("a", CompletionStatus::Success, std::nullopt));

  EXPECT_EQ(sched->metrics().admitted_total, 1u);
  EXPECT_EQ(sched->metrics().completed_success, 1u);
  EXPECT_GE(throwing.calls(), 3u);
}

TEST(SchedulerMetrics, MetricsAvailableWhenSaturated) {
  MetricsHarness h;
  for (auto id : {"a", "b", "c", "d"}) {
    ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request(id), *h.clock)));
  }
  // Snapshot remains queryable without blocking on the queue.
  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.queue_depth, 4u);
  EXPECT_EQ(m.queue_depth_high_water, 4u);
}

TEST(SchedulerMetrics, MetricsAvailableWhenOnlyHandlingCancellations) {
  MetricsHarness h;
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(h.scheduler->cancel("a", CancellationReason::ClientRequest));

  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.cancelled_queued, 1u);
  EXPECT_EQ(m.queue_depth, 0u);
}

TEST(SchedulerMetrics, EventEmissionDoesNotBlockOnSink) {
  // Smoke: many events through a recording sink; counters and events
  // stay consistent. This is not a perf benchmark, just a sanity
  // check that the scheduler does not deadlock on its own sink.
  MetricsHarness h;
  for (int i = 0; i < 8; ++i) {
    auto id = std::string{"req-"} + std::to_string(i);
    if (h.scheduler->admit(make_scheduler_request(make_infer_request(id), *h.clock))) {
      // Drain a few so we keep moving below capacity.
      if (auto n = h.scheduler->next()) {
        ASSERT_TRUE(
            h.scheduler->on_completion(n->request_id(), CompletionStatus::Success, std::nullopt));
      }
    }
  }
  // We should have observed admit + dispatch + complete trios.
  EXPECT_GE(h.sink.count(SchedulerEventKind::Admitted), 1u);
  EXPECT_GE(h.sink.count(SchedulerEventKind::Dispatched), 1u);
  EXPECT_GE(h.sink.count(SchedulerEventKind::Completed), 1u);
}

}  // namespace
