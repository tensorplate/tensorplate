// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F04-T01 / T02: InferResult validation and accessor implementations.

#include "tensorplate/core/infer_result.hpp"

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

#include <string>
#include <unordered_set>
#include <utility>
#include <variant>

namespace tensorplate {

namespace {

const InferResult::Outputs& empty_outputs() {
  static const InferResult::Outputs kEmpty;
  return kEmpty;
}

const Error& placeholder_error() {
  static const Error kPlaceholder = Error::make(Error::Code::Internal, "");
  return kPlaceholder;
}

Result<void> validate_outputs(const InferResult::Outputs& outputs) {
  std::unordered_set<std::string> seen;
  seen.reserve(outputs.size());
  for (const auto& o : outputs) {
    if (o.name.empty()) {
      return unexpected(Error::Code::ConfigInvalid,
                        "InferResult.outputs entry has empty `name`");
    }
    auto [_, inserted] = seen.insert(o.name);
    if (!inserted) {
      return unexpected(Error::Code::ConfigInvalid,
                        "InferResult.outputs has duplicate name `" + o.name + "`");
    }
  }
  return Result<void>{};
}

}  // namespace

Result<InferResult> InferResult::create_success(std::string request_id, Outputs outputs,
                                                InferenceTiming timing) {
  // request_id may be empty for synthetic "echo" responses; the wire
  // contract treats an empty request_id as not-correlated. The scheduler
  // typically populates this from the originating InferRequest.
  if (outputs.empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "InferResult success must carry at least one named output");
  }
  auto outputs_ok = validate_outputs(outputs);
  if (!outputs_ok) {
    return unexpected(std::move(outputs_ok).error());
  }

  InferResult r;
  r.request_id_ = std::move(request_id);
  r.payload_.emplace<Outputs>(std::move(outputs));
  r.timing_ = timing;
  return r;
}

InferResult InferResult::create_failure(std::string request_id, Error error,
                                        InferenceTiming timing) {
  InferResult r;
  r.request_id_ = std::move(request_id);
  r.payload_.emplace<Error>(std::move(error));
  r.timing_ = timing;
  return r;
}

const InferResult::Outputs& InferResult::outputs() const noexcept {
  if (auto* outs = std::get_if<Outputs>(&payload_)) {
    return *outs;
  }
  return empty_outputs();
}

const Error& InferResult::error() const noexcept {
  if (auto* err = std::get_if<Error>(&payload_)) {
    return *err;
  }
  return placeholder_error();
}

}  // namespace tensorplate
