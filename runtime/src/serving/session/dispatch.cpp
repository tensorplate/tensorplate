// SPDX-License-Identifier: Apache-2.0
#include "serving/session/dispatch.hpp"

#include <memory>

namespace tensorplate::serving {
namespace {
struct Charge {
  OutputKind kind = OutputKind::Result;
  std::uint64_t bytes = 0;
  std::uint64_t replace_key = 0;
};

Charge charge_of(const FinalTranscriptOutput& transcript) {
  std::uint64_t bytes = kTaskOutputMessageBytes;
  for (const auto& segment : transcript.segments) {
    bytes += kTaskOutputSegmentBytes + segment.text.size();
  }
  // A final supersedes the unsent partial of its utterance.
  return {OutputKind::Result, bytes, transcript.utterance_id};
}

Charge charge_of(const AudioChunkOutput& chunk) {
  return {OutputKind::Audio, chunk.pcm.size(), 0};
}

Charge charge_of(const SegmentCompletedOutput& /*completed*/) {
  return {OutputKind::Result, kTaskOutputMessageBytes, 0};
}
}  // namespace

OutputItem make_task_output_item(TaskOutput output) {
  const Charge charge = std::visit([](const auto& value) { return charge_of(value); }, output);
  return OutputItem{charge.kind, charge.bytes, charge.replace_key,
                    std::make_unique<TaskOutputBody>(std::move(output))};
}
}  // namespace tensorplate::serving
