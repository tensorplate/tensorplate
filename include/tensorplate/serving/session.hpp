// SPDX-License-Identifier: Apache-2.0
//
// Logical streaming sessions: the lifecycle of one client session and the
// bounded status it reports.
//
// A logical session is one client stream pinned to the deployment generation
// its serving worker serves. Every session of a worker shares the worker's
// single loaded backend; opening a session never loads weights or starts a
// process.
//
// LogicalSessionMachine is a pure state machine. It maps the current state and
// one event to the next state plus the effects its owner must carry out, and
// refuses every event the current state does not permit with a typed error. It
// owns no timers, clocks, queues, threads or I/O. The owner applies one
// session's events in order, one at a time, and performs the effects of each
// accepted event, in ascending LogicalSessionEffect order, before it applies
// the next. docs/architecture/serving-worker.md ("Logical sessions") renders
// the full transition table.
//
// Errors carry an Error::Code plus a stable snake_case reason in
// Error::context. Transport bindings translate both into their own status
// vocabulary; no transport, RPC-framework or vendor type appears here.
//
// The numeric values of every enum only ever append, and names never change.

#pragma once

#include <cstdint>
#include <initializer_list>
#include <optional>
#include <string_view>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::serving {

/// Lifecycle state of a logical session. Stable snake_case names (in
/// parentheses) come from `to_string(LogicalSessionState)`.
enum class LogicalSessionState : std::uint8_t {
  /// (`opening`) Bound to its generation; capacity reservation and session
  /// initialization are in progress. Ready has not been sent.
  Opening = 0,
  /// (`active`) Ready was sent. Input within credit is accepted.
  Active = 1,
  /// (`finalizing`) The current utterance or the submitted synthesis is being
  /// completed. After a client Finalize, input is refused until completion;
  /// after an automatic endpoint, following input is accepted for the next
  /// utterance.
  Finalizing = 2,
  /// (`draining`) No new input is accepted. Accepted work completes and is
  /// delivered, then the backend is asked to release the session's resources;
  /// the session closes once it acknowledges.
  Draining = 3,
  /// (`cancel_requested`) Cancelled or terminated: output is suppressed and
  /// cleanup requested. Resources stay reserved until the backend acknowledges
  /// their physical release. Not terminal.
  CancelRequested = 4,
  /// (`closed`) Terminal. Physical release was acknowledged and the
  /// reservation returned before the terminal outcome was written.
  Closed = 5,
  /// (`failed`) Terminal. Output is suppressed and the terminal outcome
  /// written at once; the reservation stays held until physical release is
  /// acknowledged (see LogicalSessionMachine::holds_reservation()).
  Failed = 6,
};

/// Stable snake_case name of `state`; "failed" for a value that is not an
/// enumerator.
[[nodiscard]] std::string_view to_string(LogicalSessionState state) noexcept;

/// Inverse of to_string(LogicalSessionState): std::nullopt for any other
/// text, including differently cased or padded names.
[[nodiscard]] std::optional<LogicalSessionState> logical_session_state_from_string(
    std::string_view name) noexcept;

/// True for Closed and Failed: the session's single terminal outcome has been
/// written and no later event changes what the client sees.
[[nodiscard]] constexpr bool is_terminal(LogicalSessionState state) noexcept {
  return state == LogicalSessionState::Closed || state == LogicalSessionState::Failed;
}

/// Something that happened to a logical session. Open through StatusRequest
/// arrive on the client stream; the rest come from the session's owner:
/// admission, the processing pipeline and backend, timers, pressure policies
/// and worker control.
enum class LogicalSessionEvent : std::uint8_t {
  /// (`open`) An Open on a stream whose session already exists. Always refused.
  Open = 0,
  /// (`data`) Audio or a text segment within the session's input credit.
  Data = 1,
  /// (`finalize`) Client Finalize: complete the current utterance or the
  /// submitted synthesis.
  Finalize = 2,
  /// (`cancel`) Client Cancel of the whole session.
  Cancel = 3,
  /// (`half_close`) The client closed the sending half of its stream.
  HalfClose = 4,
  /// (`ping`) Client liveness probe.
  Ping = 5,
  /// (`status_request`) Client request for a LogicalSessionStatus.
  StatusRequest = 6,
  /// (`admitted`) Capacity was reserved and session state initialized.
  Admitted = 7,
  /// (`automatic_endpoint`) An utterance end was detected automatically in
  /// accepted audio (silence or duration limit). A client Finalize is never
  /// reported as this event.
  AutomaticEndpoint = 8,
  /// (`finalize_completed`) Every finalization the session owed has completed.
  FinalizeCompleted = 9,
  /// (`drain_completed`) Every item accepted before the drain began has
  /// completed and its output was delivered.
  DrainCompleted = 10,
  /// (`drain`) The worker asked the session to finish accepted work and
  /// close: its deployment generation is retiring or the worker is shutting
  /// down.
  Drain = 11,
  /// (`abort`) Worker-side termination through the cancel path: transport
  /// cancellation or disconnect, a timeout or deadline, output that stopped
  /// making delivery progress, resource pressure, or a retirement or shutdown
  /// deadline.
  Abort = 12,
  /// (`fail`) The session failed: a client protocol, credit, validation or
  /// generation error; an admission or initialization failure; a backend
  /// protocol failure; or an internal error. Ignored once cancellation or a
  /// terminal outcome has fixed the result.
  Fail = 13,
  /// (`backend_reset`) The backend process holding the session's state was
  /// reset or reaped. Unlike Fail, it also fails a session whose cancellation
  /// is waiting for release.
  BackendReset = 14,
  /// (`release_acknowledged`) The backend acknowledged physical release of
  /// everything it held for the session, or reaping of its process was
  /// confirmed.
  ReleaseAcknowledged = 15,
};

/// Stable snake_case name of `event`; "unknown" for a value that is not an
/// enumerator. Diagnostic only; never parsed.
[[nodiscard]] std::string_view to_string(LogicalSessionEvent event) noexcept;

/// An action the owner must carry out after an accepted event. When one event
/// yields several, perform them in ascending numeric order.
enum class LogicalSessionEffect : std::uint8_t {
  /// (`suppress_output`) Stop publishing task output and discard unsent task
  /// output now. Lifecycle messages are unaffected. At most once per session.
  SuppressOutput = 0,
  /// (`emit_cancel_accepted`) Send CancelAccepted. Only after Ready and at
  /// most once per session; a Cancel that arrives before Ready ends the
  /// session without an acknowledgement.
  EmitCancelAccepted = 1,
  /// (`request_cleanup`) Ask the backend to release everything it holds for
  /// the session, cancelling unfinished work, and apply ReleaseAcknowledged
  /// when it confirms (at once if it holds nothing). At most once per session.
  RequestCleanup = 2,
  /// (`emit_ready`) Send Ready. At most once per session.
  EmitReady = 3,
  /// (`accept_input`) Take ownership of the event's input as bounded work and
  /// acknowledge it.
  AcceptInput = 4,
  /// (`emit_reply`) Answer the Ping or StatusRequest.
  EmitReply = 5,
  /// (`start_finalize`) Complete one more utterance or the submitted
  /// synthesis; apply FinalizeCompleted once every finalization owed is done.
  StartFinalize = 6,
  /// (`start_drain`) Complete every accepted item and deliver its output, then
  /// apply DrainCompleted. At most once per session.
  StartDrain = 7,
  /// (`release_slot`) Return the session's ledger slot and reservations. At
  /// most once per session, only after RequestCleanup was acknowledged.
  ReleaseSlot = 8,
  /// (`emit_terminal`) Write the session's single terminal outcome if the
  /// transport is still usable. At most once per session.
  EmitTerminal = 9,
};

/// Stable snake_case name of `effect`; "unknown" for a value that is not an
/// enumerator. Diagnostic only; never parsed.
[[nodiscard]] std::string_view to_string(LogicalSessionEffect effect) noexcept;

/// The set of effects one accepted event produces. Empty when the event needs
/// no action, for example an idempotent repeat or a late report.
class LogicalSessionEffects {
 public:
  /// The empty set.
  constexpr LogicalSessionEffects() noexcept = default;

  /// The set holding `effects`; order and duplicates do not matter.
  constexpr LogicalSessionEffects(std::initializer_list<LogicalSessionEffect> effects) noexcept {
    for (const LogicalSessionEffect effect : effects) {
      bits_ |= bit(effect);
    }
  }

  /// True if `effect` is in the set.
  [[nodiscard]] constexpr bool contains(LogicalSessionEffect effect) const noexcept {
    return (bits_ & bit(effect)) != 0U;
  }

  /// True if the set holds no effect.
  [[nodiscard]] constexpr bool empty() const noexcept { return bits_ == 0U; }

  /// Calls `visit(effect)` for every effect in the set, in execution order.
  template <typename Visitor>
  constexpr void for_each(Visitor&& visit) const {
    for (std::uint32_t value = 0; value < kBitCount; ++value) {
      if ((bits_ & (std::uint32_t{1} << value)) != 0U) {
        visit(static_cast<LogicalSessionEffect>(value));
      }
    }
  }

  friend constexpr bool operator==(const LogicalSessionEffects& lhs,
                                   const LogicalSessionEffects& rhs) noexcept = default;

 private:
  static constexpr std::uint32_t kBitCount = 32;
  static constexpr std::uint32_t bit(LogicalSessionEffect effect) noexcept {
    return std::uint32_t{1} << static_cast<std::uint32_t>(effect);
  }
  std::uint32_t bits_ = 0;
};

/// Pure lifecycle state machine of one logical session.
///
/// A value type: copyable, comparable and holding no reference to anything
/// else, so a copy can try another event order. Not thread-safe; the owner
/// applies one session's events one at a time and performs each accepted
/// event's effects before applying the next.
///
/// Every machine returned by open() holds one reservation (the session's
/// ledger slot) until an accepted event produces ReleaseSlot. The owner takes
/// the slot before calling open() and returns it itself only if open() fails.
///
/// The owner keeps the session's end cause: the Error carried by the most
/// recent accepted event that moved state() from another state into Draining,
/// CancelRequested or Failed (none for a client half-close). The terminal
/// outcome reports that cause.
class LogicalSessionMachine {
 public:
  /// Binds a new session to the deployment generation this worker serves.
  ///
  /// @param serving_generation   generation this worker serves; never zero.
  /// @param requested_generation generation named by the client's Open.
  /// @return a machine in Opening, or
  ///   - Error::Code::ConfigInvalid with context "invalid_generation" if
  ///     serving_generation is zero;
  ///   - Error::Code::NotReady with context "stale_generation" if
  ///     requested_generation differs from serving_generation, zero (an absent
  ///     field) included.
  ///   After an error no session exists: nothing is reserved and no Ready or
  ///   terminal effect is owed; the caller ends the stream with the error.
  [[nodiscard]] static Result<LogicalSessionMachine> open(std::uint64_t serving_generation,
                                                          std::uint64_t requested_generation);

  /// Applies one event.
  ///
  /// @return the effects to carry out, possibly none (publish nothing and
  ///   release any payload the event carried), or Error::Code::NotReady with
  ///   context "illegal_transition" when the current state does not permit
  ///   the event. A refused event changes nothing. The owner then ends the
  ///   session by applying Fail with the refusal as its cause; Fail is ignored
  ///   once the outcome is fixed, so doing this is always safe.
  [[nodiscard]] Result<LogicalSessionEffects> apply(LogicalSessionEvent event);

  /// Checks the generation carried by a later client message or backend
  /// report. Does not depend on or change the state.
  ///
  /// @return success if `generation` equals generation(), else
  ///   Error::Code::NotReady with context "stale_generation". The owner
  ///   discards the message and applies Fail with this error as its cause.
  [[nodiscard]] Result<void> check_generation(std::uint64_t generation) const;

  /// Current lifecycle state.
  [[nodiscard]] LogicalSessionState state() const noexcept;

  /// Deployment generation this session is bound to. Never zero.
  [[nodiscard]] std::uint64_t generation() const noexcept { return generation_; }

  /// True until ReleaseSlot has been produced: in every state except Closed,
  /// and in Failed until physical release is acknowledged.
  [[nodiscard]] bool holds_reservation() const noexcept;

  /// Equal when generation and configuration match, including configuration
  /// that state() does not show.
  friend bool operator==(const LogicalSessionMachine& lhs,
                         const LogicalSessionMachine& rhs) noexcept = default;

 private:
  /// Configuration: the public state refined by what changes the events it
  /// accepts. The architecture docs render the transition table by these
  /// names.
  enum class Phase : std::uint8_t {
    Opening = 0,
    Active = 1,
    FinalizingAfterFinalize = 2,  ///< finalizing; input refused until completion
    FinalizingAfterEndpoint = 3,  ///< finalizing; following input accepted
    DrainingWorking = 4,          ///< draining; accepted work completing
    DrainingReleasing = 5,        ///< draining; release requested
    CancelRequested = 6,
    Closed = 7,
    FailedHolding = 8,  ///< failed; release not yet acknowledged
    FailedReleased = 9,
  };
  struct Rules;  ///< Transition rules; defined in the implementation.

  explicit LogicalSessionMachine(std::uint64_t generation) noexcept : generation_(generation) {}

  Phase phase_ = Phase::Opening;
  std::uint64_t generation_ = 0;
};

/// Use of one bounded session budget, in bytes. used() <= limit() always holds.
class LogicalSessionUsage {
 public:
  /// No use of a zero budget: a budget the session's mode does not use.
  constexpr LogicalSessionUsage() noexcept = default;

  /// @return the usage, or Error::Code::ConfigInvalid with context
  ///   "usage_exceeds_limit" if used > limit.
  [[nodiscard]] static Result<LogicalSessionUsage> create(std::uint64_t used, std::uint64_t limit);

  /// Bytes in use.
  [[nodiscard]] constexpr std::uint64_t used() const noexcept { return used_; }
  /// Size of the budget in bytes.
  [[nodiscard]] constexpr std::uint64_t limit() const noexcept { return limit_; }
  /// limit() - used(); never underflows.
  [[nodiscard]] constexpr std::uint64_t available() const noexcept { return limit_ - used_; }

  friend constexpr bool operator==(const LogicalSessionUsage& lhs,
                                   const LogicalSessionUsage& rhs) noexcept = default;

 private:
  constexpr LogicalSessionUsage(std::uint64_t used, std::uint64_t limit) noexcept
      : used_(used), limit_(limit) {}
  std::uint64_t used_ = 0;
  std::uint64_t limit_ = 0;
};

/// Bounded status of one logical session: the body of a status reply.
/// Fixed size; carries no text, identifier or caller metadata.
struct LogicalSessionStatus {
  /// Lifecycle state when the status was taken.
  LogicalSessionState state = LogicalSessionState::Opening;
  /// Accepted input items (audio chunks or text segments, including one being
  /// executed or delivered) whose ownership has not been released.
  std::uint32_t input_queue_depth = 0;
  /// Accepted, not yet consumed input bytes, as received, against the input
  /// credit.
  LogicalSessionUsage input_credit_bytes;
  /// Queued, unsent PCM output bytes against the output audio budget.
  LogicalSessionUsage output_pcm_bytes;
  /// Queued, unsent control and transcript metadata bytes against its budget.
  LogicalSessionUsage output_metadata_bytes;

  friend constexpr bool operator==(const LogicalSessionStatus& lhs,
                                   const LogicalSessionStatus& rhs) noexcept = default;
};

}  // namespace tensorplate::serving
