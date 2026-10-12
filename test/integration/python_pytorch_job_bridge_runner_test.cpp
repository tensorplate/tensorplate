// SPDX-License-Identifier: Apache-2.0
//
// The bridge conformance cases (test/contract/bounded_job_bridge_conformance.hpp)
// against the Python/PyTorch adapter's bridge over the in-tree sidecar runner
// and its fixture backend: a real process and socket. The runner's jobs cannot
// be held from here, so the cases that need a held job are skipped; the unit
// tests run them over a scripted sidecar. What runs shows, among the rest,
// that the fixture bridge's results are the runner's.
//
// Skipped when the Python package is not importable; the interpreter comes
// from TP_TEST_PYTHON, else `python3`.

#include <gtest/gtest.h>

#if !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobBridgeRunner, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <signal.h>

#include <atomic>
#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <string>
#include <utility>
#include <variant>
#include <vector>

#include "tensorplate/backend/bounded_job_bridge.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/job_request.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/model_spec.hpp"

#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"
#include "bounded_job_bridge_conformance.hpp"
#include "fixture_job_bridge.hpp"
#include "job_bridge_harness.hpp"

namespace tensorplate::testing {
namespace {

std::string python() {
  const char* named = std::getenv("TP_TEST_PYTHON");
  return named != nullptr && *named != '\0' ? named : "python3";
}

std::string why_the_runner_cannot_start() {
  const std::string probe = python() + " -c 'import tensorplate_pytorch_backend' >/dev/null 2>&1";
  if (std::system(probe.c_str()) != 0) {  // NOLINT(concurrency-mt-unsafe)
    return "tensorplate_pytorch_backend is not importable by " + python();
  }
  return {};
}

/// The adapter's bridge with its session over the runner, built loaded. It
/// states no hold and none of the choices a case needs a hold to see.
class RunnerBridgeUnderTest final : public BridgeUnderTest {
 public:
  RunnerBridgeUnderTest() {
    BufferManagerConfig pool;
    pool.pool_name = "job_bridge_runner_test";
    pool.capacity_bytes = 16U << 20U;
    pool.max_buffer_bytes = 2U << 20U;
    buffers_ = BufferManager::create(pool).value();
    ExecutionSessionRuntimeHooks hooks{};
    hooks.buffer_manager = buffers_.get();
    adapters::python_pytorch::PythonPytorchConfig config;
    config.python_exe = python();
    config.startup_timeout = kPatience;
    config.health_timeout = kPatience;
    built_ = adapters::python_pytorch::make_python_pytorch_session_with_bridge(
        hooks, std::move(config),
        [this](const adapters::python_pytorch::SidecarLaunchRequest& request) {
          auto started = adapters::python_pytorch::default_fork_exec_launcher()(request);
          if (started.has_value()) {
            runner_ = started.value().pid();
          }
          return started;
        });
    const auto loaded = built_.session->load(
        ModelSpec::create("speech", ModelClass::Speech, "/dev/null", "python_pytorch").value());
    EXPECT_TRUE(loaded.has_value()) << loaded.error().message;
  }
  ~RunnerBridgeUnderTest() override { unload(); }

  Choices choices() const override { return {.refuses_vad_frames = true}; }
  BoundedJobBridge& bridge() override { return *built_.bridge; }
  BufferManager& buffers() override { return *buffers_; }
  void prime() override {
    const auto primed = built_.session->prime();
    EXPECT_TRUE(primed.has_value()) << primed.error().message;
  }
  void unload() override { (void)built_.session->unload(); }
  // The process is killed and its session is not told.
  void lose_backend() override {
    if (runner_ > 0) {
      (void)::kill(runner_, SIGKILL);
    }
  }
  void hold_jobs() override {}
  void release_jobs() override {}
  bool settled(std::chrono::milliseconds patience) override {
    return settled_by_backlog(*built_.channel, patience);
  }
  TranscriptResult transcript(const JobRequest& request) const override {
    return fixture_transcript(std::get<AudioFrames>(request.payload()));
  }
  std::vector<std::byte> speech(const JobRequest& request) const override {
    const std::size_t text_bytes = std::get<TextSegment>(request.payload()).text.size();
    return std::vector<std::byte>(kFixtureSpeechBytesPerTextByte * text_bytes, std::byte{0});
  }

 private:
  static constexpr std::chrono::seconds kPatience{30};

  std::unique_ptr<BufferManager> buffers_;
  std::atomic<int> runner_{0};
  adapters::python_pytorch::SessionWithJobBridge built_;
};

}  // namespace

INSTANTIATE_TEST_SUITE_P(PythonPytorchAdapter, BoundedJobBridgeConformance,
                         ::testing::Values(BridgeCase{
                             "runner", [] { return std::make_unique<RunnerBridgeUnderTest>(); },
                             why_the_runner_cannot_start}),
                         [](const auto& bridge_case) { return bridge_case.param.name; });

}  // namespace tensorplate::testing

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
