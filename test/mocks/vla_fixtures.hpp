// SPDX-License-Identifier: Apache-2.0
//
// V01-E06-F07-T01: SmolVLA-shaped scheduler request fixtures.
//
// These fixtures model the LeRobot PolicyServer / SmolVLA async
// request pattern at the scheduler layer:
//
//   - Named multi-input payload metadata: image_front,
//     proprioception, instruction.
//   - Action-chunk identity (RequestMetadata.action_chunk_id /
//     action_chunk_sequence) so stale-request cancellation can
//     target chunk-sequence ranges.
//   - LeRobot stale_after_sequence marker on the active request.
//   - Small fake buffers from a real BufferManager so cleanup
//     paths are observable without SmolVLA weights.
//
// The fixtures are intentionally backend-neutral: the SchedulerRequest
// declares backend_name = "python_pytorch" and model_id = "smolvla"
// to mirror the v0.1.0 SmolVLA validation path, but the scheduler
// itself never branches on the labels.

#pragma once

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/scheduler/clock.hpp"
#include "tensorplate/scheduler/scheduler.hpp"
#include "tensorplate/scheduler/scheduler_request.hpp"

namespace tensorplate::testing {

/// Allocate a fake input buffer through `manager` and pair it with a
/// minimal TensorView. Helper used by the SmolVLA fixtures.
inline NamedInput allocate_vla_input(BufferManager& manager, std::string name,
                                     std::size_t byte_size,
                                     const std::vector<std::int64_t>& shape) {
  auto buf = manager.allocate(byte_size).value();
  auto view = TensorView::create(DType::Float32, shape, Layout::RowMajor, 0, byte_size).value();
  return NamedInput{std::move(name), buf, view};
}

/// Build a SmolVLA-shaped scheduler request with named multi-input
/// payload (image_front, proprioception, instruction) and async
/// chunk identity. Buffer sizes are small fakes; tests that need
/// realistic SmolVLA shapes plug in their own sizes.
inline SchedulerRequest make_vla_request(
    BufferManager& manager, const SchedulerClock& clock, std::string request_id,
    std::int64_t action_chunk_sequence,
    std::optional<std::int64_t> stale_after_sequence = std::nullopt,
    std::optional<InferRequest::TimePoint> deadline = std::nullopt) {
  std::vector<NamedInput> inputs;
  // Pretend image_front is a 2x2 RGB float32 (48 bytes) just so the
  // scheduler has something to release; SmolVLA-true shapes are
  // immaterial here.
  inputs.push_back(allocate_vla_input(manager, "image_front", /*byte_size=*/48,
                                      /*shape=*/{1, 3, 2, 2}));
  inputs.push_back(allocate_vla_input(manager, "proprioception", /*byte_size=*/32,
                                      /*shape=*/{1, 8}));
  inputs.push_back(allocate_vla_input(manager, "instruction", /*byte_size=*/16,
                                      /*shape=*/{1, 4}));

  RequestMetadata meta;
  meta.action_chunk_id = std::string{"chunk-"} + std::to_string(action_chunk_sequence);
  meta.action_chunk_sequence = action_chunk_sequence;
  meta.stale_after_sequence = stale_after_sequence;
  meta.correlation_id = request_id;

  auto req = InferRequest::create(std::move(request_id), "lerobot/policy", std::move(inputs),
                                  std::move(meta), deadline)
                 .value();
  return SchedulerRequest{std::move(req), "python_pytorch", "smolvla", {}, clock.now()};
}

/// Helper used by tests to drive the LeRobot stale-request semantics:
/// given an `active_envelope`'s stale_after_sequence and a list of
/// queued envelopes, return the request_ids that should be cancelled
/// with reason StaleSequence. Lives in the fixture so the same
/// convention is reused across F07 tests and future V01-E07 fixtures.
inline std::vector<std::string> stale_request_ids(
    std::int64_t stale_after_sequence, const std::vector<const SchedulerRequest*>& queued) {
  std::vector<std::string> stale;
  for (const auto* env : queued) {
    const auto& seq = env->request().metadata().action_chunk_sequence;
    if (seq.has_value() && *seq <= stale_after_sequence) {
      stale.push_back(env->request_id());
    }
  }
  return stale;
}

}  // namespace tensorplate::testing
