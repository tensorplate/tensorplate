// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F01-T02: Scheduler factory and policy registry.
//
// The factory creates a concrete InferScheduler from a SchedulerConfig
// without exposing the concrete type to callers. v0.1.0 supports the
// "fifo" policy; unknown policy strings return a typed
// Error::Code::Unsupported. Adding a new scheduler implementation only
// requires registering a factory closure here; executor/serving-mode
// code is unaffected.

#pragma once

#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

#include "tensorplate/core/result.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace tensorplate {

/// Factory closure that constructs a concrete InferScheduler.
using SchedulerFactoryFn = std::function<Result<std::unique_ptr<InferScheduler>>(
    const SchedulerConfig& config, SchedulerRuntimeHooks hooks)>;

/// Process-wide policy registry. The runtime registers the built-in
/// "fifo" policy at first use through `register_builtin_scheduler_policies`.
/// Tests construct local `SchedulerPolicyRegistry` instances to avoid
/// leaking state across cases.
class SchedulerPolicyRegistry {
 public:
  SchedulerPolicyRegistry() = default;
  ~SchedulerPolicyRegistry() = default;

  SchedulerPolicyRegistry(const SchedulerPolicyRegistry&) = delete;
  SchedulerPolicyRegistry& operator=(const SchedulerPolicyRegistry&) = delete;
  SchedulerPolicyRegistry(SchedulerPolicyRegistry&&) = delete;
  SchedulerPolicyRegistry& operator=(SchedulerPolicyRegistry&&) = delete;

  /// Process-wide registry. The first call installs the built-in
  /// "fifo" policy through `register_builtin_scheduler_policies`.
  static SchedulerPolicyRegistry& global();

  /// Register a policy under `policy_name`. Duplicate registration
  /// returns `Error::Code::Internal`; empty name or null factory
  /// returns `ConfigInvalid`.
  Result<void> register_policy(std::string policy_name, SchedulerFactoryFn factory);

  /// Remove a previously registered policy. Returns false if the
  /// policy was not registered. v0.1.0 only uses this from tests.
  bool deregister_policy(std::string_view policy_name);

  /// True iff `policy_name` is registered.
  [[nodiscard]] bool is_registered(std::string_view policy_name) const;

  /// Sorted list of registered policy keys. Used by status output
  /// and scheduler-config error messages.
  [[nodiscard]] std::vector<std::string> registered_policies() const;

  /// Construct a scheduler for `config.policy` against `hooks`.
  /// Returns:
  ///   - ConfigInvalid : malformed config (e.g. queue_capacity == 0).
  ///   - Unsupported   : `config.policy` is not registered.
  ///   - any error returned by the registered factory closure.
  [[nodiscard]] Result<std::unique_ptr<InferScheduler>> create(
      const SchedulerConfig& config, SchedulerRuntimeHooks hooks = {}) const;

 private:
  mutable std::mutex mutex_;
  std::unordered_map<std::string, SchedulerFactoryFn> factories_;
};

/// Register the built-in v0.1.0 scheduler policies ("fifo") onto
/// `registry`. Safe to call multiple times; duplicates return
/// Error::Code::Internal which the caller ignores in this path.
void register_builtin_scheduler_policies(SchedulerPolicyRegistry& registry);

/// Convenience: construct a scheduler from `config` using the
/// process-wide policy registry. Forwards typed errors from
/// SchedulerPolicyRegistry::create.
[[nodiscard]] Result<std::unique_ptr<InferScheduler>> make_scheduler(
    const SchedulerConfig& config, SchedulerRuntimeHooks hooks = {});

/// Validate `config` without constructing a scheduler. The factory
/// runs the same validation internally; this entry point lets agent /
/// CLI config-parsing code surface errors before serving worker start.
[[nodiscard]] Result<void> validate_scheduler_config(const SchedulerConfig& config);

}  // namespace tensorplate
