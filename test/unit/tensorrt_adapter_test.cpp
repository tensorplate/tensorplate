// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F02 unit tests for the TensorRT adapter.
//
// These tests are compiled only when `TP_ENABLE_TENSORRT=1`. They
// exercise the registration / capability path and the no-SDK behavior
// of `do_load`. Real engine load + inference is gated by the
// `TP_HAS_TENSORRT_SDK` define and exercised by the V01-E05-F02-T03
// vision-golden fixture and the V01-E05-F06 hardware-in-loop tier.

#include <gtest/gtest.h>

#if !TP_ENABLE_TENSORRT
TEST(TensorRTAdapter, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_TENSORRT=OFF; TensorRT adapter not built";
}
#else

#include <string>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/backend/registry.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"

namespace tensorplate {
namespace {

TEST(TensorRTAdapter, RegistersUnderStableKey) {
  BackendRegistry reg;
  ASSERT_TRUE(register_tensorrt_backend(reg).has_value());
  EXPECT_TRUE(reg.is_registered("tensorrt"));
}

TEST(TensorRTAdapter, CapabilityRecordIsConsistent) {
  BackendRegistry reg;
  ASSERT_TRUE(register_tensorrt_backend(reg).has_value());
  auto cap = reg.capability("tensorrt");
  ASSERT_TRUE(cap.has_value());
  EXPECT_EQ(cap.value().backend_name(), "tensorrt");
  EXPECT_EQ(cap.value().shape_support(), ShapeSupport::Fixed);
  EXPECT_FALSE(cap.value().supports_async());
  EXPECT_FALSE(cap.value().supports_generation());
  EXPECT_FALSE(cap.value().supports_streaming());
  EXPECT_FALSE(cap.value().supports_kv_cache());
  EXPECT_TRUE(cap.value().accepts_precision(PrecisionHint::Fp16));
  EXPECT_TRUE(cap.value().accepts_precision(PrecisionHint::Int8));
  // Generation/streaming/KV-cache are v0.2 capabilities.
  EXPECT_FALSE(cap.value().supports_generation());
}

TEST(TensorRTAdapter, BackendNameMatchesCapability) {
  BackendRegistry reg;
  ASSERT_TRUE(register_tensorrt_backend(reg).has_value());
  auto session = reg.create_session("tensorrt");
  ASSERT_TRUE(session.has_value());
  EXPECT_EQ(std::string(session.value()->backend_name()), "tensorrt");
  EXPECT_FALSE(session.value()->is_ready());
}

TEST(TensorRTAdapter, LoadWithoutSdkReportsUnsupported) {
  BackendRegistry reg;
  ASSERT_TRUE(register_tensorrt_backend(reg).has_value());
  auto session = reg.create_session("tensorrt");
  ASSERT_TRUE(session.has_value());

  auto spec = ModelSpec::create("trt-vision", ModelClass::Vision, "/dev/null", "tensorrt").value();
  auto r = session.value()->load(spec);
#if TP_HAS_TENSORRT_SDK
  // With SDK present, `/dev/null` is either parseable (unlikely) or
  // returns LoadFailed. Either way the call should not return
  // Unsupported.
  if (!r.has_value()) {
    EXPECT_NE(r.error().code, Error::Code::Unsupported);
  }
#else
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
#endif
}

TEST(TensorRTAdapter, ValidateBackendHintAcceptsAdvertisedPrecision) {
  BackendRegistry reg;
  ASSERT_TRUE(register_tensorrt_backend(reg).has_value());
  auto spec =
      ModelSpec::create("v", ModelClass::Vision, "/dev/null", "tensorrt", PrecisionHint::Fp16)
          .value();
  EXPECT_TRUE(reg.validate_backend_hint(spec).has_value());
}

TEST(TensorRTAdapter, ValidateBackendHintRejectsBfloat16) {
  BackendRegistry reg;
  ASSERT_TRUE(register_tensorrt_backend(reg).has_value());
  auto spec =
      ModelSpec::create("v", ModelClass::Vision, "/dev/null", "tensorrt", PrecisionHint::BFloat16)
          .value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

}  // namespace
}  // namespace tensorplate

#endif  // TP_ENABLE_TENSORRT
