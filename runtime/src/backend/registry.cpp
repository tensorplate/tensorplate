// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01-T02 / T03: BackendRegistry implementation.

#include "tensorplate/backend/registry.hpp"

#include <algorithm>
#include <mutex>
#include <utility>

#include "tensorplate/core/error.hpp"

namespace tensorplate {

BackendRegistry& BackendRegistry::global() {
  static BackendRegistry instance;
  return instance;
}

Result<void> BackendRegistry::register_backend(BackendEntry entry) {
  if (entry.backend_name.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "backend_name must not be empty");
  }
  if (!entry.factory) {
    return unexpected(Error::Code::ConfigInvalid,
                      "backend factory must not be null for backend " + entry.backend_name);
  }
  if (entry.capability.backend_name() != entry.backend_name) {
    return unexpected(Error::Code::ConfigInvalid,
                      "capability.backend_name disagrees with entry.backend_name");
  }

  std::lock_guard<std::mutex> lock(mutex_);
  const auto& key = entry.backend_name;
  if (entries_.find(key) != entries_.end()) {
    return unexpected(Error::Code::Internal, "backend already registered: " + key);
  }
  entries_.emplace(key, std::move(entry));
  return Result<void>{};
}

bool BackendRegistry::deregister_backend(std::string_view backend_name) {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = entries_.find(std::string(backend_name));
  if (it == entries_.end()) return false;
  entries_.erase(it);
  return true;
}

bool BackendRegistry::is_registered(std::string_view backend_name) const {
  std::lock_guard<std::mutex> lock(mutex_);
  return entries_.find(std::string(backend_name)) != entries_.end();
}

std::vector<std::string> BackendRegistry::registered_backends() const {
  std::lock_guard<std::mutex> lock(mutex_);
  std::vector<std::string> names;
  names.reserve(entries_.size());
  for (const auto& kv : entries_) {
    names.push_back(kv.first);
  }
  std::sort(names.begin(), names.end());
  return names;
}

Result<BackendCapability> BackendRegistry::capability(std::string_view backend_name) const {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = entries_.find(std::string(backend_name));
  if (it == entries_.end()) {
    return unexpected(Error::Code::Unsupported,
                      "backend not registered: " + std::string(backend_name));
  }
  return it->second.capability;
}

Result<std::unique_ptr<ExecutionSession>> BackendRegistry::create_session(
    std::string_view backend_name, ExecutionSessionRuntimeHooks hooks) const {
  ExecutionSessionFactory factory;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    auto it = entries_.find(std::string(backend_name));
    if (it == entries_.end()) {
      return unexpected(Error::Code::Unsupported,
                        "backend not registered: " + std::string(backend_name));
    }
    factory = it->second.factory;
  }
  // Invoke the factory outside the mutex so adapter init (which may
  // open files, spawn sidecars, or probe the GPU) cannot deadlock the
  // registry.
  return factory(hooks);
}

Result<void> BackendRegistry::validate_backend_hint(const ModelSpec& spec) const {
  const std::string& hint = spec.backend_hint();
  auto cap_r = capability(hint);
  if (!cap_r.has_value()) {
    return unexpected(Error::Code::Unsupported,
                      "bundle backend_hint '" + hint + "' is not registered on this device");
  }
  const auto& cap = cap_r.value();
  if (!cap.accepts_precision(spec.precision_hint())) {
    return unexpected(Error::make(
        Error::Code::Unsupported, "declared precision is not supported by backend",
        "backend=" + hint +
            ", requested=" + std::string(to_string(spec.precision_hint()))));
  }
  return Result<void>{};
}

}  // namespace tensorplate
