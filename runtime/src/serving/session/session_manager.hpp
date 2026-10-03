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

#include "serving/session/credits.hpp"
#include "serving/session/output_queue.hpp"
#include "serving/session/timers.hpp"
#include "serving/session/tombstones.hpp"

namespace tensorplate::serving {

struct ManagedSessionTransition {
  std::uint64_t session_key = 0;
  LogicalSessionState state = LogicalSessionState::Opening;
  LogicalSessionEffects effects;
  std::optional<Error> cause;
  /// State, input credit and output budgets once the event was applied.
  LogicalSessionStatus status;
  /// The session's output queue, where the sink queues what the effects
  /// publish. It stays usable after the slot is released.
  std::shared_ptr<BoundedOutputQueue> output;
};

/// Owns logical-session count slots and input credit, and applies lifecycle
/// effects in order. The sink performs each transition's effects before
/// another event is applied; the manager has already suppressed the output
/// queue when a transition carries suppress_output. The sink and
/// admission/initialization hooks must not reenter this manager or throw. A
/// later stream binding supplies them.
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
                                                      const SessionBudgets& budgets,
                                                      const Initialize& initialize = {});
  /// Applies any event except `data`, which carries a size: accept_input().
  [[nodiscard]] Result<ManagedSessionTransition> apply(std::uint64_t session_key,
                                                       LogicalSessionEvent event,
                                                       std::optional<Error> cause = std::nullopt);
  /// Applies `data` for one audio chunk or text segment of `bytes` bytes.
  /// Input the state accepts is charged to the credit first; input above the
  /// credit fails the session with `input_credit_exceeded`.
  [[nodiscard]] Result<ManagedSessionTransition> accept_input(std::uint64_t session_key,
                                                              std::uint64_t bytes);
  /// Return input credit as the pipeline lets go of input (see InputCredit).
  /// A release the credit refuses is a defect in the caller and fails the
  /// session. Each returns the status after the release.
  [[nodiscard]] Result<LogicalSessionStatus> release_audio_input(std::uint64_t session_key,
                                                                 std::uint32_t chunks,
                                                                 std::uint64_t bytes);
  [[nodiscard]] Result<LogicalSessionStatus> start_text_segment(std::uint64_t session_key);
  [[nodiscard]] Result<LogicalSessionStatus> finish_text_segment(std::uint64_t session_key);
  [[nodiscard]] std::vector<ManagedSessionTransition> stop_admission_and_drain(Error cause);
  [[nodiscard]] std::vector<ManagedSessionTransition> sweep_due();
  void notify_clock_advanced() noexcept;

  [[nodiscard]] Result<LogicalSessionState> state(std::uint64_t session_key) const;
  [[nodiscard]] Result<LogicalSessionStatus> status(std::uint64_t session_key) const;
  /// What is remembered of a session whose slot was released, while its
  /// tombstone lasts. Calls naming such a session fail with `session_ended`
  /// rather than `unknown_session`.
  [[nodiscard]] std::optional<SessionTombstone> tombstone(std::uint64_t session_key) const;
  [[nodiscard]] Result<bool> heartbeat_due(std::uint64_t session_key) const;
  [[nodiscard]] Result<SchedulerClock::Duration> duration_remaining(
      std::uint64_t session_key) const;
  [[nodiscard]] std::size_t held_slots() const noexcept;
  [[nodiscard]] bool admission_open() const noexcept;

 private:
  struct Entry {
    LogicalSessionMachine machine;
    SessionTimers timers;
    InputCredit credit;
    std::shared_ptr<BoundedOutputQueue> output;
    std::optional<Error> cause;
  };
  using CreditUpdate = std::function<Result<void>(InputCredit&)>;

  SessionManager(SessionLimits limits, std::uint64_t serving_generation,
                 const SchedulerClock& clock, TransitionSink sink, AdmissionCheck admission_check);

  [[nodiscard]] Result<ManagedSessionTransition> apply_event(std::uint64_t session_key,
                                                             LogicalSessionEvent event,
                                                             std::optional<Error> cause,
                                                             std::optional<std::uint64_t> bytes);
  [[nodiscard]] std::optional<ManagedSessionTransition> apply_locked(std::uint64_t key,
                                                                     Entry& entry,
                                                                     LogicalSessionEvent event,
                                                                     std::optional<Error> cause);
  /// The credit's refusal, if the state accepts the input and the credit
  /// does not.
  [[nodiscard]] static std::optional<Error> charge_accepted_input(Entry& entry,
                                                                  std::uint64_t bytes);
  [[nodiscard]] Result<LogicalSessionStatus> update_credit(std::uint64_t session_key,
                                                           const CreditUpdate& update);
  [[nodiscard]] std::optional<SessionExpiry> expiry_locked(const Entry& entry) const;
  [[nodiscard]] static LogicalSessionStatus status_of(const Entry& entry);
  [[nodiscard]] Error missing_session_locked(std::uint64_t session_key) const;
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
  SessionTombstones tombstones_;
  std::uint64_t next_key_ = 1;
  bool admission_open_ = true;
  bool stop_timer_ = false;
  std::thread timer_thread_;
};
}  // namespace tensorplate::serving
