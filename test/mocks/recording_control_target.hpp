// SPDX-License-Identifier: Apache-2.0
//
// A ControlTarget that records what the control channel asked of it and
// answers with fixed values.

#pragma once

#include <chrono>
#include <cstdint>
#include <mutex>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "serving/control_channel.hpp"

namespace tensorplate::testing {

/// Records what the channel asked of it and answers with fixed values.
class RecordingTarget final : public serving::ControlTarget {
 public:
  ipc::WorkerLedger ledger_status() override {
    record("ledger_status");
    return ledger;
  }
  Result<void> admission_fence() override {
    record("admission_fence");
    return {};
  }
  Result<ipc::WorkerQuota> activate() override {
    record("activate");
    return quota;
  }
  Result<void> retire(std::chrono::milliseconds drain_timeout) override {
    record("retire " + std::to_string(drain_timeout.count()));
    return unexpected(Error::Code::NotReady, "still loading");
  }
  Result<ipc::WorkerQuota> assign_quota(const ipc::WorkerQuota& assigned) override {
    record("assign_quota " + std::to_string(assigned.session_count));
    return assigned;
  }
  Result<void> apply_pressure(const ipc::WorkerPressure& pressure) override {
    record("apply_pressure " + std::to_string(pressure.count.value_or(0)));
    return unexpected(Error::Code::Timeout, "pressure not applied in time");
  }

  [[nodiscard]] std::vector<std::string> calls() const {
    const std::lock_guard guard(mu_);
    return calls_;
  }

  ipc::WorkerLedger ledger{1, 2, 1, 8, {11, 12, 13}};
  ipc::WorkerQuota quota{8, {std::uint64_t{1} << 30U, std::nullopt, std::nullopt}};

 private:
  void record(std::string call) {
    const std::lock_guard guard(mu_);
    calls_.push_back(std::move(call));
  }
  mutable std::mutex mu_;
  std::vector<std::string> calls_;
};
}  // namespace tensorplate::testing
