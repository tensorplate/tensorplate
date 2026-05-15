// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/pipeline.hpp"

#include <chrono>
#include <mutex>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/scheduler/scheduler.hpp"
#include "tensorplate/scheduler/scheduler_request.hpp"
#include "tensorplate/serving/async_policy.hpp"
#include "tensorplate/serving/metrics.hpp"

namespace tensorplate::serving {

namespace {

using Clock = std::chrono::steady_clock;

double to_ms(std::chrono::nanoseconds ns) {
  return static_cast<double>(ns.count()) / 1e6;
}

}  // namespace

ServingPipeline::ServingPipeline(ServingPipelineDeps deps) : deps_(std::move(deps)) {}

ServingPipeline::~ServingPipeline() = default;

void ServingPipeline::set_stopping(bool stopping) noexcept { stopping_.store(stopping); }
bool ServingPipeline::is_stopping() const noexcept { return stopping_.load(); }
std::string_view ServingPipeline::backend_name() const noexcept { return deps_.backend_name; }

SyncOutcome ServingPipeline::run_sync(InferRequest request) {
  SyncOutcome out;
  if (stopping_.load()) {
    out.result =
        unexpected(Error::Code::NotReady, "serving worker stopping; not accepting new requests");
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_rejected_stopping();
    }
    if (deps_.buffer_manager != nullptr) {
      (void)release_request_buffers(*deps_.buffer_manager, request);
    }
    return out;
  }
  const auto t0 = Clock::now();
  // Build scheduler envelope.
  SchedulerRequest env{std::move(request), deps_.backend_name, deps_.model_id, {}, t0};
  const std::string request_id = env.request_id();
  if (auto admit = deps_.scheduler->admit(std::move(env)); !admit) {
    out.result = unexpected(admit.error());
    if (deps_.metrics != nullptr) {
      deps_.metrics->record_rejection(admit.error().code);
    }
    return out;
  }
  // Drive dispatch synchronously: pull our own request off the queue.
  auto next = deps_.scheduler->next();
  if (!next.has_value()) {
    // Another caller took it (concurrency); attempt to cancel.
    (void)deps_.scheduler->cancel(request_id, CancellationReason::ClientRequest);
    out.result = unexpected(Error::Code::Internal,
                            "serving pipeline: dispatch lost race against another thread");
    return out;
  }
  if (next->request_id() != request_id) {
    // We pulled a different request - run it ourselves and queue
    // ours back. In v0.1.0 the FIFO scheduler + in_flight_capacity
    // = 1 means this branch is reachable only when the test harness
    // already admitted other requests; we surface a typed error
    // rather than recursing.
    out.result = unexpected(Error::Code::Internal,
                            "serving pipeline: dispatched request_id mismatch");
    return out;
  }
  const auto t_dispatch = Clock::now();
  out.queue_wait = std::chrono::duration_cast<std::chrono::nanoseconds>(t_dispatch - t0);

  // Run inference.
  const auto t_exec_start = Clock::now();
  Result<InferResult> inferred = deps_.session->infer(next->request());
  const auto t_exec_end = Clock::now();
  out.execution = std::chrono::duration_cast<std::chrono::nanoseconds>(t_exec_end - t_exec_start);

  // Release the input buffers exactly once.
  if (deps_.buffer_manager != nullptr) {
    (void)release_request_buffers(*deps_.buffer_manager, next->request());
  }

  if (!inferred) {
    // Session/validation-layer rejection.
    (void)deps_.scheduler->on_completion(request_id, CompletionStatus::Failure,
                                          inferred.error().code);
    out.result = unexpected(inferred.error());
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_requests_failed();
    }
    out.total = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - t0);
    return out;
  }
  InferResult result = std::move(inferred).value();
  // Cancellation tombstone check: if cancelled while in-flight,
  // suppress completion and treat as cancelled.
  {
    std::lock_guard<std::mutex> g(cancelled_mutex_);
    if (cancelled_inflight_.erase(request_id) > 0) {
      (void)release_partial_outputs(*deps_.buffer_manager, result.outputs());
      out.result = unexpected(Error::Code::NotReady, "serving pipeline: request cancelled");
      out.total = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - t0);
      if (deps_.metrics != nullptr) {
        deps_.metrics->increment_cancelled();
      }
      return out;
    }
  }
  const auto completion_status =
      result.is_success() ? CompletionStatus::Success : CompletionStatus::Failure;
  (void)deps_.scheduler->on_completion(
      request_id, completion_status,
      result.is_success() ? std::nullopt : std::optional<Error::Code>{result.error().code});

  if (deps_.metrics != nullptr) {
    if (result.is_success()) {
      deps_.metrics->increment_requests_succeeded();
    } else {
      deps_.metrics->increment_requests_failed();
    }
    deps_.metrics->observe_queue_wait_ms(to_ms(out.queue_wait));
    deps_.metrics->observe_execution_ms(to_ms(out.execution));
  }
  out.total = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - t0);
  if (deps_.metrics != nullptr) {
    deps_.metrics->observe_total_ms(to_ms(out.total));
  }
  out.result = std::move(result);
  return out;
}

Result<AsyncAcceptOutcome> ServingPipeline::run_async(InferRequest request,
                                                      AsyncPolicyStore& store) {
  if (stopping_.load()) {
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_rejected_stopping();
    }
    if (deps_.buffer_manager != nullptr) {
      (void)release_request_buffers(*deps_.buffer_manager, request);
    }
    return unexpected(Error::Code::NotReady,
                      "serving worker stopping; not accepting new async requests");
  }
  const auto t0 = Clock::now();
  // Pre-register with the async store. Pending count must be
  // bounded before scheduler admission so duplicate ids are
  // rejected with a typed error.
  if (auto add = store.add_pending(request); !add) {
    if (deps_.metrics != nullptr) {
      deps_.metrics->record_rejection(add.error().code);
    }
    if (deps_.buffer_manager != nullptr) {
      (void)release_request_buffers(*deps_.buffer_manager, request);
    }
    return unexpected(add.error());
  }
  AsyncAcceptOutcome out;
  out.request_id = request.request_id();
  out.ingress_latency = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - t0);

  SchedulerRequest env{std::move(request), deps_.backend_name, deps_.model_id, {}, t0};
  if (auto admit = deps_.scheduler->admit(std::move(env)); !admit) {
    (void)store.cancel(out.request_id);
    if (deps_.metrics != nullptr) {
      deps_.metrics->record_rejection(admit.error().code);
    }
    return unexpected(admit.error());
  }
  return out;
}

bool ServingPipeline::dispatch_one(AsyncPolicyStore& store) {
  auto next = deps_.scheduler->next();
  if (!next.has_value()) {
    return false;
  }
  const std::string request_id = next->request_id();
  store.mark_in_flight(request_id);
  const auto t_exec_start = Clock::now();
  Result<InferResult> inferred = deps_.session->infer(next->request());
  const auto t_exec_end = Clock::now();
  const auto execution =
      std::chrono::duration_cast<std::chrono::nanoseconds>(t_exec_end - t_exec_start);
  if (deps_.buffer_manager != nullptr) {
    (void)release_request_buffers(*deps_.buffer_manager, next->request());
  }
  if (!inferred) {
    (void)deps_.scheduler->on_completion(request_id, CompletionStatus::Failure,
                                          inferred.error().code);
    (void)store.publish_failure(request_id, inferred.error());
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_requests_failed();
    }
    return true;
  }
  InferResult result = std::move(inferred).value();
  {
    std::lock_guard<std::mutex> g(cancelled_mutex_);
    if (cancelled_inflight_.erase(request_id) > 0) {
      if (deps_.buffer_manager != nullptr) {
        (void)release_partial_outputs(*deps_.buffer_manager, result.outputs());
      }
      if (deps_.metrics != nullptr) {
        deps_.metrics->increment_cancelled();
      }
      (void)deps_.scheduler->on_completion(request_id, CompletionStatus::Failure,
                                            Error::Code::NotReady);
      return true;
    }
  }
  const auto completion_status =
      result.is_success() ? CompletionStatus::Success : CompletionStatus::Failure;
  (void)deps_.scheduler->on_completion(
      request_id, completion_status,
      result.is_success() ? std::nullopt : std::optional<Error::Code>{result.error().code});
  if (deps_.metrics != nullptr) {
    if (result.is_success()) {
      deps_.metrics->increment_requests_succeeded();
    } else {
      deps_.metrics->increment_requests_failed();
    }
    deps_.metrics->observe_execution_ms(to_ms(execution));
  }
  if (!store.publish_result(request_id, std::move(result))) {
    // Result suppressed (cancelled/stale); buffers released by the store.
  }
  return true;
}

}  // namespace tensorplate::serving
