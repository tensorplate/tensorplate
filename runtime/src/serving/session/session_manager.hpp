// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <condition_variable>
#include <cstdint>
#include <functional>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <thread>
#include <vector>

#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/serving/session.hpp"

#include "serving/session/timers.hpp"

namespace tensorplate::serving {

struct ManagedSessionTransition {
  std::uint64_t session_key = 0;
  LogicalSessionState state = LogicalSessionState::Opening;
  LogicalSessionEffects effects;
  std::optional<Error> cause;
};

/// Owns logical-session count slots and applies lifecycle effects in order.
/// The sink performs each transition's effects before another event is applied.
/// The sink and admission/initialization hooks must not reenter this manager
/// or throw. A later stream binding supplies them.
class SessionManager {
 public:
  using TransitionSink = std::function<void(const ManagedSessionTransition&)>;
  using AdmissionCheck = std::function<Result<void>()>;
  using Initialize = std::function<Result<void>(std::uint64_t)>;

  [[nodiscard]] static Result<std::unique_ptr<SessionManager>> create(
      SessionLimits limits, std::uint64_t serving_generation, const SchedulerClock& clock,
      TransitionSink sink, AdmissionCheck admission_check = {}, bool start_timer_thread = true);

  ~SessionManager();
  SessionManager(const SessionManager&) = delete;
  SessionManager& operator=(const SessionManager&) = delete;

  [[nodiscard]] Result<ManagedSessionTransition> open(std::uint64_t requested_generation,
                                                      const Initialize& initialize = {});
  [[nodiscard]] Result<ManagedSessionTransition> apply(std::uint64_t session_key,
                                                       LogicalSessionEvent event,
                                                       std::optional<Error> cause = std::nullopt);
  [[nodiscard]] std::vector<ManagedSessionTransition> stop_admission_and_drain(Error cause);
  [[nodiscard]] std::vector<ManagedSessionTransition> sweep_due();
  void notify_clock_advanced() noexcept;

  [[nodiscard]] Result<LogicalSessionState> state(std::uint64_t session_key) const;
  [[nodiscard]] Result<bool> heartbeat_due(std::uint64_t session_key) const;
  [[nodiscard]] Result<SchedulerClock::Duration> duration_remaining(
      std::uint64_t session_key) const;
  [[nodiscard]] std::size_t held_slots() const noexcept;
  [[nodiscard]] bool admission_open() const noexcept;

 private:
  struct Entry {
    LogicalSessionMachine machine;
    SessionTimers timers;
    std::optional<Error> cause;
  };

  SessionManager(SessionLimits limits, std::uint64_t serving_generation,
                 const SchedulerClock& clock, TransitionSink sink, AdmissionCheck admission_check);

  [[nodiscard]] std::optional<ManagedSessionTransition> apply_locked(std::uint64_t key,
                                                                     Entry& entry,
                                                                     LogicalSessionEvent event,
                                                                     std::optional<Error> cause);
  void emit(const ManagedSessionTransition& transition) const;
  void run_timer();

  SessionLimits limits_;
  std::uint64_t serving_generation_;
  const SchedulerClock& clock_;
  TransitionSink sink_;
  AdmissionCheck admission_check_;
  mutable std::mutex serial_mu_;
  mutable std::mutex mu_;
  std::condition_variable cv_;
  std::map<std::uint64_t, Entry> entries_;
  std::uint64_t next_key_ = 1;
  bool admission_open_ = true;
  bool stop_timer_ = false;
  std::thread timer_thread_;
};
}  // namespace tensorplate::serving
