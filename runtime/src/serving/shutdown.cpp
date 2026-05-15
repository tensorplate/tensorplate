// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/shutdown.hpp"

#include <array>
#include <utility>

namespace tensorplate::serving {

namespace {

constexpr std::array<std::pair<ShutdownPhase, std::string_view>, 4> kNames{{
    {ShutdownPhase::Running, "running"},
    {ShutdownPhase::Stopping, "stopping"},
    {ShutdownPhase::Draining, "draining"},
    {ShutdownPhase::Stopped, "stopped"},
}};

}  // namespace

std::string_view to_string(ShutdownPhase phase) noexcept {
  for (const auto& [k, v] : kNames) {
    if (k == phase) {
      return v;
    }
  }
  return "running";
}

ShutdownController::ShutdownController() = default;

void ShutdownController::request(std::string reason) noexcept {
  ShutdownPhase expected = ShutdownPhase::Running;
  if (phase_.compare_exchange_strong(expected, ShutdownPhase::Stopping)) {
    {
      std::lock_guard<std::mutex> g(mutex_);
      reason_ = std::move(reason);
    }
    cv_.notify_all();
  }
}

bool ShutdownController::is_stopping() const noexcept {
  return phase_.load() != ShutdownPhase::Running;
}

ShutdownPhase ShutdownController::phase() const noexcept {
  return phase_.load();
}

void ShutdownController::enter_draining() noexcept {
  ShutdownPhase expected = ShutdownPhase::Stopping;
  phase_.compare_exchange_strong(expected, ShutdownPhase::Draining);
}

void ShutdownController::enter_stopped() noexcept {
  phase_.store(ShutdownPhase::Stopped);
}

std::optional<std::string> ShutdownController::reason() const {
  std::lock_guard<std::mutex> g(mutex_);
  return reason_;
}

ServingState ShutdownController::serving_state() const noexcept {
  switch (phase_.load()) {
    case ShutdownPhase::Running:
      return ServingState::Ready;
    case ShutdownPhase::Stopping:
      return ServingState::Stopping;
    case ShutdownPhase::Draining:
      return ServingState::Draining;
    case ShutdownPhase::Stopped:
    default:
      return ServingState::Stopped;
  }
}

void ShutdownController::wait_for_request() noexcept {
  std::unique_lock<std::mutex> g(mutex_);
  cv_.wait(g, [this] { return phase_.load() != ShutdownPhase::Running; });
}

void ShutdownController::notify_request() noexcept {
  cv_.notify_all();
}

}  // namespace tensorplate::serving
