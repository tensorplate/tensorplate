// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F04-T01 / T02: Completion, cancellation, and buffer-cleanup
// coverage. Verifies the scheduler's accounting and cleanup rules:
//
//   - on_completion removes in-flight accounting exactly once.
//   - cancel works for queued and in-flight requests.
//   - cancel/expire/shutdown release queued input buffers through the
//     V01-E03 buffer-plane cleanup helpers when a manager is wired.
//   - double completion / double cancel / cancel-after-completion are
//     deterministic typed errors, not corruption.
//   - Sync requests and SmolVLA-style async chunk requests share the
//     same cleanup path.

#include <chrono>
#include <memory>
#include <string>
#include <utility>

#include <gtest/gtest.h>

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;

struct ManagerHarness {
  std::unique_ptr<BufferManager> manager;
  std::unique_ptr<FakeSchedulerClock> clock = std::make_unique<FakeSchedulerClock>();
  RecordingSchedulerEventSink sink;
  std::unique_ptr<InferScheduler> scheduler;

  ManagerHarness() {
    BufferManagerConfig bcfg;
    bcfg.capacity_bytes = 4096;
    bcfg.max_buffer_bytes = 1024;
    manager = BufferManager::create(bcfg).value();

    SchedulerConfig scfg;
    scfg.queue_capacity = 8;
    scfg.in_flight_capacity = 4;
    scfg.deadline_margin = std::chrono::milliseconds{100};
    scfg.default_service_estimate = std::chrono::milliseconds{1};
    SchedulerRuntimeHooks hooks;
    hooks.clock = clock.get();
    hooks.event_sink = &sink;
    hooks.buffer_manager = manager.get();
    scheduler = make_scheduler(scfg, hooks).value();
  }
};

TEST(SchedulerCompletion, RemovesInFlightAccounting) {
  ManagerHarness h;
  ASSERT_TRUE(
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "a")));
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  EXPECT_EQ(h.scheduler->metrics().in_flight, 1u);

  ASSERT_TRUE(h.scheduler->on_completion("a", CompletionStatus::Success, std::nullopt));
  EXPECT_EQ(h.scheduler->metrics().in_flight, 0u);
  EXPECT_EQ(h.scheduler->metrics().completed_success, 1u);
}

TEST(SchedulerCompletion, FailureCompletionIncrementsFailureCounter) {
  ManagerHarness h;
  ASSERT_TRUE(
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "a")));
  ASSERT_TRUE(h.scheduler->next().has_value());
  ASSERT_TRUE(
      h.scheduler->on_completion("a", CompletionStatus::Failure, Error::Code::InferenceFailed));
  const auto m = h.scheduler->metrics();
  EXPECT_EQ(m.completed_failure, 1u);
  EXPECT_EQ(m.completed_success, 0u);
}

TEST(SchedulerCompletion, DuplicateCompletionIsTypedInternal) {
  ManagerHarness h;
  ASSERT_TRUE(
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "a")));
  ASSERT_TRUE(h.scheduler->next().has_value());
  ASSERT_TRUE(h.scheduler->on_completion("a", CompletionStatus::Success, std::nullopt));
  auto dup = h.scheduler->on_completion("a", CompletionStatus::Success, std::nullopt);
  ASSERT_FALSE(dup);
  EXPECT_EQ(dup.error().code, Error::Code::Internal);
  // Accounting unchanged.
  EXPECT_EQ(h.scheduler->metrics().completed_success, 1u);
}

TEST(SchedulerCompletion, UnknownRequestIdIsTypedInternal) {
  ManagerHarness h;
  auto r = h.scheduler->on_completion("not-here", CompletionStatus::Success, std::nullopt);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Internal);
}

TEST(SchedulerCancel, QueuedReleaseInputBuffers) {
  ManagerHarness h;
  ASSERT_TRUE(
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "a")));
  EXPECT_EQ(h.manager->accounting().active_count, 1u);

  ASSERT_TRUE(h.scheduler->cancel("a", CancellationReason::ClientRequest));
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 0u);
  EXPECT_EQ(h.scheduler->metrics().cancelled_queued, 1u);
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
}

TEST(SchedulerCancel, QueuedExpiryReleasesInputBuffers) {
  ManagerHarness h;
  // Push a request with a tight deadline.
  const auto deadline = h.clock->now() + std::chrono::milliseconds{5};
  ASSERT_TRUE(h.scheduler->admit(
      make_request_with_owned_buffer(*h.manager, *h.clock, "a", /*byte_size=*/64, deadline)));
  EXPECT_EQ(h.manager->accounting().active_count, 1u);

  h.clock->advance_ms(std::chrono::milliseconds{100});
  EXPECT_EQ(h.scheduler->expire_due(), 1u);
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
}

TEST(SchedulerCancel, InFlightCancelClearsAccountingAndTombstones) {
  ManagerHarness h;
  ASSERT_TRUE(
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "a")));
  auto next = h.scheduler->next();
  ASSERT_TRUE(next.has_value());
  EXPECT_EQ(h.scheduler->metrics().in_flight, 1u);

  ASSERT_TRUE(h.scheduler->cancel("a", CancellationReason::ClientRequest));
  EXPECT_EQ(h.scheduler->metrics().in_flight, 0u);
  EXPECT_EQ(h.scheduler->metrics().cancelled_in_flight, 1u);

  // Racing on_completion is a typed no-op (Internal) and does not
  // increment the success counter.
  auto race = h.scheduler->on_completion("a", CompletionStatus::Success, std::nullopt);
  ASSERT_FALSE(race);
  EXPECT_EQ(race.error().code, Error::Code::Internal);
  EXPECT_EQ(h.scheduler->metrics().completed_success, 0u);

  // Executor still owns the SchedulerRequest and is responsible for
  // releasing the input buffers; release them here so the manager
  // reaches active_count = 0 for the rest of the test fixture.
  ASSERT_TRUE(release_request_buffers(*h.manager, next->request()).clean());
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
}

TEST(SchedulerCancel, UnknownIdReturnsNotReady) {
  ManagerHarness h;
  auto r = h.scheduler->cancel("missing", CancellationReason::ClientRequest);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::NotReady);
}

TEST(SchedulerCancel, DoubleCancelInFlightSurfaceTypedNotReady) {
  ManagerHarness h;
  ASSERT_TRUE(
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "a")));
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  ASSERT_TRUE(h.scheduler->cancel("a", CancellationReason::ClientRequest));

  // Second cancel finds the tombstone and returns NotReady (typed
  // no-op) instead of corrupting state.
  auto second = h.scheduler->cancel("a", CancellationReason::ClientRequest);
  ASSERT_FALSE(second);
  EXPECT_EQ(second.error().code, Error::Code::NotReady);

  ASSERT_TRUE(release_request_buffers(*h.manager, first->request()).clean());
}

TEST(SchedulerCancel, CancelAfterCompletionReturnsNotReady) {
  ManagerHarness h;
  ASSERT_TRUE(
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "a")));
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  ASSERT_TRUE(h.scheduler->on_completion("a", CompletionStatus::Success, std::nullopt));

  auto cancel_after = h.scheduler->cancel("a", CancellationReason::ClientRequest);
  ASSERT_FALSE(cancel_after);
  EXPECT_EQ(cancel_after.error().code, Error::Code::NotReady);

  ASSERT_TRUE(release_request_buffers(*h.manager, first->request()).clean());
}

TEST(SchedulerShutdown, DrainsQueuedAndCancelsInFlight) {
  ManagerHarness h;
  for (auto id : {"a", "b", "c"}) {
    ASSERT_TRUE(h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, id)));
  }
  auto first = h.scheduler->next();
  ASSERT_TRUE(first.has_value());
  EXPECT_EQ(h.manager->accounting().active_count, 3u);

  // Shutdown drains queued ('b', 'c' release their buffers) and
  // tombstones the in-flight 'a'. Total cancelled is 3.
  EXPECT_EQ(h.scheduler->shutdown(), 3u);
  EXPECT_EQ(h.scheduler->metrics().queue_depth, 0u);
  EXPECT_EQ(h.scheduler->metrics().in_flight, 0u);

  // Queued buffers are gone; the in-flight 'a' is still owned by the
  // executor and the test releases it.
  EXPECT_EQ(h.manager->accounting().active_count, 1u);
  ASSERT_TRUE(release_request_buffers(*h.manager, first->request()).clean());
  EXPECT_EQ(h.manager->accounting().active_count, 0u);

  // Further admits are rejected NotReady.
  auto rejected =
      h.scheduler->admit(make_request_with_owned_buffer(*h.manager, *h.clock, "d"));
  ASSERT_FALSE(rejected);
  EXPECT_EQ(rejected.error().code, Error::Code::NotReady);
  // Buffers from the rejected admit are released too.
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
}

TEST(SchedulerCancel, AsyncChunkAndSyncShareCleanupPath) {
  // SmolVLA async chunk requests carry RequestMetadata with
  // action_chunk_id / sequence; the scheduler treats them through the
  // same cancel path as sync requests. This test confirms the cleanup
  // is identical.
  ManagerHarness h;
  RequestMetadata meta;
  meta.action_chunk_id = "chunk-7";
  meta.action_chunk_sequence = 7;

  // Build directly so we can attach metadata.
  auto buf = h.manager->allocate(64).value();
  auto view =
      TensorView::create(DType::Float32, {1, 16}, Layout::RowMajor, 0, 64).value();
  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{"input", buf, view});
  auto req = InferRequest::create("vla-1", "policy/endpoint", std::move(inputs), meta).value();
  SchedulerRequest envelope{std::move(req), "python_pytorch", "smolvla", {}, h.clock->now()};
  ASSERT_TRUE(h.scheduler->admit(std::move(envelope)));
  EXPECT_EQ(h.manager->accounting().active_count, 1u);

  ASSERT_TRUE(h.scheduler->cancel("vla-1", CancellationReason::StaleSequence));
  EXPECT_EQ(h.manager->accounting().active_count, 0u);
  EXPECT_EQ(h.scheduler->metrics().cancelled_queued, 1u);
}

}  // namespace
