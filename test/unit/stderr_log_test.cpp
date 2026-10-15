// SPDX-License-Identifier: Apache-2.0
//
// The serving worker's stderr records, which the agent reads line by line.

#include "serving/stderr_log.hpp"

#include <gtest/gtest.h>

#include <array>
#include <atomic>
#include <nlohmann/json.hpp>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include "tensorplate/serving/config.hpp"

namespace {

using tensorplate::ServingConfig;
using tensorplate::serving::log_stderr;

TEST(StderrLog, RecordsOfTwoThreadsEachKeepALineOfTheirOwn) {
  ServingConfig config;
  config.enable_stderr_logs = true;
  constexpr int kRecords = 5000;
  std::atomic<int> ready{0};
  ::testing::internal::CaptureStderr();
  {
    std::vector<std::thread> writers;
    for (int writer = 0; writer < 2; ++writer) {
      writers.emplace_back([&, writer] {
        // Both write at once.
        ready.fetch_add(1);
        while (ready.load() < 2) {
          std::this_thread::yield();
        }
        for (int index = 0; index < kRecords; ++index) {
          log_stderr(config, "info", "record", {{"writer", writer}, {"index", index}});
        }
      });
    }
    for (auto& writer : writers) {
      writer.join();
    }
  }
  const auto log = ::testing::internal::GetCapturedStderr();
  std::array<int, 2> next{0, 0};
  std::istringstream lines(log);
  int unparsed = 0;
  for (std::string line; std::getline(lines, line);) {
    const auto record = nlohmann::json::parse(line, nullptr, false);
    if (!record.is_object() || !record.contains("fields")) {
      ++unparsed;
      continue;
    }
    const auto writer = record["fields"].value("writer", -1);
    ASSERT_TRUE(writer == 0 || writer == 1) << line;
    EXPECT_EQ(record["fields"].value("index", -1), next.at(writer)) << line;
    ++next.at(writer);
  }
  EXPECT_EQ(unparsed, 0);
  EXPECT_EQ(next[0], kRecords);
  EXPECT_EQ(next[1], kRecords);
}

TEST(StderrLog, AConfigWithoutLogsWritesNothing) {
  ServingConfig config;
  config.enable_stderr_logs = false;
  ::testing::internal::CaptureStderr();
  log_stderr(config, "info", "record", {{"writer", 0}});
  EXPECT_EQ(::testing::internal::GetCapturedStderr(), "");
}

}  // namespace
