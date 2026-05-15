// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/health.hpp"

#include <array>
#include <chrono>
#include <mutex>
#include <nlohmann/json.hpp>
#include <utility>

#include "tensorplate/core/error.hpp"

namespace tensorplate {

namespace {

constexpr std::array<std::pair<ServingState, std::string_view>, 7> kNames{{
    {ServingState::Starting, "starting"},
    {ServingState::Ready, "ready"},
    {ServingState::Degraded, "degraded"},
    {ServingState::Failed, "failed"},
    {ServingState::Stopping, "stopping"},
    {ServingState::Draining, "draining"},
    {ServingState::Stopped, "stopped"},
}};

}  // namespace

std::string_view to_string(ServingState state) noexcept {
  for (const auto& [k, v] : kNames) {
    if (k == state) {
      return v;
    }
  }
  return "starting";
}

std::optional<ServingState> serving_state_from_string(std::string_view name) noexcept {
  for (const auto& [k, v] : kNames) {
    if (v == name) {
      return k;
    }
  }
  return std::nullopt;
}

void HealthState::set_identity(std::string endpoint, std::string backend,
                               std::optional<std::string> model_id) noexcept {
  std::lock_guard<std::mutex> g(mutex_);
  snap_.endpoint = std::move(endpoint);
  snap_.backend = std::move(backend);
  snap_.active_model_id = std::move(model_id);
}

void HealthState::set_state(ServingState next) noexcept {
  std::lock_guard<std::mutex> g(mutex_);
  if (snap_.state != next) {
    snap_.state = next;
    snap_.state_since_steady_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(
                                      std::chrono::steady_clock::now().time_since_epoch())
                                      .count();
  }
}

void HealthState::record_error(Error error) noexcept {
  std::lock_guard<std::mutex> g(mutex_);
  snap_.last_error_code = error.code;
  snap_.last_error_message = std::move(error.message);
}

void HealthState::record_queue_state(std::size_t queue_depth, std::size_t in_flight) noexcept {
  std::lock_guard<std::mutex> g(mutex_);
  snap_.queue_depth = queue_depth;
  snap_.in_flight = in_flight;
}

ServingState HealthState::state() const noexcept {
  std::lock_guard<std::mutex> g(mutex_);
  return snap_.state;
}

HealthSnapshot HealthState::snapshot() const {
  std::lock_guard<std::mutex> g(mutex_);
  return snap_;
}

std::string serialize_health_json(const HealthSnapshot& snap) {
  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["state"] = std::string{to_string(snap.state)};
  j["endpoint"] = snap.endpoint;
  j["backend"] = snap.backend;
  if (snap.active_model_id.has_value()) {
    j["active_model_id"] = *snap.active_model_id;
  }
  if (snap.last_error_code.has_value()) {
    j["last_error_code"] = std::string{to_string(*snap.last_error_code)};
  }
  if (snap.last_error_message.has_value()) {
    j["last_error_message"] = *snap.last_error_message;
  }
  j["queue_depth"] = snap.queue_depth;
  j["in_flight"] = snap.in_flight;
  j["state_since_steady_ns"] = snap.state_since_steady_ns;
  return j.dump();
}

int health_http_status(ServingState state) noexcept {
  switch (state) {
    case ServingState::Ready:
    case ServingState::Degraded:
      return 200;
    case ServingState::Starting:
    case ServingState::Failed:
    case ServingState::Stopping:
    case ServingState::Draining:
    case ServingState::Stopped:
    default:
      return 503;
  }
}

}  // namespace tensorplate
