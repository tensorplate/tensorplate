// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01: Sanity test for `register_builtin_backends`. Confirms
// that the set of registered backends matches the build flags compiled
// into `tp_runtime`. Per-adapter conformance lands in the adapter
// feature tests (V01-E05-F02, F03, F05).

#include <gtest/gtest.h>

#include <algorithm>
#include <string>
#include <vector>

#include "tensorplate/backend/builtin.hpp"
#include "tensorplate/backend/registry.hpp"

namespace tensorplate {
namespace {

bool contains(const std::vector<std::string>& names, const std::string& needle) {
  return std::find(names.begin(), names.end(), needle) != names.end();
}

TEST(BackendBuiltin, RegistersExactlyEnabledAdapters) {
  BackendRegistry reg;
  auto r = register_builtin_backends(reg);
  ASSERT_TRUE(r.has_value()) << r.error().message;

  const auto names = reg.registered_backends();

#if TP_ENABLE_TENSORRT
  EXPECT_TRUE(contains(names, "tensorrt"));
#else
  EXPECT_FALSE(contains(names, "tensorrt"));
#endif

#if TP_ENABLE_LIBTORCH
  EXPECT_TRUE(contains(names, "libtorch"));
#else
  EXPECT_FALSE(contains(names, "libtorch"));
#endif

#if TP_ENABLE_PYTHON_PYTORCH_SIDECAR
  EXPECT_TRUE(contains(names, "python_pytorch"));
#else
  EXPECT_FALSE(contains(names, "python_pytorch"));
#endif
}

}  // namespace
}  // namespace tensorplate
