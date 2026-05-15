// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F05: Serving pipeline.
//
// `ServingPipeline` connects a normalized `InferRequest` to scheduler
// admission, execution-session dispatch, completion accounting, and
// `InferResult` production. The pipeline holds the scheduler and the
// session through their public interfaces only.
//
// The pipeline does not parse HTTP; the router (V01-E07-F03) does
// that and hands the pipeline a normalized request. The pipeline
// also does not own the buffer manager; the caller does.

#pragma once

#include <atomic>
#include <chrono>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_set>

#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

class BufferManager;
class InferScheduler;
class ServingMetrics;

namespace serving {

class AsyncPolicyStore;

/// Construction-time dependencies. Owned externally (typically by
/// the composition root); the pipeline holds non-owning references.
struct ServingPipelineDeps {
  InferScheduler* scheduler = nullptr;
  ExecutionSession* session = nullptr;
  BufferManager* buffer_manager = nullptr;
  ServingMetrics* metrics = nullptr;
  std::string backend_name;
  std::string model_id;
  std::string endpoint;
};

/// Outcome of a sync `/infer` call. Either a typed Error (the
/// scheduler/session boundary rejected the work) or a published
/// `InferResult` (success or adapter-side failure).
struct SyncOutcome {
  Result<InferResult> result{unexpected(Error::Code::Internal, "uninitialized sync outcome")};
  std::chrono::nanoseconds queue_wait{0};
  std::chrono::nanoseconds execution{0};
  std::chrono::nanoseconds total{0};
};

/// Outcome of an accepted async request. The pipeline returns the
/// scheduler-side handle so the caller can publish the async-store
/// entry.
struct AsyncAcceptOutcome {
  std::string request_id;
  std::chrono::nanoseconds ingress_latency{0};
};

class ServingPipeline {
 public:
  explicit ServingPipeline(ServingPipelineDeps deps);
  ~ServingPipeline();
  ServingPipeline(const ServingPipeline&) = delete;
  ServingPipeline& operator=(const ServingPipeline&) = delete;
  ServingPipeline(ServingPipeline&&) = delete;
  ServingPipeline& operator=(ServingPipeline&&) = delete;

  /// Synchronous request flow. Admits, dispatches, infers, completes,
  /// and releases input buffers. The success path returns an owned
  /// `InferResult`; the caller serializes it and releases the result
  /// buffers after the response is written.
  ///
  /// Errors:
  ///   - ConfigInvalid: invalid InferRequest envelope.
  ///   - NotReady: serving is stopping or session not Ready.
  ///   - OOMError / Timeout / Unsupported / Internal: surfaced
  ///     from scheduler `admit`.
  [[nodiscard]] SyncOutcome run_sync(InferRequest request);

  /// Async request flow. Admits the request through the same
  /// scheduler interface as sync but returns immediately after
  /// admission. The async-store records the pending entry and the
  /// dispatcher thread runs `dispatch_one` to make progress.
  ///
  /// Errors mirror `run_sync`.
  [[nodiscard]] Result<AsyncAcceptOutcome> run_async(InferRequest request,
                                                    AsyncPolicyStore& store);

  /// Dispatch the next admitted request through the scheduler.
  /// Called by the async dispatcher thread. Returns true if a
  /// request was dispatched (success or failure), false if the
  /// queue was empty.
  bool dispatch_one(AsyncPolicyStore& store);

  /// Mark the pipeline as stopping. Subsequent `run_sync` and
  /// `run_async` calls return `NotReady` immediately without
  /// touching the scheduler. Shutdown drain still uses the
  /// scheduler directly.
  void set_stopping(bool stopping) noexcept;
  [[nodiscard]] bool is_stopping() const noexcept;

  /// Backend name used for scheduler envelopes. Surfaced through
  /// metrics labels.
  [[nodiscard]] std::string_view backend_name() const noexcept;

 private:
  ServingPipelineDeps deps_;
  std::atomic<bool> stopping_{false};

  // Cancellation tombstones: a sync executor cannot `on_completion`
  // for a request that was cancelled while dispatched. The pipeline
  // checks the set under a mutex before completing.
  std::mutex cancelled_mutex_;
  std::unordered_set<std::string> cancelled_inflight_;
};

}  // namespace serving
}  // namespace tensorplate
