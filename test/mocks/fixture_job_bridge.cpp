// SPDX-License-Identifier: Apache-2.0

#include "fixture_job_bridge.hpp"

#include <chrono>
#include <cstdint>
#include <memory>
#include <utility>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::testing {

namespace {

// Not written yet: the bridge refuses whatever it is asked.
Unexpected unwritten() {
  return unexpected(Error::Code::Unsupported, "the fixture job bridge is not written yet");
}

}  // namespace

TranscriptResult fixture_transcript(const AudioFrames& /*audio*/) {
  return {};
}

Result<JobResult> fixture_job_result(const JobRequest& /*request*/, BufferManager& /*buffers*/) {
  return unwritten();
}

FixtureJobBridge::FixtureJobBridge(BufferManager& buffers, ResultFunction result)
    : buffers_(buffers), result_(std::move(result)) {}

FixtureJobBridge::~FixtureJobBridge() = default;

Result<void> FixtureJobBridge::set_event_sink(std::shared_ptr<JobEventSink> /*sink*/) {
  return unwritten();
}

Result<void> FixtureJobBridge::submit(JobRequest /*request*/) {
  return unwritten();
}

Result<void> FixtureJobBridge::cancel(std::uint64_t /*job_id*/) {
  return unwritten();
}

Result<void> FixtureJobBridge::release_session(std::uint64_t /*session_key*/,
                                               std::uint64_t /*generation*/) {
  return unwritten();
}

Result<void> FixtureJobBridge::health() {
  return unwritten();
}

void FixtureJobBridge::prime() {}
void FixtureJobBridge::lose_backend() {}
void FixtureJobBridge::unload() {}
void FixtureJobBridge::hold_jobs() {}
void FixtureJobBridge::release_jobs() {}
void FixtureJobBridge::fail_next_job(Error /*error*/) {}
void FixtureJobBridge::set_health(Result<void> /*health*/) {}

bool FixtureJobBridge::settled(std::chrono::milliseconds /*patience*/) {
  return true;
}

}  // namespace tensorplate::testing
