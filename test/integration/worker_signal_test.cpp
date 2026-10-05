// SPDX-License-Identifier: Apache-2.0

#include <fcntl.h>
#include <gtest/gtest.h>
#include <unistd.h>

#include <chrono>
#include <csignal>
#include <filesystem>
#include <fstream>
#include <nlohmann/json.hpp>
#include <sstream>
#include <string>
#include <thread>
#include <tuple>
#include <vector>

#include "tensorplate/serving/worker.hpp"

#include "control_socket_harness.hpp"

#ifdef TP_SERVING_WORKER_BINARY

namespace tensorplate {
namespace {
using namespace std::chrono_literals;

class WorkerSignal : public ::testing::TestWithParam<std::tuple<bool, int>> {
 protected:
  void SetUp() override {
    std::string pattern = (std::filesystem::temp_directory_path() / "tp-signal-XXXXXX").string();
    const auto* created = ::mkdtemp(pattern.data());
    ASSERT_NE(created, nullptr);
    dir_ = created;
  }

  void TearDown() override { std::filesystem::remove_all(dir_); }

  std::string config(bool sidecar = false) const {
    nlohmann::json deployment = {{"use_mock_session", !sidecar}, {"endpoint", "signal-test"}};
    if (std::get<0>(GetParam())) {
      deployment["generation"] = 7;
    }
    if (sidecar) {
      deployment["backend"] = "python_pytorch";
      deployment["model"] = {{"model_id", "signal-test"},
                             {"model_class", "custom"},
                             {"artifact_path", "/dev/null"},
                             {"backend_hint", "python_pytorch"}};
    }
    return nlohmann::json{{"schema_version", "0.1"},
                          {"bind", {{"host", "127.0.0.1"}, {"port", 0}}},
                          {"deployment", deployment},
                          {"enable_stderr_logs", true}}
        .dump();
  }

  testing::ChildProcess launch(bool sidecar = false) {
    const testing::OwnedFd null_input{::open("/dev/null", O_RDONLY)};
    std::vector<std::string> environment;
    if (sidecar) {
      const auto wrapper = dir_ / "sidecar";
      {
        std::ofstream out(wrapper);
        out << "#!/bin/sh\nexec \"${TP_TEST_PYTHON:-python3}\" \"$TP_SIGNAL_SIDECAR\" \"$@\"\n";
      }
      std::filesystem::permissions(wrapper, std::filesystem::perms::owner_all);
      environment = {"TP_PYTHON_PYTORCH_EXECUTABLE=" + wrapper.string(),
                     "TP_SIGNAL_SIDECAR=" + (std::filesystem::path{TP_SOURCE_DIR} /
                                             "test/mocks/worker_signal_sidecar.py")
                                                .string(),
                     "TP_SIGNAL_TEST_DIR=" + dir_.string()};
    }
    auto worker = testing::ChildProcess::spawn(
        {TP_SERVING_WORKER_BINARY, "--config-json", config(sidecar)},
        std::get<0>(GetParam()) ? control_.worker.get() : null_input.get(), dir_ / "stderr.txt",
        environment);
    control_.worker.reset();
    return worker;
  }

  bool await_log(testing::ChildProcess& worker, const char* message) const {
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (std::chrono::steady_clock::now() < deadline) {
      if (worker.stderr_text().find(message) != std::string::npos) {
        return true;
      }
      std::this_thread::sleep_for(5ms);
    }
    return false;
  }

  bool await_gate(const char* stage) const {
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (std::chrono::steady_clock::now() < deadline) {
      if (std::filesystem::exists(dir_ / stage)) {
        return true;
      }
      std::this_thread::sleep_for(5ms);
    }
    return false;
  }

  void release(const char* stage) const { std::ofstream{dir_ / (std::string{stage} + "-release")}; }

  void expect_clean_exit(testing::ChildProcess& worker) const {
    const auto status = worker.wait_exit(5s);
    ASSERT_TRUE(status.has_value()) << worker.stderr_text();
    EXPECT_EQ(*status, static_cast<int>(ServingExitCode::Ok)) << worker.stderr_text();
    std::istringstream logs(worker.stderr_text());
    std::string line;
    int requested = 0;
    int complete = 0;
    while (std::getline(logs, line)) {
      const auto record = nlohmann::json::parse(line, nullptr, false);
      if (!record.is_object()) {
        continue;
      }
      if (record.value("message", "") == "shutdown requested") {
        ++requested;
        EXPECT_EQ(record["fields"].value("reason", ""),
                  std::get<1>(GetParam()) == SIGINT ? "SIGINT" : "SIGTERM");
      }
      if (record.value("message", "") == "shutdown complete") {
        ++complete;
      }
    }
    EXPECT_EQ(requested, 1) << worker.stderr_text();
    EXPECT_EQ(complete, 1) << worker.stderr_text();
  }

  std::filesystem::path dir_;
  testing::ControlSocketPair control_ = testing::ControlSocketPair::create();
};

TEST_P(WorkerSignal, ReadyWorkerExitsCleanly) {
  auto worker = launch();
  ASSERT_TRUE(await_log(worker, "http server bound")) << worker.stderr_text();
  ASSERT_EQ(::kill(worker.pid(), SIGPIPE), 0);
  ASSERT_FALSE(worker.wait_exit(30ms).has_value()) << worker.stderr_text();
  ASSERT_EQ(::kill(worker.pid(), std::get<1>(GetParam())), 0);
  expect_clean_exit(worker);
}

#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST_P(WorkerSignal, SignalDuringLoadIsRetained) {
  release("unload");
  auto worker = launch(true);
  ASSERT_TRUE(await_gate("load")) << worker.stderr_text();
  ASSERT_EQ(::kill(worker.pid(), std::get<1>(GetParam())), 0);
  // Keep load blocked long enough to deliver the signal before releasing it.
  EXPECT_FALSE(worker.wait_exit(100ms).has_value());
  release("load");
  expect_clean_exit(worker);
  EXPECT_EQ(worker.stderr_text().find("http server bound"), std::string::npos);
}

TEST_P(WorkerSignal, SecondSignalDuringShutdownDoesNotReenter) {
  release("load");
  auto worker = launch(true);
  ASSERT_TRUE(await_log(worker, "http server bound")) << worker.stderr_text();
  ASSERT_EQ(::kill(worker.pid(), std::get<1>(GetParam())), 0);
  ASSERT_TRUE(await_gate("unload")) << worker.stderr_text();
  const int second = std::get<1>(GetParam()) == SIGINT ? SIGTERM : SIGINT;
  ASSERT_EQ(::kill(worker.pid(), second), 0);
  EXPECT_FALSE(worker.wait_exit(100ms).has_value());
  release("unload");
  expect_clean_exit(worker);
}
#endif

INSTANTIATE_TEST_SUITE_P(Process, WorkerSignal,
                         ::testing::Combine(::testing::Bool(), ::testing::Values(SIGTERM, SIGINT)),
                         [](const ::testing::TestParamInfo<WorkerSignal::ParamType>& info) {
                           return std::string{std::get<0>(info.param) ? "Member" : "Plain"} +
                                  (std::get<1>(info.param) == SIGINT ? "Interrupt" : "Terminate");
                         });

}  // namespace
}  // namespace tensorplate

#endif
