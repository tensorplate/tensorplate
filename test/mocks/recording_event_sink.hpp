// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F06: Test sinks that record every `SessionEvent` the NVI
// wrapper emits. Shared across T1/T2 tests so event ordering and
// payload assertions live in one place.

#pragma once

#include <atomic>
#include <cstddef>
#include <mutex>
#include <stdexcept>
#include <utility>
#include <vector>

#include "tensorplate/core/execution_session.hpp"

namespace tensorplate::testing {

/// Recording sink that captures every `SessionEvent` for later
/// assertions. Thread-safe.
class RecordingEventSink final : public SessionEventSink {
 public:
  void on_event(const SessionEvent& event) override {
    std::lock_guard<std::mutex> guard(mu_);
    events_.push_back(event);
  }

  [[nodiscard]] std::vector<SessionEvent> events() const {
    std::lock_guard<std::mutex> guard(mu_);
    return events_;
  }

  [[nodiscard]] std::size_t size() const noexcept {
    std::lock_guard<std::mutex> guard(mu_);
    return events_.size();
  }

  void clear() noexcept {
    std::lock_guard<std::mutex> guard(mu_);
    events_.clear();
  }

 private:
  mutable std::mutex mu_;
  std::vector<SessionEvent> events_;
};

/// Sink that throws from `on_event` to prove the NVI wrapper swallows
/// the exception and continues without corrupting session state.
class ThrowingEventSink final : public SessionEventSink {
 public:
  void on_event(const SessionEvent& /*event*/) override {
    ++calls_;
    throw std::runtime_error("event sink intentionally throwing");
  }

  [[nodiscard]] std::size_t calls() const noexcept { return calls_.load(); }

 private:
  std::atomic<std::size_t> calls_{0};
};

}  // namespace tensorplate::testing
