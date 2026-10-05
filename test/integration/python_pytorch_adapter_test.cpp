// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F05 / V01-E05-F06 integration test for the Python/PyTorch
// sidecar adapter. Spawns the in-tree `tensorplate_pytorch_backend`
// Python runner with the fixture backend and exercises the full
// `ExecutionSession` lifecycle (load, prime, infer, unload) through
// the C++ adapter.
//
// The test is gated on:
//   - TP_ENABLE_PYTHON_PYTORCH_SIDECAR=1 (the build flag)
//   - the in-tree Python package being importable (skip otherwise).
//
// CI provisions Python and pip-installs the backend package
// (`backends/python_pytorch/`) in editable mode; the test discovers the
// interpreter via the `TP_TEST_PYTHON` environment variable, falling
// back to `python3`.

#include <gtest/gtest.h>

#if !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(PythonPytorchAdapter, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <fcntl.h>
#include <sys/wait.h>
#include <unistd.h>

#include <chrono>
#include <csignal>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <memory>
#include <nlohmann/json.hpp>
#include <sstream>
#include <string>
#include <system_error>
#include <thread>
#include <utility>
#include <vector>

#include "tensorplate/backend/builtin.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/model_spec.hpp"

namespace tensorplate {
namespace {

std::string locate_python() {
  if (const char* env = std::getenv("TP_TEST_PYTHON"); env != nullptr && *env != '\0') {
    return env;
  }
  return "python3";
}

bool python_backend_available() {
  const std::string python = locate_python();
  std::string probe =
      python + " -c 'import tensorplate_pytorch_backend; print(\"ok\")' >/dev/null 2>&1";
  return std::system(probe.c_str()) == 0;
}

std::unique_ptr<BufferManager> make_manager() {
  BufferManagerConfig cfg;
  cfg.pool_name = "py_pytorch_test";
  cfg.capacity_bytes = 1 << 20;
  cfg.max_buffer_bytes = 1 << 18;
  auto r = BufferManager::create(std::move(cfg));
  EXPECT_TRUE(r.has_value());
  return std::move(r).value();
}

class PythonPytorchAdapterFixture : public ::testing::Test {
 protected:
  void SetUp() override {
    if (!python_backend_available()) {
      GTEST_SKIP() << "tensorplate_pytorch_backend not importable from " << locate_python()
                   << " (CI should install backends/python_pytorch/)";
    }
    setenv("TP_TEST_PYTHON_EXE", locate_python().c_str(), 1);
  }
};

TEST_F(PythonPytorchAdapterFixture, RegistersUnderStableKey) {
  BackendRegistry reg;
  ASSERT_TRUE(register_builtin_backends(reg).has_value());
  EXPECT_TRUE(reg.is_registered("python_pytorch"));
  auto cap = reg.capability("python_pytorch");
  ASSERT_TRUE(cap.has_value());
  EXPECT_FALSE(cap.value().supports_async());
  EXPECT_FALSE(cap.value().supports_generation());
}

TEST_F(PythonPytorchAdapterFixture, FixtureBackendEchoesInputs) {
  BackendRegistry reg;
  ASSERT_TRUE(register_builtin_backends(reg).has_value());

  auto manager = make_manager();
  ExecutionSessionRuntimeHooks hooks{};
  hooks.buffer_manager = manager.get();
  auto session_r = reg.create_session("python_pytorch", hooks);
  ASSERT_TRUE(session_r.has_value());
  auto session = std::move(session_r).value();

  auto spec =
      ModelSpec::create("smolvla-fixture", ModelClass::Vla, "/dev/null", "python_pytorch").value();
  auto load_r = session->load(spec);
  ASSERT_TRUE(load_r.has_value()) << load_r.error().message;
  ASSERT_TRUE(session->prime().has_value());

  // 16 bytes of float32: 4 values.
  std::vector<std::byte> input_bytes(16);
  for (std::size_t i = 0; i < input_bytes.size(); ++i) {
    input_bytes[i] = static_cast<std::byte>(i);
  }
  auto buf_r = manager->allocate(input_bytes.size());
  ASSERT_TRUE(buf_r.has_value());
  auto buf = buf_r.value();
  auto dst = manager->data(buf);
  ASSERT_TRUE(dst.has_value());
  std::memcpy(dst.value().data(), input_bytes.data(), input_bytes.size());

  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{"in0", buf, tv});
  auto req = InferRequest::create("req-1", "/infer", std::move(inputs)).value();

  auto r = session->infer(req);
  ASSERT_TRUE(r.has_value()) << r.error().message;
  ASSERT_TRUE(r.value().is_success()) << r.value().error().message;
  const auto& outs = r.value().outputs();
  ASSERT_EQ(outs.size(), 1u);
  EXPECT_EQ(outs[0].name, "echo_in0");
  auto out_view = manager->view(outs[0].buffer, outs[0].tensor);
  ASSERT_TRUE(out_view.has_value());
  ASSERT_EQ(out_view.value().size(), input_bytes.size());
  EXPECT_EQ(std::memcmp(out_view.value().data(), input_bytes.data(), input_bytes.size()), 0);

  // Release the allocated buffers.
  (void)manager->release_if_owned(buf);
  (void)manager->release_if_owned(outs[0].buffer);

  ASSERT_TRUE(session->unload().has_value());
}

TEST_F(PythonPytorchAdapterFixture, InferBeforePrimeReturnsNotReady) {
  BackendRegistry reg;
  ASSERT_TRUE(register_builtin_backends(reg).has_value());
  auto manager = make_manager();
  ExecutionSessionRuntimeHooks hooks{};
  hooks.buffer_manager = manager.get();
  auto session = reg.create_session("python_pytorch", hooks).value();
  auto spec = ModelSpec::create("m", ModelClass::Vla, "/dev/null", "python_pytorch").value();
  ASSERT_TRUE(session->load(spec).has_value());

  auto tv = TensorView::create(DType::Float32, {1, 1}).value();
  auto buf = manager->allocate(4).value();
  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{"x", buf, tv});
  auto req = InferRequest::create("r", "/infer", std::move(inputs)).value();
  auto r = session->infer(req);
  // The NVI wrapper rejects with NotReady before the adapter is hit.
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::NotReady);
  (void)manager->release_if_owned(buf);
  ASSERT_TRUE(session->unload().has_value());
}

TEST_F(PythonPytorchAdapterFixture, InferAsyncReturnsUnsupportedWithoutAllocatingOutputs) {
  BackendRegistry reg;
  ASSERT_TRUE(register_builtin_backends(reg).has_value());
  auto manager = make_manager();
  ExecutionSessionRuntimeHooks hooks{};
  hooks.buffer_manager = manager.get();
  auto session = reg.create_session("python_pytorch", hooks).value();
  auto spec = ModelSpec::create("m", ModelClass::Vla, "/dev/null", "python_pytorch").value();
  ASSERT_TRUE(session->load(spec).has_value());
  ASSERT_TRUE(session->prime().has_value());

  auto tv = TensorView::create(DType::Float32, {1, 1}).value();
  auto buf = manager->allocate(4).value();
  std::vector<NamedInput> inputs;
  inputs.push_back(NamedInput{"x", buf, tv});
  auto req = InferRequest::create("async-r", "/infer", std::move(inputs)).value();

  const auto before = manager->accounting().active_count;
  auto r = session->infer_async(req);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
  EXPECT_EQ(manager->accounting().active_count, before);

  (void)manager->release_if_owned(buf);
  ASSERT_TRUE(session->unload().has_value());
}

// Sends this process's stderr, which a sidecar it starts inherits, to a file
// until finish().
class StderrCapture {
 public:
  explicit StderrCapture(std::filesystem::path file)
      : file_(std::move(file)), saved_(::dup(STDERR_FILENO)) {
    const int fd = ::open(file_.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (fd >= 0) {
      active_ = saved_ >= 0 && ::dup2(fd, STDERR_FILENO) == STDERR_FILENO;
      ::close(fd);
    }
  }
  StderrCapture(const StderrCapture&) = delete;
  StderrCapture& operator=(const StderrCapture&) = delete;
  ~StderrCapture() { restore(); }

  [[nodiscard]] bool active() const noexcept { return active_; }

  std::string finish() {
    restore();
    std::ifstream in(file_);
    return {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
  }

 private:
  void restore() {
    if (saved_ >= 0) {
      std::fflush(stderr);
      ::dup2(saved_, STDERR_FILENO);
      ::close(saved_);
      saved_ = -1;
    }
  }

  std::filesystem::path file_;
  int saved_;
  bool active_ = false;
};

struct ScratchDir {
  explicit ScratchDir(std::filesystem::path p) : path(std::move(p)) {}
  ScratchDir(const ScratchDir&) = delete;
  ScratchDir& operator=(const ScratchDir&) = delete;
  ~ScratchDir() {
    std::error_code ignored;
    std::filesystem::remove_all(path, ignored);
  }

  std::filesystem::path path;
};

// The sidecar's error edge, seen from the adapter: a load that fails on a
// path or profile name carrying a canary returns a typed error with a fixed
// message and no context, and the sidecar writes none of it to stderr.
TEST_F(PythonPytorchAdapterFixture, LoadErrorsCarryNoPathOrProfileName) {
  const std::string canary = "tp-canary-7f3a9c";
  const ScratchDir scratch{std::filesystem::temp_directory_path() /
                           ("tp-sidecar-edge-" + std::to_string(::getpid()))};
  const auto& dir = scratch.path;
  std::filesystem::remove_all(dir);
  std::filesystem::create_directories(dir / canary);
  std::filesystem::create_directories(dir / (canary + ".json"));
  {
    std::ofstream entry(dir / "entry.json");
    entry << R"({"backend_profile": ")" << canary << R"("})";
  }
  const std::vector<std::pair<std::filesystem::path, std::string>> cases = {
      {dir / canary / "missing.json", "sidecar config not found"},
      {dir / (canary + ".json"), "sidecar config could not be read"},
      {dir / "entry.json", "no sidecar backend is registered for the requested profile"},
  };

  for (const auto& [artifact_path, message] : cases) {
    SCOPED_TRACE(artifact_path.string());
    BackendRegistry reg;
    ASSERT_TRUE(register_builtin_backends(reg).has_value());
    auto manager = make_manager();
    ExecutionSessionRuntimeHooks hooks{};
    hooks.buffer_manager = manager.get();
    auto session = reg.create_session("python_pytorch", hooks).value();
    auto spec = ModelSpec::create("m", ModelClass::Custom, artifact_path.string(), "python_pytorch")
                    .value();

    StderrCapture capture(dir / "stderr.txt");
    ASSERT_TRUE(capture.active());
    auto load_r = session->load(spec);
    const std::string written = capture.finish();

    ASSERT_FALSE(load_r.has_value());
    EXPECT_EQ(load_r.error().code, Error::Code::ConfigInvalid);
    EXPECT_EQ(load_r.error().message, message);
    EXPECT_FALSE(load_r.error().context.has_value());
    EXPECT_EQ(written.find(canary), std::string::npos) << written;
  }
}

// Sets a variable for one test and puts back what it found.
class ScopedEnv {
 public:
  ScopedEnv(const char* name, const std::string& value) : name_(name) {
    if (const char* old = std::getenv(name); old != nullptr) {
      saved_ = old;
      had_value_ = true;
    }
    setenv(name, value.c_str(), 1);
  }
  ScopedEnv(const ScopedEnv&) = delete;
  ScopedEnv& operator=(const ScopedEnv&) = delete;
  ~ScopedEnv() {
    if (had_value_) {
      setenv(name_, saved_.c_str(), 1);
    } else {
      unsetenv(name_);
    }
  }

 private:
  const char* name_;
  std::string saved_;
  bool had_value_ = false;
};

std::string read_text(const std::filesystem::path& path) {
  std::ifstream in(path);
  return {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
}

// The packaged descriptor and the packaged `faster_whisper` declaration as
// an install leaves them under `dir`, with the profile's environment moved
// under `dir` too. Its interpreter writes how it was started to
// `dir/started` and then runs the test's Python.
std::filesystem::path install_faster_whisper_profile(const std::filesystem::path& dir) {
  const std::filesystem::path source{TP_SOURCE_DIR};
  const auto backend = dir / "backends" / "python_pytorch";
  const auto environment_root = dir / "speech-runtime";
  std::filesystem::create_directories(backend / "runner_profiles.d");
  std::filesystem::create_directories(environment_root / "bin");
  std::filesystem::copy_file(source / "packaging/backend-metadata/python_pytorch.json",
                             backend / "backend.json");
  std::string declaration =
      read_text(source / "packaging/backend-metadata/runner_profiles/faster_whisper.json");
  const std::string packaged_root = "/usr/lib/tensorplate/speech-runtime";
  for (auto at = declaration.find(packaged_root); at != std::string::npos;
       at = declaration.find(packaged_root, at + environment_root.string().size())) {
    declaration.replace(at, packaged_root.size(), environment_root.string());
  }
  { std::ofstream(backend / "runner_profiles.d" / "faster_whisper.json") << declaration; }

  const auto interpreter = environment_root / "bin" / "python";
  {
    std::ofstream script(interpreter);
    script
        << "#!/bin/sh\n"
        << "printf '%s\\n' \"$0\" \"$LD_LIBRARY_PATH\" \"$ORT_DISABLE_TELEMETRY\" \"$TMPDIR\" > '"
        << (dir / "started").string() << "'\n"
        << "exec '" << locate_python() << "' \"$@\"\n";
  }
  std::filesystem::permissions(interpreter, std::filesystem::perms::owner_all);
  return interpreter;
}

std::vector<std::string> lines_of(const std::filesystem::path& path) {
  std::ifstream in(path);
  std::vector<std::string> lines;
  for (std::string line; std::getline(in, line);) {
    lines.push_back(line);
  }
  return lines;
}

// Through the registered backend and a real process: a model that selects a
// runner profile is served by the interpreter the installed descriptor
// declares, while the variable that names one for other models points at a
// file that does not exist.
TEST_F(PythonPytorchAdapterFixture, RunnerProfileIsServedByTheDescriptorsInterpreter) {
  const ScratchDir scratch{std::filesystem::temp_directory_path() /
                           ("tp-runner-profile-" + std::to_string(::getpid()))};
  const auto& dir = scratch.path;
  std::filesystem::remove_all(dir);
  const auto interpreter = install_faster_whisper_profile(dir);
  const ScopedEnv descriptors{"TP_BACKEND_DESCRIPTOR_DIR", (dir / "backends").string()};
  const ScopedEnv other{"TP_PYTHON_PYTORCH_EXECUTABLE", (dir / "no-such-python").string()};
  std::filesystem::create_directories(dir / "tmp");
  const ScopedEnv worker_tmp{"TMPDIR", (dir / "tmp").string()};

  BackendRegistry reg;
  ASSERT_TRUE(register_builtin_backends(reg).has_value());
  auto manager = make_manager();
  ExecutionSessionRuntimeHooks hooks{};
  hooks.buffer_manager = manager.get();
  auto session = reg.create_session("python_pytorch", hooks).value();
  auto spec = ModelSpec::create("fixture", ModelClass::Vla, "/dev/null", "python_pytorch",
                                PrecisionHint::Auto, std::nullopt, "faster_whisper")
                  .value();

  auto load_r = session->load(spec);
  ASSERT_TRUE(load_r.has_value()) << load_r.error().message;
  EXPECT_TRUE(session->prime().has_value());
  EXPECT_TRUE(session->unload().has_value());

  EXPECT_EQ(lines_of(dir / "started"),
            (std::vector<std::string>{
                interpreter.string(),
                (dir / "speech-runtime/lib/python3.12/site-packages/nvidia/cublas/lib").string(),
                "1",
                (dir / "tmp").string(),
            }));
}

#ifdef TP_SERVING_WORKER_BINARY

struct WorkerRun {
  int exit_status;
  std::string stderr_text;
};

WorkerRun run_serving_worker(const std::string& arguments, const std::filesystem::path& dir) {
  const std::filesystem::path stderr_file = dir / "worker-stderr.txt";
  const std::string command = std::string{"'"} + TP_SERVING_WORKER_BINARY + "' " + arguments +
                              " 2> '" + stderr_file.string() + "'";
  const int status = std::system(command.c_str());
  std::ifstream in(stderr_file);
  return {WIFEXITED(status) ? WEXITSTATUS(status) : -1,
          {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()}};
}

// The last line of a worker's stderr as JSON, without its clock reading.
nlohmann::json last_record(const std::string& text) {
  std::istringstream lines(text);
  std::string line;
  std::string last;
  while (std::getline(lines, line)) {
    if (!line.empty()) {
      last = line;
    }
  }
  auto record = nlohmann::json::parse(last, nullptr, /*allow_exceptions=*/false);
  if (record.is_object()) {
    record.erase("ts_ns");
  }
  return record;
}

// run_serving_worker for a worker that must refuse to start: one still
// running at `limit` is killed with its children and reported as -1, where
// waiting on it would never return.
WorkerRun run_serving_worker_within(std::chrono::seconds limit, const std::string& arguments,
                                    const std::filesystem::path& dir) {
  const std::filesystem::path stderr_file = dir / "worker-stderr.txt";
  const std::string command = std::string{"exec '"} + TP_SERVING_WORKER_BINARY + "' " + arguments;
  const pid_t pid = ::fork();
  if (pid == 0) {
    ::setpgid(0, 0);
    const int fd = ::open(stderr_file.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0 || ::dup2(fd, STDERR_FILENO) < 0) {
      std::_Exit(126);
    }
    ::execl("/bin/sh", "sh", "-c", command.c_str(), static_cast<char*>(nullptr));
    std::_Exit(127);
  }
  int status = 0;
  bool exited = false;
  const auto deadline = std::chrono::steady_clock::now() + limit;
  while (pid > 0 && !exited && std::chrono::steady_clock::now() < deadline) {
    exited = ::waitpid(pid, &status, WNOHANG) == pid;
    if (!exited) {
      std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }
  }
  if (pid > 0 && !exited) {
    ::kill(-pid, SIGKILL);
    ::waitpid(pid, &status, 0);
  }
  std::ifstream in(stderr_file);
  return {exited && WIFEXITED(status) ? WEXITSTATUS(status) : -1,
          {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()}};
}

// From the worker's configuration to its last stderr line: a model whose
// runner profile no installed package declares is refused with a typed code
// before any sidecar starts, although the environment names an interpreter.
TEST_F(PythonPytorchAdapterFixture, WorkerRefusesARunnerProfileThatIsNotInstalled) {
  const ScratchDir scratch{std::filesystem::temp_directory_path() /
                           ("tp-worker-runner-profile-" + std::to_string(::getpid()))};
  const auto& dir = scratch.path;
  std::filesystem::remove_all(dir);
  install_faster_whisper_profile(dir);
  const ScopedEnv descriptors{"TP_BACKEND_DESCRIPTOR_DIR", (dir / "backends").string()};

  nlohmann::json config;
  config["schema_version"] = "0.1";
  config["bind"] = {{"host", "127.0.0.1"}, {"port", 0}, {"allow_non_loopback", false}};
  config["enable_stderr_logs"] = true;
  config["deployment"] = {{"use_mock_session", false},
                          {"endpoint", "tts"},
                          {"backend", "python_pytorch"},
                          {"model",
                           {{"model_id", "tts"},
                            {"model_class", "speech"},
                            {"artifact_path", "/dev/null"},
                            {"backend_hint", "python_pytorch"},
                            {"precision_hint", "auto"},
                            {"runner_profile", "kokoro"}}}};
  const auto config_path = dir / "serving.json";
  { std::ofstream(config_path) << config.dump(); }

  const auto run = run_serving_worker_within(std::chrono::seconds{30},
                                             "--config '" + config_path.string() + "'", dir);

  ASSERT_EQ(run.exit_status, 65) << run.stderr_text;
  const auto record = last_record(run.stderr_text);
  ASSERT_TRUE(record.is_object()) << run.stderr_text;
  EXPECT_EQ(record["message"], "worker startup failed");
  EXPECT_EQ(record["fields"]["code"], "unsupported");
  EXPECT_EQ(record["fields"]["message"], "runner profile is not installed");
  EXPECT_FALSE(std::filesystem::exists(dir / "started"));
}

// The stderr the agent's deploy tests replay is what this worker writes: a
// Kokoro entry selecting an undeclared voice is refused by the runner, and
// the worker's last line carries the runner's code before it exits.
TEST_F(PythonPytorchAdapterFixture, WorkerReportsTheRunnersLoadCodeBeforeExiting) {
  const std::filesystem::path source{TP_SOURCE_DIR};
  const ScratchDir scratch{std::filesystem::temp_directory_path() /
                           ("tp-worker-startup-" + std::to_string(::getpid()))};
  const auto& dir = scratch.path;
  std::filesystem::remove_all(dir);
  std::filesystem::create_directories(dir);
  std::filesystem::copy(source / "test/models/bundles/v0_1/tts_kokoro_candidate", dir / "bundle",
                        std::filesystem::copy_options::recursive);
  const auto entry_path = dir / "bundle" / "tts-kokoro-candidate.json";
  std::string entry;
  {
    std::ifstream in(entry_path);
    entry.assign(std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>());
  }
  const std::string declared = R"("voice": "af_heart")";
  const auto at = entry.find(declared);
  ASSERT_NE(at, std::string::npos);
  entry.replace(at, declared.size(), R"("voice": "af_undeclared")");
  { std::ofstream(entry_path, std::ios::trunc) << entry; }

  nlohmann::json config;
  config["schema_version"] = "0.1";
  config["bind"] = {{"host", "127.0.0.1"}, {"port", 0}, {"allow_non_loopback", false}};
  config["enable_stderr_logs"] = true;
  config["deployment"] = {{"use_mock_session", false},
                          {"endpoint", "kokoro-undeclared-voice"},
                          {"backend", "python_pytorch"},
                          {"model",
                           {{"model_id", "kokoro-undeclared-voice"},
                            {"model_class", "custom"},
                            {"artifact_path", entry_path.string()},
                            {"backend_hint", "python_pytorch"},
                            {"precision_hint", "auto"}}}};
  const auto config_path = dir / "serving.json";
  { std::ofstream(config_path) << config.dump(); }

  const auto run = run_serving_worker("--config '" + config_path.string() + "'", dir);

  EXPECT_EQ(run.exit_status, 65) << run.stderr_text;
  std::ifstream recorded(source /
                         "agent/tests/fixtures/worker_stderr/kokoro_undeclared_voice.stderr");
  ASSERT_TRUE(recorded.is_open());
  const std::string fixture{std::istreambuf_iterator<char>(recorded),
                            std::istreambuf_iterator<char>()};
  const auto expected = last_record(fixture);
  ASSERT_TRUE(expected.is_object());
  EXPECT_EQ(expected["message"], "worker startup failed");
  EXPECT_EQ(expected["fields"]["code"], "unsupported");
  EXPECT_EQ(last_record(run.stderr_text), expected) << run.stderr_text;
}

// A config the worker cannot parse is reported the same way, with stderr
// logs never having been enabled.
TEST(ServingWorkerStartup, ConfigErrorIsReportedAsTheLastStderrLine) {
  const ScratchDir scratch{std::filesystem::temp_directory_path() /
                           ("tp-worker-config-" + std::to_string(::getpid()))};
  std::filesystem::remove_all(scratch.path);
  std::filesystem::create_directories(scratch.path);

  const auto run = run_serving_worker("--config-json '{'", scratch.path);

  EXPECT_EQ(run.exit_status, 64) << run.stderr_text;
  const auto record = last_record(run.stderr_text);
  ASSERT_TRUE(record.is_object()) << run.stderr_text;
  EXPECT_EQ(record["level"], "error");
  EXPECT_EQ(record["component"], "serving");
  EXPECT_EQ(record["message"], "worker startup failed");
  EXPECT_EQ(record["fields"]["code"], "config_invalid");
  EXPECT_TRUE(record["fields"]["message"].is_string());
}

#endif  // TP_SERVING_WORKER_BINARY

}  // namespace
}  // namespace tensorplate

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
