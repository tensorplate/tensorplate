// SPDX-License-Identifier: Apache-2.0
//
// The bridge conformance cases against the in-process fixture bridge.

#include "bounded_job_bridge_conformance.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <cstddef>
#include <memory>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"

#include "fixture_job_bridge.hpp"

namespace tensorplate::testing {
namespace {

std::unique_ptr<BufferManager> make_buffers() {
  BufferManagerConfig config;
  config.pool_name = "job_bridge_test";
  config.capacity_bytes = 16U << 20U;
  config.max_buffer_bytes = 2U << 20U;
  return BufferManager::create(config).value();
}

class FixtureBridgeUnderTest final : public BridgeUnderTest {
 public:
  BoundedJobBridge& bridge() override { return bridge_; }
  BufferManager& buffers() override { return *buffers_; }
  void prime() override { bridge_.prime(); }
  void unload() override { bridge_.unload(); }
  void lose_backend() override { bridge_.lose_backend(); }
  void hold_jobs() override { bridge_.hold_jobs(); }
  void release_jobs() override { bridge_.release_jobs(); }
  bool settled(std::chrono::milliseconds patience) override { return bridge_.settled(patience); }
  std::size_t waiting_jobs() const override { return FixtureJobBridge::kWaitingJobs; }
  TranscriptResult transcript(const JobRequest& request) const override {
    return fixture_transcript(std::get<AudioFrames>(request.payload()));
  }
  std::vector<std::byte> speech(const JobRequest& request) const override {
    const std::size_t text_bytes = std::get<TextSegment>(request.payload()).text.size();
    return std::vector<std::byte>(kFixtureSpeechBytesPerTextByte * text_bytes, std::byte{0});
  }

 private:
  std::unique_ptr<BufferManager> buffers_ = make_buffers();
  FixtureJobBridge bridge_{*buffers_};
};

}  // namespace

INSTANTIATE_TEST_SUITE_P(Bridges, BoundedJobBridgeConformance,
                         ::testing::Values(BridgeCase{
                             "fixture", [] { return std::make_unique<FixtureBridgeUnderTest>(); }}),
                         [](const auto& bridge_case) { return bridge_case.param.name; });

}  // namespace tensorplate::testing
