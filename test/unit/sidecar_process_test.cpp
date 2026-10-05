// SPDX-License-Identifier: Apache-2.0
//
// Regression coverage for Python sidecar process ownership.

#include <gtest/gtest.h>

#if !defined(TP_ENABLE_PYTHON_PYTORCH_SIDECAR) || !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(SidecarProcess, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <chrono>
#include <filesystem>
#include <fstream>
#include <string>
#include <thread>
#include <utility>

#include "adapters/python_pytorch/sidecar_process.hpp"
#include "control_socket_harness.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

TEST(SidecarProcess, ReapedLivenessDisarmsTerminateCallback) {
  bool terminate_called = false;
  SidecarHandle handle = SidecarHandle::make(
      12345, [&](SidecarHandle&) { terminate_called = true; },
      [](SidecarHandle& h) {
        h.mark_exited();
        return false;
      });

  EXPECT_FALSE(handle.is_alive());
  handle.terminate();

  EXPECT_FALSE(terminate_called);
  EXPECT_EQ(handle.pid(), -1);
}

std::string short_lived_executable() {
  for (const char* path : {"/usr/bin/true", "/bin/true"}) {
    if (std::filesystem::exists(path)) {
      return path;
    }
  }
  return {};
}

TEST(SidecarProcess, LivenessReapClearsPidBeforeTerminate) {
  const std::string executable = short_lived_executable();
  if (executable.empty()) {
    GTEST_SKIP() << "No true executable available for short-lived child test";
  }

  SidecarLaunchRequest req;
  req.python_exe = executable;
  req.socket_path = "/tmp/tp_sidecar_unused.sock";

  auto handle_r = default_fork_exec_launcher()(req);
  ASSERT_TRUE(handle_r.has_value()) << handle_r.error().message;
  SidecarHandle handle = std::move(handle_r).value();
  ASSERT_GT(handle.pid(), 0);

  bool alive = true;
  for (int i = 0; i < 100; ++i) {
    alive = handle.is_alive();
    if (!alive) {
      break;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
  }

  ASSERT_FALSE(alive);
  EXPECT_EQ(handle.pid(), -1);

  handle.terminate();
  EXPECT_EQ(handle.pid(), -1);
}

// The launched program lists the descriptors it was started with: whatever
// the launching process held open, only its 0, 1 and 2 may reach it.
TEST(SidecarProcess, LaunchedChildInheritsNoDescriptorAboveTwo) {
#if !defined(__linux__)
  GTEST_SKIP() << "reads /proc/self/fd";
#else
  const auto dir =
      std::filesystem::temp_directory_path() / ("tp-sidecar-fds-" + std::to_string(::getpid()));
  std::filesystem::create_directories(dir);
  const auto script = dir / "fake-sidecar";
  const auto report = dir / "fds.txt";
  const auto probe = dir / "probe.txt";
  {
    // ls inherits what the shell was started with, plus the directory it
    // reads; its own stdout is the listing.
    std::ofstream out(script);
    out << "#!/bin/sh\n"
        << "echo \"$TP_LAUNCH_PROBE\" > \"$TP_PROBE_FILE\"\n"
        << "ls -l /proc/self/fd > \"$TP_FD_REPORT.tmp\"\n"
        << "mv \"$TP_FD_REPORT.tmp\" \"$TP_FD_REPORT\"\n";
  }
  std::filesystem::permissions(script, std::filesystem::perms::owner_all);

  // What a worker holds when it launches its sidecar, none of it
  // close-on-exec: both ends of a socket pair, a pipe and an open file.
  int pair[2] = {-1, -1};
  int pipe_ends[2] = {-1, -1};
  ASSERT_EQ(::socketpair(AF_UNIX, SOCK_STREAM, 0, pair), 0);
  ASSERT_EQ(::pipe(pipe_ends), 0);
  const testing::OwnedFd first{pair[0]};
  const testing::OwnedFd second{pair[1]};
  const testing::OwnedFd read_end{pipe_ends[0]};
  const testing::OwnedFd write_end{pipe_ends[1]};
  const testing::OwnedFd file{::open((dir / "held.txt").c_str(), O_WRONLY | O_CREAT, 0600)};
  ASSERT_GE(file.get(), 0);
  const auto own = [](int fd) {
    return std::filesystem::read_symlink("/proc/self/fd/" + std::to_string(fd)).string();
  };
  const std::string own_stdin = own(STDIN_FILENO);
  const std::string own_stderr = own(STDERR_FILENO);

  // An override replaces a variable the launching process has.
  ASSERT_EQ(::setenv("TP_LAUNCH_PROBE", "from-the-parent", 1), 0);
  SidecarLaunchRequest req;
  req.python_exe = script.string();
  req.socket_path = (dir / "unused.sock").string();
  req.environment = {"TP_FD_REPORT=" + report.string(), "TP_PROBE_FILE=" + probe.string(),
                     "TP_LAUNCH_PROBE=from-the-request"};
  auto handle_r = default_fork_exec_launcher()(req);
  ::unsetenv("TP_LAUNCH_PROBE");
  ASSERT_TRUE(handle_r.has_value()) << handle_r.error().message;
  SidecarHandle handle = std::move(handle_r).value();
  for (int i = 0; i < 500 && !std::filesystem::exists(report); ++i) {
    std::this_thread::sleep_for(std::chrono::milliseconds(10));
  }
  ASSERT_TRUE(std::filesystem::exists(report));
  handle.terminate();

  std::ifstream in(report);
  std::string line;
  int standard = 0;
  while (std::getline(in, line)) {
    const auto arrow = line.find(" -> ");
    if (arrow == std::string::npos) {
      continue;
    }
    const int fd = std::stoi(line.substr(line.rfind(' ', arrow - 1) + 1));
    const std::string target = line.substr(arrow + 4);
    if (fd == STDIN_FILENO) {
      EXPECT_EQ(target, own_stdin) << line;
    } else if (fd == STDOUT_FILENO) {
      EXPECT_EQ(target, report.string() + ".tmp") << line;
    } else if (fd == STDERR_FILENO) {
      EXPECT_EQ(target, own_stderr) << line;
    } else {
      // The one descriptor ls opens itself.
      EXPECT_TRUE(target.rfind("/proc/", 0) == 0 && target.ends_with("/fd")) << line;
    }
    standard += fd <= STDERR_FILENO ? 1 : 0;
    for (const int held : {first.get(), second.get()}) {
      EXPECT_EQ(target.find("socket:[" + std::to_string(testing::inode_of(held)) + "]"),
                std::string::npos)
          << line;
    }
  }
  EXPECT_EQ(standard, 3);
  std::ifstream probed(probe);
  std::string seen;
  std::getline(probed, seen);
  EXPECT_EQ(seen, "from-the-request");
  std::filesystem::remove_all(dir);
#endif
}

// Both ways of closing: one kernel call where there is one, and the scan
// that replaces it elsewhere. Run in a child, which reports by exit status.
TEST(SidecarProcess, InheritedDescriptorsAreClosedWithAndWithoutCloseRange) {
  for (const bool try_close_range : {true, false}) {
    SCOPED_TRACE(try_close_range);
    const pid_t pid = ::fork();
    ASSERT_GE(pid, 0);
    if (pid == 0) {
      const int low = ::dup(STDERR_FILENO);
      const int high = ::fcntl(STDERR_FILENO, F_DUPFD, 200);
      detail::close_inherited_descriptors(256, try_close_range);
      const auto closed = [](int fd) { return ::fcntl(fd, F_GETFD) == -1 && errno == EBADF; };
      const bool kept = !closed(STDIN_FILENO) && !closed(STDOUT_FILENO) && !closed(STDERR_FILENO);
      ::_exit(low > STDERR_FILENO && high >= 200 && closed(low) && closed(high) && kept ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(::waitpid(pid, &status, 0), pid);
    EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
  }
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
