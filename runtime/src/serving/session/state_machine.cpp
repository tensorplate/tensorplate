// SPDX-License-Identifier: Apache-2.0
//
// Logical-session lifecycle: names, generation binding and the transition
// rules of LogicalSessionMachine.
//
// The rules are written as predicates over the machine's configuration, not as
// a copy of the transition table, so a mistake in one is not shared by the
// other; test/mocks/session_transition_fixtures.hpp holds the table the unit
// tests hold these rules to.

#include <cstdint>
#include <optional>
#include <string>
#include <string_view>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::serving {

namespace {

// No default in these switches: -Wswitch makes an enumerator appended without
// a name a build error. An empty result means the value is not an enumerator.
constexpr std::string_view name_of(LogicalSessionState state) noexcept {
  switch (state) {
    case LogicalSessionState::Opening:
      return "opening";
    case LogicalSessionState::Active:
      return "active";
    case LogicalSessionState::Finalizing:
      return "finalizing";
    case LogicalSessionState::Draining:
      return "draining";
    case LogicalSessionState::CancelRequested:
      return "cancel_requested";
    case LogicalSessionState::Closed:
      return "closed";
    case LogicalSessionState::Failed:
      return "failed";
  }
  return {};
}

constexpr std::string_view name_of(LogicalSessionEvent event) noexcept {
  switch (event) {
    case LogicalSessionEvent::Open:
      return "open";
    case LogicalSessionEvent::Data:
      return "data";
    case LogicalSessionEvent::Finalize:
      return "finalize";
    case LogicalSessionEvent::Cancel:
      return "cancel";
    case LogicalSessionEvent::HalfClose:
      return "half_close";
    case LogicalSessionEvent::Ping:
      return "ping";
    case LogicalSessionEvent::StatusRequest:
      return "status_request";
    case LogicalSessionEvent::Admitted:
      return "admitted";
    case LogicalSessionEvent::AutomaticEndpoint:
      return "automatic_endpoint";
    case LogicalSessionEvent::FinalizeCompleted:
      return "finalize_completed";
    case LogicalSessionEvent::DrainCompleted:
      return "drain_completed";
    case LogicalSessionEvent::Drain:
      return "drain";
    case LogicalSessionEvent::Abort:
      return "abort";
    case LogicalSessionEvent::Fail:
      return "fail";
    case LogicalSessionEvent::BackendReset:
      return "backend_reset";
    case LogicalSessionEvent::ReleaseAcknowledged:
      return "release_acknowledged";
  }
  return {};
}

constexpr std::string_view name_of(LogicalSessionEffect effect) noexcept {
  switch (effect) {
    case LogicalSessionEffect::SuppressOutput:
      return "suppress_output";
    case LogicalSessionEffect::EmitCancelAccepted:
      return "emit_cancel_accepted";
    case LogicalSessionEffect::RequestCleanup:
      return "request_cleanup";
    case LogicalSessionEffect::EmitReady:
      return "emit_ready";
    case LogicalSessionEffect::AcceptInput:
      return "accept_input";
    case LogicalSessionEffect::EmitReply:
      return "emit_reply";
    case LogicalSessionEffect::StartFinalize:
      return "start_finalize";
    case LogicalSessionEffect::StartDrain:
      return "start_drain";
    case LogicalSessionEffect::ReleaseSlot:
      return "release_slot";
    case LogicalSessionEffect::EmitTerminal:
      return "emit_terminal";
  }
  return {};
}

constexpr std::string_view kIllegalTransition = "illegal_transition";
constexpr std::string_view kStaleGeneration = "stale_generation";
constexpr std::string_view kInvalidGeneration = "invalid_generation";
constexpr std::string_view kUsageExceedsLimit = "usage_exceeds_limit";

Error stale_generation(std::uint64_t expected, std::uint64_t observed) {
  return Error::make(Error::Code::NotReady,
                     "logical session is bound to deployment generation " +
                         std::to_string(expected) + ", not " + std::to_string(observed),
                     std::string{kStaleGeneration});
}

}  // namespace

std::string_view to_string(LogicalSessionState state) noexcept {
  const std::string_view name = name_of(state);
  return name.empty() ? std::string_view{"failed"} : name;
}

std::optional<LogicalSessionState> logical_session_state_from_string(
    std::string_view name) noexcept {
  if (name.empty()) {
    return std::nullopt;
  }
  // States are dense from 0 (values only ever append), so the first value
  // without a name ends the search.
  for (std::uint8_t value = 0;; ++value) {
    const auto state = static_cast<LogicalSessionState>(value);
    const std::string_view candidate = name_of(state);
    if (candidate.empty()) {
      return std::nullopt;
    }
    if (candidate == name) {
      return state;
    }
  }
}

std::string_view to_string(LogicalSessionEvent event) noexcept {
  const std::string_view name = name_of(event);
  return name.empty() ? std::string_view{"unknown"} : name;
}

std::string_view to_string(LogicalSessionEffect effect) noexcept {
  const std::string_view name = name_of(effect);
  return name.empty() ? std::string_view{"unknown"} : name;
}

/// The transition rules, grouped by what produced the event.
struct LogicalSessionMachine::Rules {
  using Event = LogicalSessionEvent;
  using Effect = LogicalSessionEffect;

  struct Step {
    Phase to;
    LogicalSessionEffects effects;
  };
  using Next = std::optional<Step>;

  static constexpr Step stay(Phase phase, LogicalSessionEffects effects = {}) noexcept {
    return Step{phase, effects};
  }

  /// Cancellation or a terminal outcome has fixed the result.
  static constexpr bool outcome_fixed(Phase phase) noexcept {
    return phase == Phase::CancelRequested || phase == Phase::Closed ||
           phase == Phase::FailedHolding || phase == Phase::FailedReleased;
  }
  static constexpr bool finalizing(Phase phase) noexcept {
    return phase == Phase::FinalizingAfterFinalize || phase == Phase::FinalizingAfterEndpoint;
  }
  static constexpr bool draining(Phase phase) noexcept {
    return phase == Phase::DrainingWorking || phase == Phase::DrainingReleasing;
  }
  /// Ready was sent and the outcome is still open.
  static constexpr bool serving(Phase phase) noexcept {
    return phase == Phase::Active || finalizing(phase) || draining(phase);
  }
  static constexpr bool accepts_input(Phase phase) noexcept {
    return phase == Phase::Active || phase == Phase::FinalizingAfterEndpoint;
  }
  /// Cleanup was requested while the outcome is still open.
  static constexpr bool cleanup_requested(Phase phase) noexcept {
    return phase == Phase::DrainingReleasing;
  }

  static Next next(Phase phase, Event event) noexcept {
    switch (event) {
      case Event::Open:
      case Event::Data:
      case Event::Finalize:
      case Event::Ping:
      case Event::StatusRequest:
        return on_client_request(phase, event);
      case Event::HalfClose:
      case Event::Drain:
        return on_drain_request(phase, event);
      case Event::Admitted:
      case Event::AutomaticEndpoint:
      case Event::FinalizeCompleted:
      case Event::DrainCompleted:
        return on_progress(phase, event);
      case Event::Cancel:
      case Event::Abort:
      case Event::Fail:
      case Event::BackendReset:
        return on_termination(phase, event);
      case Event::ReleaseAcknowledged:
        return on_release(phase);
    }
    return std::nullopt;
  }

  /// Open, Data, Finalize, Ping and StatusRequest from the client.
  static Next on_client_request(Phase phase, Event event) noexcept {
    // A second Open, and anything before Ready or after the outcome is fixed,
    // is a protocol violation.
    if (event == Event::Open || !serving(phase)) {
      return std::nullopt;
    }
    if (event == Event::Ping || event == Event::StatusRequest) {
      return stay(phase, {Effect::EmitReply});
    }
    if (draining(phase)) {
      // Input racing a drain is not taken; accepted work still completes.
      return stay(phase);
    }
    if (event == Event::Data) {
      if (!accepts_input(phase)) {
        return std::nullopt;  // after a client Finalize, until completion
      }
      return stay(phase, {Effect::AcceptInput});
    }
    // Finalize: a repeat while a client finalization is running changes
    // nothing; after an automatic endpoint it takes over the finalization.
    if (phase == Phase::FinalizingAfterFinalize) {
      return stay(phase);
    }
    return Step{Phase::FinalizingAfterFinalize, {Effect::StartFinalize}};
  }

  /// HalfClose from the client, Drain from the worker.
  static Next on_drain_request(Phase phase, Event event) noexcept {
    if (phase == Phase::Opening) {
      // No session exists for the client yet: a half-close is a protocol
      // violation, and a drain cancels.
      if (event == Event::HalfClose) {
        return std::nullopt;
      }
      return Step{Phase::CancelRequested, {Effect::SuppressOutput, Effect::RequestCleanup}};
    }
    if (draining(phase) || outcome_fixed(phase)) {
      return stay(phase);  // the first drain wins; nothing changes after the outcome
    }
    return Step{Phase::DrainingWorking, {Effect::StartDrain}};
  }

  /// Admitted, AutomaticEndpoint, FinalizeCompleted and DrainCompleted from
  /// admission and the processing pipeline.
  static Next on_progress(Phase phase, Event event) noexcept {
    if (outcome_fixed(phase)) {
      return stay(phase);  // a late report
    }
    switch (event) {
      case Event::Admitted:
        if (phase != Phase::Opening) {
          return std::nullopt;
        }
        return Step{Phase::Active, {Effect::EmitReady}};
      case Event::AutomaticEndpoint:
        if (phase == Phase::Active) {
          return Step{Phase::FinalizingAfterEndpoint, {Effect::StartFinalize}};
        }
        if (finalizing(phase) || phase == Phase::DrainingWorking) {
          return stay(phase, {Effect::StartFinalize});
        }
        return std::nullopt;
      case Event::FinalizeCompleted:
        if (finalizing(phase)) {
          return Step{Phase::Active, {}};
        }
        if (phase == Phase::DrainingWorking) {
          return stay(phase);
        }
        return std::nullopt;
      case Event::DrainCompleted:
        if (phase != Phase::DrainingWorking) {
          return std::nullopt;
        }
        return Step{Phase::DrainingReleasing, {Effect::RequestCleanup}};
      default:
        return std::nullopt;
    }
  }

  /// Cancel from the client; Abort, Fail and BackendReset from the owner.
  static Next on_termination(Phase phase, Event event) noexcept {
    if (phase == Phase::CancelRequested && event == Event::BackendReset) {
      return Step{Phase::FailedHolding, {Effect::EmitTerminal}};
    }
    if (outcome_fixed(phase)) {
      return stay(phase);
    }
    const bool cleaned = cleanup_requested(phase);
    if (event == Event::Fail || event == Event::BackendReset) {
      return Step{Phase::FailedHolding,
                  cleaned ? LogicalSessionEffects{Effect::SuppressOutput, Effect::EmitTerminal}
                          : LogicalSessionEffects{Effect::SuppressOutput, Effect::RequestCleanup,
                                                  Effect::EmitTerminal}};
    }
    // Cancel and Abort: CancelAccepted answers only a client Cancel after Ready.
    const bool acknowledge = event == Event::Cancel && serving(phase);
    if (acknowledge) {
      return Step{Phase::CancelRequested,
                  cleaned
                      ? LogicalSessionEffects{Effect::SuppressOutput, Effect::EmitCancelAccepted}
                      : LogicalSessionEffects{Effect::SuppressOutput, Effect::EmitCancelAccepted,
                                              Effect::RequestCleanup}};
    }
    return Step{Phase::CancelRequested,
                cleaned ? LogicalSessionEffects{Effect::SuppressOutput}
                        : LogicalSessionEffects{Effect::SuppressOutput, Effect::RequestCleanup}};
  }

  /// ReleaseAcknowledged from the backend.
  static Next on_release(Phase phase) noexcept {
    switch (phase) {
      case Phase::DrainingReleasing:
      case Phase::CancelRequested:
        return Step{Phase::Closed, {Effect::ReleaseSlot, Effect::EmitTerminal}};
      case Phase::FailedHolding:
        return Step{Phase::FailedReleased, {Effect::ReleaseSlot}};
      case Phase::Closed:
      case Phase::FailedReleased:
        return stay(phase);  // a duplicate acknowledgement
      case Phase::Opening:
      case Phase::Active:
      case Phase::FinalizingAfterFinalize:
      case Phase::FinalizingAfterEndpoint:
      case Phase::DrainingWorking:
        return std::nullopt;  // nothing was asked to be released
    }
    return std::nullopt;
  }
};

Result<LogicalSessionMachine> LogicalSessionMachine::open(std::uint64_t serving_generation,
                                                          std::uint64_t requested_generation) {
  if (serving_generation == 0) {
    return unexpected(Error::make(Error::Code::ConfigInvalid,
                                  "logical session opened on a worker without a deployment "
                                  "generation",
                                  std::string{kInvalidGeneration}));
  }
  if (requested_generation != serving_generation) {
    return unexpected(stale_generation(serving_generation, requested_generation));
  }
  return LogicalSessionMachine{serving_generation};
}

Result<LogicalSessionEffects> LogicalSessionMachine::apply(LogicalSessionEvent event) {
  const Rules::Next step = Rules::next(phase_, event);
  if (!step.has_value()) {
    return unexpected(Error::make(Error::Code::NotReady,
                                  "logical session in state '" + std::string{to_string(state())} +
                                      "' refuses event '" + std::string{to_string(event)} + "'",
                                  std::string{kIllegalTransition}));
  }
  phase_ = step->to;
  return step->effects;
}

Result<void> LogicalSessionMachine::check_generation(std::uint64_t generation) const {
  if (generation != generation_) {
    return unexpected(stale_generation(generation_, generation));
  }
  return {};
}

LogicalSessionState LogicalSessionMachine::state() const noexcept {
  switch (phase_) {
    case Phase::Opening:
      return LogicalSessionState::Opening;
    case Phase::Active:
      return LogicalSessionState::Active;
    case Phase::FinalizingAfterFinalize:
    case Phase::FinalizingAfterEndpoint:
      return LogicalSessionState::Finalizing;
    case Phase::DrainingWorking:
    case Phase::DrainingReleasing:
      return LogicalSessionState::Draining;
    case Phase::CancelRequested:
      return LogicalSessionState::CancelRequested;
    case Phase::Closed:
      return LogicalSessionState::Closed;
    case Phase::FailedHolding:
    case Phase::FailedReleased:
      return LogicalSessionState::Failed;
  }
  return LogicalSessionState::Failed;
}

bool LogicalSessionMachine::holds_reservation() const noexcept {
  switch (phase_) {
    case Phase::Closed:
    case Phase::FailedReleased:
      return false;
    case Phase::Opening:
    case Phase::Active:
    case Phase::FinalizingAfterFinalize:
    case Phase::FinalizingAfterEndpoint:
    case Phase::DrainingWorking:
    case Phase::DrainingReleasing:
    case Phase::CancelRequested:
    case Phase::FailedHolding:
      return true;
  }
  return true;
}

Result<LogicalSessionUsage> LogicalSessionUsage::create(std::uint64_t used, std::uint64_t limit) {
  if (used > limit) {
    return unexpected(Error::make(Error::Code::ConfigInvalid,
                                  "session budget use " + std::to_string(used) +
                                      " exceeds its limit " + std::to_string(limit),
                                  std::string{kUsageExceedsLimit}));
  }
  return LogicalSessionUsage{used, limit};
}

}  // namespace tensorplate::serving
