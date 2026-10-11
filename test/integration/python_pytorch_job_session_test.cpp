// SPDX-License-Identifier: Apache-2.0
//
// The Python/PyTorch adapter's session with speech jobs enabled, against the
// in-tree sidecar runner and its fixture backend over a real process and
// socket: the session negotiates, starts and stops the job channel itself,
// and the test runs jobs on the channel the session hands out.
//
// Skipped when the Python package is not importable; the interpreter comes
// from TP_TEST_PYTHON, else `python3`.

#include <gtest/gtest.h>

#if !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(JobSessionRunner, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <unistd.h>

#include <chrono>
#include <cstddef>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <memory>
#include <nlohmann/json.hpp>
#include <string>
#include <string_view>
#include <variant>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/job_event.hpp"
#include "tensorplate/core/job_result.hpp"
#include "tensorplate/core/model_spec.hpp"
#include "tensorplate/core/result.hpp"

#include "adapters/python_pytorch/job_channel.hpp"
#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"
#include "job_session_harness.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

using testing::JobSessionHarness;
using Json = nlohmann::json;
using Kinds = JobSessionHarness::Kinds;
using enum JobEventKind;

std::string python() {
  const char* named = std::getenv("TP_TEST_PYTHON");
  return named != nullptr && *named != '\0' ? named : "python3";
}

PythonPytorchConfig with_jobs() {
  PythonPytorchConfig config;
  config.python_exe = python();
  config.speech_jobs = true;
  config.startup_timeout = JobSessionHarness::kPatience;
  config.health_timeout = JobSessionHarness::kPatience;
  return config;
}

void expect_refused(const Result<void>& result, Error::Code code, std::string_view reason) {
  ASSERT_FALSE(result.has_value()) << reason;
  EXPECT_EQ(result.error().code, code) << reason;
  EXPECT_EQ(result.error().context.value_or(""), reason);
}

class JobSessionRunner : public ::testing::Test {
 protected:
  void SetUp() override {
    const std::string probe = python() + " -c 'import tensorplate_pytorch_backend' >/dev/null 2>&1";
    if (std::system(probe.c_str()) != 0) {  // NOLINT(concurrency-mt-unsafe)
      GTEST_SKIP() << "tensorplate_pytorch_backend is not importable by " << python();
    }
    h = std::make_unique<JobSessionHarness>(with_jobs(), default_fork_exec_launcher());
  }

  void TearDown() override {
    h.reset();
    if (!entry_.empty()) {
      std::filesystem::remove_all(entry_.parent_path());
    }
  }

  // The session loads the fixture and primes. `job_delay_ms` is how long the
  // fixture holds every job open.
  void up(int job_delay_ms = 0) {
    std::string artifact = "/dev/null";
    if (job_delay_ms != 0) {
      const auto dir =
          std::filesystem::temp_directory_path() / ("tp_job_session_" + std::to_string(::getpid()));
      std::filesystem::create_directories(dir);
      entry_ = dir / "entry.json";
      std::ofstream(entry_) << Json{{"job_delay_ms", job_delay_ms}}.dump();
      artifact = entry_.string();
    }
    const auto spec = ModelSpec::create("speech", ModelClass::Speech, artifact, "python_pytorch");
    ASSERT_TRUE(spec.has_value());
    const auto loaded = h->session->load(spec.value());
    ASSERT_TRUE(loaded.has_value()) << loaded.error().message;
    const auto primed = h->session->prime();
    ASSERT_TRUE(primed.has_value()) << primed.error().message;
  }

  std::filesystem::path entry_;
  std::unique_ptr<JobSessionHarness> h;
};

TEST_F(JobSessionRunner, BothSpeechClassesRunOnAChannelTheSessionStartedAndUnloadStopsIt) {
  expect_refused(h->channel->submit(JobSessionHarness::tts(1, 1)), Error::Code::NotReady,
                 "backend_not_ready");
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h->submit(h->stt(1, 1)).has_value());
  ASSERT_TRUE(h->submit(JobSessionHarness::tts(2, 1)).has_value());
  ASSERT_TRUE(h->wait_kinds(1, {Accepted, Completed, Released}));
  ASSERT_TRUE(h->wait_kinds(2, {Accepted, Completed, Released}));
  std::vector<std::size_t> chunks;
  for (const JobEvent& event : h->sink->events()) {
    if (event.kind() != Completed) {
      continue;
    }
    if (const auto* heard = std::get_if<TranscriptResult>(&event.result()->payload())) {
      EXPECT_EQ(heard->text, "hello");
    } else {
      const auto& chunk = std::get<AudioChunkResult>(event.result()->payload());
      chunks.push_back(chunk.pcm.byte_size);
      EXPECT_TRUE(h->manager->release(chunk.pcm.buffer).has_value());
    }
  }
  // The fixture returns 80 samples of silence for each byte of text.
  EXPECT_EQ(chunks, (std::vector<std::size_t>{160 * 12}));

  const auto unloaded = h->session->unload();
  ASSERT_TRUE(unloaded.has_value()) << unloaded.error().message;
  expect_refused(h->channel->submit(JobSessionHarness::tts(3, 1)), Error::Code::Unavailable,
                 "backend_unavailable");

  // The same session loads again, on a new sidecar and a restarted channel.
  ASSERT_NO_FATAL_FAILURE(up());
  ASSERT_TRUE(h->submit(h->stt(3, 2)).has_value());
  ASSERT_TRUE(h->wait_kinds(3, {Accepted, Completed, Released}));
  EXPECT_EQ(h->channel->counters(), JobChannelCounters{});
}

TEST_F(JobSessionRunner, DestroyingTheSessionWithAJobOpenFailsItAndReleasesIt) {
  // The fixture's hold is never waited out: the job has no terminal event
  // until the session goes, whether the runner has started it or not.
  ASSERT_NO_FATAL_FAILURE(up(/*job_delay_ms=*/60'000));
  ASSERT_TRUE(h->submit(h->stt(1, 1)).has_value());
  ASSERT_TRUE(h->wait_kinds(1, {Accepted}));
  h->session.reset();
  ASSERT_EQ(h->sink->kinds_for(1), (Kinds{Accepted, Failed, Released}));
  const Error failure = *h->sink->events().at(1).error();
  EXPECT_EQ(failure.code, Error::Code::Unavailable);
  EXPECT_EQ(failure.context.value_or(""), kSessionUnloaded);
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
