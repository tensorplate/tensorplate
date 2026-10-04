// SPDX-License-Identifier: Apache-2.0
//
// Loader for test/unit/fixtures/session_budgets.json: the budget derivations
// and the step-by-step input credit and output queue cases.

#pragma once

#include <cstdint>
#include <fstream>
#include <nlohmann/json.hpp>
#include <stdexcept>
#include <string>

#include "tensorplate/core/result.hpp"

#include "serving/session/credits.hpp"

namespace tensorplate::testing {

inline nlohmann::json load_session_budget_fixture() {
  const std::string path = std::string(TP_SOURCE_DIR) + "/test/unit/fixtures/session_budgets.json";
  std::ifstream input(path);
  if (!input.good()) {
    throw std::runtime_error("cannot open " + path);
  }
  return nlohmann::json::parse(input);
}

/// Budgets for a fixture `{"input": "audio"|"text", "bytes_per_second": n}`.
inline Result<serving::SessionBudgets> session_budgets_from(const nlohmann::json& row) {
  const auto input = row.at("input").get<std::string>();
  const auto rate = row.at("bytes_per_second").get<std::uint64_t>();
  if (input == "audio") {
    return serving::SessionBudgets::for_audio_input(rate);
  }
  if (input == "text") {
    return serving::SessionBudgets::for_text_input(rate);
  }
  throw std::runtime_error("unknown input kind " + input);
}

/// Context of a failed result, or "ok".
template <typename T>
std::string outcome_of(const Result<T>& result) {
  return result ? "ok" : result.error().context.value_or("");
}
}  // namespace tensorplate::testing
