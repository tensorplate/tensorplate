// SPDX-License-Identifier: Apache-2.0
#include "serving/session/session_manager.hpp"

#include <algorithm>
#include <chrono>
#include <limits>
#include <string>
#include <system_error>
#include <utility>

namespace tensorplate::serving {
namespace {
Error expiry_error(SessionExpiry expiry) {
  switch (expiry) {
    case SessionExpiry::Idle:
      return Error::make(Error::Code::Timeout, "logical session was idle", "idle_timeout");
    case SessionExpiry::Heartbeat:
      return Error::make(Error::Code::Timeout, "logical session lost heartbeat",
                         "heartbeat_timeout");
    case SessionExpiry::MaxDuration:
      return Error::make(Error::Code::Timeout, "logical session reached maximum duration",
                         "max_duration");
    case SessionExpiry::SlowConsumer:
      return Error::make(Error::Code::ResourceExhausted,
                         "logical session output made no delivery progress", "slow_consumer");
    case SessionExpiry::FinalizeDeadline:
      return Error::make(Error::Code::Timeout,
                         "logical session did not finish finalizing or draining in time",
                         "finalize_timeout");
  }
  return Error::make(Error::Code::Internal, "unknown session timeout", "unknown_timeout");
}

bool client_event(LogicalSessionEvent event) noexcept {
  switch (event) {
    case LogicalSessionEvent::Open:
    case LogicalSessionEvent::Data:
    case LogicalSessionEvent::Finalize:
    case LogicalSessionEvent::Cancel:
    case LogicalSessionEvent::HalfClose:
    case LogicalSessionEvent::Ping:
    case LogicalSessionEvent::StatusRequest:
      return true;
    default:
      return false;
  }
}

Error refused_event(LogicalSessionState state, LogicalSessionEvent event) {
  const std::string where = "logical session in state '" + std::string{to_string(state)} + "'";
  if (client_event(event)) {
    return Error::make(Error::Code::NotReady,
                       where + " refuses event '" + std::string{to_string(event)} + "'",
                       "illegal_transition");
  }
  return Error::make(Error::Code::Internal,
                     where + " did not expect report '" + std::string{to_string(event)} + "'",
                     "unexpected_report");
}
}  // namespace

SessionManager::SessionManager(SessionLimits limits, std::uint64_t serving_generation,
                               const SchedulerClock& clock, TransitionSink sink,
                               AdmissionCheck admission_check)
    : limits_(limits),
      serving_generation_(serving_generation),
      clock_(clock),
      sink_(std::move(sink)),
      admission_check_(std::move(admission_check)) {}

Result<std::unique_ptr<SessionManager>> SessionManager::create(
    SessionLimits limits, std::uint64_t serving_generation, const SchedulerClock& clock,
    TransitionSink sink, AdmissionCheck admission_check, bool start_timer_thread) {
  if (serving_generation == 0 || !sink) {
    return unexpected(Error::make(Error::Code::ConfigInvalid, "invalid session manager setup",
                                  "invalid_session_manager"));
  }
  auto manager = std::unique_ptr<SessionManager>(new SessionManager(
      limits, serving_generation, clock, std::move(sink), std::move(admission_check)));
  if (start_timer_thread) {
    try {
      manager->timer_thread_ = std::thread([owner = manager.get()] { owner->run_timer(); });
    } catch (const std::system_error&) {
      return unexpected(Error::make(Error::Code::Internal, "session timer thread could not start",
                                    "timer_thread_unavailable"));
    }
  }
  return manager;
}

SessionManager::~SessionManager() {
  {
    std::lock_guard guard(mu_);
    stop_timer_ = true;
  }
  cv_.notify_all();
  if (timer_thread_.joinable()) {
    timer_thread_.join();
  }
}

Result<ManagedSessionTransition> SessionManager::open(std::uint64_t requested_generation,
                                                      const SessionBudgets& budgets,
                                                      const Initialize& initialize) {
  std::lock_guard serial(serial_mu_);
  ManagedSessionTransition transition;
  std::optional<Error> initialization_failure;
  {
    std::lock_guard guard(mu_);
    if (!admission_open_) {
      return unexpected(
          Error::make(Error::Code::NotReady, "session admission is closed", "admission_closed"));
    }
    if (entries_.size() >= limits_.max_sessions()) {
      return unexpected(Error::make(Error::Code::ResourceExhausted, "session count limit reached",
                                    "session_count_limit"));
    }
    if (next_key_ == std::numeric_limits<std::uint64_t>::max()) {
      return unexpected(Error::make(Error::Code::ResourceExhausted, "session key space exhausted",
                                    "session_key_exhausted"));
    }
    auto machine = LogicalSessionMachine::open(serving_generation_, requested_generation);
    if (!machine) {
      return unexpected(machine.error());
    }
    if (admission_check_) {
      auto check = admission_check_();
      if (!check) {
        return unexpected(check.error());
      }
    }
    const auto key = next_key_++;
    const auto now = clock_.now();
    auto output = std::make_shared<BoundedOutputQueue>(budgets);
    auto [it, inserted] =
        entries_.emplace(key, Entry{*machine,
                                    SessionTimers{limits_, budgets.input_kind(), now},
                                    InputCredit{budgets},
                                    std::move(output),
                                    {}});
    if (!inserted) {
      return unexpected(
          Error::make(Error::Code::Internal, "session key collision", "session_key_collision"));
    }
    if (initialize) {
      auto initialized = initialize(key);
      if (!initialized) {
        initialization_failure = initialized.error();
      }
    }
    if (!initialization_failure) {
      const auto expiry = expiry_locked(it->second);
      if (expiry) {
        initialization_failure = expiry_error(*expiry);
      }
    }
    if (initialization_failure) {
      const auto failed =
          apply_locked(key, it->second, LogicalSessionEvent::Fail, initialization_failure);
      if (!failed) {
        entries_.erase(it);
        return unexpected(Error::make(Error::Code::Internal,
                                      "session initialization failure transition refused",
                                      "initialization_transition_refused"));
      }
      transition = *failed;
    } else {
      const auto admitted = apply_locked(key, it->second, LogicalSessionEvent::Admitted, {});
      if (!admitted) {
        entries_.erase(it);
        return unexpected(Error::make(Error::Code::Internal, "session admission transition refused",
                                      "admission_transition_refused"));
      }
      transition = *admitted;
    }
  }
  cv_.notify_one();
  emit(transition);
  if (initialization_failure) {
    return unexpected(*initialization_failure);
  }
  return transition;
}

std::optional<Error> SessionManager::charge_accepted_input(Entry& entry, std::uint64_t bytes) {
  // Input a draining session ignores is not charged.
  auto trial = entry.machine;
  const auto accepted = trial.apply(LogicalSessionEvent::Data);
  if (!accepted || !accepted->contains(LogicalSessionEffect::AcceptInput)) {
    return std::nullopt;
  }
  const auto charged = entry.credit.charge(bytes);
  return charged ? std::nullopt : std::optional{charged.error()};
}

std::optional<ManagedSessionTransition> SessionManager::apply_locked(std::uint64_t key,
                                                                     Entry& entry,
                                                                     LogicalSessionEvent event,
                                                                     std::optional<Error> cause) {
  const auto before = entry.machine.state();
  auto effects = entry.machine.apply(event);
  if (!effects) {
    return std::nullopt;
  }
  const auto after = entry.machine.state();
  if (before != after &&
      (after == LogicalSessionState::Draining || after == LogicalSessionState::CancelRequested ||
       after == LogicalSessionState::Failed)) {
    if (cause) {
      entry.cause = std::move(cause);
    } else if (event == LogicalSessionEvent::Cancel) {
      entry.cause = Error::make(Error::Code::Cancelled, "session cancelled", "client_cancelled");
    }
  }
  const auto now = clock_.now();
  entry.timers.on_client_activity(event, now);
  entry.timers.on_transition(before, event, after, now);
  if (effects->contains(LogicalSessionEffect::SuppressOutput)) {
    entry.output->suppress();
  }
  const auto status = status_of(entry);
  return ManagedSessionTransition{key, event, after, *effects, entry.cause, status, entry.output};
}

Result<ManagedSessionTransition> SessionManager::apply(std::uint64_t session_key,
                                                       LogicalSessionEvent event,
                                                       std::optional<Error> cause) {
  if ((event == LogicalSessionEvent::Abort || event == LogicalSessionEvent::Fail ||
       event == LogicalSessionEvent::BackendReset) &&
      !cause) {
    return unexpected(Error::make(Error::Code::ConfigInvalid, "missing session failure cause",
                                  "missing_session_cause"));
  }
  if (event == LogicalSessionEvent::Data) {
    return unexpected(Error::make(Error::Code::ConfigInvalid, "session input without its size",
                                  "input_without_size"));
  }
  return apply_event(session_key, event, std::move(cause), std::nullopt);
}

Result<ManagedSessionTransition> SessionManager::accept_input(std::uint64_t session_key,
                                                              std::uint64_t bytes) {
  return apply_event(session_key, LogicalSessionEvent::Data, std::nullopt, bytes);
}

Result<ManagedSessionTransition> SessionManager::apply_event(std::uint64_t session_key,
                                                             LogicalSessionEvent event,
                                                             std::optional<Error> cause,
                                                             std::optional<std::uint64_t> bytes) {
  std::lock_guard serial(serial_mu_);
  const bool from_client = client_event(event);
  std::optional<ManagedSessionTransition> expired_transition;
  std::optional<Error> timeout_refusal;
  std::optional<Error> early_release_refusal;
  {
    std::lock_guard guard(mu_);
    const auto it = entries_.find(session_key);
    if (it == entries_.end()) {
      return unexpected(missing_session_locked(session_key));
    }
    const auto expiry = expiry_locked(it->second);
    if (expiry) {
      if (event == LogicalSessionEvent::ReleaseAcknowledged) {
        auto before_timeout = it->second.machine;
        const auto accepted = before_timeout.apply(event);
        if (!accepted) {
          early_release_refusal = accepted.error();
        }
      }
      timeout_refusal = expiry_cause(it->second, *expiry);
      expired_transition =
          apply_locked(session_key, it->second, LogicalSessionEvent::Abort, timeout_refusal);
      if (!expired_transition) {
        return unexpected(Error::make(Error::Code::Internal, "session timeout transition refused",
                                      "timeout_transition_refused"));
      }
    }
  }
  if (expired_transition) {
    cv_.notify_one();
    emit(*expired_transition);
    if (from_client && timeout_refusal) {
      return unexpected(*timeout_refusal);
    }
    if (early_release_refusal) {
      return unexpected(*early_release_refusal);
    }
  }

  ManagedSessionTransition transition;
  std::optional<Error> refusal;
  bool reactivated = false;
  {
    std::lock_guard guard(mu_);
    const auto it = entries_.find(session_key);
    const auto before = it->second.machine.state();
    std::optional<ManagedSessionTransition> result;
    if (bytes) {
      refusal = charge_accepted_input(it->second, *bytes);
    }
    if (!refusal) {
      result = apply_locked(session_key, it->second, event, std::move(cause));
      if (!result) {
        refusal = refused_event(it->second.machine.state(), event);
      }
    }
    if (refusal) {
      result = apply_locked(session_key, it->second, LogicalSessionEvent::Fail, refusal);
    }
    if (!result) {
      return unexpected(Error::make(Error::Code::Internal, "session failure transition refused",
                                    "failure_transition_refused"));
    }
    transition = *result;
    reactivated =
        !refusal && event == LogicalSessionEvent::FinalizeCompleted && before != transition.state;
    if (transition.effects.contains(LogicalSessionEffect::ReleaseSlot)) {
      tombstones_.record(session_key, it->second.machine.generation(), transition.state,
                         transition.cause, clock_.now());
      entries_.erase(it);
    }
  }
  cv_.notify_one();
  emit(transition, reactivated);
  if (refusal) {
    return unexpected(*refusal);
  }
  return transition;
}

Result<void> SessionManager::check_generation(std::uint64_t session_key, std::uint64_t generation) {
  std::optional<Error> stale;
  {
    std::lock_guard serial(serial_mu_);
    std::lock_guard guard(mu_);
    const auto it = entries_.find(session_key);
    if (it == entries_.end()) {
      return unexpected(missing_session_locked(session_key));
    }
    const auto checked = it->second.machine.check_generation(generation);
    if (checked) {
      return {};
    }
    stale = checked.error();
  }
  // Fail changes nothing once the outcome is fixed or the slot is gone; the
  // message is stale either way.
  (void)apply_event(session_key, LogicalSessionEvent::Fail, stale, std::nullopt);
  return unexpected(*stale);
}

std::vector<ManagedSessionTransition> SessionManager::stop_admission_and_drain(Error cause) {
  std::lock_guard serial(serial_mu_);
  std::vector<ManagedSessionTransition> transitions;
  std::vector<std::uint64_t> keys;
  {
    std::lock_guard guard(mu_);
    admission_open_ = false;
    transitions.reserve(entries_.size());
    keys.reserve(entries_.size());
    for (const auto& [key, entry] : entries_) {
      (void)entry;
      keys.push_back(key);
    }
  }
  for (const auto key : keys) {
    std::optional<ManagedSessionTransition> transition;
    {
      std::lock_guard guard(mu_);
      const auto it = entries_.find(key);
      auto result = apply_locked(key, it->second, LogicalSessionEvent::Drain, cause);
      if (result && !result->effects.empty()) {
        transition = *result;
      }
    }
    if (transition) {
      transitions.push_back(*transition);
      emit(*transition);
    }
  }
  cv_.notify_one();
  return transitions;
}

std::vector<ManagedSessionTransition> SessionManager::sweep_due() {
  std::lock_guard serial(serial_mu_);
  std::vector<ManagedSessionTransition> transitions;
  std::vector<std::uint64_t> keys;
  {
    std::lock_guard guard(mu_);
    transitions.reserve(entries_.size());
    keys.reserve(entries_.size());
    for (const auto& [key, entry] : entries_) {
      (void)entry;
      keys.push_back(key);
    }
  }
  for (const auto key : keys) {
    std::optional<ManagedSessionTransition> transition;
    {
      std::lock_guard guard(mu_);
      const auto it = entries_.find(key);
      const auto expiry = expiry_locked(it->second);
      if (!expiry) {
        continue;
      }
      auto result = apply_locked(key, it->second, LogicalSessionEvent::Abort,
                                 expiry_cause(it->second, *expiry));
      if (result) {
        transition = *result;
      }
    }
    if (transition) {
      transitions.push_back(*transition);
      emit(*transition);
    }
  }
  cv_.notify_one();
  return transitions;
}

void SessionManager::notify_clock_advanced() noexcept {
  // The timer holds mu_ from reading the clock until it waits; passing
  // through mu_ here keeps this wake-up from landing in between and being lost.
  { const std::lock_guard guard(mu_); }
  cv_.notify_one();
}

Result<LogicalSessionState> SessionManager::state(std::uint64_t session_key) const {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  const auto it = entries_.find(session_key);
  if (it == entries_.end()) {
    return unexpected(missing_session_locked(session_key));
  }
  return it->second.machine.state();
}

Result<bool> SessionManager::heartbeat_due(std::uint64_t session_key) const {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  const auto it = entries_.find(session_key);
  if (it == entries_.end()) {
    return unexpected(missing_session_locked(session_key));
  }
  const auto state = it->second.machine.state();
  if (state != LogicalSessionState::Active && state != LogicalSessionState::Finalizing) {
    return false;
  }
  return it->second.timers.heartbeat_due(clock_.now());
}

Result<SchedulerClock::Duration> SessionManager::duration_remaining(
    std::uint64_t session_key) const {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  const auto it = entries_.find(session_key);
  if (it == entries_.end()) {
    return unexpected(missing_session_locked(session_key));
  }
  return it->second.timers.duration_remaining(clock_.now());
}

Result<LogicalSessionStatus> SessionManager::status(std::uint64_t session_key) const {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  const auto it = entries_.find(session_key);
  if (it == entries_.end()) {
    return unexpected(missing_session_locked(session_key));
  }
  return status_of(it->second);
}

std::optional<SessionTombstone> SessionManager::tombstone(std::uint64_t session_key) const {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  return tombstones_.find(session_key, clock_.now());
}

Result<LogicalSessionStatus> SessionManager::release_audio_input(std::uint64_t session_key,
                                                                 std::uint32_t chunks,
                                                                 std::uint64_t bytes) {
  return update_credit(session_key, [chunks, bytes](InputCredit& credit) {
    return credit.release_audio(chunks, bytes);
  });
}

Result<LogicalSessionStatus> SessionManager::start_text_segment(std::uint64_t session_key) {
  return update_credit(session_key, [](InputCredit& credit) { return credit.start_segment(); });
}

Result<LogicalSessionStatus> SessionManager::finish_text_segment(std::uint64_t session_key) {
  return update_credit(session_key, [](InputCredit& credit) { return credit.finish_segment(); });
}

Result<LogicalSessionStatus> SessionManager::update_credit(std::uint64_t session_key,
                                                           const CreditUpdate& update) {
  std::lock_guard serial(serial_mu_);
  std::optional<Error> defect;
  std::optional<ManagedSessionTransition> failed;
  ManagedSessionTransition returned;
  {
    std::lock_guard guard(mu_);
    const auto it = entries_.find(session_key);
    if (it == entries_.end()) {
      return unexpected(missing_session_locked(session_key));
    }
    Entry& entry = it->second;
    const auto updated = update(entry.credit);
    if (!updated) {
      defect = updated.error();
      failed = apply_locked(session_key, entry, LogicalSessionEvent::Fail, defect);
    }
    returned = ManagedSessionTransition{session_key, std::nullopt,     entry.machine.state(), {},
                                        entry.cause, status_of(entry), entry.output};
  }
  if (failed) {
    cv_.notify_one();
    emit(*failed);
  }
  if (defect) {
    return unexpected(*defect);
  }
  emit(returned, true);
  return returned.status;
}

LogicalSessionStatus SessionManager::status_of(const Entry& entry) {
  return LogicalSessionStatus{entry.machine.state(), entry.credit.depth(), entry.credit.usage(),
                              entry.output->pcm_usage(), entry.output->metadata_usage()};
}

Error SessionManager::expiry_cause(const Entry& entry, SessionExpiry expiry) {
  // While the finalize deadline runs, a session has a cause only if a drain
  // brought one: the worker's. It says why the session ends, so that a
  // client reopens elsewhere; the deadline only decides when.
  if (expiry == SessionExpiry::FinalizeDeadline && entry.cause) {
    return *entry.cause;
  }
  return expiry_error(expiry);
}

std::optional<SessionExpiry> SessionManager::expiry_locked(const Entry& entry) const {
  return entry.timers.expired(clock_.now(), entry.machine.state(), entry.output->stalled_since());
}

Error SessionManager::missing_session_locked(std::uint64_t session_key) const {
  if (tombstones_.find(session_key, clock_.now())) {
    return Error::make(Error::Code::NotReady, "logical session has ended", "session_ended");
  }
  return Error::make(Error::Code::NotReady, "logical session is no longer active",
                     "unknown_session");
}

std::size_t SessionManager::held_slots() const noexcept {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  return entries_.size();
}

bool SessionManager::admission_open() const noexcept {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  return admission_open_;
}

void SessionManager::emit(const ManagedSessionTransition& transition, bool announced) const {
  if (announced || !transition.effects.empty()) {
    sink_(transition);
  }
}

void SessionManager::run_timer() {
  for (;;) {
    {
      std::unique_lock lock(mu_);
      if (stop_timer_) {
        return;
      }
      std::optional<SchedulerClock::TimePoint> next;
      for (const auto& [key, entry] : entries_) {
        (void)key;
        const auto deadline =
            entry.timers.next_deadline(entry.machine.state(), entry.output->stalled_since());
        if (deadline && (!next || *deadline < *next)) {
          next = deadline;
        }
      }
      if (!next) {
        cv_.wait(lock);
      } else {
        const auto until_due = *next - clock_.now();
        const auto delay =
            std::clamp(std::chrono::duration_cast<std::chrono::milliseconds>(until_due),
                       std::chrono::milliseconds{0}, std::chrono::milliseconds{1000});
        cv_.wait_for(lock, delay);
      }
      if (stop_timer_) {
        return;
      }
    }
    (void)sweep_due();
  }
}
}  // namespace tensorplate::serving
