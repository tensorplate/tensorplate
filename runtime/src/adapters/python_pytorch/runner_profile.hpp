// SPDX-License-Identifier: Apache-2.0
//
// Installed runner profiles, as the sidecar launcher reads them from the
// backend descriptor (`protocol/schemas/backend_descriptor.json`).

#pragma once

#include <filesystem>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/core/result.hpp"

#include "sidecar_process.hpp"

namespace tensorplate::adapters::python_pytorch {

/// Where one installed runner profile's sidecar runs.
struct RunnerEnvironment {
  std::string interpreter;
  std::string environment_root;
  std::vector<std::string> library_search_paths;

  friend bool operator==(const RunnerEnvironment&, const RunnerEnvironment&) = default;
};

/// One entry of the descriptor's merged `runner_profiles`.
struct InstalledRunnerProfile {
  std::string id;
  RunnerEnvironment environment;
};

/// The backend's descriptor in the installed package channel:
/// `TP_BACKEND_DESCRIPTOR_DIR` when set, the packaged directory otherwise.
/// A relative override is `ConfigInvalid`.
[[nodiscard]] Result<std::filesystem::path> installed_descriptor_path();

/// The runner profiles of the descriptor at `descriptor_path` as installed:
/// its own `runner_profiles`, then the `*.json` declarations of
/// `runner_profiles.d/` beside it in file name order.
///
/// Refuses the whole list with `ConfigInvalid`, never one entry: a file that
/// cannot be read or parsed, a declaration of another backend, an entry that
/// breaks a runner profile rule, or an id declared twice. Of the descriptor
/// itself only `backend_name`, `schema_version` and `runner_profiles` are
/// read, and whether a profile's packages are installed is not asked: both
/// are the agent's checks.
[[nodiscard]] Result<std::vector<InstalledRunnerProfile>> read_installed_runner_profiles(
    const std::filesystem::path& descriptor_path);

/// The environment of profile `id` in that list; `Unsupported` when the
/// descriptor declares no such profile.
[[nodiscard]] Result<RunnerEnvironment> read_runner_environment(
    const std::filesystem::path& descriptor_path, std::string_view id);

/// The launch of a sidecar in `environment`: its interpreter, and a child
/// environment holding exactly the profile's library search path, the
/// onnxruntime telemetry switch and `temp_dir` as `TMPDIR`.
///
/// `Unavailable` when the interpreter is not an executable file, or when
/// `temp_dir` is not a directory this process can write to and search on a
/// filesystem that allows execution.
[[nodiscard]] Result<SidecarLaunchRequest> runner_launch_request(
    const RunnerEnvironment& environment, const std::filesystem::path& temp_dir);

}  // namespace tensorplate::adapters::python_pytorch
