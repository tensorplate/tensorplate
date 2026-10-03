// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <optional>

#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

#include "serving/session/credits.hpp"

namespace tensorplate::serving {

/// What a queued server message is, which decides its budget and whether
/// suppression discards it.
enum class OutputKind : std::uint8_t {
  /// Task output: PCM, charged to the PCM budget.
  Audio = 0,
  /// Task output: a replaceable hypothesis, charged to the metadata budget
  /// short of its lifecycle reserve.
  Partial = 1,
  /// Task output that is never replaced: a final, an endpoint or a
  /// completion. Charged like Partial.
  Result = 2,
  /// A lifecycle message or reply. Charged to the whole metadata budget;
  /// survives suppression.
  Control = 3,
  /// The session's single terminal outcome. Not charged, so it is accepted
  /// when the budget is full; nothing is accepted after it.
  Terminal = 4,
};

/// Body of a queued message. The transport binding defines the concrete
/// types; the queue never looks inside one. A destructor must not call the
/// queue or the session manager: bodies may be destroyed while either works.
class OutputBody {
 public:
  OutputBody() = default;
  OutputBody(const OutputBody&) = delete;
  OutputBody& operator=(const OutputBody&) = delete;
  virtual ~OutputBody() = default;
};

struct OutputItem {
  OutputKind kind = OutputKind::Control;
  /// Budget charge: the PCM bytes of Audio, the encoded size of anything else
  /// (at least one).
  std::uint64_t bytes = 0;
  /// Partial: the nonzero utterance whose hypothesis this is. Result: the
  /// utterance whose unsent partial it supersedes, or zero. Control: nonzero
  /// for a reply that replaces the unsent Control with the same key, a reply
  /// whose newest instance says everything. Zero for Audio and Terminal.
  std::uint64_t replace_key = 0;
  std::unique_ptr<OutputBody> body;
};

/// An item handed to the transport, with the sequence the stream assigns it.
struct SequencedOutput {
  std::uint64_t sequence = 0;
  OutputItem item;
};

enum class OutputOffer : std::uint8_t {
  /// Appended.
  Queued = 0,
  /// Took the place of the unsent item of the same kind and key.
  Replaced = 1,
  /// No room. A producer of task output keeps the item and pauses until
  /// output drains. A lifecycle reply is refused only when the peer has left
  /// the whole metadata budget unread; the sink does not queue it, and the
  /// session ends by the no-progress limit or, once cancelled, by its
  /// terminal outcome.
  Full = 2,
  /// Task output after suppression: the producer discards the item.
  Suppressed = 3,
  /// After the terminal outcome: the producer discards the item.
  Closed = 4,
};

/// Bounded queue of one stream's server messages, in sending order.
///
/// The stream assigns sequences when the transport takes an item, so an
/// unsent partial or keyed reply can still be replaced. An item stays charged
/// to its budget from offer() until delivered(): what the transport holds
/// counts.
///
/// Thread-safe. It outlives the session's slot, because lifecycle messages
/// and the terminal outcome are still delivered after release. Every `now`
/// comes from the clock the session manager was created with.
class BoundedOutputQueue {
 public:
  explicit BoundedOutputQueue(const SessionBudgets& budgets) noexcept;

  /// Queues `item` if it is valid and there is room. Only Queued and Replaced
  /// move from `item`; any other outcome leaves it and the queue unchanged.
  /// @return ConfigInvalid with context "invalid_output_item" (no body, no
  ///   bytes, a partial without its utterance, a key on a kind that takes
  ///   none) or "output_item_too_large" (it exceeds all its kind may use).
  [[nodiscard]] Result<OutputOffer> offer(OutputItem& item, SchedulerClock::TimePoint now);

  /// Hands the oldest unsent item to the transport under the next sequence.
  [[nodiscard]] std::optional<SequencedOutput> take();

  /// The transport delivered the item taken as `sequence`.
  /// @return Internal with context "delivery_out_of_order" unless `sequence`
  ///   is the oldest taken, undelivered one.
  [[nodiscard]] Result<void> delivered(std::uint64_t sequence, SchedulerClock::TimePoint now);

  /// Discards unsent task output and refuses task output from now on.
  void suppress();

  /// Since when output has awaited delivery without progress; none while
  /// nothing awaits delivery. Only a delivery moves it.
  [[nodiscard]] std::optional<SchedulerClock::TimePoint> stalled_since() const;

  [[nodiscard]] LogicalSessionUsage pcm_usage() const;
  [[nodiscard]] LogicalSessionUsage metadata_usage() const;
  /// Sequence of the last item taken; zero before the first.
  [[nodiscard]] std::uint64_t last_sequence() const;

 private:
  struct InFlight {
    std::uint64_t sequence = 0;
    OutputKind kind = OutputKind::Control;
    std::uint64_t bytes = 0;
  };

  [[nodiscard]] bool pending_locked() const noexcept;
  /// The unsent item of `kind` carrying the nonzero `key`, or the end.
  [[nodiscard]] std::deque<OutputItem>::iterator find_unsent_locked(OutputKind kind,
                                                                    std::uint64_t key);

  [[nodiscard]] std::uint64_t limit_for(OutputKind kind) const noexcept;

  std::uint64_t pcm_limit_;
  std::uint64_t metadata_limit_;
  std::uint64_t control_reserve_;
  mutable std::mutex mu_;
  std::deque<OutputItem> unsent_;
  std::deque<InFlight> in_flight_;
  std::uint64_t pcm_used_ = 0;
  std::uint64_t metadata_used_ = 0;
  std::uint64_t last_sequence_ = 0;
  std::optional<SchedulerClock::TimePoint> stalled_since_;
  bool suppressed_ = false;
  bool closed_ = false;
};
}  // namespace tensorplate::serving
