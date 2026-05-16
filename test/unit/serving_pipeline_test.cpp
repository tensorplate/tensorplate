// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F05: serving pipeline scheduler-dispatch regression tests.

#include <gtest/gtest.h>

#include <atomic>
#include <chrono>
#include <future>
#include <memory>
#include <optional>
#include <span>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/ingress.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/serving/async_policy.hpp"
#include "tensorplate/serving/metrics.hpp"
#include "tensorplate/serving/pipeline.hpp"

#include "serving/mock_session.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::serving;

InferRequest make_request(
    BufferManager& manager, std::string request_id,
    std::optional<std::int64_t> action_sequence = std::nullopt,
    std::optional<std::chrono::milliseconds> relative_deadline = std::nullopt) {
  std::vector<std::byte> payload(4);
  for (std::size_t i = 0; i < payload.size(); ++i) {
    payload[i] = static_cast<std::byte>(i);
  }
  auto view = TensorView::create(DType::UInt8, {4}, Layout::RowMajor);
  if (!view) {
    throw std::runtime_error(view.error().message);
  }
  IngressInput input{"image", std::span<const std::byte>(payload.data(), payload.size()),
                     view.value()};
  auto inputs = build_named_inputs(manager, {input});
  if (!inputs) {
    throw std::runtime_error(inputs.error().message);
  }
  RequestMetadata metadata;
  if (action_sequence.has_value()) {
    metadata.action_chunk_sequence = *action_sequence;
    metadata.action_chunk_id = "chunk-" + std::to_string(*action_sequence);
  }
  auto request = relative_deadline.has_value()
                     ? InferRequest::create_with_relative_deadline(std::move(request_id), "default",
                                                                   std::move(inputs).value(),
                                                                   metadata, relative_deadline)
                     : InferRequest::create(std::move(request_id), "default",
                                            std::move(inputs).value(), metadata);
  if (!request) {
    throw std::runtime_error(request.error().message);
  }
  return std::move(request).value();
}

TEST(ServingPipeline, SyncWaitsForOwnCompletionWhenAsyncIsQueuedFirst) {
  auto manager_r = BufferManager::create(BufferManagerConfig{});
  ASSERT_TRUE(manager_r);
  auto manager = std::move(manager_r).value();

  MockSessionConfig mock_cfg;
  MockServingSession session(*manager, mock_cfg);
  auto spec = ModelSpec::create("mock-model", ModelClass::Custom, "mock://", "mock");
  ASSERT_TRUE(spec);
  ASSERT_TRUE(session.load(spec.value()));
  ASSERT_TRUE(session.prime());

  SchedulerConfig scheduler_cfg;
  scheduler_cfg.queue_capacity = 4;
  scheduler_cfg.in_flight_capacity = 1;
  SchedulerRuntimeHooks hooks;
  hooks.buffer_manager = manager.get();
  auto scheduler_r = make_scheduler(scheduler_cfg, hooks);
  ASSERT_TRUE(scheduler_r);
  auto scheduler = std::move(scheduler_r).value();

  ServingMetrics metrics;
  AsyncPolicyStore store(AsyncPolicyConfig{}, *manager, nullptr, &metrics);
  ServingPipelineDeps deps;
  deps.scheduler = scheduler.get();
  deps.session = &session;
  deps.buffer_manager = manager.get();
  deps.metrics = &metrics;
  deps.backend_name = "mock";
  deps.model_id = "mock-model";
  deps.endpoint = "default";
  ServingPipeline pipeline(deps);

  auto async_accept = pipeline.run_async(make_request(*manager, "async-1", 1), store);
  ASSERT_TRUE(async_accept);

  auto sync_future = std::async(
      std::launch::async, [&] { return pipeline.run_sync(make_request(*manager, "sync-1")); });

  for (int i = 0; i < 100 && scheduler->metrics().admitted_total < 2; ++i) {
    std::this_thread::sleep_for(std::chrono::milliseconds{1});
  }
  ASSERT_EQ(scheduler->metrics().admitted_total, 2U);

  ASSERT_TRUE(pipeline.dispatch_one(store));
  ASSERT_TRUE(pipeline.dispatch_one(store));
  ASSERT_EQ(sync_future.wait_for(std::chrono::seconds{1}), std::future_status::ready);

  auto sync = sync_future.get();
  ASSERT_TRUE(sync.result) << sync.result.error().message;
  EXPECT_EQ(sync.result.value().request_id(), "sync-1");
  EXPECT_EQ(store.snapshot("async-1")->status, AsyncStatus::Completed);

  (void)release_partial_outputs(*manager, sync.result.value().outputs());
  auto async_result = store.take_completed_result("async-1");
  ASSERT_TRUE(async_result.has_value());
  (void)release_partial_outputs(*manager, async_result->outputs());
  EXPECT_EQ(manager->accounting().active_count, 0U);
}

TEST(ServingPipeline, SyncDeadlineExpiryReturnsTimeoutWhenNotDispatched) {
  auto manager_r = BufferManager::create(BufferManagerConfig{});
  ASSERT_TRUE(manager_r);
  auto manager = std::move(manager_r).value();

  MockSessionConfig mock_cfg;
  MockServingSession session(*manager, mock_cfg);
  auto spec = ModelSpec::create("mock-model", ModelClass::Custom, "mock://", "mock");
  ASSERT_TRUE(spec);
  ASSERT_TRUE(session.load(spec.value()));
  ASSERT_TRUE(session.prime());

  SchedulerConfig scheduler_cfg;
  scheduler_cfg.queue_capacity = 4;
  scheduler_cfg.in_flight_capacity = 1;
  SchedulerRuntimeHooks hooks;
  hooks.buffer_manager = manager.get();
  auto scheduler_r = make_scheduler(scheduler_cfg, hooks);
  ASSERT_TRUE(scheduler_r);
  auto scheduler = std::move(scheduler_r).value();

  ServingMetrics metrics;
  ServingPipelineDeps deps;
  deps.scheduler = scheduler.get();
  deps.session = &session;
  deps.buffer_manager = manager.get();
  deps.metrics = &metrics;
  deps.backend_name = "mock";
  deps.model_id = "mock-model";
  deps.endpoint = "default";
  ServingPipeline pipeline(deps);

  auto sync_future = std::async(std::launch::async, [&] {
    return pipeline.run_sync(
        make_request(*manager, "sync-deadline", std::nullopt, std::chrono::milliseconds{30}));
  });

  ASSERT_EQ(sync_future.wait_for(std::chrono::seconds{1}), std::future_status::ready);
  auto sync = sync_future.get();
  ASSERT_FALSE(sync.result);
  EXPECT_EQ(sync.result.error().code, Error::Code::Timeout);
  EXPECT_EQ(manager->accounting().active_count, 0U);
}

}  // namespace
