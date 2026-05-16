// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/pipeline.hpp"

#include <chrono>
#include <condition_variable>
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

struct ServingPipeline::SyncWaiter {
  std::mutex mutex;
  std::condition_variable cv;
  bool done = false;
  bool dispatched = false;
  bool abandoned = false;
  SyncOutcome outcome;
  Clock::time_point admitted_at{};
};

ServingPipeline::ServingPipeline(ServingPipelineDeps deps) : deps_(std::move(deps)) {}

ServingPipeline::~ServingPipeline() = default;

void ServingPipeline::set_stopping(bool stopping) noexcept {
  stopping_.store(stopping);
}
bool ServingPipeline::is_stopping() const noexcept {
  return stopping_.load();
}
std::string_view ServingPipeline::backend_name() const noexcept {
  return deps_.backend_name;
}

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
  const std::string request_id = request.request_id();
  const auto deadline = request.deadline();
  auto waiter = std::make_shared<SyncWaiter>();
  waiter->admitted_at = t0;

  {
    std::lock_guard<std::mutex> g(sync_waiters_mutex_);
    if (sync_waiters_.contains(request_id)) {
      out.result = unexpected(Error::Code::ConfigInvalid,
                              "serving pipeline: duplicate active sync request_id");
      if (deps_.buffer_manager != nullptr) {
        (void)release_request_buffers(*deps_.buffer_manager, request);
      }
      return out;
    }
    sync_waiters_.emplace(request_id, waiter);
  }

  // Build scheduler envelope.
  SchedulerRequest env{std::move(request), deps_.backend_name, deps_.model_id, {}, t0};
  if (auto admit = deps_.scheduler->admit(std::move(env)); !admit) {
    {
      std::lock_guard<std::mutex> g(sync_waiters_mutex_);
      sync_waiters_.erase(request_id);
    }
    out.result = unexpected(admit.error());
    if (deps_.metrics != nullptr) {
      deps_.metrics->record_rejection(admit.error().code);
    }
    return out;
  }

  {
    std::unique_lock<std::mutex> g(waiter->mutex);
    while (!waiter->done) {
      if (deadline.has_value() && !waiter->dispatched) {
        if (Clock::now() >= *deadline) {
          waiter->abandoned = true;
          break;
        }
        waiter->cv.wait_until(g, *deadline, [&] { return waiter->done || waiter->dispatched; });
        continue;
      }
      waiter->cv.wait(g, [&] { return waiter->done; });
    }
    if (waiter->abandoned && !waiter->done) {
      g.unlock();
      {
        std::lock_guard<std::mutex> map_g(sync_waiters_mutex_);
        if (auto it = sync_waiters_.find(request_id);
            it != sync_waiters_.end() && it->second == waiter) {
          sync_waiters_.erase(it);
        }
      }
      (void)deps_.scheduler->cancel(request_id, CancellationReason::ClientRequest);
      out.result = unexpected(Error::Code::Timeout,
                              "serving pipeline: sync request deadline expired before dispatch");
      out.total = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - t0);
      if (deps_.metrics != nullptr) {
        deps_.metrics->record_rejection(Error::Code::Timeout);
        deps_.metrics->observe_total_ms(to_ms(out.total));
      }
      return out;
    }
    out = std::move(waiter->outcome);
  }

  {
    std::lock_guard<std::mutex> g(sync_waiters_mutex_);
    sync_waiters_.erase(request_id);
  }
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
  std::shared_ptr<SyncWaiter> sync_waiter;
  {
    std::lock_guard<std::mutex> g(sync_waiters_mutex_);
    if (auto it = sync_waiters_.find(request_id); it != sync_waiters_.end()) {
      sync_waiter = it->second;
    }
  }
  if (sync_waiter != nullptr) {
    bool abandoned = false;
    {
      std::lock_guard<std::mutex> g(sync_waiter->mutex);
      abandoned = sync_waiter->abandoned;
      if (!abandoned) {
        sync_waiter->dispatched = true;
      }
    }
    sync_waiter->cv.notify_one();
    if (abandoned) {
      if (deps_.buffer_manager != nullptr) {
        (void)release_request_buffers(*deps_.buffer_manager, next->request());
      }
      (void)deps_.scheduler->on_completion(request_id, CompletionStatus::Failure,
                                           Error::Code::Timeout);
      return true;
    }
  }
  if (sync_waiter == nullptr) {
    store.mark_in_flight(request_id);
  }
  const auto t_dispatch = Clock::now();
  SyncOutcome sync_out;
  if (sync_waiter != nullptr) {
    sync_out.queue_wait =
        std::chrono::duration_cast<std::chrono::nanoseconds>(t_dispatch - sync_waiter->admitted_at);
  }
  const auto t_exec_start = Clock::now();
  Result<InferResult> inferred = deps_.session->infer(next->request());
  const auto t_exec_end = Clock::now();
  const auto execution =
      std::chrono::duration_cast<std::chrono::nanoseconds>(t_exec_end - t_exec_start);
  if (sync_waiter != nullptr) {
    sync_out.execution = execution;
  }
  if (deps_.buffer_manager != nullptr) {
    (void)release_request_buffers(*deps_.buffer_manager, next->request());
  }
  if (!inferred) {
    (void)deps_.scheduler->on_completion(request_id, CompletionStatus::Failure,
                                         inferred.error().code);
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_requests_failed();
    }
    if (sync_waiter != nullptr) {
      sync_out.result = unexpected(inferred.error());
      sync_out.total = std::chrono::duration_cast<std::chrono::nanoseconds>(
          Clock::now() - sync_waiter->admitted_at);
      {
        std::lock_guard<std::mutex> g(sync_waiter->mutex);
        sync_waiter->outcome = std::move(sync_out);
        sync_waiter->done = true;
      }
      sync_waiter->cv.notify_one();
    } else {
      (void)store.publish_failure(request_id, inferred.error());
    }
    return true;
  }
  InferResult result = std::move(inferred).value();
  const auto completion_status =
      result.is_success() ? CompletionStatus::Success : CompletionStatus::Failure;
  auto completion_r = deps_.scheduler->on_completion(
      request_id, completion_status,
      result.is_success() ? std::nullopt : std::optional<Error::Code>{result.error().code});
  if (!completion_r) {
    if (deps_.buffer_manager != nullptr) {
      (void)release_partial_outputs(*deps_.buffer_manager, result.outputs());
    }
    if (deps_.metrics != nullptr) {
      deps_.metrics->increment_requests_failed();
    }
    if (sync_waiter != nullptr) {
      sync_out.result = unexpected(completion_r.error());
      sync_out.total = std::chrono::duration_cast<std::chrono::nanoseconds>(
          Clock::now() - sync_waiter->admitted_at);
      {
        std::lock_guard<std::mutex> g(sync_waiter->mutex);
        sync_waiter->outcome = std::move(sync_out);
        sync_waiter->done = true;
      }
      sync_waiter->cv.notify_one();
    } else {
      (void)store.publish_failure(request_id, completion_r.error());
    }
    return true;
  }
  if (deps_.metrics != nullptr) {
    if (result.is_success()) {
      deps_.metrics->increment_requests_succeeded();
    } else {
      deps_.metrics->increment_requests_failed();
    }
    deps_.metrics->observe_execution_ms(to_ms(execution));
  }
  if (sync_waiter != nullptr) {
    sync_out.result = std::move(result);
    sync_out.total = std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() -
                                                                          sync_waiter->admitted_at);
    if (deps_.metrics != nullptr) {
      deps_.metrics->observe_queue_wait_ms(to_ms(sync_out.queue_wait));
      deps_.metrics->observe_total_ms(to_ms(sync_out.total));
    }
    {
      std::lock_guard<std::mutex> g(sync_waiter->mutex);
      sync_waiter->outcome = std::move(sync_out);
      sync_waiter->done = true;
    }
    sync_waiter->cv.notify_one();
  } else if (!store.publish_result(request_id, std::move(result))) {
    // Result suppressed (cancelled/stale); buffers released by the store.
  }
  return true;
}

}  // namespace tensorplate::serving
