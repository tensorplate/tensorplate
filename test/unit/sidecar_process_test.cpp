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
#include <string>
#include <thread>
#include <utility>

#include "adapters/python_pytorch/sidecar_process.hpp"

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

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
