// SPDX-License-Identifier: Apache-2.0
//
// A serving worker started by hand on a port another process listens on.

#include <arpa/inet.h>
#include <fcntl.h>
#include <gtest/gtest.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <nlohmann/json.hpp>
#include <optional>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include "tensorplate/serving/worker.hpp"

#include "control_socket_harness.hpp"

#ifdef TP_SERVING_WORKER_BINARY

namespace tensorplate {
namespace {
using namespace std::chrono_literals;

/// Far past a worker's own start: only a worker that binds and serves waits it out.
constexpr auto kStartWait = 60s;

std::string worker_config(std::uint16_t http_port, std::optional<std::uint16_t> stream_port) {
  nlohmann::json config = {{"schema_version", "0.1"},
                           {"bind", {{"host", "127.0.0.1"}, {"port", http_port}}},
                           {"deployment", {{"use_mock_session", true}, {"endpoint", "bind-test"}}},
                           {"enable_stderr_logs", true}};
  if (stream_port) {
    config["streaming"] = {{"enabled", true}, {"port", *stream_port}};
  }
  return config.dump();
}

std::filesystem::path scratch_dir(const char* name) {
  auto dir = std::filesystem::temp_directory_path() /
             (std::string{"tp-bind-"} + name + "-" + std::to_string(::getpid()));
  std::filesystem::remove_all(dir);
  std::filesystem::create_directories(dir);
  return dir;
}

std::vector<nlohmann::json> stderr_records(const std::string& text) {
  std::vector<nlohmann::json> records;
  std::istringstream lines(text);
  std::string line;
  while (std::getline(lines, line)) {
    if (!line.empty()) {
      records.push_back(nlohmann::json::parse(line, nullptr, false));
    }
  }
  return records;
}

/// Whether this host binds the documentation address 192.0.2.1, as one
/// that has it or binds any address does.
bool binds_documentation_address() {
  const testing::OwnedFd fd{::socket(AF_INET, SOCK_STREAM, 0)};
  sockaddr_in address{};
  address.sin_family = AF_INET;
  ::inet_pton(AF_INET, "192.0.2.1", &address.sin_addr);
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
  return ::bind(fd.get(), reinterpret_cast<const sockaddr*>(&address), sizeof(address)) == 0;
}

/// The HTTP port `worker` logged as bound, once it has.
std::optional<std::uint16_t> logged_http_port(const testing::ChildProcess& worker) {
  const auto give_up = std::chrono::steady_clock::now() + kStartWait;
  while (std::chrono::steady_clock::now() < give_up) {
    for (const auto& record : stderr_records(worker.stderr_text())) {
      if (record.is_object() && record.value("message", "") == "http server bound") {
        return record["fields"].value("port", std::uint16_t{0});
      }
    }
    std::this_thread::sleep_for(10ms);
  }
  return std::nullopt;
}

/// The record a worker that could not start ends its stderr with.
void expect_startup_record(const std::string& stderr_text, const std::string& code,
                           const std::string& message) {
  const auto records = stderr_records(stderr_text);
  ASSERT_FALSE(records.empty());
  const auto& record = records.back();
  ASSERT_TRUE(record.is_object()) << stderr_text;
  EXPECT_EQ(record.value("component", ""), "serving");
  EXPECT_EQ(record.value("message", ""), "worker startup failed") << stderr_text;
  const auto fields = record.value("fields", nlohmann::json::object());
  EXPECT_EQ(fields.value("code", ""), code);
  EXPECT_EQ(fields.value("message", ""), message);
}

/// The last line of the recording the agent's deploy tests replay, without
/// its clock reading and with `port` for the port it was recorded on.
nlohmann::json recorded_record(std::uint16_t port) {
  std::ifstream in(std::filesystem::path{TP_SOURCE_DIR} /
                   "agent/tests/fixtures/worker_stderr/http_port_taken.stderr");
  const auto records =
      stderr_records({std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()});
  if (records.empty() || !records.back().is_object()) {
    return {};
  }
  auto record = records.back();
  record.erase("ts_ns");
  auto message = record.value("fields", nlohmann::json::object()).value("message", "");
  const std::string host = "127.0.0.1:";
  if (const auto at = message.find(host); at != std::string::npos) {
    const auto from = at + host.size();
    message.replace(from, message.find(':', from) - from, std::to_string(port));
  }
  record["fields"]["message"] = message;
  return record;
}

TEST(WorkerBind, AWorkerOnAnotherWorkersHttpPortExitsWithARecord) {
  const auto dir = scratch_dir("http");
  const testing::OwnedFd null_input{::open("/dev/null", O_RDONLY)};
  ASSERT_GE(null_input.get(), 0);
  auto holder = testing::ChildProcess::spawn(
      {TP_SERVING_WORKER_BINARY, "--config-json", worker_config(0, std::nullopt)}, null_input.get(),
      dir / "holder.txt");
  const auto port = logged_http_port(holder);
  ASSERT_TRUE(port.has_value()) << holder.stderr_text();

  auto second = testing::ChildProcess::spawn(
      {TP_SERVING_WORKER_BINARY, "--config-json", worker_config(*port, std::nullopt)},
      null_input.get(), dir / "second.txt");
  const auto exit_status = second.wait_exit(kStartWait);

  ASSERT_TRUE(exit_status) << "both workers serve port " << *port;
  EXPECT_EQ(*exit_status, static_cast<int>(ServingExitCode::ServeError));
  expect_startup_record(second.stderr_text(), "unavailable",
                        "http server: could not bind 127.0.0.1:" + std::to_string(*port) + ": " +
                            std::strerror(EADDRINUSE));
  auto actual = stderr_records(second.stderr_text()).back();
  actual.erase("ts_ns");
  EXPECT_EQ(actual, recorded_record(*port));
  EXPECT_FALSE(holder.wait_exit(0ms).has_value()) << "the worker holding the port exited";
  std::filesystem::remove_all(dir);
}

// Any other start failure is reported the same way, with its own code.
TEST(WorkerBind, AWorkerOnAnAddressTheHostLacksExitsWithARecord) {
  if (binds_documentation_address()) {
    GTEST_SKIP() << "this host binds the documentation address 192.0.2.1";
  }
  const auto dir = scratch_dir("absent");
  const testing::OwnedFd null_input{::open("/dev/null", O_RDONLY)};
  ASSERT_GE(null_input.get(), 0);
  const nlohmann::json config = {
      {"schema_version", "0.1"},
      {"bind", {{"host", "192.0.2.1"}, {"port", 0}, {"allow_non_loopback", true}}},
      {"deployment", {{"use_mock_session", true}, {"endpoint", "bind-test"}}},
      {"enable_stderr_logs", true}};
  auto worker = testing::ChildProcess::spawn(
      {TP_SERVING_WORKER_BINARY, "--config-json", config.dump()}, null_input.get(),
      dir / "worker.txt", {"TP_E2E_ALLOW_NON_LOOPBACK=1"});
  const auto exit_status = worker.wait_exit(kStartWait);

  ASSERT_TRUE(exit_status) << "the worker serves an address this host does not have";
  EXPECT_EQ(*exit_status, static_cast<int>(ServingExitCode::ServeError));
  expect_startup_record(
      worker.stderr_text(), "load_failed",
      std::string{"http server: could not bind 192.0.2.1:0: "} + std::strerror(EADDRNOTAVAIL));
  std::filesystem::remove_all(dir);
}

// ThreadSanitizer reports races inside the gRPC archives, built without it, in
// the worker this case starts, as it does in the stream listener's own cases.
#if TP_ENABLE_STREAMING_GRPC && !TP_ENABLE_TSAN
TEST(WorkerBind, AWorkerOnATakenStreamPortExitsWithARecord) {
  const auto dir = scratch_dir("stream");
  const testing::OwnedFd taken{::socket(AF_INET, SOCK_STREAM, 0)};
  sockaddr_in address{};
  address.sin_family = AF_INET;
  address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
  socklen_t length = sizeof(address);
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
  ASSERT_EQ(::bind(taken.get(), reinterpret_cast<const sockaddr*>(&address), length), 0);
  ASSERT_EQ(::listen(taken.get(), 1), 0);
  // NOLINTNEXTLINE(cppcoreguidelines-pro-type-reinterpret-cast): POSIX socket address.
  ASSERT_EQ(::getsockname(taken.get(), reinterpret_cast<sockaddr*>(&address), &length), 0);
  const auto port = ntohs(address.sin_port);
  const testing::OwnedFd null_input{::open("/dev/null", O_RDONLY)};
  ASSERT_GE(null_input.get(), 0);

  auto worker = testing::ChildProcess::spawn(
      {TP_SERVING_WORKER_BINARY, "--config-json", worker_config(0, port)}, null_input.get(),
      dir / "worker.txt");
  const auto exit_status = worker.wait_exit(kStartWait);

  ASSERT_TRUE(exit_status) << "the worker serves a stream port another socket holds";
  EXPECT_EQ(*exit_status, static_cast<int>(ServingExitCode::ServeError));
  // gRPC gives the listener no cause for a port it could not bind.
  expect_startup_record(worker.stderr_text(), "unavailable",
                        "stream listener could not bind 127.0.0.1:" + std::to_string(port));
  std::filesystem::remove_all(dir);
}
#endif

}  // namespace
}  // namespace tensorplate

#endif  // TP_SERVING_WORKER_BINARY
