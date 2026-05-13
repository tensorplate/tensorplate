// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F05-T01: Python sidecar process supervisor implementation.

#include "sidecar_process.hpp"

#include <signal.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <cerrno>
#include <csignal>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/ipc/unix_socket.hpp"

namespace tensorplate::adapters::python_pytorch {

namespace {

std::atomic<std::uint64_t> g_next_socket_seq{0};

std::string make_socket_path() {
  const char* tmp_env = std::getenv("TMPDIR");
  std::filesystem::path tmp = (tmp_env != nullptr && *tmp_env != '\0')
                                  ? std::filesystem::path(tmp_env)
                                  : std::filesystem::temp_directory_path();
  const auto seq = g_next_socket_seq.fetch_add(1, std::memory_order_relaxed);
  return (tmp / ("tp_sidecar_" + std::to_string(::getpid()) + "_" + std::to_string(seq) + ".sock"))
      .string();
}

}  // namespace

// -----------------------------------------------------------------------------
// SidecarHandle
// -----------------------------------------------------------------------------

SidecarHandle SidecarHandle::make(int pid, TerminateFn terminate, IsAliveFn is_alive) {
  return SidecarHandle{pid, std::move(terminate), std::move(is_alive)};
}

SidecarHandle::SidecarHandle(int pid, TerminateFn terminate, IsAliveFn is_alive)
    : pid_(pid), terminate_(std::move(terminate)), is_alive_(std::move(is_alive)) {}

SidecarHandle::SidecarHandle(SidecarHandle&& other) noexcept
    : pid_(other.pid_),
      terminate_(std::move(other.terminate_)),
      is_alive_(std::move(other.is_alive_)) {
  other.pid_ = -1;
  other.terminate_ = {};
  other.is_alive_ = {};
}

SidecarHandle& SidecarHandle::operator=(SidecarHandle&& other) noexcept {
  if (this != &other) {
    terminate();
    pid_ = other.pid_;
    terminate_ = std::move(other.terminate_);
    is_alive_ = std::move(other.is_alive_);
    other.pid_ = -1;
    other.terminate_ = {};
    other.is_alive_ = {};
  }
  return *this;
}

SidecarHandle::~SidecarHandle() { terminate(); }

bool SidecarHandle::is_alive() const noexcept {
  if (!is_alive_) return false;
  return is_alive_(*this);
}

void SidecarHandle::terminate() noexcept {
  if (terminate_) {
    auto tf = std::move(terminate_);
    terminate_ = {};
    tf(*this);
  }
  pid_ = -1;
  is_alive_ = {};
}

// -----------------------------------------------------------------------------
// Default fork+exec launcher
// -----------------------------------------------------------------------------

namespace {

bool reap_pid(int pid, bool wait_for_exit) noexcept {
  if (pid <= 0) return true;
  const int flags = wait_for_exit ? 0 : WNOHANG;
  int status = 0;
  const int r = ::waitpid(pid, &status, flags);
  return r > 0 || (r == 0 && !wait_for_exit);
}

}  // namespace

SidecarLauncher default_fork_exec_launcher() {
  return [](const SidecarLaunchRequest& req) -> Result<SidecarHandle> {
    // Build argv before forking. After fork() we cannot allocate.
    std::vector<std::string> argv_storage;
    argv_storage.reserve(6 + req.extra_args.size());
    argv_storage.push_back(req.python_exe);
    argv_storage.emplace_back("-m");
    argv_storage.emplace_back("tensorplate_pytorch_backend");
    argv_storage.emplace_back("--socket");
    argv_storage.push_back(req.socket_path);
    for (const auto& e : req.extra_args) argv_storage.push_back(e);
    std::vector<char*> argv;
    argv.reserve(argv_storage.size() + 1);
    for (auto& s : argv_storage) argv.push_back(s.data());
    argv.push_back(nullptr);

    std::vector<std::string> env_storage = req.environment;
    std::vector<char*> envp_raw;
    envp_raw.reserve(env_storage.size() + 1);
    for (auto& e : env_storage) envp_raw.push_back(e.data());
    envp_raw.push_back(nullptr);

    const pid_t pid = ::fork();
    if (pid < 0) {
      return unexpected(Error::make(Error::Code::LoadFailed,
                                    std::string("fork failed: ") + std::strerror(errno)));
    }
    if (pid == 0) {
      // Child.
      if (!env_storage.empty()) {
        // Apply each env var with putenv. setenv copies, putenv does
        // not; we keep env_storage alive until exec which discards it.
        for (auto& e : env_storage) {
          ::putenv(e.data());
        }
      }
      ::execvp(req.python_exe.c_str(), argv.data());
      // Exec failed.
      std::_Exit(127);
    }
    // Parent.
    auto terminate_fn = [](SidecarHandle& h) {
      const int pid_local = h.pid();
      if (pid_local <= 0) return;
      ::kill(pid_local, SIGTERM);
      // Give the child a brief moment to clean up.
      for (int i = 0; i < 50; ++i) {
        int status = 0;
        const int r = ::waitpid(pid_local, &status, WNOHANG);
        if (r != 0) return;
        ::usleep(10 * 1000);
      }
      ::kill(pid_local, SIGKILL);
      (void)reap_pid(pid_local, /*wait_for_exit=*/true);
    };
    auto is_alive_fn = [](const SidecarHandle& h) {
      const int pid_local = h.pid();
      if (pid_local <= 0) return false;
      int status = 0;
      const int r = ::waitpid(pid_local, &status, WNOHANG);
      return r == 0;
    };
    return SidecarHandle::make(pid, std::move(terminate_fn), std::move(is_alive_fn));
  };
}

// -----------------------------------------------------------------------------
// SidecarProcess
// -----------------------------------------------------------------------------

Result<std::unique_ptr<SidecarProcess>> SidecarProcess::start(const SidecarLaunchRequest& req,
                                                              const SidecarLauncher& launcher,
                                                              TimePoint deadline) {
  if (!launcher) {
    return unexpected(Error::Code::ConfigInvalid, "sidecar launcher must not be null");
  }

  auto proc = std::unique_ptr<SidecarProcess>(new SidecarProcess);
  proc->socket_path_ =
      req.socket_path.empty() ? make_socket_path() : req.socket_path;

  auto listener_r = ipc::UnixSocket::create_stream();
  if (!listener_r.has_value()) return unexpected(listener_r.error());
  proc->listener_ = std::move(listener_r).value();
  if (auto r = proc->listener_.bind_and_listen(proc->socket_path_); !r.has_value()) {
    std::filesystem::remove(proc->socket_path_);
    return unexpected(r.error());
  }

  SidecarLaunchRequest effective = req;
  effective.socket_path = proc->socket_path_;
  auto handle_r = launcher(effective);
  if (!handle_r.has_value()) {
    std::filesystem::remove(proc->socket_path_);
    return unexpected(handle_r.error());
  }
  proc->handle_ = std::move(handle_r).value();

  auto accept_r = proc->listener_.accept(deadline);
  if (!accept_r.has_value()) {
    proc->handle_.terminate();
    std::filesystem::remove(proc->socket_path_);
    return unexpected(accept_r.error());
  }
  proc->client_ = std::move(accept_r).value();
  // Listener no longer needed; close it so the path can be unlinked.
  proc->listener_.close();
  return proc;
}

SidecarProcess::~SidecarProcess() { shutdown(); }

bool SidecarProcess::is_alive() const noexcept { return handle_.is_alive(); }

void SidecarProcess::shutdown() noexcept {
  client_.close();
  listener_.close();
  handle_.terminate();
  if (!socket_path_.empty()) {
    std::error_code ec;
    std::filesystem::remove(socket_path_, ec);
    socket_path_.clear();
  }
}

}  // namespace tensorplate::adapters::python_pytorch
