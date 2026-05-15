// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F04: AsyncPolicyStore unit tests.

#include <gtest/gtest.h>

#include <chrono>
#include <memory>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/async_policy.hpp"
#include "tensorplate/serving/config.hpp"

#include "scheduler_fixtures.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::serving;
using namespace tensorplate::testing;

InferRequest make_async_request(std::string id, std::int64_t seq,
                                std::optional<std::int64_t> stale = std::nullopt) {
  RequestMetadata md;
  md.correlation_id = id + "-cid";
  md.action_chunk_id = id + "-chunk";
  md.action_chunk_sequence = seq;
  md.stale_after_sequence = stale;
  return make_infer_request(std::move(id), "default", std::nullopt, std::move(md));
}

TEST(AsyncPolicyStore, AddPendingRejectsDuplicate) {
  BufferManagerConfig bcfg;
  auto mgr = BufferManager::create(bcfg).value();
  SystemSchedulerClock clk;
  AsyncPolicyConfig cfg;
  AsyncPolicyStore store(cfg, *mgr, &clk, nullptr);
  auto r1 = store.add_pending(make_async_request("a", 1));
  ASSERT_TRUE(r1);
  auto r2 = store.add_pending(make_async_request("a", 1));
  ASSERT_FALSE(r2);
  EXPECT_EQ(r2.error().code, Error::Code::Internal);
}

TEST(AsyncPolicyStore, MarkInFlightAndPublish) {
  BufferManagerConfig bcfg;
  auto mgr = BufferManager::create(bcfg).value();
  SystemSchedulerClock clk;
  AsyncPolicyStore store(AsyncPolicyConfig{}, *mgr, &clk, nullptr);
  ASSERT_TRUE(store.add_pending(make_async_request("a", 1)));
  store.mark_in_flight("a");
  EXPECT_EQ(store.counts().in_flight, 1U);
  // Publish empty success.
  std::vector<NamedOutput> outs;
  auto res = InferResult::create_success("a", std::move(outs));
  // create_success rejects empty outputs; emulate by going through failure.
  auto fail = InferResult::create_failure("a", Error{Error::Code::InferenceFailed, "x", std::nullopt});
  EXPECT_TRUE(store.publish_result("a", std::move(fail)));
  // Failure status: counts have moved from in_flight; completed
  // increments only on success. Failure publish via publish_result
  // is intentionally treated as a completion entry (the result holds
  // an Error). It increments completed_total because the entry is
  // available for client pickup.
  EXPECT_EQ(store.counts().completed, 1U);
  auto snap = store.snapshot("a");
  ASSERT_TRUE(snap.has_value());
  EXPECT_EQ(snap->status, AsyncStatus::Completed);
}

TEST(AsyncPolicyStore, StaleBeforeSequenceMarksAndReturnsQueuedIds) {
  BufferManagerConfig bcfg;
  auto mgr = BufferManager::create(bcfg).value();
  SystemSchedulerClock clk;
  AsyncPolicyStore store(AsyncPolicyConfig{}, *mgr, &clk, nullptr);
  ASSERT_TRUE(store.add_pending(make_async_request("a", 1)));
  ASSERT_TRUE(store.add_pending(make_async_request("b", 2)));
  ASSERT_TRUE(store.add_pending(make_async_request("c", 3)));
  store.mark_in_flight("a");
  auto staled = store.mark_stale_before_sequence(2);
  // Both a (in_flight) and b (pending) are <= 2 -> they appear in
  // the returned list; c is not.
  ASSERT_EQ(staled.size(), 2U);
  EXPECT_EQ(store.counts().stale, 2U);
  EXPECT_EQ(store.counts().pending, 1U);
}

TEST(AsyncPolicyStore, CancelTransitionsAndDecrements) {
  BufferManagerConfig bcfg;
  auto mgr = BufferManager::create(bcfg).value();
  SystemSchedulerClock clk;
  AsyncPolicyStore store(AsyncPolicyConfig{}, *mgr, &clk, nullptr);
  ASSERT_TRUE(store.add_pending(make_async_request("a", 1)));
  EXPECT_TRUE(store.cancel("a"));
  EXPECT_FALSE(store.cancel("a"));
  EXPECT_EQ(store.counts().cancelled, 1U);
}

TEST(AsyncPolicyStore, EnforcesBoundedMaxPending) {
  BufferManagerConfig bcfg;
  auto mgr = BufferManager::create(bcfg).value();
  SystemSchedulerClock clk;
  AsyncPolicyConfig cfg;
  cfg.max_pending = 2;
  AsyncPolicyStore store(cfg, *mgr, &clk, nullptr);
  ASSERT_TRUE(store.add_pending(make_async_request("a", 1)));
  ASSERT_TRUE(store.add_pending(make_async_request("b", 2)));
  auto r = store.add_pending(make_async_request("c", 3));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::OOMError);
}

}  // namespace
