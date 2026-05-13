// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01-T02 / T03: Public backend registry.
//
// `BackendRegistry` maps stable backend key strings (e.g. "tensorrt",
// "libtorch", "python_pytorch") to a (capability, factory) pair. The
// serving worker and the agent obtain `ExecutionSession` instances by
// asking the registry; no caller above this header should branch on the
// concrete adapter type.
//
// Registration is explicit: adapters call `register_builtin_backends`
// (or a test helper for fakes) on a registry instance. The runtime
// exposes a process-wide instance through `BackendRegistry::global()`
// for production code while tests construct their own registry to keep
// global state out of unit tests.
//
// Bundle backend-hint validation:
//   `validate_backend_hint` checks that the declared `backend_hint`
//   maps to a registered backend and that the declared
//   `precision_hint` is accepted by the backend's capability record.
//   Validation does not consult device probing or fallback heuristics:
//   v0.1.0 fails fast with a typed error and never silently redirects.

#pragma once

#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

/// Factory closure that constructs a concrete ExecutionSession. The
/// closure may fail with a typed error (most commonly `LoadFailed` or
/// `ConfigInvalid`) if the adapter cannot be initialized at all on the
/// host (e.g., missing CUDA library, missing Python interpreter path).
/// The factory is *not* called from the constructor of the registry;
/// it runs at session-creation time so registration is cheap.
using ExecutionSessionFactory =
    std::function<Result<std::unique_ptr<ExecutionSession>>(ExecutionSessionRuntimeHooks)>;

/// One registry entry: stable backend key, capability record, and the
/// factory that builds a session. Equality is identity-by-name; the
/// factory closure is not compared.
struct BackendEntry {
  std::string backend_name;
  BackendCapability capability;
  ExecutionSessionFactory factory;
};

/// Thread-safe execution-backend registry. The registry owns
/// `BackendEntry` records by value; once registered, references and
/// pointers to a record remain valid until the registry is destroyed
/// or the record is explicitly removed.
class BackendRegistry {
 public:
  BackendRegistry() = default;
  ~BackendRegistry() = default;

  BackendRegistry(const BackendRegistry&) = delete;
  BackendRegistry& operator=(const BackendRegistry&) = delete;
  BackendRegistry(BackendRegistry&&) = delete;
  BackendRegistry& operator=(BackendRegistry&&) = delete;

  /// Process-wide registry. Adapters that opt into static-init
  /// registration use this instance; production code obtains sessions
  /// from it. Tests should construct local `BackendRegistry` instances
  /// instead to avoid leaking state across tests.
  static BackendRegistry& global();

  /// Register an adapter. The `capability.backend_name()` must equal
  /// `entry.backend_name`. Duplicate registration of the same key
  /// returns `ConfigInvalid` so deterministic adapter lists survive
  /// build-flag misconfiguration.
  ///
  /// Returns:
  ///   - `ConfigInvalid` if `backend_name` is empty, if `factory` is
  ///     null, or if `capability.backend_name()` disagrees with
  ///     `backend_name`.
  ///   - `Internal` if the key is already registered.
  Result<void> register_backend(BackendEntry entry);

  /// Remove a previously registered backend. Returns false if the key
  /// was not registered. v0.1.0 only uses this from tests; production
  /// code never deregisters.
  bool deregister_backend(std::string_view backend_name);

  /// True iff `backend_name` is registered.
  [[nodiscard]] bool is_registered(std::string_view backend_name) const;

  /// Stable, sorted list of registered backend keys. Used by
  /// `tensorplate status` and bundle-error messages.
  [[nodiscard]] std::vector<std::string> registered_backends() const;

  /// Fetch a backend's capability record. Returns `Unsupported` for an
  /// unknown backend so bundle validation can surface a precise error.
  [[nodiscard]] Result<BackendCapability> capability(std::string_view backend_name) const;

  /// Construct a fresh, unloaded `ExecutionSession` for `backend_name`.
  /// The factory may surface adapter-initialization errors
  /// (`LoadFailed`, `ConfigInvalid`, `OOMError`); the registry
  /// forwards them unchanged.
  [[nodiscard]] Result<std::unique_ptr<ExecutionSession>> create_session(
      std::string_view backend_name, ExecutionSessionRuntimeHooks hooks = {}) const;

  /// Validate a model spec's `backend_hint` and precision hint against
  /// the registry. The bundle pipeline calls this before staging:
  ///   - Unknown backend -> `Unsupported`.
  ///   - Declared precision is not in the backend's supported list ->
  ///     `Unsupported`.
  /// Validation does not silently rewrite the spec.
  [[nodiscard]] Result<void> validate_backend_hint(const ModelSpec& spec) const;

 private:
  mutable std::mutex mutex_;
  std::unordered_map<std::string, BackendEntry> entries_;
};

}  // namespace tensorplate
