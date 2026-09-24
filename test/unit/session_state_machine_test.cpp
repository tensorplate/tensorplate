// SPDX-License-Identifier: Apache-2.0
//
// Logical-session lifecycle: the machine against the normative transition
// table in test/mocks/session_transition_fixtures.hpp, plus invariants that do
// not come from the table.

#include <gtest/gtest.h>

#include <cstddef>
#include <cstdint>
#include <deque>
#include <initializer_list>
#include <limits>
#include <optional>
#include <regex>
#include <set>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/serving/session.hpp"

#include "session_transition_fixtures.hpp"

namespace {

using tensorplate::Error;
using tensorplate::serving::is_terminal;
using tensorplate::serving::logical_session_state_from_string;
using tensorplate::serving::LogicalSessionEffects;
using tensorplate::serving::LogicalSessionMachine;
using tensorplate::serving::LogicalSessionStatus;
using tensorplate::serving::LogicalSessionUsage;
using tensorplate::serving::to_string;
namespace fx = tensorplate::testing::session_transitions;
using fx::Config;
using fx::Ev;
using fx::Fx;
using fx::St;

LogicalSessionMachine opened() {
  return LogicalSessionMachine::open(fx::kGeneration, fx::kGeneration).value();
}

/// Applies `events` in order, requiring each to be accepted, and returns the
/// effects of each.
std::vector<LogicalSessionEffects> run(LogicalSessionMachine& machine,
                                       std::initializer_list<Ev> events) {
  std::vector<LogicalSessionEffects> out;
  for (const Ev event : events) {
    auto result = machine.apply(event);
    EXPECT_TRUE(result.has_value())
        << "refused " << to_string(event) << " in " << to_string(machine.state());
    out.push_back(result.has_value() ? result.value() : LogicalSessionEffects{});
  }
  return out;
}

void expect_refused(LogicalSessionMachine& machine, Ev event) {
  const LogicalSessionMachine before = machine;
  auto result = machine.apply(event);
  ASSERT_FALSE(result.has_value()) << "accepted " << to_string(event);
  EXPECT_EQ(result.error().code, fx::kRefusalCode);
  EXPECT_EQ(result.error().context, std::string{fx::kRefusalReason});
  EXPECT_EQ(machine, before);
}

// --- The table ------------------------------------------------------------

TEST(LogicalSessionMachine, FixtureIsWellFormed) {
  std::set<std::string_view> names;
  for (std::size_t i = 0; i < fx::kConfigs.size(); ++i) {
    EXPECT_EQ(fx::row(fx::kConfigs.at(i).config), i) << fx::kConfigs.at(i).name;
    names.insert(fx::kConfigs.at(i).name);
  }
  EXPECT_EQ(names.size(), fx::kConfigCount);
  for (std::size_t i = 0; i < fx::kEvents.size(); ++i) {
    EXPECT_EQ(fx::column(fx::kEvents.at(i)), i) << to_string(fx::kEvents.at(i));
  }
  std::size_t legal = 0;
  std::size_t refused = 0;
  for (const auto& cells : fx::kTable) {
    for (const auto& cell : cells) {
      (cell.legal ? legal : refused) += 1;
    }
  }
  EXPECT_EQ(legal, fx::kLegalCellCount);
  EXPECT_EQ(refused, fx::kRefusedCellCount);
  EXPECT_EQ(legal + refused, fx::kConfigCount * fx::kEventCount);
}

TEST(LogicalSessionMachine, ConfigurationsAreReachableAndDistinct) {
  std::vector<LogicalSessionMachine> machines;
  for (const fx::ConfigSpec& spec : fx::kConfigs) {
    SCOPED_TRACE(std::string{spec.name});
    LogicalSessionMachine machine = opened();
    for (const Ev event : spec.path) {
      ASSERT_TRUE(machine.apply(event).has_value()) << "path step " << to_string(event);
    }
    EXPECT_EQ(machine.state(), spec.state);
    EXPECT_EQ(machine.holds_reservation(), spec.holds_reservation);
    for (const Ev termination : {Ev::Cancel, Ev::Abort, Ev::Fail}) {
      LogicalSessionMachine probe = machine;
      auto result = probe.apply(termination);
      ASSERT_TRUE(result.has_value()) << to_string(termination);
      const bool ignored = result.value().empty() && probe == machine;
      EXPECT_EQ(ignored, spec.outcome_fixed) << to_string(termination);
    }
    machines.push_back(machine);
  }
  for (std::size_t i = 0; i < machines.size(); ++i) {
    for (std::size_t j = i + 1; j < machines.size(); ++j) {
      EXPECT_NE(machines.at(i), machines.at(j))
          << fx::kConfigs.at(i).name << " and " << fx::kConfigs.at(j).name << " coincide";
    }
  }
}

TEST(LogicalSessionMachine, EveryLegalCellMatches) {
  for (const fx::ConfigSpec& spec : fx::kConfigs) {
    for (const Ev event : fx::kEvents) {
      const fx::Cell& cell = fx::cell(spec.config, event);
      if (!cell.legal) {
        continue;
      }
      SCOPED_TRACE(std::string{spec.name} + " + " + std::string{to_string(event)});
      LogicalSessionMachine machine = fx::reference(spec.config);
      auto result = machine.apply(event);
      ASSERT_TRUE(result.has_value()) << result.error().message;
      EXPECT_EQ(result.value(), cell.effects);
      EXPECT_EQ(machine, fx::reference(cell.to));
    }
  }
}

TEST(LogicalSessionMachine, EveryRefusedCellIsTypedAndInert) {
  for (const fx::ConfigSpec& spec : fx::kConfigs) {
    for (const Ev event : fx::kEvents) {
      if (fx::cell(spec.config, event).legal) {
        continue;
      }
      SCOPED_TRACE(std::string{spec.name} + " + " + std::string{to_string(event)});
      LogicalSessionMachine machine = fx::reference(spec.config);
      const LogicalSessionMachine before = machine;
      auto result = machine.apply(event);
      ASSERT_FALSE(result.has_value());
      EXPECT_EQ(result.error().code, fx::kRefusalCode);
      EXPECT_EQ(result.error().context, std::string{fx::kRefusalReason});
      EXPECT_NE(result.error().message.find(std::string{to_string(spec.state)}), std::string::npos);
      EXPECT_NE(result.error().message.find(std::string{to_string(event)}), std::string::npos);
      EXPECT_EQ(machine, before);
    }
  }
}

// --- Invariants that do not come from the table ----------------------------

constexpr std::uint32_t bit(Fx effect) {
  return std::uint32_t{1} << static_cast<unsigned>(effect);
}

std::uint32_t bits_of(const LogicalSessionEffects& effects) {
  std::uint32_t bits = 0;
  effects.for_each([&](Fx effect) { bits |= bit(effect); });
  return bits;
}

constexpr std::uint32_t kOnceOnly =
    bit(Fx::EmitReady) | bit(Fx::EmitCancelAccepted) | bit(Fx::SuppressOutput) |
    bit(Fx::RequestCleanup) | bit(Fx::StartDrain) | bit(Fx::ReleaseSlot) | bit(Fx::EmitTerminal);
constexpr std::uint32_t kAfterReadyOnly = bit(Fx::EmitCancelAccepted) | bit(Fx::AcceptInput) |
                                          bit(Fx::EmitReply) | bit(Fx::StartFinalize) |
                                          bit(Fx::StartDrain);

TEST(LogicalSessionMachine, ReachableSpaceIsTheFixtureAndKeepsInvariants) {
  struct Node {
    LogicalSessionMachine machine;
    std::uint32_t seen;
  };
  std::deque<Node> queue{{opened(), 0}};
  std::set<std::pair<Config, std::uint32_t>> visited;
  std::set<Config> reached;
  while (!queue.empty()) {
    const Node node = queue.front();
    queue.pop_front();
    const auto config = fx::identify(node.machine);
    ASSERT_TRUE(config.has_value()) << "reached a configuration the fixture does not list";
    if (!visited.insert({*config, node.seen}).second) {
      continue;
    }
    reached.insert(*config);
    const bool was_terminal = is_terminal(node.machine.state());
    EXPECT_EQ(was_terminal, (node.seen & bit(Fx::EmitTerminal)) != 0U);
    EXPECT_EQ(node.machine.holds_reservation(), (node.seen & bit(Fx::ReleaseSlot)) == 0U);

    // Liveness: termination plus release always ends terminal and released.
    LogicalSessionMachine ending = node.machine;
    (void)ending.apply(Ev::Abort);
    ASSERT_TRUE(ending.apply(Ev::ReleaseAcknowledged).has_value());
    EXPECT_TRUE(is_terminal(ending.state()));
    EXPECT_FALSE(ending.holds_reservation());

    for (const Ev event : fx::kEvents) {
      SCOPED_TRACE(std::string{fx::spec(*config).name} + " + " + std::string{to_string(event)});
      LogicalSessionMachine next = node.machine;
      auto result = next.apply(event);
      if (!result.has_value()) {
        EXPECT_EQ(next, node.machine);
        continue;
      }
      const std::uint32_t effects = bits_of(result.value());
      EXPECT_EQ(effects & kOnceOnly & node.seen, 0U) << "a once-only effect repeated";
      if ((node.seen & bit(Fx::EmitTerminal)) != 0U) {
        EXPECT_EQ(effects & ~bit(Fx::ReleaseSlot), 0U) << "output after the terminal outcome";
      }
      if ((node.seen & bit(Fx::SuppressOutput)) != 0U) {
        EXPECT_EQ(effects & ~(bit(Fx::ReleaseSlot) | bit(Fx::EmitTerminal)), 0U)
            << "work after output was suppressed";
      }
      if ((effects & bit(Fx::ReleaseSlot)) != 0U) {
        EXPECT_NE(node.seen & bit(Fx::RequestCleanup), 0U) << "release before cleanup";
      }
      if ((effects & kAfterReadyOnly) != 0U) {
        EXPECT_NE(node.seen & bit(Fx::EmitReady), 0U) << "client output before Ready";
      }
      if (was_terminal) {
        EXPECT_TRUE(is_terminal(next.state())) << "left a terminal state";
      }
      queue.push_back({next, node.seen | effects});
    }
  }
  EXPECT_EQ(reached.size(), fx::kConfigCount);
}

// --- Generation binding ------------------------------------------------------

TEST(LogicalSessionMachine, OpenBindsGeneration) {
  for (const fx::OpenCase& c : fx::kOpenCases) {
    SCOPED_TRACE("serving=" + std::to_string(c.serving) +
                 " requested=" + std::to_string(c.requested));
    auto result = LogicalSessionMachine::open(c.serving, c.requested);
    if (!c.code.has_value()) {
      ASSERT_TRUE(result.has_value()) << result.error().message;
      EXPECT_EQ(result.value().state(), St::Opening);
      EXPECT_EQ(result.value().generation(), c.serving);
      EXPECT_TRUE(result.value().holds_reservation());
      continue;
    }
    ASSERT_FALSE(result.has_value());
    EXPECT_EQ(result.error().code, *c.code);
    EXPECT_EQ(result.error().context, std::string{c.reason});
  }
}

TEST(LogicalSessionMachine, CheckGenerationIsStateIndependent) {
  for (const fx::ConfigSpec& spec : fx::kConfigs) {
    const LogicalSessionMachine machine = fx::reference(spec.config);
    for (const fx::GenerationCase& c : fx::kGenerationCases) {
      SCOPED_TRACE(std::string{spec.name} + " observed=" + std::to_string(c.observed));
      auto result = machine.check_generation(c.observed);
      EXPECT_EQ(result.has_value(), c.matches);
      if (!c.matches) {
        EXPECT_EQ(result.error().code, Error::Code::NotReady);
        EXPECT_EQ(result.error().context, std::string{fx::kStaleReason});
      }
      EXPECT_EQ(machine, fx::reference(spec.config));
    }
  }
}

TEST(LogicalSessionMachine, StaleGenerationEscalation) {
  // The owner discards a stale message and applies Fail: fatal while live,
  // inert once cancellation or a terminal outcome fixed the result.
  for (const fx::ConfigSpec& spec : fx::kConfigs) {
    SCOPED_TRACE(std::string{spec.name});
    LogicalSessionMachine machine = fx::reference(spec.config);
    ASSERT_FALSE(machine.check_generation(fx::kGeneration + 1).has_value());
    const LogicalSessionMachine before = machine;
    auto result = machine.apply(Ev::Fail);
    ASSERT_TRUE(result.has_value());
    if (spec.outcome_fixed) {
      EXPECT_TRUE(result.value().empty());
      EXPECT_EQ(machine, before);
    } else {
      EXPECT_EQ(machine.state(), St::Failed);
      EXPECT_TRUE(result.value().contains(Fx::SuppressOutput));
      EXPECT_TRUE(result.value().contains(Fx::EmitTerminal));
      EXPECT_TRUE(machine.holds_reservation());
    }
  }
}

// --- Normative scenarios ------------------------------------------------------

using Effects = std::vector<LogicalSessionEffects>;

TEST(LogicalSessionScenario, SpeechToTextNormalPath) {
  LogicalSessionMachine m = opened();
  EXPECT_EQ(run(m, {Ev::Admitted, Ev::Data, Ev::AutomaticEndpoint, Ev::Data, Ev::FinalizeCompleted,
                    Ev::HalfClose, Ev::DrainCompleted, Ev::ReleaseAcknowledged}),
            (Effects{{Fx::EmitReady},
                     {Fx::AcceptInput},
                     {Fx::StartFinalize},
                     {Fx::AcceptInput},
                     {},
                     {Fx::StartDrain},
                     {Fx::RequestCleanup},
                     {Fx::ReleaseSlot, Fx::EmitTerminal}}));
  EXPECT_EQ(m.state(), St::Closed);
  EXPECT_FALSE(m.holds_reservation());
}

TEST(LogicalSessionScenario, DataAfterClientFinalizeIsRefusedThenFails) {
  LogicalSessionMachine m = opened();
  run(m, {Ev::Admitted, Ev::Finalize});
  expect_refused(m, Ev::Data);
  EXPECT_EQ(
      run(m, {Ev::Fail, Ev::ReleaseAcknowledged}),
      (Effects{{Fx::SuppressOutput, Fx::RequestCleanup, Fx::EmitTerminal}, {Fx::ReleaseSlot}}));
  EXPECT_EQ(m.state(), St::Failed);
}

TEST(LogicalSessionScenario, CancelHoldsTheReservationUntilRelease) {
  LogicalSessionMachine m = opened();
  run(m, {Ev::Admitted});
  EXPECT_EQ(run(m, {Ev::Cancel}),
            (Effects{{Fx::SuppressOutput, Fx::EmitCancelAccepted, Fx::RequestCleanup}}));
  const auto late =
      run(m, {Ev::Cancel, Ev::HalfClose, Ev::Admitted, Ev::AutomaticEndpoint, Ev::FinalizeCompleted,
              Ev::DrainCompleted, Ev::Drain, Ev::Abort, Ev::Fail, Ev::Cancel, Ev::Abort, Ev::Fail});
  for (const LogicalSessionEffects& effects : late) {
    EXPECT_TRUE(effects.empty());
  }
  EXPECT_EQ(m.state(), St::CancelRequested);
  EXPECT_TRUE(m.holds_reservation());
  EXPECT_EQ(run(m, {Ev::ReleaseAcknowledged}), (Effects{{Fx::ReleaseSlot, Fx::EmitTerminal}}));
  EXPECT_EQ(m.state(), St::Closed);
}

TEST(LogicalSessionScenario, RetirementDrainThenDeadline) {
  LogicalSessionMachine m = opened();
  EXPECT_EQ(run(m, {Ev::Admitted, Ev::Drain, Ev::Data, Ev::Abort, Ev::ReleaseAcknowledged}),
            (Effects{{Fx::EmitReady},
                     {Fx::StartDrain},
                     {},
                     {Fx::SuppressOutput, Fx::RequestCleanup},
                     {Fx::ReleaseSlot, Fx::EmitTerminal}}));
  EXPECT_EQ(m.state(), St::Closed);
}

TEST(LogicalSessionScenario, BackendResetDuringCancelFails) {
  LogicalSessionMachine m = opened();
  run(m, {Ev::Admitted, Ev::Cancel});
  EXPECT_EQ(run(m, {Ev::BackendReset}), (Effects{{Fx::EmitTerminal}}));
  EXPECT_EQ(m.state(), St::Failed);
  EXPECT_TRUE(m.holds_reservation());
  EXPECT_EQ(run(m, {Ev::ReleaseAcknowledged}), (Effects{{Fx::ReleaseSlot}}));
  EXPECT_FALSE(m.holds_reservation());
}

TEST(LogicalSessionScenario, CancelBeforeReadyIsNotAcknowledged) {
  LogicalSessionMachine m = opened();
  EXPECT_EQ(
      run(m, {Ev::Cancel, Ev::Admitted, Ev::ReleaseAcknowledged}),
      (Effects{{Fx::SuppressOutput, Fx::RequestCleanup}, {}, {Fx::ReleaseSlot, Fx::EmitTerminal}}));
  EXPECT_EQ(m.state(), St::Closed);
}

TEST(LogicalSessionScenario, AdmissionFailureNeverSendsReady) {
  LogicalSessionMachine m = opened();
  EXPECT_EQ(run(m, {Ev::Fail}),
            (Effects{{Fx::SuppressOutput, Fx::RequestCleanup, Fx::EmitTerminal}}));
  EXPECT_EQ(m.state(), St::Failed);
  EXPECT_EQ(run(m, {Ev::Admitted}), (Effects{{}}));
  EXPECT_EQ(run(m, {Ev::ReleaseAcknowledged}), (Effects{{Fx::ReleaseSlot}}));
}

TEST(LogicalSessionScenario, DuplicateReleaseIsInert) {
  LogicalSessionMachine closed = fx::reference(Config::Closed);
  EXPECT_EQ(run(closed, {Ev::ReleaseAcknowledged}), (Effects{{}}));
  LogicalSessionMachine failed = fx::reference(Config::FailedReleased);
  EXPECT_EQ(run(failed, {Ev::ReleaseAcknowledged}), (Effects{{}}));
}

TEST(LogicalSessionScenario, CancelAfterDrainCompletedRequestsNoSecondCleanup) {
  LogicalSessionMachine m = fx::reference(Config::DrainingReleasing);
  EXPECT_EQ(run(m, {Ev::Cancel}), (Effects{{Fx::SuppressOutput, Fx::EmitCancelAccepted}}));
}

TEST(LogicalSessionScenario, SecondOpenIsRefused) {
  LogicalSessionMachine m = opened();
  expect_refused(m, Ev::Open);
  run(m, {Ev::Admitted});
  expect_refused(m, Ev::Open);
}

// --- Names ------------------------------------------------------------------

static_assert(static_cast<int>(St::Opening) == 0 && static_cast<int>(St::Active) == 1 &&
              static_cast<int>(St::Finalizing) == 2 && static_cast<int>(St::Draining) == 3 &&
              static_cast<int>(St::CancelRequested) == 4 && static_cast<int>(St::Closed) == 5 &&
              static_cast<int>(St::Failed) == 6);
static_assert(static_cast<int>(Ev::Open) == 0 && static_cast<int>(Ev::ReleaseAcknowledged) == 15);
static_assert(static_cast<int>(Fx::SuppressOutput) == 0 && static_cast<int>(Fx::EmitTerminal) == 9);
// Execution order the effect docs promise.
static_assert(Fx::SuppressOutput < Fx::EmitCancelAccepted &&
              Fx::EmitCancelAccepted < Fx::RequestCleanup && Fx::ReleaseSlot < Fx::EmitTerminal);

TEST(LogicalSessionNames, StateNamesRoundTrip) {
  const std::vector<std::pair<St, std::string_view>> names{
      {St::Opening, "opening"},
      {St::Active, "active"},
      {St::Finalizing, "finalizing"},
      {St::Draining, "draining"},
      {St::CancelRequested, "cancel_requested"},
      {St::Closed, "closed"},
      {St::Failed, "failed"}};
  for (const auto& [state, name] : names) {
    EXPECT_EQ(to_string(state), name);
    EXPECT_EQ(logical_session_state_from_string(name), state);
  }
  for (const std::string_view bad : {"", "Active", "cancel-requested", "closed ", "unknown"}) {
    EXPECT_FALSE(logical_session_state_from_string(bad).has_value()) << "'" << bad << "'";
  }
  EXPECT_EQ(to_string(static_cast<St>(7)), "failed");
}

template <typename Enum>
std::vector<std::string_view> names_until_unknown() {
  std::vector<std::string_view> out;
  for (unsigned value = 0; value < 256; ++value) {
    const std::string_view name = to_string(static_cast<Enum>(value));
    if (name == "unknown") {
      break;
    }
    out.push_back(name);
  }
  return out;
}

TEST(LogicalSessionNames, EventAndEffectNamesAreStable) {
  const std::regex snake{"^[a-z][a-z_]*$"};
  const auto events = names_until_unknown<Ev>();
  EXPECT_EQ(events.size(), fx::kEventCount);
  const auto effects = names_until_unknown<Fx>();
  EXPECT_EQ(effects.size(), 10U);
  for (const auto& list : {events, effects}) {
    EXPECT_EQ(std::set<std::string_view>(list.begin(), list.end()).size(), list.size());
    for (const std::string_view name : list) {
      EXPECT_TRUE(std::regex_match(std::string{name}, snake)) << name;
    }
  }
  EXPECT_EQ(to_string(Ev::HalfClose), "half_close");
  EXPECT_EQ(to_string(Ev::ReleaseAcknowledged), "release_acknowledged");
  EXPECT_EQ(to_string(Fx::EmitCancelAccepted), "emit_cancel_accepted");
  EXPECT_EQ(to_string(Fx::EmitTerminal), "emit_terminal");
  EXPECT_EQ(to_string(static_cast<Ev>(200)), "unknown");
  EXPECT_EQ(to_string(static_cast<Fx>(200)), "unknown");
}

// --- Value objects ------------------------------------------------------------

TEST(LogicalSessionValues, EffectsSetSemantics) {
  static_assert(LogicalSessionEffects{}.empty());
  static_assert(LogicalSessionEffects{Fx::EmitReady}.contains(Fx::EmitReady));
  static_assert(!LogicalSessionEffects{Fx::EmitReady}.contains(Fx::EmitTerminal));
  EXPECT_EQ((LogicalSessionEffects{Fx::EmitTerminal, Fx::SuppressOutput}),
            (LogicalSessionEffects{Fx::SuppressOutput, Fx::EmitTerminal, Fx::SuppressOutput}));
  EXPECT_NE((LogicalSessionEffects{Fx::EmitTerminal}), (LogicalSessionEffects{}));
  std::vector<Fx> order;
  LogicalSessionEffects{Fx::EmitTerminal, Fx::RequestCleanup, Fx::SuppressOutput}.for_each(
      [&](Fx effect) { order.push_back(effect); });
  EXPECT_EQ(order, (std::vector<Fx>{Fx::SuppressOutput, Fx::RequestCleanup, Fx::EmitTerminal}));
}

TEST(LogicalSessionValues, UsageAndStatus) {
  constexpr std::uint64_t kMax = std::numeric_limits<std::uint64_t>::max();
  for (const auto& [used, limit] :
       std::vector<std::pair<std::uint64_t, std::uint64_t>>{{0, 0}, {5, 5}, {3, 8}, {kMax, kMax}}) {
    auto usage = LogicalSessionUsage::create(used, limit);
    ASSERT_TRUE(usage.has_value());
    EXPECT_EQ(usage.value().used(), used);
    EXPECT_EQ(usage.value().limit(), limit);
    EXPECT_EQ(usage.value().available(), limit - used);
  }
  auto over = LogicalSessionUsage::create(6, 5);
  ASSERT_FALSE(over.has_value());
  EXPECT_EQ(over.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(over.error().context, std::string{"usage_exceeds_limit"});

  static_assert(std::is_trivially_copyable_v<LogicalSessionStatus>);
  const LogicalSessionStatus status;
  EXPECT_EQ(status.state, St::Opening);
  EXPECT_EQ(status.input_queue_depth, 0U);
  EXPECT_EQ(status.input_credit_bytes, LogicalSessionUsage{});
  LogicalSessionStatus other = status;
  other.input_queue_depth = 1;
  EXPECT_NE(other, status);
}

TEST(LogicalSessionValues, MachineIsAValue) {
  static_assert(std::is_trivially_copyable_v<LogicalSessionMachine>);
  LogicalSessionMachine a = opened();
  LogicalSessionMachine b = a;
  run(b, {Ev::Admitted});
  EXPECT_EQ(a.state(), St::Opening);
  EXPECT_EQ(b.state(), St::Active);
  EXPECT_NE(a, b);
  LogicalSessionMachine c = opened();
  EXPECT_EQ(a, c);
  auto other_generation = LogicalSessionMachine::open(fx::kGeneration + 1, fx::kGeneration + 1);
  ASSERT_TRUE(other_generation.has_value());
  EXPECT_NE(a, other_generation.value());
}

}  // namespace
