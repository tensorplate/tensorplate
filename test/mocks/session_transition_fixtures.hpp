// SPDX-License-Identifier: Apache-2.0
//
// Normative transition table of tensorplate::serving::LogicalSessionMachine.
//
// Every (configuration, event) cell is spelled out: the configuration and the
// exact effect set an accepted event must produce, or X for a refusal that
// leaves the machine unchanged. A refused client event is a protocol
// violation (Error::Code::NotReady, context "illegal_transition"); a refused
// owner report is a defect (Error::Code::Internal, "unexpected_report").
// A configuration is a public state refined by what changes the events it
// accepts; each is reached by replaying its path on
// LogicalSessionMachine::open(kGeneration, kGeneration).
//
// The machine follows this table: change the table first, then the machine.
// The runtime never includes this file.

#pragma once

#include <array>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <optional>
#include <ostream>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/serving/session.hpp"

namespace tensorplate::testing::session_transitions {

using serving::LogicalSessionEffects;
using serving::LogicalSessionMachine;
using St = serving::LogicalSessionState;
using Ev = serving::LogicalSessionEvent;
using Fx = serving::LogicalSessionEffect;

inline constexpr std::uint64_t kGeneration = 7;
inline constexpr std::uint64_t kMaxGeneration = std::numeric_limits<std::uint64_t>::max();
inline constexpr std::string_view kStaleReason = "stale_generation";
inline constexpr std::string_view kInvalidGenerationReason = "invalid_generation";
inline constexpr std::size_t kConfigCount = 10;
inline constexpr std::size_t kEventCount = 16;
inline constexpr std::size_t kLegalCellCount = 108;
inline constexpr std::size_t kRefusedCellCount = 52;

/// The machine's configurations, in the order of kConfigs and kTable rows.
enum class Config : std::uint8_t {
  Opening,
  Active,
  FinalizingAfterFinalize,
  FinalizingAfterEndpoint,
  DrainingWorking,
  DrainingReleasing,
  CancelRequested,
  Closed,
  FailedHolding,
  FailedReleased,
};

struct ConfigSpec {
  Config config;
  std::string_view name;
  /// Events applied in order after open(kGeneration, kGeneration).
  std::vector<Ev> path;
  St state;
  bool holds_reservation;
  /// Cancel, Abort and Fail no longer change anything.
  bool outcome_fixed;
};

inline const std::array<ConfigSpec, kConfigCount> kConfigs{{
    {Config::Opening, "opening", {}, St::Opening, true, false},
    {Config::Active, "active", {Ev::Admitted}, St::Active, true, false},
    {Config::FinalizingAfterFinalize,
     "finalizing_after_finalize",
     {Ev::Admitted, Ev::Finalize},
     St::Finalizing,
     true,
     false},
    {Config::FinalizingAfterEndpoint,
     "finalizing_after_endpoint",
     {Ev::Admitted, Ev::AutomaticEndpoint},
     St::Finalizing,
     true,
     false},
    {Config::DrainingWorking,
     "draining_working",
     {Ev::Admitted, Ev::HalfClose},
     St::Draining,
     true,
     false},
    {Config::DrainingReleasing,
     "draining_releasing",
     {Ev::Admitted, Ev::HalfClose, Ev::DrainCompleted},
     St::Draining,
     true,
     false},
    {Config::CancelRequested,
     "cancel_requested",
     {Ev::Admitted, Ev::Cancel},
     St::CancelRequested,
     true,
     true},
    {Config::Closed,
     "closed",
     {Ev::Admitted, Ev::Cancel, Ev::ReleaseAcknowledged},
     St::Closed,
     false,
     true},
    {Config::FailedHolding, "failed_holding", {Ev::Admitted, Ev::Fail}, St::Failed, true, true},
    {Config::FailedReleased,
     "failed_released",
     {Ev::Admitted, Ev::Fail, Ev::ReleaseAcknowledged},
     St::Failed,
     false,
     true},
}};

/// The error a refused event carries, by who sends the event.
struct Refusal {
  Error::Code code;
  std::string_view reason;
};
inline constexpr Refusal kClientRefusal{Error::Code::NotReady, "illegal_transition"};
inline constexpr Refusal kOwnerRefusal{Error::Code::Internal, "unexpected_report"};

/// Events that arrive on the client stream; every other event is an owner
/// report.
inline constexpr std::array<Ev, 7> kClientEvents{
    Ev::Open, Ev::Data, Ev::Finalize, Ev::Cancel, Ev::HalfClose, Ev::Ping, Ev::StatusRequest};

constexpr Refusal refusal(Ev event) noexcept {
  for (const Ev client : kClientEvents) {
    if (client == event) {
      return kClientRefusal;
    }
  }
  return kOwnerRefusal;
}

/// Column order of kTable.
inline constexpr std::array<Ev, kEventCount> kEvents{Ev::Open,
                                                     Ev::Data,
                                                     Ev::Finalize,
                                                     Ev::Cancel,
                                                     Ev::HalfClose,
                                                     Ev::Ping,
                                                     Ev::StatusRequest,
                                                     Ev::Admitted,
                                                     Ev::AutomaticEndpoint,
                                                     Ev::FinalizeCompleted,
                                                     Ev::DrainCompleted,
                                                     Ev::Drain,
                                                     Ev::Abort,
                                                     Ev::Fail,
                                                     Ev::BackendReset,
                                                     Ev::ReleaseAcknowledged};

/// No default: an event appended without a column is a -Wswitch build error.
constexpr std::size_t column(Ev event) noexcept {
  switch (event) {
    case Ev::Open:
      return 0;
    case Ev::Data:
      return 1;
    case Ev::Finalize:
      return 2;
    case Ev::Cancel:
      return 3;
    case Ev::HalfClose:
      return 4;
    case Ev::Ping:
      return 5;
    case Ev::StatusRequest:
      return 6;
    case Ev::Admitted:
      return 7;
    case Ev::AutomaticEndpoint:
      return 8;
    case Ev::FinalizeCompleted:
      return 9;
    case Ev::DrainCompleted:
      return 10;
    case Ev::Drain:
      return 11;
    case Ev::Abort:
      return 12;
    case Ev::Fail:
      return 13;
    case Ev::BackendReset:
      return 14;
    case Ev::ReleaseAcknowledged:
      return 15;
  }
  return kEventCount;
}

constexpr std::size_t row(Config config) noexcept {
  return static_cast<std::size_t>(config);
}

struct Cell {
  bool legal = false;
  Config to = Config::Opening;
  LogicalSessionEffects effects{};
};

/// A refused cell.
inline constexpr Cell X{};

/// An accepted cell moving to (or staying in) `to` with `effects`.
constexpr Cell go(Config to, LogicalSessionEffects effects = {}) noexcept {
  return Cell{true, to, effects};
}

inline constexpr LogicalSessionEffects kReady{Fx::EmitReady};
inline constexpr LogicalSessionEffects kInput{Fx::AcceptInput};
inline constexpr LogicalSessionEffects kReply{Fx::EmitReply};
inline constexpr LogicalSessionEffects kFinalize{Fx::StartFinalize};
inline constexpr LogicalSessionEffects kDrain{Fx::StartDrain};
inline constexpr LogicalSessionEffects kCleanup{Fx::RequestCleanup};
inline constexpr LogicalSessionEffects kCancelQuiet{Fx::SuppressOutput, Fx::RequestCleanup};
inline constexpr LogicalSessionEffects kCancelAcknowledged{
    Fx::SuppressOutput, Fx::EmitCancelAccepted, Fx::RequestCleanup};
inline constexpr LogicalSessionEffects kCancelAcknowledgedAfterCleanup{Fx::SuppressOutput,
                                                                       Fx::EmitCancelAccepted};
inline constexpr LogicalSessionEffects kSuppress{Fx::SuppressOutput};
inline constexpr LogicalSessionEffects kFailure{Fx::SuppressOutput, Fx::RequestCleanup,
                                                Fx::EmitTerminal};
inline constexpr LogicalSessionEffects kFailureAfterCleanup{Fx::SuppressOutput, Fx::EmitTerminal};
inline constexpr LogicalSessionEffects kTerminal{Fx::EmitTerminal};
inline constexpr LogicalSessionEffects kClose{Fx::ReleaseSlot, Fx::EmitTerminal};
inline constexpr LogicalSessionEffects kRelease{Fx::ReleaseSlot};

using C = Config;

// clang-format off
// Columns: open, data, finalize, cancel, half_close, ping, status_request, admitted,
//          automatic_endpoint, finalize_completed, drain_completed, drain, abort, fail,
//          backend_reset, release_acknowledged
inline constexpr std::array<std::array<Cell, kEventCount>, kConfigCount> kTable{{
    /* opening */
    {{X, X, X, go(C::CancelRequested, kCancelQuiet), X, X, X, go(C::Active, kReady), X, X, X,
      go(C::CancelRequested, kCancelQuiet), go(C::CancelRequested, kCancelQuiet),
      go(C::FailedHolding, kFailure), go(C::FailedHolding, kFailure), X}},
    /* active */
    {{X, go(C::Active, kInput), go(C::FinalizingAfterFinalize, kFinalize),
      go(C::CancelRequested, kCancelAcknowledged), go(C::DrainingWorking, kDrain),
      go(C::Active, kReply), go(C::Active, kReply), X, go(C::FinalizingAfterEndpoint, kFinalize), X,
      X, go(C::DrainingWorking, kDrain), go(C::CancelRequested, kCancelQuiet),
      go(C::FailedHolding, kFailure), go(C::FailedHolding, kFailure), X}},
    /* finalizing_after_finalize */
    {{X, X, go(C::FinalizingAfterFinalize), go(C::CancelRequested, kCancelAcknowledged),
      go(C::DrainingWorking, kDrain), go(C::FinalizingAfterFinalize, kReply),
      go(C::FinalizingAfterFinalize, kReply), X, go(C::FinalizingAfterFinalize, kFinalize),
      go(C::Active), X, go(C::DrainingWorking, kDrain), go(C::CancelRequested, kCancelQuiet),
      go(C::FailedHolding, kFailure), go(C::FailedHolding, kFailure), X}},
    /* finalizing_after_endpoint */
    {{X, go(C::FinalizingAfterEndpoint, kInput), go(C::FinalizingAfterFinalize, kFinalize),
      go(C::CancelRequested, kCancelAcknowledged), go(C::DrainingWorking, kDrain),
      go(C::FinalizingAfterEndpoint, kReply), go(C::FinalizingAfterEndpoint, kReply), X,
      go(C::FinalizingAfterEndpoint, kFinalize), go(C::Active), X, go(C::DrainingWorking, kDrain),
      go(C::CancelRequested, kCancelQuiet), go(C::FailedHolding, kFailure),
      go(C::FailedHolding, kFailure), X}},
    /* draining_working */
    {{X, go(C::DrainingWorking), go(C::DrainingWorking), go(C::CancelRequested, kCancelAcknowledged),
      go(C::DrainingWorking), go(C::DrainingWorking, kReply), go(C::DrainingWorking, kReply), X,
      go(C::DrainingWorking, kFinalize), go(C::DrainingWorking), go(C::DrainingReleasing, kCleanup),
      go(C::DrainingWorking), go(C::CancelRequested, kCancelQuiet),
      go(C::FailedHolding, kFailure), go(C::FailedHolding, kFailure), X}},
    /* draining_releasing */
    {{X, go(C::DrainingReleasing), go(C::DrainingReleasing),
      go(C::CancelRequested, kCancelAcknowledgedAfterCleanup), go(C::DrainingReleasing),
      go(C::DrainingReleasing, kReply), go(C::DrainingReleasing, kReply), X, X, X, X,
      go(C::DrainingReleasing), go(C::CancelRequested, kSuppress),
      go(C::FailedHolding, kFailureAfterCleanup), go(C::FailedHolding, kFailureAfterCleanup),
      go(C::Closed, kClose)}},
    /* cancel_requested */
    {{X, X, X, go(C::CancelRequested), go(C::CancelRequested), X, X, go(C::CancelRequested),
      go(C::CancelRequested), go(C::CancelRequested), go(C::CancelRequested),
      go(C::CancelRequested), go(C::CancelRequested), go(C::CancelRequested),
      go(C::FailedHolding, kTerminal), go(C::Closed, kClose)}},
    /* closed */
    {{X, X, X, go(C::Closed), go(C::Closed), X, X, go(C::Closed), go(C::Closed), go(C::Closed),
      go(C::Closed), go(C::Closed), go(C::Closed), go(C::Closed), go(C::Closed), go(C::Closed)}},
    /* failed_holding */
    {{X, X, X, go(C::FailedHolding), go(C::FailedHolding), X, X, go(C::FailedHolding),
      go(C::FailedHolding), go(C::FailedHolding), go(C::FailedHolding), go(C::FailedHolding),
      go(C::FailedHolding), go(C::FailedHolding), go(C::FailedHolding),
      go(C::FailedReleased, kRelease)}},
    /* failed_released */
    {{X, X, X, go(C::FailedReleased), go(C::FailedReleased), X, X, go(C::FailedReleased),
      go(C::FailedReleased), go(C::FailedReleased), go(C::FailedReleased),
      go(C::FailedReleased), go(C::FailedReleased), go(C::FailedReleased),
      go(C::FailedReleased), go(C::FailedReleased)}},
}};
// clang-format on

/// Cell for (`config`, `event`).
inline const Cell& cell(Config config, Ev event) {
  return kTable.at(row(config)).at(column(event));
}

/// Generation binding at Open: serving generation, requested generation, the
/// expected error code (nullopt for success) and reason.
struct OpenCase {
  std::uint64_t serving;
  std::uint64_t requested;
  std::optional<Error::Code> code;
  std::string_view reason;
};

inline constexpr std::array<OpenCase, 8> kOpenCases{{
    {kGeneration, kGeneration, std::nullopt, ""},
    {kMaxGeneration, kMaxGeneration, std::nullopt, ""},
    // A retired generation.
    {kGeneration, kGeneration - 1, Error::Code::NotReady, kStaleReason},
    // Routed to the wrong worker.
    {kGeneration, kGeneration + 1, Error::Code::NotReady, kStaleReason},
    // The field was absent from the Open.
    {kGeneration, 0, Error::Code::NotReady, kStaleReason},
    {kGeneration, kMaxGeneration, Error::Code::NotReady, kStaleReason},
    // A worker without a generation.
    {0, 0, Error::Code::ConfigInvalid, kInvalidGenerationReason},
    // The serving generation is checked before the mismatch.
    {0, kGeneration, Error::Code::ConfigInvalid, kInvalidGenerationReason},
}};

/// Generations carried by later messages and reports, and whether they match.
struct GenerationCase {
  std::uint64_t observed;
  bool matches;
};

inline constexpr std::array<GenerationCase, 5> kGenerationCases{{
    {kGeneration, true},
    {kGeneration - 1, false},
    {kGeneration + 1, false},
    {0, false},
    {kMaxGeneration, false},
}};

/// The configuration's spec.
inline const ConfigSpec& spec(Config config) {
  return kConfigs.at(row(config));
}

/// A machine in `config`, reached by replaying its path. Every step of a path
/// is expected to be accepted; the fixture test checks that it is.
inline LogicalSessionMachine reference(Config config) {
  auto opened = LogicalSessionMachine::open(kGeneration, kGeneration);
  LogicalSessionMachine machine = std::move(opened).value();
  for (const Ev event : spec(config).path) {
    (void)machine.apply(event);
  }
  return machine;
}

/// The configuration `machine` equals, if any.
inline std::optional<Config> identify(const LogicalSessionMachine& machine) {
  for (const ConfigSpec& candidate : kConfigs) {
    LogicalSessionMachine probe = reference(candidate.config);
    if (probe.generation() == machine.generation() && probe == machine) {
      return candidate.config;
    }
  }
  return std::nullopt;
}

}  // namespace tensorplate::testing::session_transitions

namespace tensorplate::serving {

/// gtest diagnostics: "suppress_output|request_cleanup", or "none".
inline void PrintTo(const LogicalSessionEffects& effects, std::ostream* os) {
  bool first = true;
  effects.for_each([&](LogicalSessionEffect effect) {
    *os << (first ? "" : "|") << to_string(effect);
    first = false;
  });
  if (first) {
    *os << "none";
  }
}

/// gtest diagnostics: a machine prints as its configuration and generation.
inline void PrintTo(const LogicalSessionMachine& machine, std::ostream* os) {
  const auto config = testing::session_transitions::identify(machine);
  *os << (config ? testing::session_transitions::spec(*config).name : "unlisted") << "@"
      << machine.generation() << " (" << to_string(machine.state()) << ")";
}

}  // namespace tensorplate::serving
