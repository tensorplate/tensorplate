// SPDX-License-Identifier: Apache-2.0
//
// Loader and replay for test/unit/fixtures/session_lifecycle.json: lifecycle
// cases run step by step against a SessionManager on a fake clock, with a
// stand-in for the stream that owns the session's output.
//
// A step is {"op": ...}. "expect" is the step's outcome and defaults to "ok":
// the context of a refusal, or the answer of an "offer". After any step,
// "effects" lists what the sink was asked to do during it, in order; "state"
// is the session's state, read from its tombstone once the slot is released;
// "held_slots" and "depth" read the manager and the session's status.

#pragma once

#include <gtest/gtest.h>

#include <chrono>
#include <cstdint>
#include <fstream>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

#include "fake_scheduler_clock.hpp"
#include "serving/session/session_manager.hpp"
#include "session_budget_fixture.hpp"

namespace tensorplate::testing {

inline nlohmann::json load_session_lifecycle_fixture() {
  const std::string path =
      std::string(TP_SOURCE_DIR) + "/test/unit/fixtures/session_lifecycle.json";
  std::ifstream input(path);
  if (!input.good()) {
    throw std::runtime_error("cannot open " + path);
  }
  return nlohmann::json::parse(input);
}

inline std::chrono::milliseconds fixture_ms(const nlohmann::json& object, const char* name) {
  return std::chrono::milliseconds{object.at(name).get<std::int64_t>()};
}

inline serving::OutputItem lifecycle_output_item(serving::OutputKind kind, std::uint64_t bytes,
                                                 std::uint64_t key = 0) {
  return serving::OutputItem{kind, bytes, key, std::make_unique<serving::OutputBody>()};
}

/// What the stream does with a session's effects: it writes the terminal
/// outcome into the output queue, and records every effect it was handed.
struct LifecycleStream {
  explicit LifecycleStream(const SchedulerClock& clock_in) : clock(clock_in) {}

  void on_transition(const serving::ManagedSessionTransition& transition) {
    transition.effects.for_each([&](serving::LogicalSessionEffect effect) {
      effects.emplace_back(serving::to_string(effect));
      if (effect != serving::LogicalSessionEffect::EmitTerminal) {
        return;
      }
      auto terminal = lifecycle_output_item(serving::OutputKind::Terminal, 1);
      const auto offered = transition.output->offer(terminal, clock.now());
      if (offered && *offered == serving::OutputOffer::Queued) {
        ++terminals_written;
      }
      terminal_cause = transition.cause ? transition.cause->context : std::nullopt;
    });
  }

  const SchedulerClock& clock;
  std::vector<std::string> effects;
  std::uint32_t terminals_written = 0;
  std::optional<std::string> terminal_cause;
};

/// One fixture case: a manager of the fixture's generation, one session and
/// the stream standing in for its owner.
class SessionLifecycleReplay {
 public:
  SessionLifecycleReplay(const nlohmann::json& fixture, const nlohmann::json& scenario)
      : budgets_(session_budgets_from(scenario).value()),
        manager_(serving::SessionManager::create(
                     limits_of(scenario), fixture.at("generation").get<std::uint64_t>(), clock_,
                     [this](const auto& transition) { stream_.on_transition(transition); }, {},
                     false)
                     .value()) {}

  /// Replays `steps`, stopping at the first step that cannot run.
  void run(const nlohmann::json& steps) {
    for (const auto& step : steps) {
      SCOPED_TRACE(step.dump());
      if (!run_step(step)) {
        return;
      }
    }
  }

  [[nodiscard]] const LifecycleStream& stream() const noexcept { return stream_; }
  [[nodiscard]] serving::SessionManager& manager() noexcept { return *manager_; }
  [[nodiscard]] FakeSchedulerClock& clock() noexcept { return clock_; }

 private:
  static serving::SessionLimits limits_of(const nlohmann::json& scenario) {
    if (!scenario.contains("limits")) {
      return serving::SessionLimits::defaults();
    }
    const auto& limits = scenario.at("limits");
    return serving::SessionLimits::create(
               fixture_ms(limits, "idle_timeout_ms"), fixture_ms(limits, "heartbeat_interval_ms"),
               fixture_ms(limits, "liveness_timeout_ms"), fixture_ms(limits, "max_duration_ms"),
               limits.at("max_sessions").get<std::uint32_t>())
        .value();
  }

  static std::optional<serving::LogicalSessionEvent> event_from(const std::string& name) {
    for (std::uint8_t value = 0;; ++value) {
      const auto event = static_cast<serving::LogicalSessionEvent>(value);
      const auto candidate = serving::to_string(event);
      if (candidate == "unknown") {
        return std::nullopt;
      }
      if (candidate == name) {
        return event;
      }
    }
  }

  static std::optional<serving::OutputKind> kind_from(const std::string& name) {
    if (name == "audio") {
      return serving::OutputKind::Audio;
    }
    if (name == "partial") {
      return serving::OutputKind::Partial;
    }
    if (name == "result") {
      return serving::OutputKind::Result;
    }
    if (name == "control") {
      return serving::OutputKind::Control;
    }
    return std::nullopt;
  }

  static std::string name_of(serving::OutputOffer offer) {
    switch (offer) {
      case serving::OutputOffer::Queued:
        return "queued";
      case serving::OutputOffer::Replaced:
        return "replaced";
      case serving::OutputOffer::Full:
        return "full";
      case serving::OutputOffer::Suppressed:
        return "suppressed";
      case serving::OutputOffer::Closed:
        return "closed";
    }
    return "unknown";
  }

  [[nodiscard]] std::string state_name() const {
    if (const auto live = manager_->state(key_)) {
      return std::string{serving::to_string(*live)};
    }
    if (const auto ended = manager_->tombstone(key_)) {
      return std::string{serving::to_string(ended->state)};
    }
    return "unknown";
  }

  /// False when the step could not run and the case must stop.
  [[nodiscard]] bool run_step(const nlohmann::json& step) {
    const auto op = step.at("op").get<std::string>();
    if (op == "repeat") {
      for (std::uint32_t round = 0; round < step.at("times").get<std::uint32_t>(); ++round) {
        for (const auto& inner : step.at("steps")) {
          if (!run_step(inner)) {
            return false;
          }
        }
      }
      return true;
    }
    stream_.effects.clear();
    std::optional<std::string> outcome;
    if (op == "open") {
      const auto opened = manager_->open(step.at("generation").get<std::uint64_t>(), budgets_);
      outcome = outcome_of(opened);
      if (opened) {
        key_ = opened->session_key;
        output_ = opened->output;
      }
    } else if (op == "event") {
      const auto event = event_from(step.at("event").get<std::string>());
      if (!event) {
        ADD_FAILURE() << "unknown fixture event";
        return false;
      }
      std::optional<Error> cause;
      if (step.contains("cause")) {
        const auto reason = step.at("cause").get<std::string>();
        cause = Error::make(Error::Code::Unavailable, reason, reason);
      }
      outcome = outcome_of(manager_->apply(key_, *event, std::move(cause)));
    } else if (op == "input") {
      outcome = outcome_of(manager_->accept_input(key_, step.at("bytes").get<std::uint64_t>()));
    } else if (op == "start_segment") {
      outcome = outcome_of(manager_->start_text_segment(key_));
    } else if (op == "finish_segment") {
      outcome = outcome_of(manager_->finish_text_segment(key_));
    } else if (op == "release_audio") {
      outcome = outcome_of(manager_->release_audio_input(
          key_, step.at("chunks").get<std::uint32_t>(), step.at("bytes").get<std::uint64_t>()));
    } else if (op == "offer") {
      const auto kind = kind_from(step.at("kind").get<std::string>());
      if (!kind || !output_) {
        ADD_FAILURE() << "offer without a known kind or an open session";
        return false;
      }
      auto item = lifecycle_output_item(*kind, step.at("bytes").get<std::uint64_t>(),
                                        step.value("key", std::uint64_t{0}));
      const auto offered = output_->offer(item, clock_.now());
      outcome = offered ? name_of(*offered) : offered.error().context.value_or("");
    } else if (op == "deliver") {
      for (std::uint32_t item = 0; item < step.at("items").get<std::uint32_t>(); ++item) {
        const auto sent = output_ ? output_->take() : std::nullopt;
        if (!sent || !output_->delivered(sent->sequence, clock_.now())) {
          ADD_FAILURE() << "nothing to deliver";
          return false;
        }
      }
    } else if (op == "advance") {
      clock_.advance(fixture_ms(step, "ms"));
    } else if (op == "sweep") {
      std::vector<std::string> expired;
      for (const auto& transition : manager_->sweep_due()) {
        expired.push_back(transition.cause ? transition.cause->context.value_or("") : "");
      }
      EXPECT_EQ(expired, step.at("expired").get<std::vector<std::string>>());
    } else {
      ADD_FAILURE() << "unknown fixture op " << op;
      return false;
    }
    if (outcome) {
      EXPECT_EQ(*outcome, step.value("expect", std::string{"ok"}));
    }
    if (step.contains("effects")) {
      EXPECT_EQ(stream_.effects, step.at("effects").get<std::vector<std::string>>());
    }
    if (step.contains("state")) {
      EXPECT_EQ(state_name(), step.at("state").get<std::string>());
    }
    if (step.contains("held_slots")) {
      EXPECT_EQ(manager_->held_slots(), step.at("held_slots").get<std::size_t>());
    }
    if (step.contains("depth")) {
      const auto status = manager_->status(key_);
      EXPECT_TRUE(status);
      if (status) {
        EXPECT_EQ(status->input_queue_depth, step.at("depth").get<std::uint32_t>());
      }
    }
    return true;
  }

  FakeSchedulerClock clock_;
  LifecycleStream stream_{clock_};
  serving::SessionBudgets budgets_;
  std::unique_ptr<serving::SessionManager> manager_;
  std::uint64_t key_ = 0;
  std::shared_ptr<serving::BoundedOutputQueue> output_;
};
}  // namespace tensorplate::testing
