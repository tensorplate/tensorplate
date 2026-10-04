// SPDX-License-Identifier: Apache-2.0
//
// Stand-in for what surrounds a SessionManager in a serving worker: the
// stream that writes each session's lifecycle messages into its output
// queue, the backend that completes finalizations and drains and
// acknowledges release on a thread of its own, and optionally the peer that
// reads every queue. Thread-safe.
//
// stop() must run before the manager is destroyed: the backend reports to it.

#pragma once

#include <array>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/scheduler/clock.hpp"

#include "serving/session/session_manager.hpp"

namespace tensorplate::testing {

/// Body of a lifecycle message: the effect that asked for it.
struct LifecycleMessage final : serving::OutputBody {
  explicit LifecycleMessage(serving::LogicalSessionEffect effect_in) : effect(effect_in) {}
  serving::LogicalSessionEffect effect;
};

class SyntheticSessionHost {
 public:
  /// A message the peer read: its kind and, for a lifecycle message, the
  /// effect that asked for it.
  struct Sent {
    serving::OutputKind kind = serving::OutputKind::Control;
    std::optional<serving::LogicalSessionEffect> lifecycle;
  };

  /// What one session's stream and peer saw.
  struct Session {
    /// How often each effect was handed to the stream, by effect value.
    std::array<std::uint32_t, 10> effects{};
    std::uint32_t terminals_written = 0;
    std::optional<std::string> terminal_cause;
    serving::LogicalSessionState state = serving::LogicalSessionState::Opening;
    std::shared_ptr<serving::BoundedOutputQueue> output;
    /// The messages the peer read, in sending order.
    std::vector<Sent> sent;
    /// When the peer read CancelAccepted.
    std::optional<std::chrono::steady_clock::time_point> cancel_accepted_at;

    [[nodiscard]] std::uint32_t count(serving::LogicalSessionEffect effect) const {
      return effects.at(static_cast<std::size_t>(effect));
    }
  };

  explicit SyntheticSessionHost(const SchedulerClock& clock, bool start_peer = false)
      : clock_(clock), backend_([this] { run_backend(); }) {
    if (start_peer) {
      peer_ = std::thread([this] { run_peer(); });
    }
  }
  ~SyntheticSessionHost() { stop(); }
  SyntheticSessionHost(const SyntheticSessionHost&) = delete;
  SyntheticSessionHost& operator=(const SyntheticSessionHost&) = delete;

  /// The manager whose sink this is.
  void attach(serving::SessionManager& manager) {
    {
      const std::lock_guard guard(mu_);
      manager_ = &manager;
    }
    cv_.notify_all();
  }

  void stop() {
    {
      const std::lock_guard guard(mu_);
      stopping_ = true;
    }
    cv_.notify_all();
    if (backend_.joinable()) {
      backend_.join();
    }
    if (peer_.joinable()) {
      peer_.join();
    }
  }

  /// A backend busy with a job reports nothing until it is unblocked.
  void block_backend(bool blocked) {
    {
      const std::lock_guard guard(mu_);
      backend_blocked_ = blocked;
    }
    cv_.notify_all();
  }

  void on_transition(const serving::ManagedSessionTransition& transition) {
    using Effect = serving::LogicalSessionEffect;
    using Event = serving::LogicalSessionEvent;
    {
      const std::lock_guard guard(mu_);
      auto& seen = sessions_[transition.session_key];
      seen.state = transition.state;
      seen.output = transition.output;
      transition.effects.for_each([&](Effect effect) {
        ++seen.effects.at(static_cast<std::size_t>(effect));
        switch (effect) {
          case Effect::EmitReady:
          case Effect::EmitCancelAccepted:
            (void)write(seen, serving::OutputKind::Control, effect, 0);
            break;
          case Effect::EmitReply:
            (void)write(seen, serving::OutputKind::Control, effect, 1);
            break;
          case Effect::EmitTerminal:
            if (write(seen, serving::OutputKind::Terminal, effect, 0)) {
              ++seen.terminals_written;
            }
            seen.terminal_cause = transition.cause ? transition.cause->context : std::nullopt;
            break;
          case Effect::StartFinalize:
            reports_.emplace_back(transition.session_key, Event::FinalizeCompleted);
            break;
          case Effect::StartDrain:
            reports_.emplace_back(transition.session_key, Event::DrainCompleted);
            break;
          case Effect::RequestCleanup:
            reports_.emplace_back(transition.session_key, Event::ReleaseAcknowledged);
            break;
          case Effect::ReleaseSlot:
            ++released_;
            break;
          default:
            break;
        }
      });
    }
    cv_.notify_all();
  }

  [[nodiscard]] Session session(std::uint64_t key) const {
    const std::lock_guard guard(mu_);
    const auto it = sessions_.find(key);
    return it == sessions_.end() ? Session{} : it->second;
  }

  /// Waits until `count` sessions have released their slot.
  [[nodiscard]] bool wait_released(std::size_t count, std::chrono::milliseconds timeout) const {
    std::unique_lock lock(mu_);
    return cv_.wait_for(lock, timeout, [&] { return released_ >= count; });
  }

  /// Waits until the peer has read `key`'s CancelAccepted or terminal outcome.
  [[nodiscard]] bool wait_sent(std::uint64_t key, serving::LogicalSessionEffect effect,
                               std::chrono::milliseconds timeout) const {
    std::unique_lock lock(mu_);
    return cv_.wait_for(lock, timeout, [&] {
      const auto it = sessions_.find(key);
      if (it == sessions_.end()) {
        return false;
      }
      if (effect == serving::LogicalSessionEffect::EmitCancelAccepted) {
        return it->second.cancel_accepted_at.has_value();
      }
      return !it->second.sent.empty() &&
             it->second.sent.back().kind == serving::OutputKind::Terminal;
    });
  }

  /// A producer queued task output for the peer to read.
  void output_written() {
    {
      const std::lock_guard guard(mu_);
      ++written_;
    }
    cv_.notify_all();
  }

  /// Reads everything every queue holds, as the peer does.
  void read_all() {
    const std::lock_guard reading(read_mu_);
    std::vector<std::pair<std::uint64_t, std::shared_ptr<serving::BoundedOutputQueue>>> outputs;
    {
      const std::lock_guard guard(mu_);
      for (const auto& [key, seen] : sessions_) {
        outputs.emplace_back(key, seen.output);
      }
    }
    for (const auto& [key, output] : outputs) {
      while (auto message = output->take()) {
        const auto read_at = std::chrono::steady_clock::now();
        (void)output->delivered(message->sequence, clock_.now());
        const std::lock_guard guard(mu_);
        auto& seen = sessions_[key];
        const auto* lifecycle = dynamic_cast<const LifecycleMessage*>(message->item.body.get());
        seen.sent.push_back(Sent{message->item.kind, lifecycle != nullptr
                                                         ? std::optional{lifecycle->effect}
                                                         : std::nullopt});
        if (lifecycle != nullptr &&
            lifecycle->effect == serving::LogicalSessionEffect::EmitCancelAccepted) {
          seen.cancel_accepted_at = read_at;
        }
      }
    }
    cv_.notify_all();
  }

 private:
  using Report = std::pair<std::uint64_t, serving::LogicalSessionEvent>;

  /// Caller holds mu_.
  [[nodiscard]] bool write(Session& seen, serving::OutputKind kind,
                           serving::LogicalSessionEffect effect, std::uint64_t replace_key) {
    serving::OutputItem item{kind, 16, replace_key, std::make_unique<LifecycleMessage>(effect)};
    const auto offered = seen.output->offer(item, clock_.now());
    ++written_;
    return offered && *offered == serving::OutputOffer::Queued;
  }

  void run_backend() {
    std::unique_lock lock(mu_);
    for (;;) {
      cv_.wait(lock, [&] {
        return stopping_ || (!backend_blocked_ && manager_ != nullptr && !reports_.empty());
      });
      if (stopping_) {
        return;
      }
      const auto [key, event] = reports_.front();
      reports_.pop_front();
      auto* manager = manager_;
      lock.unlock();
      // One report completes every finalization owed, so it is sent only
      // while the session is still finalizing; only this thread ends that.
      if (event != serving::LogicalSessionEvent::FinalizeCompleted ||
          manager->state(key).value_or(serving::LogicalSessionState::Closed) ==
              serving::LogicalSessionState::Finalizing) {
        (void)manager->apply(key, event);
      }
      lock.lock();
    }
  }

  void run_peer() {
    std::uint64_t read = 0;
    for (;;) {
      {
        std::unique_lock lock(mu_);
        cv_.wait(lock, [&] { return stopping_ || written_ != read; });
        if (stopping_) {
          return;
        }
        read = written_;
      }
      read_all();
    }
  }

  const SchedulerClock& clock_;
  std::mutex read_mu_;
  mutable std::mutex mu_;
  mutable std::condition_variable cv_;
  serving::SessionManager* manager_ = nullptr;
  std::map<std::uint64_t, Session> sessions_;
  std::deque<Report> reports_;
  std::size_t released_ = 0;
  std::uint64_t written_ = 0;
  bool backend_blocked_ = false;
  bool stopping_ = false;
  std::thread backend_;
  std::thread peer_;
};
}  // namespace tensorplate::testing
