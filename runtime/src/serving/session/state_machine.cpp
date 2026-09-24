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

/// The transition rules land with the machine; until then nothing is accepted.
struct LogicalSessionMachine::Rules {
  static void next() noexcept {}
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
  // Placeholder until the transition rules land: every event is refused.
  (void)Rules::next;
  return unexpected(Error::make(Error::Code::NotReady,
                                "logical session in state '" + std::string{to_string(state())} +
                                    "' refuses event '" + std::string{to_string(event)} + "'",
                                std::string{kIllegalTransition}));
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
