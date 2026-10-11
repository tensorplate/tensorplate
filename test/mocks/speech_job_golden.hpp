// SPDX-License-Identifier: Apache-2.0
//
// The recorded golden frames of the speech job messages
// (protocol/rust/tests/fixtures/python_pytorch_ipc_speech_jobs_*.jsonl), read
// for a replay. The including target defines TP_SOURCE_DIR.

#pragma once

#include <gtest/gtest.h>

#include <algorithm>
#include <filesystem>
#include <fstream>
#include <nlohmann/json.hpp>
#include <string>
#include <vector>

namespace tensorplate::testing::speech_golden {

using Json = nlohmann::json;

inline const std::string kGoldenDirectory =
    std::string{TP_SOURCE_DIR} + "/protocol/rust/tests/fixtures/";
inline const std::string kGoldenPrefix = "python_pytorch_ipc_speech_jobs_";
inline const std::string kGoldenSuffix = ".jsonl";

// Every golden file on disk, by the part of its name that tells them apart.
inline std::vector<std::string> golden_files() {
  std::vector<std::string> names;
  for (const auto& entry : std::filesystem::directory_iterator(kGoldenDirectory)) {
    const std::string file = entry.path().filename().string();
    if (file.starts_with(kGoldenPrefix) && file.ends_with(kGoldenSuffix)) {
      names.push_back(file.substr(kGoldenPrefix.size(),
                                  file.size() - kGoldenPrefix.size() - kGoldenSuffix.size()));
    }
  }
  std::sort(names.begin(), names.end());
  return names;
}

inline std::vector<Json> golden(const std::string& name) {
  std::ifstream in(kGoldenDirectory + kGoldenPrefix + name + kGoldenSuffix);
  EXPECT_TRUE(in.is_open()) << name;
  std::vector<Json> lines;
  for (std::string line; std::getline(in, line);) {
    lines.push_back(Json::parse(line));
  }
  return lines;
}

inline bool sent_by_adapter(const Json& line) {
  const auto kind = line.at("kind").get<std::string>();
  return kind == "job_submit" || kind == "job_cancel" || kind == "session_release" ||
         kind == "load_model";
}

}  // namespace tensorplate::testing::speech_golden
