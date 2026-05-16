// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/async_policy.hpp"

#include <algorithm>
#include <array>
#include <chrono>
#include <list>
#include <memory>
#include <mutex>
#include <utility>

#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/serving/metrics.hpp"

namespace tensorplate::serving {

namespace {

constexpr std::array<std::pair<AsyncStatus, std::string_view>, 7> kAsyncStatusNames{{
    {AsyncStatus::Pending, "pending"},
    {AsyncStatus::InFlight, "in_flight"},
    {AsyncStatus::Completed, "completed"},
    {AsyncStatus::Cancelled, "cancelled"},
    {AsyncStatus::Stale, "stale"},
    {AsyncStatus::Failed, "failed"},
    {AsyncStatus::Expired, "expired"},
}};

}  // namespace

std::string_view to_string(AsyncStatus status) noexcept {
  for (const auto& [k, v] : kAsyncStatusNames) {
    if (k == status) {
      return v;
    }
  }
  return "pending";
}

std::optional<AsyncStatus> async_status_from_string(std::string_view name) noexcept {
  for (const auto& [k, v] : kAsyncStatusNames) {
    if (v == name) {
      return k;
    }
  }
  return std::nullopt;
}

struct AsyncPolicyStore::Entry {
  std::string request_id;
  std::optional<std::string> correlation_id;
  std::optional<std::string> action_chunk_id;
  std::optional<std::int64_t> action_chunk_sequence;
  AsyncStatus status = AsyncStatus::Pending;
  std::optional<InferResult> result;
  std::optional<Error> error;
  std::vector<NamedInput> retained_inputs;
  SchedulerClock::TimePoint completed_at{};
  SchedulerClock::TimePoint accepted_at{};
};

AsyncPolicyStore::AsyncPolicyStore(AsyncPolicyConfig config, BufferManager& buffer_manager,
                                   const SchedulerClock* clock, ServingMetrics* metrics)
    : config_(config), buffer_manager_(buffer_manager), clock_(clock), metrics_(metrics) {}

AsyncPolicyStore::~AsyncPolicyStore() {
  // Release any retained buffers so the buffer manager sees zero
  // active request-owned buffers on destruction.
  std::lock_guard<std::mutex> g(mutex_);
  for (auto& entry : entries_) {
    if (entry->result.has_value()) {
      (void)release_partial_outputs(buffer_manager_, entry->result->outputs());
    }
    // Inputs were transferred out of the store at admit; the
    // pipeline owns input cleanup. Defensive sweep below covers
    // accept-but-never-dispatched cancellation paths.
    for (auto& in : entry->retained_inputs) {
      (void)buffer_manager_.release_if_owned(in.buffer);
    }
  }
  entries_.clear();
  by_id_.clear();
}

Result<void> AsyncPolicyStore::add_pending(const InferRequest& request) {
  std::lock_guard<std::mutex> g(mutex_);
  if (counts_.pending + counts_.in_flight >= config_.max_pending) {
    return unexpected(Error::Code::OOMError, "async store: max_pending exceeded");
  }
  if (by_id_.contains(request.request_id())) {
    return unexpected(Error::Code::Internal,
                      "async store: duplicate request_id " + request.request_id());
  }
  auto entry = std::make_unique<Entry>();
  entry->request_id = request.request_id();
  entry->correlation_id = request.metadata().correlation_id;
  entry->action_chunk_id = request.metadata().action_chunk_id;
  entry->action_chunk_sequence = request.metadata().action_chunk_sequence;
  entry->accepted_at = clock_ != nullptr ? clock_->now() : SchedulerClock::TimePoint{};
  by_id_[entry->request_id] = entry.get();
  entries_.push_back(std::move(entry));
  ++counts_.pending;
  if (metrics_ != nullptr) {
    metrics_->increment_async_accepted();
  }
  return Result<void>{};
}

void AsyncPolicyStore::mark_in_flight(std::string_view request_id) noexcept {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    return;
  }
  Entry* e = it->second;
  if (e->status == AsyncStatus::Pending) {
    e->status = AsyncStatus::InFlight;
    if (counts_.pending > 0) {
      --counts_.pending;
    }
    ++counts_.in_flight;
  }
}

bool AsyncPolicyStore::publish_result(std::string_view request_id, InferResult result) {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    // Entry evicted or never existed; release buffers locally.
    (void)release_partial_outputs(buffer_manager_, result.outputs());
    return false;
  }
  Entry* e = it->second;
  if (e->status == AsyncStatus::Cancelled || e->status == AsyncStatus::Stale ||
      e->status == AsyncStatus::Expired) {
    // Suppress: drop buffers immediately.
    (void)release_partial_outputs(buffer_manager_, result.outputs());
    return false;
  }
  if (e->status == AsyncStatus::InFlight && counts_.in_flight > 0) {
    --counts_.in_flight;
  } else if (e->status == AsyncStatus::Pending && counts_.pending > 0) {
    --counts_.pending;
  }
  e->status = AsyncStatus::Completed;
  e->result = std::move(result);
  e->completed_at = clock_ != nullptr ? clock_->now() : SchedulerClock::TimePoint{};
  ++counts_.completed;
  if (metrics_ != nullptr) {
    metrics_->increment_async_completed();
  }
  return true;
}

bool AsyncPolicyStore::publish_failure(std::string_view request_id, Error error) {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    return false;
  }
  Entry* e = it->second;
  if (e->status == AsyncStatus::Cancelled || e->status == AsyncStatus::Stale ||
      e->status == AsyncStatus::Expired) {
    return false;
  }
  if (e->status == AsyncStatus::InFlight && counts_.in_flight > 0) {
    --counts_.in_flight;
  } else if (e->status == AsyncStatus::Pending && counts_.pending > 0) {
    --counts_.pending;
  }
  e->status = AsyncStatus::Failed;
  e->error = std::move(error);
  e->completed_at = clock_ != nullptr ? clock_->now() : SchedulerClock::TimePoint{};
  ++counts_.failed;
  return true;
}

bool AsyncPolicyStore::cancel(std::string_view request_id) {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    return false;
  }
  Entry* e = it->second;
  if (e->status == AsyncStatus::Cancelled) {
    return false;
  }
  if (e->status == AsyncStatus::Pending && counts_.pending > 0) {
    --counts_.pending;
  } else if (e->status == AsyncStatus::InFlight && counts_.in_flight > 0) {
    --counts_.in_flight;
  } else if (e->status == AsyncStatus::Completed) {
    if (e->result.has_value()) {
      (void)release_partial_outputs(buffer_manager_, e->result->outputs());
      e->result.reset();
    }
    if (counts_.completed > 0) {
      --counts_.completed;
    }
  }
  // Release any retained inputs (defensive; ownership normally moves
  // to the scheduler on admit).
  for (auto& in : e->retained_inputs) {
    (void)buffer_manager_.release_if_owned(in.buffer);
  }
  e->retained_inputs.clear();
  e->status = AsyncStatus::Cancelled;
  ++counts_.cancelled;
  if (metrics_ != nullptr) {
    metrics_->increment_async_cancelled();
  }
  return true;
}

bool AsyncPolicyStore::mark_stale(std::string_view request_id) {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    return false;
  }
  Entry* e = it->second;
  if (e->status == AsyncStatus::Stale) {
    return false;
  }
  if (e->status == AsyncStatus::Completed && e->result.has_value()) {
    (void)release_partial_outputs(buffer_manager_, e->result->outputs());
    e->result.reset();
    if (counts_.completed > 0) {
      --counts_.completed;
    }
  } else if (e->status == AsyncStatus::Pending && counts_.pending > 0) {
    --counts_.pending;
  } else if (e->status == AsyncStatus::InFlight && counts_.in_flight > 0) {
    --counts_.in_flight;
  }
  e->status = AsyncStatus::Stale;
  ++counts_.stale;
  if (metrics_ != nullptr) {
    metrics_->increment_async_stale();
  }
  return true;
}

// NOLINTNEXTLINE(readability-function-cognitive-complexity)
std::vector<std::string> AsyncPolicyStore::mark_stale_before_sequence(
    std::int64_t stale_after_sequence) {
  std::vector<std::string> staled;
  std::lock_guard<std::mutex> g(mutex_);
  for (auto& entry : entries_) {
    if (entry->status == AsyncStatus::Cancelled || entry->status == AsyncStatus::Stale) {
      continue;
    }
    if (entry->action_chunk_sequence.has_value() &&
        *entry->action_chunk_sequence <= stale_after_sequence) {
      const auto previous = entry->status;
      if (entry->status == AsyncStatus::Completed && entry->result.has_value()) {
        (void)release_partial_outputs(buffer_manager_, entry->result->outputs());
        entry->result.reset();
        if (counts_.completed > 0) {
          --counts_.completed;
        }
      } else if (entry->status == AsyncStatus::Pending && counts_.pending > 0) {
        --counts_.pending;
      } else if (entry->status == AsyncStatus::InFlight && counts_.in_flight > 0) {
        --counts_.in_flight;
      }
      entry->status = AsyncStatus::Stale;
      ++counts_.stale;
      if (metrics_ != nullptr) {
        metrics_->increment_async_stale();
      }
      if (previous == AsyncStatus::Pending || previous == AsyncStatus::InFlight) {
        staled.push_back(entry->request_id);
      }
    }
  }
  return staled;
}

std::optional<AsyncEntrySnapshot> AsyncPolicyStore::snapshot(std::string_view request_id) const {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    return std::nullopt;
  }
  const Entry* e = it->second;
  AsyncEntrySnapshot out;
  out.request_id = e->request_id;
  out.correlation_id = e->correlation_id;
  out.action_chunk_id = e->action_chunk_id;
  out.action_chunk_sequence = e->action_chunk_sequence;
  out.status = e->status;
  out.result = e->result;
  out.error = e->error;
  return out;
}

std::optional<InferResult> AsyncPolicyStore::take_completed_result(std::string_view request_id) {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    return std::nullopt;
  }
  Entry* e = it->second;
  if (e->status != AsyncStatus::Completed || !e->result.has_value()) {
    return std::nullopt;
  }

  InferResult result = std::move(*e->result);
  e->result.reset();
  if (counts_.completed > 0) {
    --counts_.completed;
  }
  by_id_.erase(it);
  for (auto lit = entries_.begin(); lit != entries_.end(); ++lit) {
    if (lit->get() == e) {
      entries_.erase(lit);
      break;
    }
  }
  return result;
}

bool AsyncPolicyStore::release_completed(std::string_view request_id) {
  std::lock_guard<std::mutex> g(mutex_);
  auto it = by_id_.find(std::string(request_id));
  if (it == by_id_.end()) {
    return false;
  }
  Entry* e = it->second;
  if (e->result.has_value()) {
    (void)release_partial_outputs(buffer_manager_, e->result->outputs());
    e->result.reset();
  }
  if (e->status == AsyncStatus::Completed && counts_.completed > 0) {
    --counts_.completed;
  }
  // Remove the entry entirely.
  by_id_.erase(it);
  for (auto lit = entries_.begin(); lit != entries_.end(); ++lit) {
    if (lit->get() == e) {
      entries_.erase(lit);
      return true;
    }
  }
  return true;
}

// NOLINTNEXTLINE(readability-function-cognitive-complexity)
void AsyncPolicyStore::enforce_bounds() {
  std::lock_guard<std::mutex> g(mutex_);
  if (clock_ == nullptr) {
    return;
  }
  const auto now = clock_->now();
  // Evict completed entries past TTL.
  for (auto it = entries_.begin(); it != entries_.end();) {
    auto* e = it->get();
    const bool completed_like =
        e->status == AsyncStatus::Completed || e->status == AsyncStatus::Failed ||
        e->status == AsyncStatus::Cancelled || e->status == AsyncStatus::Stale ||
        e->status == AsyncStatus::Expired;
    if (completed_like && (now - e->completed_at) > config_.completed_ttl) {
      if (e->result.has_value()) {
        (void)release_partial_outputs(buffer_manager_, e->result->outputs());
        e->result.reset();
      }
      by_id_.erase(e->request_id);
      if (e->status == AsyncStatus::Completed && counts_.completed > 0) {
        --counts_.completed;
      }
      it = entries_.erase(it);
      if (metrics_ != nullptr) {
        metrics_->increment_async_evicted();
      }
    } else {
      ++it;
    }
  }
  // Evict oldest completed entries while above max_completed cap.
  std::size_t completed_count = 0;
  for (const auto& e : entries_) {
    if (e->status == AsyncStatus::Completed) {
      ++completed_count;
    }
  }
  while (completed_count > config_.max_completed) {
    for (auto it = entries_.begin(); it != entries_.end(); ++it) {
      if ((*it)->status == AsyncStatus::Completed) {
        if ((*it)->result.has_value()) {
          (void)release_partial_outputs(buffer_manager_, (*it)->result->outputs());
        }
        by_id_.erase((*it)->request_id);
        if (counts_.completed > 0) {
          --counts_.completed;
        }
        entries_.erase(it);
        if (metrics_ != nullptr) {
          metrics_->increment_async_evicted();
        }
        --completed_count;
        break;
      }
    }
  }
}

void AsyncPolicyStore::cancel_all() {
  std::vector<std::string> targets;
  {
    std::lock_guard<std::mutex> g(mutex_);
    targets.reserve(entries_.size());
    for (const auto& e : entries_) {
      if (e->status == AsyncStatus::Pending || e->status == AsyncStatus::InFlight ||
          e->status == AsyncStatus::Completed) {
        targets.push_back(e->request_id);
      }
    }
  }
  for (const auto& id : targets) {
    cancel(id);
  }
}

AsyncPolicyStore::CountSnapshot AsyncPolicyStore::counts() const {
  std::lock_guard<std::mutex> g(mutex_);
  return counts_;
}

}  // namespace tensorplate::serving
