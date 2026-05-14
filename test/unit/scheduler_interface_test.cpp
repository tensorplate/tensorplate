// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F01-T01 / T02: InferScheduler interface, factory, and
// concept compile-time check coverage.
//
// These tests pin the public contract:
//   - SchedulerConfig validation surfaces typed errors.
//   - Unknown policy returns Unsupported.
//   - FIFO policy is registered under the stable "fifo" key.
//   - Executor-style code can hold the scheduler through the abstract
//     interface and never branches on the concrete type.

#include <chrono>
#include <memory>
#include <string>

#include <gtest/gtest.h>

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/pressure.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

TEST(SchedulerFactory, FifoIsRegisteredOnGlobalRegistry) {
  SchedulerPolicyRegistry& registry = SchedulerPolicyRegistry::global();
  ASSERT_TRUE(registry.is_registered("fifo"));
  const auto policies = registry.registered_policies();
  EXPECT_NE(std::find(policies.begin(), policies.end(), std::string{"fifo"}), policies.end());
}

TEST(SchedulerFactory, UnknownPolicyReturnsUnsupported) {
  SchedulerConfig config;
  config.policy = "no-such-policy";
  auto result = make_scheduler(config);
  ASSERT_FALSE(result);
  EXPECT_EQ(result.error().code, Error::Code::Unsupported);
}

TEST(SchedulerFactory, ZeroQueueCapacityIsConfigInvalid) {
  SchedulerConfig config;
  config.queue_capacity = 0;
  auto result = make_scheduler(config);
  ASSERT_FALSE(result);
  EXPECT_EQ(result.error().code, Error::Code::ConfigInvalid);
}

TEST(SchedulerFactory, ZeroInFlightCapacityIsConfigInvalid) {
  SchedulerConfig config;
  config.in_flight_capacity = 0;
  auto result = make_scheduler(config);
  ASSERT_FALSE(result);
  EXPECT_EQ(result.error().code, Error::Code::ConfigInvalid);
}

TEST(SchedulerFactory, NegativeDeadlineMarginIsConfigInvalid) {
  SchedulerConfig config;
  config.deadline_margin = std::chrono::milliseconds{-1};
  auto result = make_scheduler(config);
  ASSERT_FALSE(result);
  EXPECT_EQ(result.error().code, Error::Code::ConfigInvalid);
}

TEST(SchedulerFactory, EmptyPolicyIsConfigInvalid) {
  SchedulerConfig config;
  config.policy = "";
  auto result = make_scheduler(config);
  ASSERT_FALSE(result);
  EXPECT_EQ(result.error().code, Error::Code::ConfigInvalid);
}

TEST(SchedulerFactory, ValidateSchedulerConfigMirrorsCreate) {
  SchedulerConfig good;
  EXPECT_TRUE(validate_scheduler_config(good));

  SchedulerConfig bad_policy;
  bad_policy.policy = "missing";
  auto v = validate_scheduler_config(bad_policy);
  ASSERT_FALSE(v);
  EXPECT_EQ(v.error().code, Error::Code::Unsupported);
}

TEST(SchedulerFactory, MakeSchedulerReturnsInterfacePointer) {
  SchedulerConfig config;
  config.queue_capacity = 4;
  config.in_flight_capacity = 1;
  auto scheduler = make_scheduler(config);
  ASSERT_TRUE(scheduler);
  // Caller never sees the concrete type.
  std::unique_ptr<InferScheduler> sched = std::move(scheduler).value();
  ASSERT_NE(sched, nullptr);
  EXPECT_EQ(sched->policy_name(), "fifo");
  EXPECT_EQ(sched->metrics().policy, "fifo");
  EXPECT_EQ(sched->metrics().queue_depth, 0u);
}

TEST(SchedulerFactory, ExecutorStyleCallerOnlyHoldsInterfaceReference) {
  SchedulerConfig config;
  auto scheduler = make_scheduler(config).value();
  // A helper that emulates serving-worker / executor code: it
  // receives the scheduler through the interface only.
  auto observe_through_interface = [](InferScheduler& sched) {
    return std::pair<std::string, std::size_t>{std::string{sched.policy_name()},
                                               sched.metrics().queue_depth};
  };
  const auto [policy, depth] = observe_through_interface(*scheduler);
  EXPECT_EQ(policy, "fifo");
  EXPECT_EQ(depth, 0u);
}

TEST(SchedulerFactory, LocalRegistryDuplicateRegistrationFails) {
  SchedulerPolicyRegistry local;
  register_builtin_scheduler_policies(local);
  auto err =
      local.register_policy("fifo", [](const SchedulerConfig&, SchedulerRuntimeHooks)
                                        -> Result<std::unique_ptr<InferScheduler>> {
        return unexpected(Error::Code::Internal, "test");
      });
  ASSERT_FALSE(err);
  EXPECT_EQ(err.error().code, Error::Code::Internal);
}

TEST(SchedulerFactory, EmptyNameRegistrationIsConfigInvalid) {
  SchedulerPolicyRegistry local;
  auto err = local.register_policy("", [](const SchedulerConfig&, SchedulerRuntimeHooks)
                                            -> Result<std::unique_ptr<InferScheduler>> {
    return unexpected(Error::Code::Internal, "test");
  });
  ASSERT_FALSE(err);
  EXPECT_EQ(err.error().code, Error::Code::ConfigInvalid);
}

TEST(SchedulerFactory, NullFactoryIsConfigInvalid) {
  SchedulerPolicyRegistry local;
  auto err = local.register_policy("custom", {});
  ASSERT_FALSE(err);
  EXPECT_EQ(err.error().code, Error::Code::ConfigInvalid);
}

TEST(SchedulerFactory, DeregisterPolicy) {
  SchedulerPolicyRegistry local;
  register_builtin_scheduler_policies(local);
  ASSERT_TRUE(local.is_registered("fifo"));
  EXPECT_TRUE(local.deregister_policy("fifo"));
  EXPECT_FALSE(local.is_registered("fifo"));
  EXPECT_FALSE(local.deregister_policy("fifo"));
}

TEST(SchedulerEnums, EventKindStringRoundTrip) {
  for (auto kind : {SchedulerEventKind::Admitted, SchedulerEventKind::AdmissionRejected,
                    SchedulerEventKind::Dispatched, SchedulerEventKind::Completed,
                    SchedulerEventKind::Cancelled, SchedulerEventKind::Expired,
                    SchedulerEventKind::MemoryPressure, SchedulerEventKind::ThermalPressure}) {
    const auto name = to_string(kind);
    const auto parsed = scheduler_event_kind_from_string(name);
    ASSERT_TRUE(parsed.has_value()) << name;
    EXPECT_EQ(*parsed, kind);
  }
}

TEST(SchedulerEnums, UnknownEventKindReturnsNullopt) {
  EXPECT_FALSE(scheduler_event_kind_from_string("not_a_kind").has_value());
}

TEST(SchedulerEnums, PressureRoundTrip) {
  EXPECT_EQ(to_string(PressureSource::Memory), "memory");
  EXPECT_EQ(to_string(PressureSource::Thermal), "thermal");
  EXPECT_EQ(pressure_source_from_string("memory"), PressureSource::Memory);
  EXPECT_EQ(pressure_source_from_string("thermal"), PressureSource::Thermal);
  EXPECT_EQ(to_string(PressureSeverity::Normal), "normal");
  EXPECT_EQ(to_string(PressureSeverity::Warning), "warning");
  EXPECT_EQ(to_string(PressureSeverity::Critical), "critical");
  EXPECT_EQ(pressure_severity_from_string("critical"), PressureSeverity::Critical);
  EXPECT_FALSE(pressure_source_from_string("nvml").has_value());
}

TEST(SchedulerClock, FakeClockAdvancesMonotonically) {
  FakeSchedulerClock clock;
  const auto t0 = clock.now();
  clock.advance(std::chrono::milliseconds{5});
  const auto t1 = clock.now();
  EXPECT_GT(t1, t0);
  // Negative deltas are ignored (monotonic).
  clock.advance(std::chrono::milliseconds{-100});
  EXPECT_EQ(clock.now(), t1);
}

TEST(SchedulerRequest, EnvelopePreservesIdentity) {
  FakeSchedulerClock clock;
  auto req = make_infer_request("req-1", "endpoint-a");
  ServiceEstimate est;
  est.estimated_service_time = std::chrono::milliseconds{3};
  SchedulerRequest envelope{std::move(req), "tensorrt", "model-a", est, clock.now(),
                             /*priority=*/7};
  EXPECT_EQ(envelope.request_id(), "req-1");
  EXPECT_EQ(envelope.endpoint(), "endpoint-a");
  EXPECT_EQ(envelope.backend_name(), "tensorrt");
  EXPECT_EQ(envelope.model_id(), "model-a");
  EXPECT_EQ(envelope.priority(), 7);
  ASSERT_TRUE(envelope.estimate().estimated_service_time.has_value());
  EXPECT_EQ(*envelope.estimate().estimated_service_time,
            std::chrono::duration_cast<SchedulerClock::Duration>(std::chrono::milliseconds{3}));
}

}  // namespace
