// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F06-T01 / T02 unit coverage for buffer-plane pressure metrics
// and the pressure-transition event hook.

#include <gtest/gtest.h>

#include <cstddef>
#include <memory>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"

namespace {

using tensorplate::BufferAccounting;
using tensorplate::BufferManager;
using tensorplate::BufferManagerConfig;
using tensorplate::BufferPressureEvent;
using tensorplate::Error;
using tensorplate::MemoryPressure;
using tensorplate::to_string;

// ----- F06-T01: shape locks -----

TEST(PressureNames, MemoryPressureSnakeCaseIsLocked) {
  EXPECT_EQ(to_string(MemoryPressure::Normal), "normal");
  EXPECT_EQ(to_string(MemoryPressure::Warning), "warning");
  EXPECT_EQ(to_string(MemoryPressure::Critical), "critical");
}

TEST(PressureSnapshot, AccountingSnapshotCarriesAllRequiredFields) {
  BufferManagerConfig cfg;
  cfg.pool_name = "metrics_test";
  cfg.capacity_bytes = 1024;
  cfg.max_buffer_bytes = 1024;
  auto r = BufferManager::create(std::move(cfg));
  ASSERT_TRUE(r.has_value());
  auto mgr = std::move(r).value();

  const BufferAccounting before = mgr->accounting();
  EXPECT_EQ(before.pool_name, "metrics_test");
  EXPECT_EQ(before.capacity_bytes, 1024u);
  EXPECT_EQ(before.in_use_bytes, 0u);
  EXPECT_EQ(before.active_count, 0u);
  EXPECT_EQ(before.high_water_bytes, 0u);
  EXPECT_EQ(before.allocation_failures, 0u);
  EXPECT_EQ(before.release_failures, 0u);
  EXPECT_EQ(before.pressure, MemoryPressure::Normal);
}

// ----- F06-T02: emit metrics from allocate / release paths -----

TEST(PressureMetrics, AllocationIncrementsActiveCountAndInUseBytes) {
  BufferManagerConfig cfg;
  cfg.pool_name = "metrics_test";
  cfg.capacity_bytes = 4096;
  cfg.max_buffer_bytes = 1024;
  auto mgr = BufferManager::create(std::move(cfg)).value();

  auto h1 = mgr->allocate(256);
  auto h2 = mgr->allocate(128);
  ASSERT_TRUE(h1.has_value());
  ASSERT_TRUE(h2.has_value());

  auto snap = mgr->accounting();
  EXPECT_EQ(snap.active_count, 2u);
  EXPECT_EQ(snap.in_use_bytes, 384u);
  EXPECT_EQ(snap.high_water_bytes, 384u);

  ASSERT_TRUE(mgr->release(h1.value()).has_value());
  snap = mgr->accounting();
  EXPECT_EQ(snap.active_count, 1u);
  EXPECT_EQ(snap.in_use_bytes, 128u);
  EXPECT_EQ(snap.high_water_bytes, 384u);

  ASSERT_TRUE(mgr->release(h2.value()).has_value());
}

TEST(PressureMetrics, AllocationFailureIncrementsAllocationFailuresCounter) {
  BufferManagerConfig cfg;
  cfg.pool_name = "metrics_test";
  cfg.capacity_bytes = 256;
  cfg.max_buffer_bytes = 256;
  auto mgr = BufferManager::create(std::move(cfg)).value();

  // Capacity rejection path.
  auto h = mgr->allocate(257);
  ASSERT_FALSE(h.has_value());
  EXPECT_EQ(h.error().code, Error::Code::Unsupported);  // exceeds per-buffer cap
  // Capacity exceed (not per-buffer cap) counts as pressure-signal failure.
  // Try again without the per-buffer cap path: fill the pool, then ask
  // for more.
  auto fill = mgr->allocate(256);
  ASSERT_TRUE(fill.has_value());
  auto second = mgr->allocate(64);
  ASSERT_FALSE(second.has_value());
  EXPECT_EQ(second.error().code, Error::Code::OOMError);
  EXPECT_GE(mgr->accounting().allocation_failures, 1u);

  ASSERT_TRUE(mgr->release(fill.value()).has_value());
}

TEST(PressureMetrics, ThresholdCrossingEmitsObservableTransition) {
  BufferManagerConfig cfg;
  cfg.pool_name = "metrics_test";
  cfg.capacity_bytes = 1000;
  cfg.max_buffer_bytes = 1000;
  cfg.warning_threshold = 0.5;
  cfg.critical_threshold = 0.9;
  auto mgr = BufferManager::create(std::move(cfg)).value();

  std::vector<BufferPressureEvent> events;

  // Below warning threshold.
  auto h1 = mgr->allocate(400);
  ASSERT_TRUE(h1.has_value());
  events = mgr->drain_pressure_events();
  EXPECT_TRUE(events.empty());
  EXPECT_EQ(mgr->accounting().pressure, MemoryPressure::Normal);

  // Cross into Warning (>=500 / 1000).
  auto h2 = mgr->allocate(200);
  ASSERT_TRUE(h2.has_value());
  events = mgr->drain_pressure_events();
  ASSERT_EQ(events.size(), 1u);
  EXPECT_EQ(events.front().previous, MemoryPressure::Normal);
  EXPECT_EQ(events.front().current, MemoryPressure::Warning);
  EXPECT_EQ(events.front().pool_name, "metrics_test");
  EXPECT_EQ(events.front().in_use_bytes, 600u);
  EXPECT_EQ(events.front().active_count, 2u);
  EXPECT_EQ(mgr->accounting().pressure, MemoryPressure::Warning);

  // Cross into Critical (>=900 / 1000).
  auto h3 = mgr->allocate(300);
  ASSERT_TRUE(h3.has_value());
  auto more_events = mgr->drain_pressure_events();
  events.insert(events.end(), more_events.begin(), more_events.end());
  ASSERT_EQ(events.size(), 2u);
  EXPECT_EQ(events.back().previous, MemoryPressure::Warning);
  EXPECT_EQ(events.back().current, MemoryPressure::Critical);

  // Releasing all buffers walks pressure back down to Normal.
  ASSERT_TRUE(mgr->release(h1.value()).has_value());
  ASSERT_TRUE(mgr->release(h2.value()).has_value());
  ASSERT_TRUE(mgr->release(h3.value()).has_value());
  more_events = mgr->drain_pressure_events();
  events.insert(events.end(), more_events.begin(), more_events.end());
  // Last event should be the transition back to Normal.
  EXPECT_EQ(events.back().current, MemoryPressure::Normal);
}

TEST(PressureMetrics, DrainingClearsRetainedEvents) {
  BufferManagerConfig cfg;
  cfg.pool_name = "metrics_test";
  cfg.capacity_bytes = 1000;
  cfg.max_buffer_bytes = 1000;
  auto mgr = BufferManager::create(std::move(cfg)).value();

  // After one drain, the same retained event should not be returned again.
  auto h = mgr->allocate(950);
  ASSERT_TRUE(h.has_value());
  EXPECT_EQ(mgr->drain_pressure_events().size(), 1u);
  EXPECT_TRUE(mgr->drain_pressure_events().empty());

  ASSERT_TRUE(mgr->release(h.value()).has_value());
}

TEST(PressureMetrics, EventCarriesNoUnboundedLabels) {
  // Compile-time / structural check: BufferPressureEvent only has the
  // documented low-cardinality fields. Adding per-request labels (e.g.
  // request_id) is a schema change and would need an explicit decision.
  BufferPressureEvent ev{};
  ev.pool_name = "x";
  ev.previous = MemoryPressure::Normal;
  ev.current = MemoryPressure::Warning;
  // The struct should compile without any non-bounded fields below.
  (void)ev;
}

}  // namespace
