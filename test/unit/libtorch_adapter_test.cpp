// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F03 unit tests for the LibTorch native adapter.

#include <gtest/gtest.h>

#if !TP_ENABLE_LIBTORCH
TEST(LibTorchAdapter, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_LIBTORCH=OFF; LibTorch adapter not built";
}
#else

#include <string>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"

#include "adapters/libtorch/libtorch_session.hpp"

namespace tensorplate {
namespace {

TEST(LibTorchAdapter, RegistersUnderStableKey) {
  BackendRegistry reg;
  ASSERT_TRUE(register_libtorch_backend(reg).has_value());
  EXPECT_TRUE(reg.is_registered("libtorch"));
}

TEST(LibTorchAdapter, CapabilityRecordIsConsistent) {
  BackendRegistry reg;
  ASSERT_TRUE(register_libtorch_backend(reg).has_value());
  auto cap = reg.capability("libtorch");
  ASSERT_TRUE(cap.has_value());
  EXPECT_EQ(cap.value().backend_name(), "libtorch");
  EXPECT_EQ(cap.value().shape_support(), ShapeSupport::Dynamic);
  EXPECT_TRUE(cap.value().accepts_precision(PrecisionHint::Fp32));
  EXPECT_TRUE(cap.value().accepts_precision(PrecisionHint::Fp16));
  EXPECT_TRUE(cap.value().accepts_precision(PrecisionHint::BFloat16));
  EXPECT_FALSE(cap.value().accepts_precision(PrecisionHint::Int8));
  EXPECT_FALSE(cap.value().supports_async());
  EXPECT_FALSE(cap.value().supports_generation());
}

TEST(LibTorchAdapter, BackendNameMatchesCapability) {
  BackendRegistry reg;
  ASSERT_TRUE(register_libtorch_backend(reg).has_value());
  auto session = reg.create_session("libtorch");
  ASSERT_TRUE(session.has_value());
  EXPECT_EQ(std::string(session.value()->backend_name()), "libtorch");
  EXPECT_FALSE(session.value()->is_ready());
}

TEST(LibTorchAdapter, LoadWithoutSdkReportsUnsupported) {
  BackendRegistry reg;
  ASSERT_TRUE(register_libtorch_backend(reg).has_value());
  auto session = reg.create_session("libtorch");
  ASSERT_TRUE(session.has_value());

  auto spec = ModelSpec::create("torch-m", ModelClass::Vision, "/dev/null", "libtorch").value();
  auto r = session.value()->load(spec);
#if TP_HAS_LIBTORCH_SDK
  if (!r.has_value()) {
    EXPECT_NE(r.error().code, Error::Code::Unsupported);
  }
#else
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
#endif
}

TEST(LibTorchAdapter, PythonPytorchBundleNeverRedirectsToLibtorch) {
  // Co-register libtorch but not python_pytorch. Validate that a bundle
  // declaring `backend_hint: python_pytorch` still fails with
  // `Unsupported` rather than silently selecting the libtorch adapter.
  BackendRegistry reg;
  ASSERT_TRUE(register_libtorch_backend(reg).has_value());
  auto spec = ModelSpec::create("smolvla", ModelClass::Vla, "/dev/null", "python_pytorch").value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

}  // namespace
}  // namespace tensorplate

#endif  // TP_ENABLE_LIBTORCH
