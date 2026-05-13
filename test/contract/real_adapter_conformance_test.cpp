// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F06-T01: Real-adapter contract harness.
//
// Reuses the V01-E04 ExecutionSession conformance suite
// (`test/contract/execution_session_conformance.hpp`) and runs it
// against every adapter compiled into this build of `tp_runtime` that
// is capable of completing the success branch. The conformance suite
// drives each adapter only through an `ExecutionSession*`, so a passing
// run proves the adapter respects the V01-E04 NVI contract end to end.

#include <gtest/gtest.h>

#include <cstdlib>
#include <memory>
#include <string>

#include "tensorplate/backend/builtin.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/execution_session.hpp"

#include "execution_session_conformance.hpp"

namespace tensorplate {
namespace {

#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR

bool python_backend_available() {
  const char* env = std::getenv("TP_TEST_PYTHON");
  std::string probe = std::string(env != nullptr ? env : "python3") +
                      " -c 'import tensorplate_pytorch_backend' >/dev/null 2>&1";
  return std::system(probe.c_str()) == 0;
}

BackendRegistry& shared_registry() {
  static BackendRegistry reg;
  static bool inited = false;
  if (!inited) {
    auto r = register_builtin_backends(reg);
    EXPECT_TRUE(r.has_value()) << (r.has_value() ? "" : r.error().message);
    inited = true;
  }
  return reg;
}

BufferManager* shared_manager() {
  static std::unique_ptr<BufferManager> mgr = []() {
    BufferManagerConfig bm;
    bm.pool_name = "real-adapter-conformance";
    bm.capacity_bytes = 1 << 20;
    bm.max_buffer_bytes = 1 << 18;
    return BufferManager::create(std::move(bm)).value();
  }();
  return mgr.get();
}

TEST(RealAdapterConformance, PythonPytorchSatisfiesV01E04Contract) {
  if (!python_backend_available()) {
    GTEST_SKIP() << "tensorplate_pytorch_backend not importable";
  }

  testing::ConformanceConfig cfg;
  cfg.expected_backend_name = "python_pytorch";
  cfg.backend_hint = "python_pytorch";

  testing::SessionFactory factory = []() -> std::unique_ptr<ExecutionSession> {
    ExecutionSessionRuntimeHooks hooks{};
    hooks.buffer_manager = shared_manager();
    auto session = shared_registry().create_session("python_pytorch", hooks);
    EXPECT_TRUE(session.has_value()) << (session.has_value() ? "" : session.error().message);
    return std::move(session).value();
  };
  testing::run_execution_session_conformance(cfg, factory);
}

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR

}  // namespace
}  // namespace tensorplate
