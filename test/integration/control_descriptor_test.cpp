// SPDX-License-Identifier: Apache-2.0
//
// The serving worker binary and the control descriptor it inherits as fd 0.

#include <fcntl.h>
#include <gtest/gtest.h>
#include <unistd.h>

#include <chrono>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <nlohmann/json.hpp>
#include <optional>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include "tensorplate/ipc/worker_control.hpp"
#include "tensorplate/serving/worker.hpp"

#include "control_socket_harness.hpp"

#ifdef TP_SERVING_WORKER_BINARY

namespace tensorplate {
namespace {
using namespace std::chrono_literals;

constexpr std::uint64_t kGeneration = 7;
constexpr const char* kDeployment = "member-under-test";

/// A serving config for a member of a deployment generation: the agent
/// controls such a worker over the socket it passes as fd 0.
std::string member_config(std::uint16_t port) {
  const nlohmann::json config = {
      {"schema_version", "0.1"},
      {"bind", {{"host", "127.0.0.1"}, {"port", port}}},
      {"deployment",
       {{"use_mock_session", true}, {"endpoint", kDeployment}, {"generation", kGeneration}}},
      {"enable_stderr_logs", false}};
  return config.dump();
}

std::filesystem::path scratch_dir(const char* name) {
  const auto dir = std::filesystem::temp_directory_path() /
                   (std::string{"tp-control-"} + name + "-" + std::to_string(::getpid()));
  std::filesystem::create_directories(dir);
  return dir;
}

/// The last line of a worker's stderr, parsed.
nlohmann::json last_stderr_record(const std::string& text) {
  std::istringstream lines(text);
  std::string line;
  std::string last;
  while (std::getline(lines, line)) {
    if (!line.empty()) {
      last = line;
    }
  }
  return nlohmann::json::parse(last, nullptr, false);
}

TEST(ControlDescriptor, MemberWithoutItsControlSocketFailsToStartAndNeverListens) {
  const auto dir = scratch_dir("absent");
  const auto port = testing::unused_loopback_port();
  const testing::OwnedFd null_input{::open("/dev/null", O_RDONLY)};
  ASSERT_GE(null_input.get(), 0);
  auto worker =
      testing::ChildProcess::spawn({TP_SERVING_WORKER_BINARY, "--config-json", member_config(port)},
                                   null_input.get(), dir / "stderr.txt");

  std::optional<int> exit_status;
  bool listened = false;
  const auto give_up = std::chrono::steady_clock::now() + 5s;
  while (!exit_status && std::chrono::steady_clock::now() < give_up) {
    listened = listened || testing::loopback_port_listening(port);
    exit_status = worker.wait_exit(20ms);
  }
  EXPECT_FALSE(listened);
  ASSERT_TRUE(exit_status) << "the worker kept running without a control socket";
  EXPECT_EQ(*exit_status, static_cast<int>(ServingExitCode::LoadError));
  const auto record = last_stderr_record(worker.stderr_text());
  ASSERT_TRUE(record.is_object()) << worker.stderr_text();
  EXPECT_EQ(record.value("component", ""), "serving");
  EXPECT_EQ(record.value("message", ""), "worker startup failed");
  EXPECT_EQ(record["fields"].value("code", ""), "unavailable");
  std::filesystem::remove_all(dir);
}
TEST(ControlDescriptor, MemberWhoseAgentEndIsAlreadyClosedFailsToStart) {
  const auto dir = scratch_dir("closed");
  const auto port = testing::unused_loopback_port();
  auto pair = testing::ControlSocketPair::create();
  pair.agent.reset();
  auto worker =
      testing::ChildProcess::spawn({TP_SERVING_WORKER_BINARY, "--config-json", member_config(port)},
                                   pair.worker.get(), dir / "stderr.txt");
  const auto exit_status = worker.wait_exit(5s);
  ASSERT_TRUE(exit_status) << "the worker kept running without its agent";
  EXPECT_EQ(*exit_status, static_cast<int>(ServingExitCode::LoadError));
  EXPECT_FALSE(testing::loopback_port_listening(port));
  const auto record = last_stderr_record(worker.stderr_text());
  ASSERT_TRUE(record.is_object()) << worker.stderr_text();
  EXPECT_EQ(record["fields"].value("code", ""), "unavailable");
  std::filesystem::remove_all(dir);
}

#if defined(__linux__)

/// Descriptor number and target of every entry of /proc/<pid>/fd.
std::vector<std::pair<int, std::string>> descriptors_of(pid_t pid) {
  std::vector<std::pair<int, std::string>> found;
  const std::filesystem::path dir = "/proc/" + std::to_string(pid) + "/fd";
  for (const auto& entry : std::filesystem::directory_iterator(dir)) {
    std::error_code failed;
    const auto target = std::filesystem::read_symlink(entry.path(), failed);
    if (!failed) {
      found.emplace_back(std::stoi(entry.path().filename().string()), target.string());
    }
  }
  return found;
}

TEST(ControlDescriptor, MemberAnswersItsAgentAndKeepsTheSocketPrivate) {
  const auto dir = scratch_dir("member");
  const auto port = testing::unused_loopback_port();
  auto pair = testing::ControlSocketPair::create();
  const std::string worker_end =
      "socket:[" + std::to_string(testing::inode_of(pair.worker.get())) + "]";
  auto worker =
      testing::ChildProcess::spawn({TP_SERVING_WORKER_BINARY, "--config-json", member_config(port)},
                                   pair.worker.get(), dir / "stderr.txt");
  pair.worker.reset();

  ipc::WorkerControlRequest request;
  request.transaction_id = "runtime";
  request.correlation_id = "poll-1";
  request.op = ipc::WorkerControlOp::LedgerStatus;
  request.member = ipc::WorkerMember{kDeployment, kGeneration};
  ASSERT_TRUE(
      testing::send_frame(pair.agent.get(), ipc::encode_worker_control_request(request).value()));
  std::string reply;
  ASSERT_EQ(testing::read_frame(pair.agent.get(), 5s, reply), testing::FrameRead::Frame)
      << worker.stderr_text();
  const auto response = ipc::decode_worker_control_response(reply);
  ASSERT_TRUE(response.has_value()) << reply;
  EXPECT_TRUE(ipc::worker_control_answers(*response, request).has_value());
  EXPECT_EQ(response->status, ipc::WorkerControlStatus::Ok);

  const auto give_up = std::chrono::steady_clock::now() + 5s;
  while (!testing::loopback_port_listening(port) && std::chrono::steady_clock::now() < give_up) {
    std::this_thread::sleep_for(10ms);
  }
  ASSERT_TRUE(testing::loopback_port_listening(port)) << worker.stderr_text();

  int control_descriptors = 0;
  for (const auto& [fd, target] : descriptors_of(worker.pid())) {
    if (fd == STDIN_FILENO) {
      EXPECT_EQ(target, "/dev/null");
    }
    if (target != worker_end) {
      continue;
    }
    ++control_descriptors;
    EXPECT_GT(fd, STDERR_FILENO);
    std::ifstream info("/proc/" + std::to_string(worker.pid()) + "/fdinfo/" + std::to_string(fd));
    std::string field;
    unsigned long flags = 0;
    while (info >> field) {
      if (field == "flags:") {
        info >> std::oct >> flags;
        break;
      }
    }
    EXPECT_NE(flags & static_cast<unsigned long>(O_CLOEXEC), 0UL) << "fd " << fd;
  }
  EXPECT_EQ(control_descriptors, 1);

  // Losing the agent does not end the worker: it keeps serving what it has.
  pair.agent.reset();
  EXPECT_FALSE(worker.wait_exit(300ms).has_value());
  EXPECT_TRUE(testing::loopback_port_listening(port));
  worker.kill_and_reap();
  std::filesystem::remove_all(dir);
}

// The worker launches this script as its sidecar. It lists what it was
// started with and exits, so the load fails; the listing is the result.
TEST(ControlDescriptor, SidecarOfAMemberIsStartedWithNoEndOfTheControlChannel) {
  const auto dir = scratch_dir("sidecar");
  const auto script = dir / "fake-sidecar";
  const auto report = dir / "fds.txt";
  {
    std::ofstream out(script);
    out << "#!/bin/sh\n"
        << "ls -l /proc/self/fd > \"$TP_FD_REPORT.tmp\"\n"
        << "mv \"$TP_FD_REPORT.tmp\" \"$TP_FD_REPORT\"\n";
  }
  std::filesystem::permissions(script, std::filesystem::perms::owner_all);

  auto pair = testing::ControlSocketPair::create();
  const std::vector<std::string> ends = {
      "socket:[" + std::to_string(testing::inode_of(pair.worker.get())) + "]",
      "socket:[" + std::to_string(testing::inode_of(pair.agent.get())) + "]"};
  const std::filesystem::path bundle_entry =
      std::filesystem::path{TP_SOURCE_DIR} /
      "test/models/bundles/v0_1/tts_kokoro_candidate/tts-kokoro-candidate.json";
  const nlohmann::json config = {{"schema_version", "0.1"},
                                 {"bind", {{"host", "127.0.0.1"}, {"port", 0}}},
                                 {"enable_stderr_logs", false},
                                 {"deployment",
                                  {{"use_mock_session", false},
                                   {"endpoint", kDeployment},
                                   {"generation", kGeneration},
                                   {"backend", "python_pytorch"},
                                   {"model",
                                    {{"model_id", "sidecar-under-test"},
                                     {"model_class", "custom"},
                                     {"artifact_path", bundle_entry.string()},
                                     {"backend_hint", "python_pytorch"}}}}}};
  auto worker = testing::ChildProcess::spawn(
      {TP_SERVING_WORKER_BINARY, "--config-json", config.dump()}, pair.worker.get(),
      dir / "stderr.txt",
      {"TP_PYTHON_PYTORCH_EXECUTABLE=" + script.string(), "TP_FD_REPORT=" + report.string()});
  pair.worker.reset();

  // A worker built without the backend exits at once; one with it launches
  // the script well inside the wait.
  const auto give_up = std::chrono::steady_clock::now() + 30s;
  while (!std::filesystem::exists(report) && std::chrono::steady_clock::now() < give_up &&
         !worker.wait_exit(10ms).has_value()) {
  }
  if (!std::filesystem::exists(report)) {
    const auto text = worker.stderr_text();
    if (text.find("python_pytorch") != std::string::npos &&
        text.find("unsupported") != std::string::npos) {
      GTEST_SKIP() << "built without the Python sidecar backend";
    }
    FAIL() << "the worker launched no sidecar: " << text;
  }

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
    for (const auto& end : ends) {
      EXPECT_NE(target, end) << line;
    }
    if (fd == STDIN_FILENO) {
      EXPECT_EQ(target, "/dev/null") << line;
    } else if (fd == STDOUT_FILENO) {
      EXPECT_EQ(target, report.string() + ".tmp") << line;
    } else if (fd == STDERR_FILENO) {
      EXPECT_EQ(target, (dir / "stderr.txt").string()) << line;
    }
    if (fd <= STDERR_FILENO) {
      ++standard;
    } else {
      // The one descriptor ls opens itself.
      EXPECT_TRUE(target.rfind("/proc/", 0) == 0 && target.ends_with("/fd")) << line;
    }
  }
  EXPECT_EQ(standard, 3);
  worker.kill_and_reap();
  std::filesystem::remove_all(dir);
}

#endif  // __linux__
}  // namespace
}  // namespace tensorplate

#endif  // TP_SERVING_WORKER_BINARY
