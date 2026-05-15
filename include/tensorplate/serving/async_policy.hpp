// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F04: LeRobot-compatible async-policy state store.
//
// Accepted async-policy requests live through the scheduler the same
// way sync `/infer` requests do; only the response shape differs.
// `AsyncPolicyStore` is the bounded in-memory registry that maps a
// request id to its current lifecycle state, the result (once
// available), and the LeRobot-shaped chunk identity / stale-sequence
// metadata used to attribute cancellations.
//
// Lifecycle states (wire names match the response field):
//
//   Pending     Admitted, not yet dispatched.
//   InFlight    Dispatched, executor running.
//   Completed   InferResult published; buffers transferred to the
//               store and released when the entry is evicted.
//   Cancelled   ClientRequest or shutdown cancellation. Buffers
//               released at cancellation time.
//   Stale       Marked stale by `mark_stale_before_sequence`; result
//               (if any) suppressed.
//   Failed      Scheduler / session reported a typed error.
//   Expired     Deadline passed before completion.
//
// Bounded retention is enforced by `max_completed`, `completed_ttl`,
// and `max_pending` from `AsyncPolicyConfig`.

#pragma once

#include <chrono>
#include <cstdint>
#include <list>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/config.hpp"

namespace tensorplate {

class ServingMetrics;
class InferScheduler;

namespace serving {

enum class AsyncStatus : std::uint8_t {
  Pending = 0,
  InFlight = 1,
  Completed = 2,
  Cancelled = 3,
  Stale = 4,
  Failed = 5,
  Expired = 6,
};

[[nodiscard]] std::string_view to_string(AsyncStatus status) noexcept;
[[nodiscard]] std::optional<AsyncStatus> async_status_from_string(std::string_view name) noexcept;

struct AsyncEntrySnapshot {
  std::string request_id;
  std::optional<std::string> correlation_id;
  std::optional<std::string> action_chunk_id;
  std::optional<std::int64_t> action_chunk_sequence;
  AsyncStatus status = AsyncStatus::Pending;
  std::optional<InferResult> result;
  std::optional<Error> error;
};

/// Store of accepted async-policy requests. Thread-safe. The store
/// holds buffer-plane references for the lifetime of an entry; the
/// destructor releases everything still held.
class AsyncPolicyStore {
 public:
  AsyncPolicyStore(AsyncPolicyConfig config, BufferManager& buffer_manager,
                   const SchedulerClock* clock, ServingMetrics* metrics);
  ~AsyncPolicyStore();

  AsyncPolicyStore(const AsyncPolicyStore&) = delete;
  AsyncPolicyStore& operator=(const AsyncPolicyStore&) = delete;
  AsyncPolicyStore(AsyncPolicyStore&&) = delete;
  AsyncPolicyStore& operator=(AsyncPolicyStore&&) = delete;

  /// Register a newly-admitted async request.
  ///
  /// Errors:
  ///   - OOMError: max_pending exceeded.
  ///   - Internal: request_id already present.
  [[nodiscard]] Result<void> add_pending(const InferRequest& request);

  /// Mark `request_id` as in-flight. No-op if the entry has been
  /// cancelled in the meantime.
  void mark_in_flight(std::string_view request_id) noexcept;

  /// Publish a successful result for `request_id`. Stores the
  /// result (including BufferRefs); subsequent `release_completed`
  /// or eviction will release the buffers.
  ///
  /// Returns true if the result was stored; false if the entry was
  /// already cancelled, stale, or evicted.
  bool publish_result(std::string_view request_id, InferResult result);

  /// Mark `request_id` as failed and record the error. Same return
  /// semantics as `publish_result`.
  bool publish_failure(std::string_view request_id, Error error);

  /// Cancel `request_id`. Releases any retained input buffers. Used
  /// by the explicit cancel route and by shutdown.
  ///
  /// Returns true if the entry transitioned to Cancelled.
  bool cancel(std::string_view request_id);

  /// Mark `request_id` as stale. Used by the stale-sequence
  /// cancellation path: a request that has already completed is
  /// still tombstoned so the result will not be returned.
  bool mark_stale(std::string_view request_id);

  /// Mark every pending / in-flight / completed entry whose
  /// `action_chunk_sequence` is <= `stale_after_sequence` as stale.
  /// Returns the ids that were transitioned (callers feed them to
  /// `InferScheduler::cancel` so queued work is cancelled). Stale
  /// completed entries have their result suppressed and buffers
  /// released.
  [[nodiscard]] std::vector<std::string> mark_stale_before_sequence(
      std::int64_t stale_after_sequence);

  /// Snapshot of one entry. Returns std::nullopt if not present.
  [[nodiscard]] std::optional<AsyncEntrySnapshot> snapshot(std::string_view request_id) const;

  /// Release the entry. If a result is stored, its buffers are
  /// released through the buffer manager. Returns true if the entry
  /// existed.
  bool release_completed(std::string_view request_id);

  /// Periodically called by the dispatcher thread to evict expired
  /// or oldest-completed entries when bounds are exceeded.
  void enforce_bounds();

  /// Cancel every pending and in-flight entry. Used by shutdown.
  void cancel_all();

  /// Snapshot of bookkeeping counts; used by tests and metrics
  /// observers.
  struct CountSnapshot {
    std::size_t pending = 0;
    std::size_t in_flight = 0;
    std::size_t completed = 0;
    std::size_t cancelled = 0;
    std::size_t stale = 0;
    std::size_t failed = 0;
    std::size_t expired = 0;
  };
  [[nodiscard]] CountSnapshot counts() const;

 private:
  struct Entry;

  AsyncPolicyConfig config_;
  BufferManager& buffer_manager_;
  const SchedulerClock* clock_;
  ServingMetrics* metrics_;

  mutable std::mutex mutex_;
  std::list<std::unique_ptr<Entry>> entries_;
  std::unordered_map<std::string, Entry*> by_id_;
  CountSnapshot counts_{};
};

}  // namespace serving
}  // namespace tensorplate
