// SPDX-License-Identifier: Apache-2.0
#include "serving/stderr_log.hpp"

#include <chrono>
#include <cstdio>
#include <string>

namespace tensorplate::serving {

void log_stderr(const ServingConfig& cfg, std::string_view level, std::string_view message,
                const nlohmann::json& fields) {
  if (!cfg.enable_stderr_logs) {
    return;
  }
  nlohmann::json j;
  j["ts_ns"] = std::chrono::duration_cast<std::chrono::nanoseconds>(
                   std::chrono::steady_clock::now().time_since_epoch())
                   .count();
  j["level"] = std::string{level};
  j["component"] = "serving";
  j["message"] = std::string{message};
  if (!fields.empty()) {
    j["fields"] = fields;
  }
  auto line = j.dump();
  line.push_back('\n');
  // One call: stdio locks the stream for it, against every other writer in
  // the process.
  (void)std::fwrite(line.data(), 1, line.size(), stderr);
}

}  // namespace tensorplate::serving
