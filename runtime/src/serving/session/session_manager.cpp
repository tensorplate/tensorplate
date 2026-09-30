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
Error missing_session() {
  return Error::make(Error::Code::NotReady, "logical session is no longer active",
                     "unknown_session");
}

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
    auto [it, inserted] = entries_.emplace(key, Entry{*machine, SessionTimers{limits_, now}, {}});
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
      const auto expiry = it->second.timers.expired(clock_.now(), it->second.machine.state());
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
  entry.timers.on_client_activity(event, clock_.now());
  return ManagedSessionTransition{key, after, *effects, entry.cause};
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
  std::lock_guard serial(serial_mu_);
  const bool from_client = client_event(event);
  std::optional<ManagedSessionTransition> expired_transition;
  std::optional<Error> timeout_refusal;
  std::optional<Error> early_release_refusal;
  {
    std::lock_guard guard(mu_);
    const auto it = entries_.find(session_key);
    if (it == entries_.end()) {
      return unexpected(missing_session());
    }
    const auto expiry = it->second.timers.expired(clock_.now(), it->second.machine.state());
    if (expiry) {
      if (event == LogicalSessionEvent::ReleaseAcknowledged) {
        auto before_timeout = it->second.machine;
        const auto accepted = before_timeout.apply(event);
        if (!accepted) {
          early_release_refusal = accepted.error();
        }
      }
      timeout_refusal = expiry_error(*expiry);
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
  {
    std::lock_guard guard(mu_);
    const auto it = entries_.find(session_key);
    auto result = apply_locked(session_key, it->second, event, std::move(cause));
    if (!result) {
      refusal = refused_event(it->second.machine.state(), event);
      result = apply_locked(session_key, it->second, LogicalSessionEvent::Fail, refusal);
    }
    if (!result) {
      return unexpected(Error::make(Error::Code::Internal, "session failure transition refused",
                                    "failure_transition_refused"));
    }
    transition = *result;
    if (transition.effects.contains(LogicalSessionEffect::ReleaseSlot)) {
      entries_.erase(it);
    }
  }
  cv_.notify_one();
  emit(transition);
  if (refusal) {
    return unexpected(*refusal);
  }
  return transition;
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
      const auto expiry = it->second.timers.expired(clock_.now(), it->second.machine.state());
      if (!expiry) {
        continue;
      }
      auto result =
          apply_locked(key, it->second, LogicalSessionEvent::Abort, expiry_error(*expiry));
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
  cv_.notify_one();
}

Result<LogicalSessionState> SessionManager::state(std::uint64_t session_key) const {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  const auto it = entries_.find(session_key);
  if (it == entries_.end()) {
    return unexpected(missing_session());
  }
  return it->second.machine.state();
}

Result<bool> SessionManager::heartbeat_due(std::uint64_t session_key) const {
  std::lock_guard serial(serial_mu_);
  std::lock_guard guard(mu_);
  const auto it = entries_.find(session_key);
  if (it == entries_.end()) {
    return unexpected(missing_session());
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
    return unexpected(missing_session());
  }
  return it->second.timers.duration_remaining(clock_.now());
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

void SessionManager::emit(const ManagedSessionTransition& transition) const {
  if (!transition.effects.empty()) {
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
        const auto deadline = entry.timers.next_deadline(entry.machine.state());
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
