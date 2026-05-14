// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F06-T01 / T02: Memory and thermal pressure-aware admission.
//
// Verifies the conservative v0.1.0 pressure-aware admission hook:
//
//   - Pressure signals carry source, severity, monotonic timestamp,
//     and bounded detail with no vendor SDK type.
//   - Memory pressure can be derived from V01-E03 buffer-plane
//     accounting (BufferAccounting::pressure -> PressureSeverity).
//   - The scheduler records the most recent severity per source.
//   - SchedulerConfig::pressure_reject_threshold gates new admission;
//     'normal' disables pressure-based rejection (record-only mode).
//   - Pressure above the threshold rejects with Error::Code::OOMError
//     and increments admission_rejected_pressure.
//   - Queued and in-flight work is NOT cancelled by baseline pressure.
//   - Pressure events appear on the SchedulerEventSink.

#include <chrono>
#include <memory>
#include <utility>

#include <gtest/gtest.h>

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/pressure.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

struct PressureHarness {
  std::unique_ptr<FakeSchedulerClock> clock = std::make_unique<FakeSchedulerClock>();
  RecordingSchedulerEventSink sink;
  std::unique_ptr<InferScheduler> scheduler;

  PressureHarness(PressureSeverity threshold = PressureSeverity::Critical) {
    SchedulerConfig scfg;
    scfg.queue_capacity = 4;
    scfg.in_flight_capacity = 2;
    scfg.deadline_margin = std::chrono::milliseconds{500};
    scfg.pressure_reject_threshold = threshold;
    SchedulerRuntimeHooks hooks;
    hooks.clock = clock.get();
    hooks.event_sink = &sink;
    scheduler = make_scheduler(scfg, hooks).value();
  }
};

TEST(SchedulerPressure, SignalRecordsSeverityAndEmitsEvent) {
  PressureHarness h;
  PressureSignal sig{PressureSource::Memory, PressureSeverity::Warning, h.clock->now(),
                      "test:warn"};
  h.scheduler->on_pressure(sig);

  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.last_memory_severity, PressureSeverity::Warning);
  EXPECT_EQ(m.pressure_events_memory, 1u);

  ASSERT_GE(h.sink.size(), 1u);
  const auto last = h.sink.events().back();
  EXPECT_EQ(last.kind, SchedulerEventKind::MemoryPressure);
  ASSERT_TRUE(last.pressure_severity.has_value());
  EXPECT_EQ(*last.pressure_severity, PressureSeverity::Warning);
}

TEST(SchedulerPressure, ThermalEventDistinctFromMemory) {
  PressureHarness h;
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Thermal, PressureSeverity::Critical, h.clock->now(), "hot"});
  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.pressure_events_thermal, 1u);
  EXPECT_EQ(m.pressure_events_memory, 0u);
  EXPECT_EQ(m.last_thermal_severity, PressureSeverity::Critical);
  EXPECT_EQ(m.last_memory_severity, PressureSeverity::Normal);

  ASSERT_GE(h.sink.size(), 1u);
  const auto last = h.sink.events().back();
  EXPECT_EQ(last.kind, SchedulerEventKind::ThermalPressure);
}

TEST(SchedulerPressure, BelowThresholdAdmitsNormally) {
  PressureHarness h;  // Threshold defaults to Critical.
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Warning, h.clock->now(), ""});
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  EXPECT_EQ(h.scheduler->metrics().admitted_total, 1u);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_pressure, 0u);
}

TEST(SchedulerPressure, AtCriticalRejectsAdmission) {
  PressureHarness h;
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Critical, h.clock->now(), ""});
  auto r = h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::OOMError);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_pressure, 1u);
}

TEST(SchedulerPressure, ThermalCriticalAlsoRejects) {
  PressureHarness h;
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Thermal, PressureSeverity::Critical, h.clock->now(), ""});
  auto r = h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::OOMError);
}

TEST(SchedulerPressure, RecordOnlyModeNeverRejects) {
  // Threshold "normal" means no rejection, only recording.
  PressureHarness h{PressureSeverity::Normal};
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Critical, h.clock->now(), ""});
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  EXPECT_EQ(h.scheduler->metrics().last_memory_severity, PressureSeverity::Critical);
  EXPECT_EQ(h.scheduler->metrics().admission_rejected_pressure, 0u);
}

TEST(SchedulerPressure, WarningThresholdRejectsAtWarningOrAbove) {
  PressureHarness h{PressureSeverity::Warning};
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Warning, h.clock->now(), ""});
  auto r = h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::OOMError);
}

TEST(SchedulerPressure, RecoveringSeverityRestoresAdmission) {
  PressureHarness h;
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Critical, h.clock->now(), ""});
  ASSERT_FALSE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  // Drop severity back to Normal.
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Normal, h.clock->now(), ""});
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("b"), *h.clock)));
  EXPECT_EQ(h.scheduler->metrics().admitted_total, 1u);
}

TEST(SchedulerPressure, QueuedAndInFlightWorkSurvivesPressure) {
  PressureHarness h;
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("a"), *h.clock)));
  ASSERT_TRUE(h.scheduler->admit(make_scheduler_request(make_infer_request("b"), *h.clock)));
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());

  // Pressure spikes; baseline policy must NOT cancel queued or
  // in-flight work.
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Critical, h.clock->now(), ""});
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 1u);
  EXPECT_EQ(h.scheduler->metrics().in_flight, 1u);
  EXPECT_EQ(h.scheduler->metrics().cancelled_queued, 0u);
  EXPECT_EQ(h.scheduler->metrics().cancelled_in_flight, 0u);
}

TEST(SchedulerPressure, BufferPlaneAccountingDerivesPressure) {
  // The pressure value object must be constructible from the V01-E03
  // BufferAccounting::pressure (MemoryPressure -> PressureSeverity).
  // This test exercises the integration path documented in
  // docs/architecture/scheduler.md without requiring the buffer
  // plane to call into the scheduler directly.
  BufferManagerConfig bcfg;
  bcfg.capacity_bytes = 1024;
  bcfg.max_buffer_bytes = 1024;
  bcfg.warning_threshold = 0.50;
  bcfg.critical_threshold = 0.75;
  auto manager = BufferManager::create(bcfg).value();

  // Allocate enough to push pressure to Critical (>=75%).
  auto buf = manager->allocate(800).value();
  const auto acct = manager->accounting();
  ASSERT_EQ(acct.pressure, MemoryPressure::Critical);

  // Build a PressureSignal from the buffer-plane signal. v0.1.0 does
  // not auto-bridge MemoryPressure to PressureSeverity in code; the
  // mapping is enum-equivalent and lives in the V01-E07 wiring.
  PressureSeverity sev = PressureSeverity::Normal;
  switch (acct.pressure) {
    case MemoryPressure::Normal:
      sev = PressureSeverity::Normal;
      break;
    case MemoryPressure::Warning:
      sev = PressureSeverity::Warning;
      break;
    case MemoryPressure::Critical:
      sev = PressureSeverity::Critical;
      break;
  }

  PressureHarness h;
  h.scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, sev, h.clock->now(), acct.pool_name});
  EXPECT_EQ(h.scheduler->metrics().last_memory_severity, PressureSeverity::Critical);

  // Cleanup the buffer so the test fixture leaves no allocator state.
  ASSERT_TRUE(manager->release(buf));
}

}  // namespace
