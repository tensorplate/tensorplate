// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F05: Python/PyTorch sidecar adapter - internal header.
//
// This is the C++ adapter that supervises one Python sidecar process
// per execution session and translates `ExecutionSession` lifecycle
// calls into the schema-defined messages on the Unix-domain socket
// (V01-E05-F04). Adapter and sidecar communicate exclusively over the
// socket; the adapter does not expose Python types in any public
// runtime header.

#pragma once

#include <chrono>
#include <filesystem>
#include <memory>
#include <string>
#include <string_view>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/result.hpp"

#include "job_channel.hpp"
#include "sidecar_process.hpp"

namespace tensorplate::adapters::python_pytorch {

inline constexpr std::string_view kBackendName = "python_pytorch";

/// Adapter configuration tunables. Defaults are chosen for the v0.1.0
/// SmolVLA validation flow on Jetson Orin Nano 8GB Super.
struct PythonPytorchConfig {
  /// Interpreter for a deployment that selects no runner profile.
  std::string python_exe = "python3";
  /// Backend descriptor a deployment's runner profile is resolved against;
  /// empty selects the installed one.
  std::filesystem::path descriptor_path;
  std::chrono::milliseconds startup_timeout{std::chrono::seconds{15}};
  std::chrono::milliseconds infer_timeout{std::chrono::seconds{30}};
  std::chrono::milliseconds health_timeout{std::chrono::seconds{2}};
  /// The load enables `speech_jobs_v1` and hands the socket to the job
  /// channel. Set only for a session built to run speech jobs: the sidecar
  /// refuses such a load for a runner that runs none.
  bool speech_jobs = false;
  /// Longest the job channel's writer waits for the sidecar to take a frame.
  std::chrono::milliseconds write_timeout{std::chrono::seconds{5}};
};

[[nodiscard]] BackendCapability make_python_pytorch_capability();

/// A session that starts its sidecar through `launcher`. The registered
/// backend passes the fork+exec launcher.
[[nodiscard]] std::unique_ptr<ExecutionSession> make_python_pytorch_session(
    ExecutionSessionRuntimeHooks hooks, PythonPytorchConfig config, SidecarLauncher launcher);

/// A session and its job channel. The session starts the channel on a load
/// that enabled jobs (`config.speech_jobs`), admits jobs once prime's health
/// check has passed, closes admission before it unloads, and stops the
/// channel when it unloads, is destroyed or loses the connection. A holder of
/// `channel` sets the sink and submits, cancels and releases; it never
/// starts or stops. Without `config.speech_jobs` the channel is never
/// started and refuses every job. With it, a load without a buffer manager
/// in `hooks` is refused (Error::Code::ConfigInvalid) before a sidecar is
/// started.
///
/// A stop waits for a running sink callback and delivers on the calling
/// thread, and prime, unload and the destructor can all stop: none of them
/// is called from a sink callback or while holding a lock a callback takes.
/// The buffer manager of `hooks` outlives `channel`.
///
/// unload() cancels nothing. The sidecar answers an unload only once the job
/// it is running has ended, so the call waits behind that job for up to
/// `config.startup_timeout`. A job that outlasts the bound makes the unload
/// fail with Error::Code::Timeout: open jobs then fail with kBackendReset,
/// not kSessionUnloaded, and the sidecar is ended. Ending or cancelling open
/// jobs before an unload is the holder's.
struct SessionWithJobChannel {
  std::unique_ptr<ExecutionSession> session;
  std::shared_ptr<JobChannel> channel;
};

[[nodiscard]] SessionWithJobChannel make_python_pytorch_session_with_channel(
    ExecutionSessionRuntimeHooks hooks, PythonPytorchConfig config, SidecarLauncher launcher);

}  // namespace tensorplate::adapters::python_pytorch

namespace tensorplate {

[[nodiscard]] Result<void> register_python_pytorch_backend(BackendRegistry& registry);

}  // namespace tensorplate
