// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F01-T02: Scheduler policy registry and factory implementation.

#include "tensorplate/scheduler/factory.hpp"

#include <algorithm>
#include <mutex>
#include <string>
#include <utility>

#include "scheduler/fifo_scheduler.hpp"

namespace tensorplate {

namespace {

// Validate fields shared across all scheduler policies. Policy-specific
// fields (none in v0.1.0 beyond the keys here) are validated by the
// concrete factory closure.
Result<void> validate_common_config(const SchedulerConfig& config) {
  if (config.policy.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "scheduler policy is required");
  }
  if (config.queue_capacity == 0) {
    return unexpected(Error::Code::ConfigInvalid, "scheduler queue_capacity must be > 0");
  }
  if (config.in_flight_capacity == 0) {
    return unexpected(Error::Code::ConfigInvalid, "scheduler in_flight_capacity must be > 0");
  }
  if (config.deadline_margin.count() < 0) {
    return unexpected(Error::Code::ConfigInvalid, "scheduler deadline_margin must be >= 0");
  }
  if (config.default_service_estimate.count() < 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "scheduler default_service_estimate must be >= 0");
  }
  return {};
}

}  // namespace

SchedulerPolicyRegistry& SchedulerPolicyRegistry::global() {
  static SchedulerPolicyRegistry instance;
  static std::once_flag once;
  std::call_once(once, [&]() { register_builtin_scheduler_policies(instance); });
  return instance;
}

Result<void> SchedulerPolicyRegistry::register_policy(std::string policy_name,
                                                      SchedulerFactoryFn factory) {
  if (policy_name.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "scheduler policy name is required");
  }
  if (!factory) {
    return unexpected(Error::Code::ConfigInvalid, "scheduler factory closure must not be null");
  }
  std::lock_guard<std::mutex> guard(mutex_);
  if (factories_.find(policy_name) != factories_.end()) {
    return unexpected(Error::Code::Internal, "scheduler policy already registered");
  }
  factories_.emplace(std::move(policy_name), std::move(factory));
  return {};
}

bool SchedulerPolicyRegistry::deregister_policy(std::string_view policy_name) {
  std::lock_guard<std::mutex> guard(mutex_);
  auto it = factories_.find(std::string{policy_name});
  if (it == factories_.end()) {
    return false;
  }
  factories_.erase(it);
  return true;
}

bool SchedulerPolicyRegistry::is_registered(std::string_view policy_name) const {
  std::lock_guard<std::mutex> guard(mutex_);
  return factories_.find(std::string{policy_name}) != factories_.end();
}

std::vector<std::string> SchedulerPolicyRegistry::registered_policies() const {
  std::lock_guard<std::mutex> guard(mutex_);
  std::vector<std::string> keys;
  keys.reserve(factories_.size());
  for (const auto& kv : factories_) {
    keys.push_back(kv.first);
  }
  std::sort(keys.begin(), keys.end());
  return keys;
}

Result<std::unique_ptr<InferScheduler>> SchedulerPolicyRegistry::create(
    const SchedulerConfig& config, SchedulerRuntimeHooks hooks) const {
  if (auto v = validate_common_config(config); !v) {
    return unexpected(std::move(v).error());
  }
  SchedulerFactoryFn factory;
  {
    std::lock_guard<std::mutex> guard(mutex_);
    auto it = factories_.find(config.policy);
    if (it == factories_.end()) {
      return unexpected(Error::Code::Unsupported,
                        std::string{"unknown scheduler policy: "} + config.policy);
    }
    factory = it->second;
  }
  return factory(config, hooks);
}

void register_builtin_scheduler_policies(SchedulerPolicyRegistry& registry) {
  // FIFO is the v0.1.0 default. Errors from the FIFO factory closure
  // forward unchanged so config-time problems are visible to callers.
  (void)registry.register_policy(
      "fifo",
      [](const SchedulerConfig& config,
         SchedulerRuntimeHooks hooks) -> Result<std::unique_ptr<InferScheduler>> {
        return FifoScheduler::create(config, hooks);
      });
}

Result<std::unique_ptr<InferScheduler>> make_scheduler(const SchedulerConfig& config,
                                                       SchedulerRuntimeHooks hooks) {
  return SchedulerPolicyRegistry::global().create(config, hooks);
}

Result<void> validate_scheduler_config(const SchedulerConfig& config) {
  if (auto v = validate_common_config(config); !v) {
    return v;
  }
  if (!SchedulerPolicyRegistry::global().is_registered(config.policy)) {
    return unexpected(Error::Code::Unsupported,
                      std::string{"unknown scheduler policy: "} + config.policy);
  }
  return {};
}

}  // namespace tensorplate
