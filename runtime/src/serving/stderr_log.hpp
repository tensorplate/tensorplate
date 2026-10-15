// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <nlohmann/json.hpp>
#include <string_view>

#include "tensorplate/serving/config.hpp"

namespace tensorplate::serving {

/// Writes one JSON record of the serving worker to stderr when `cfg`
/// enables its logs: `ts_ns` on the steady clock, `level`, `component`,
/// `message` and `fields` when there are any. The record and its newline go
/// out in one write, so records of two threads never share a line.
void log_stderr(const ServingConfig& cfg, std::string_view level, std::string_view message,
                const nlohmann::json& fields = {});

}  // namespace tensorplate::serving
