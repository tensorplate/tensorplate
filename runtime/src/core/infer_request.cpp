// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F03-T01 / T02: InferRequest validation and deadline arithmetic.

#include "tensorplate/core/infer_request.hpp"

#include <chrono>
#include <string>
#include <unordered_set>
#include <utility>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate {

namespace {

Result<void> validate_inputs(const std::vector<NamedInput>& inputs) {
  if (inputs.empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "InferRequest.inputs must contain at least one named input");
  }
  std::unordered_set<std::string> seen;
  seen.reserve(inputs.size());
  for (const auto& in : inputs) {
    if (in.name.empty()) {
      return unexpected(Error::Code::ConfigInvalid, "InferRequest.inputs entry has empty `name`");
    }
    auto [_, inserted] = seen.insert(in.name);
    if (!inserted) {
      return unexpected(Error::Code::ConfigInvalid,
                        "InferRequest.inputs has duplicate name `" + in.name + "`");
    }
  }
  return Result<void>{};
}

}  // namespace

Result<InferRequest> InferRequest::create(std::string request_id, std::string endpoint,
                                          std::vector<NamedInput> inputs, RequestMetadata metadata,
                                          std::optional<TimePoint> deadline) {
  if (request_id.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "InferRequest.request_id must be non-empty");
  }
  if (endpoint.empty()) {
    return unexpected(Error::Code::ConfigInvalid, "InferRequest.endpoint must be non-empty");
  }
  auto inputs_ok = validate_inputs(inputs);
  if (!inputs_ok) {
    return unexpected(std::move(inputs_ok).error());
  }

  InferRequest req;
  req.request_id_ = std::move(request_id);
  req.endpoint_ = std::move(endpoint);
  req.inputs_ = std::move(inputs);
  req.metadata_ = std::move(metadata);
  req.deadline_ = deadline;
  return req;
}

Result<InferRequest> InferRequest::create_with_relative_deadline(
    std::string request_id, std::string endpoint, std::vector<NamedInput> inputs,
    RequestMetadata metadata, std::optional<Duration> relative_deadline) {
  std::optional<TimePoint> absolute;
  if (relative_deadline.has_value()) {
    if (relative_deadline->count() < 0) {
      return unexpected(Error::Code::ConfigInvalid,
                        "InferRequest relative deadline must be >= 0 ms");
    }
    absolute = Clock::now() + *relative_deadline;
  }
  return InferRequest::create(std::move(request_id), std::move(endpoint), std::move(inputs),
                              std::move(metadata), absolute);
}

bool InferRequest::is_expired() const noexcept {
  return deadline_.has_value() && Clock::now() >= *deadline_;
}

std::optional<InferRequest::Duration> InferRequest::time_until_deadline() const noexcept {
  if (!deadline_.has_value()) {
    return std::nullopt;
  }
  const auto delta = *deadline_ - Clock::now();
  const auto delta_ms = std::chrono::duration_cast<Duration>(delta);
  if (delta_ms.count() < 0) {
    return Duration::zero();
  }
  return delta_ms;
}

}  // namespace tensorplate
