// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F05-T01: Python sidecar process supervisor - internal header.
//
// `SidecarProcess` owns the lifecycle of one Python sidecar subprocess
// and the Unix-domain socket the C++ adapter exchanges frames on. A
// sidecar is started by the adapter at `do_load` time and torn down by
// the adapter at `do_unload` time; the supervisor is also responsible
// for killing the process when the adapter detects a transport failure
// or malformed response, so the OS does not retain a zombie. Active
// heartbeat polling is owned by the scheduler/supervision layer.

#pragma once

#include <chrono>
#include <functional>
#include <memory>
#include <string>
#include <vector>

#include "tensorplate/core/result.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

namespace tensorplate::adapters::python_pytorch {

/// Sidecar launcher abstraction. The default launcher runs
/// `fork()` + `execvp(python_exe, ["-m", "tensorplate_pytorch_backend",
/// "--socket", socket_path])`. Tests inject a launcher that connects
/// the `SidecarRunner` in the parent process via a socketpair so the
/// C++ adapter can be exercised without spawning a real subprocess.
struct SidecarLaunchRequest {
  /// Absolute path to the Unix-domain socket the supervisor has bound.
  std::string socket_path;
  /// Python interpreter to invoke (defaults to "python3").
  std::string python_exe = "python3";
  /// Additional CLI arguments forwarded to the sidecar (e.g.
  /// `--default-backend fixture`).
  std::vector<std::string> extra_args;
  /// Environment overrides (key=value) applied to the child.
  std::vector<std::string> environment;
};

/// Opaque process handle. The fork+exec launcher uses a real pid; the
/// in-process test launcher uses pid = 0 and is_alive returns the
/// thread state. The non-default constructor exists only for the
/// launcher implementations.
class SidecarHandle {
 public:
  using TerminateFn = std::function<void(SidecarHandle&)>;
  using IsAliveFn = std::function<bool(SidecarHandle&)>;

  SidecarHandle() noexcept = default;

  static SidecarHandle make(int pid, TerminateFn terminate, IsAliveFn is_alive);

  ~SidecarHandle();
  SidecarHandle(const SidecarHandle&) = delete;
  SidecarHandle& operator=(const SidecarHandle&) = delete;
  SidecarHandle(SidecarHandle&& other) noexcept;
  SidecarHandle& operator=(SidecarHandle&& other) noexcept;

  [[nodiscard]] int pid() const noexcept { return pid_; }
  [[nodiscard]] bool is_alive() noexcept;
  void mark_exited() noexcept;
  void terminate() noexcept;

 private:
  SidecarHandle(int pid, TerminateFn terminate, IsAliveFn is_alive);

  int pid_ = -1;
  TerminateFn terminate_;
  IsAliveFn is_alive_;
};

using SidecarLauncher = std::function<Result<SidecarHandle>(const SidecarLaunchRequest&)>;

/// Default launcher: fork+exec the Python interpreter with the runner
/// module. The child connects to the socket the parent has bound;
/// the parent accepts the connection in `SidecarProcess::start`.
[[nodiscard]] SidecarLauncher default_fork_exec_launcher();

/// Owns one sidecar process and its connected socket.
class SidecarProcess {
 public:
  using Clock = ipc::UnixSocket::Clock;
  using TimePoint = ipc::UnixSocket::TimePoint;

  /// Start the sidecar:
  ///   1. Pick a socket path in TMPDIR.
  ///   2. Create + bind + listen on that path.
  ///   3. Invoke the launcher.
  ///   4. Accept the child's connection.
  ///   5. Switch the socket to non-blocking and return.
  /// On any failure, the bound socket is unlinked and the launched
  /// process (if any) is terminated.
  [[nodiscard]] static Result<std::unique_ptr<SidecarProcess>> start(
      const SidecarLaunchRequest& req, const SidecarLauncher& launcher, TimePoint deadline);

  ~SidecarProcess();
  SidecarProcess(const SidecarProcess&) = delete;
  SidecarProcess& operator=(const SidecarProcess&) = delete;

  [[nodiscard]] ipc::UnixSocket& socket() noexcept { return client_; }
  [[nodiscard]] const ipc::UnixSocket& socket() const noexcept { return client_; }
  [[nodiscard]] bool is_alive() noexcept;

  /// Terminate the child and unlink the socket path. Idempotent.
  void shutdown() noexcept;

 private:
  SidecarProcess() = default;

  std::string socket_path_;
  ipc::UnixSocket listener_;
  ipc::UnixSocket client_;
  SidecarHandle handle_;
};

}  // namespace tensorplate::adapters::python_pytorch
