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

#include <unistd.h>

#include <chrono>
#include <cstddef>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <memory>
#include <string>
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

}  // namespace
}  // namespace tensorplate

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
