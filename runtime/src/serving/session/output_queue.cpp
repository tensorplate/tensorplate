// SPDX-License-Identifier: Apache-2.0
#include "serving/session/output_queue.hpp"

#include <algorithm>
#include <string>
#include <utility>

namespace tensorplate::serving {
namespace {
bool task_output(OutputKind kind) noexcept {
  return kind == OutputKind::Audio || kind == OutputKind::Partial || kind == OutputKind::Result;
}

bool valid(const OutputItem& item) noexcept {
  if (!item.body || item.bytes == 0) {
    return false;
  }
  switch (item.kind) {
    case OutputKind::Audio:
    case OutputKind::Terminal:
      return item.replace_key == 0;
    case OutputKind::Partial:
      return item.replace_key != 0;
    case OutputKind::Result:
    case OutputKind::Control:
      return true;
  }
  return false;
}

Error invalid_item(std::string message, std::string context) {
  return Error::make(Error::Code::ConfigInvalid, std::move(message), std::move(context));
}
}  // namespace

BoundedOutputQueue::BoundedOutputQueue(const SessionBudgets& budgets) noexcept
    : pcm_limit_(budgets.output_pcm_bytes()),
      metadata_limit_(budgets.output_metadata_bytes()),
      control_reserve_(budgets.output_control_reserve_bytes()) {}

std::uint64_t BoundedOutputQueue::limit_for(OutputKind kind) const noexcept {
  if (kind == OutputKind::Audio) {
    return pcm_limit_;
  }
  return task_output(kind) ? metadata_limit_ - control_reserve_ : metadata_limit_;
}

Result<OutputOffer> BoundedOutputQueue::offer(OutputItem& item, SchedulerClock::TimePoint now) {
  if (!valid(item)) {
    return unexpected(invalid_item("output item is not well formed", "invalid_output_item"));
  }
  const std::uint64_t limit = limit_for(item.kind);
  if (item.bytes > limit) {
    return unexpected(
        invalid_item("output item exceeds all its kind may use", "output_item_too_large"));
  }
  // Declared before the lock so that a replaced body is destroyed outside it.
  OutputItem replaced;
  std::shared_ptr<const Consumer> consumer;
  OutputOffer outcome = OutputOffer::Full;
  {
    std::lock_guard guard(mu_);
    outcome = queue_locked(item, now, replaced);
    if (outcome == OutputOffer::Queued) {
      consumer = consumer_;
    }
  }
  if (consumer) {
    (*consumer)();
  }
  return outcome;
}

bool BoundedOutputQueue::set_consumer(Consumer on_takeable) {
  if (!on_takeable) {
    return false;
  }
  auto consumer = std::make_shared<const Consumer>(std::move(on_takeable));
  {
    std::lock_guard guard(mu_);
    if (consumer_) {
      return false;
    }
    consumer_ = consumer;
    if (unsent_.empty()) {
      return true;
    }
  }
  (*consumer)();
  return true;
}

OutputOffer BoundedOutputQueue::queue_locked(OutputItem& item, SchedulerClock::TimePoint now,
                                             OutputItem& replaced) {
  const std::uint64_t limit = limit_for(item.kind);
  if (closed_) {
    return OutputOffer::Closed;
  }
  if (suppressed_ && task_output(item.kind)) {
    return OutputOffer::Suppressed;
  }
  const bool was_pending = pending_locked();
  if (item.kind != OutputKind::Terminal) {
    // A result supersedes the partial of its utterance; a partial or a keyed
    // reply replaces its own kind.
    const OutputKind replaces = item.kind == OutputKind::Result ? OutputKind::Partial : item.kind;
    const auto old = find_unsent_locked(replaces, item.replace_key);
    std::uint64_t& used = item.kind == OutputKind::Audio ? pcm_used_ : metadata_used_;
    const std::uint64_t returned = old == unsent_.end() ? 0 : old->bytes;
    const std::uint64_t others = used - returned;
    // A replacement no larger than what it replaces always fits.
    if (item.bytes > returned && (others > limit || item.bytes > limit - others)) {
      return OutputOffer::Full;
    }
    used = others + item.bytes;
    if (old != unsent_.end()) {
      replaced = std::move(*old);
      if (item.kind == replaces) {
        *old = std::move(item);
        return OutputOffer::Replaced;
      }
      unsent_.erase(old);
    }
  } else {
    closed_ = true;
  }
  unsent_.push_back(std::move(item));
  if (!was_pending) {
    stalled_since_ = now;
  }
  return OutputOffer::Queued;
}

std::optional<SequencedOutput> BoundedOutputQueue::take() {
  std::lock_guard guard(mu_);
  if (unsent_.empty()) {
    return std::nullopt;
  }
  // A session lives at most 60 minutes, so the sequence cannot wrap.
  SequencedOutput output{++last_sequence_, std::move(unsent_.front())};
  unsent_.pop_front();
  in_flight_.push_back(InFlight{output.sequence, output.item.kind, output.item.bytes});
  return output;
}

Result<void> BoundedOutputQueue::delivered(std::uint64_t sequence, SchedulerClock::TimePoint now) {
  std::lock_guard guard(mu_);
  if (in_flight_.empty() || in_flight_.front().sequence != sequence) {
    return unexpected(Error::make(Error::Code::Internal, "delivery confirmed out of sequence order",
                                  "delivery_out_of_order"));
  }
  const InFlight done = in_flight_.front();
  in_flight_.pop_front();
  if (done.kind == OutputKind::Audio) {
    pcm_used_ -= done.bytes;
  } else if (done.kind != OutputKind::Terminal) {
    metadata_used_ -= done.bytes;
  }
  stalled_since_ = pending_locked() ? std::optional{now} : std::nullopt;
  return {};
}

void BoundedOutputQueue::suppress() {
  // Declared before the lock so discarded bodies are destroyed outside it.
  std::deque<OutputItem> discarded;
  std::lock_guard guard(mu_);
  suppressed_ = true;
  for (auto it = unsent_.begin(); it != unsent_.end();) {
    if (!task_output(it->kind)) {
      ++it;
      continue;
    }
    (it->kind == OutputKind::Audio ? pcm_used_ : metadata_used_) -= it->bytes;
    discarded.push_back(std::move(*it));
    it = unsent_.erase(it);
  }
  if (!pending_locked()) {
    stalled_since_.reset();
  }
}

std::optional<SchedulerClock::TimePoint> BoundedOutputQueue::stalled_since() const {
  std::lock_guard guard(mu_);
  return stalled_since_;
}

LogicalSessionUsage BoundedOutputQueue::pcm_usage() const {
  std::lock_guard guard(mu_);
  return LogicalSessionUsage::create(pcm_used_, pcm_limit_).value_or(LogicalSessionUsage{});
}

LogicalSessionUsage BoundedOutputQueue::metadata_usage() const {
  std::lock_guard guard(mu_);
  return LogicalSessionUsage::create(metadata_used_, metadata_limit_)
      .value_or(LogicalSessionUsage{});
}

std::uint64_t BoundedOutputQueue::last_sequence() const {
  std::lock_guard guard(mu_);
  return last_sequence_;
}

std::deque<OutputItem>::iterator BoundedOutputQueue::find_unsent_locked(OutputKind kind,
                                                                        std::uint64_t key) {
  if (key == 0) {
    return unsent_.end();
  }
  return std::find_if(unsent_.begin(), unsent_.end(), [kind, key](const OutputItem& queued) {
    return queued.kind == kind && queued.replace_key == key;
  });
}

bool BoundedOutputQueue::pending_locked() const noexcept {
  return !unsent_.empty() || !in_flight_.empty();
}
}  // namespace tensorplate::serving
