// SPDX-License-Identifier: Apache-2.0
//
// V01-E05-F01-T02 / T03: Unit tests for `BackendRegistry`.

#include "tensorplate/backend/registry.hpp"

#include <gtest/gtest.h>

#include <memory>
#include <string>

#include "tensorplate/backend/capability.hpp"
#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"

#include "mock_execution_session.hpp"

namespace tensorplate {
namespace {

using testing::MockSession;

BackendCapability simple_cap(const std::string& name,
                             std::vector<PrecisionHint> precision = {PrecisionHint::Fp32}) {
  return BackendCapability::create(name, std::move(precision)).value();
}

ExecutionSessionFactory mock_factory(const std::string& name) {
  return [name](ExecutionSessionRuntimeHooks hooks)
             -> Result<std::unique_ptr<ExecutionSession>> {
    return std::unique_ptr<ExecutionSession>(
        new MockSession(name, hooks.event_sink, hooks.buffer_manager));
  };
}

TEST(BackendRegistry, RegisterAndLookup) {
  BackendRegistry reg;
  auto entry = BackendEntry{"mock", simple_cap("mock"), mock_factory("mock")};
  ASSERT_TRUE(reg.register_backend(std::move(entry)).has_value());

  EXPECT_TRUE(reg.is_registered("mock"));
  EXPECT_FALSE(reg.is_registered("absent"));

  auto cap = reg.capability("mock");
  ASSERT_TRUE(cap.has_value());
  EXPECT_EQ(cap.value().backend_name(), "mock");

  auto session = reg.create_session("mock");
  ASSERT_TRUE(session.has_value());
  EXPECT_EQ(std::string(session.value()->backend_name()), "mock");
}

TEST(BackendRegistry, DuplicateRegistrationRejected) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"mock", simple_cap("mock"), mock_factory("mock")}).has_value());

  auto r = reg.register_backend({"mock", simple_cap("mock"), mock_factory("mock")});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Internal);
}

TEST(BackendRegistry, EmptyNameRejected) {
  BackendRegistry reg;
  // capability with empty name fails to construct; build it manually
  // through a non-empty name then mismatch the registry entry.
  auto cap = simple_cap("real");
  auto r = reg.register_backend({"", std::move(cap), mock_factory("real")});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendRegistry, NullFactoryRejected) {
  BackendRegistry reg;
  auto r = reg.register_backend({"mock", simple_cap("mock"), nullptr});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendRegistry, NameMismatchRejected) {
  BackendRegistry reg;
  auto r = reg.register_backend({"mock", simple_cap("other"), mock_factory("mock")});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(BackendRegistry, LookupUnknownReturnsUnsupported) {
  BackendRegistry reg;
  auto cap = reg.capability("absent");
  ASSERT_FALSE(cap.has_value());
  EXPECT_EQ(cap.error().code, Error::Code::Unsupported);

  auto session = reg.create_session("absent");
  ASSERT_FALSE(session.has_value());
  EXPECT_EQ(session.error().code, Error::Code::Unsupported);
}

TEST(BackendRegistry, RegisteredBackendsSortedAndStable) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"libtorch", simple_cap("libtorch"), mock_factory("libtorch")})
                  .has_value());
  ASSERT_TRUE(reg.register_backend({"tensorrt", simple_cap("tensorrt"), mock_factory("tensorrt")})
                  .has_value());
  ASSERT_TRUE(reg.register_backend(
                     {"python_pytorch", simple_cap("python_pytorch"), mock_factory("python_pytorch")})
                  .has_value());

  auto names = reg.registered_backends();
  ASSERT_EQ(names.size(), 3u);
  EXPECT_EQ(names[0], "libtorch");
  EXPECT_EQ(names[1], "python_pytorch");
  EXPECT_EQ(names[2], "tensorrt");
}

TEST(BackendRegistry, DeregisterReturnsTrueOnHit) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"mock", simple_cap("mock"), mock_factory("mock")}).has_value());
  EXPECT_TRUE(reg.deregister_backend("mock"));
  EXPECT_FALSE(reg.is_registered("mock"));
  EXPECT_FALSE(reg.deregister_backend("mock"));
}

TEST(BackendRegistry, ValidateBackendHintRejectsUnknown) {
  BackendRegistry reg;
  auto spec = ModelSpec::create("m", ModelClass::Vision, "/dev/null", "missing").value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

TEST(BackendRegistry, ValidateBackendHintAcceptsAutoPrecision) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"tensorrt",
                                    simple_cap("tensorrt", {PrecisionHint::Fp16}),
                                    mock_factory("tensorrt")})
                  .has_value());
  auto spec = ModelSpec::create("vision-m", ModelClass::Vision, "/dev/null", "tensorrt",
                                PrecisionHint::Auto)
                  .value();
  ASSERT_TRUE(reg.validate_backend_hint(spec).has_value());
}

TEST(BackendRegistry, ValidateBackendHintRejectsUnsupportedPrecision) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"tensorrt",
                                    simple_cap("tensorrt", {PrecisionHint::Fp16}),
                                    mock_factory("tensorrt")})
                  .has_value());
  auto spec = ModelSpec::create("vision-m", ModelClass::Vision, "/dev/null", "tensorrt",
                                PrecisionHint::Fp32)
                  .value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
  EXPECT_TRUE(r.error().context.has_value());
}

TEST(BackendRegistry, PythonPytorchDoesNotFallBackToLibtorch) {
  BackendRegistry reg;
  ASSERT_TRUE(reg.register_backend({"libtorch", simple_cap("libtorch"), mock_factory("libtorch")})
                  .has_value());
  auto spec = ModelSpec::create("smolvla", ModelClass::Vla, "/dev/null", "python_pytorch").value();
  auto r = reg.validate_backend_hint(spec);
  ASSERT_FALSE(r.has_value()) << "python_pytorch must not silently redirect to libtorch";
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);

  auto session = reg.create_session("python_pytorch");
  ASSERT_FALSE(session.has_value());
  EXPECT_EQ(session.error().code, Error::Code::Unsupported);
}

struct NoopSink final : SessionEventSink {
  void on_event(const SessionEvent& /*event*/) override {}
};

TEST(BackendRegistry, FactoryReceivesHooks) {
  BackendRegistry reg;
  bool seen_sink = false;
  bool seen_manager = false;

  ExecutionSessionFactory f = [&](ExecutionSessionRuntimeHooks hooks)
      -> Result<std::unique_ptr<ExecutionSession>> {
    seen_sink = (hooks.event_sink != nullptr);
    seen_manager = (hooks.buffer_manager != nullptr);
    return std::unique_ptr<ExecutionSession>(new MockSession("mock"));
  };
  ASSERT_TRUE(reg.register_backend({"mock", simple_cap("mock"), std::move(f)}).has_value());

  NoopSink sink;
  BufferManagerConfig bm_cfg;
  bm_cfg.pool_name = "registry-test";
  bm_cfg.capacity_bytes = 1024;
  bm_cfg.max_buffer_bytes = 1024;
  auto bm = BufferManager::create(bm_cfg);
  ASSERT_TRUE(bm.has_value());

  ExecutionSessionRuntimeHooks hooks{};
  hooks.event_sink = &sink;
  hooks.buffer_manager = bm.value().get();
  auto s = reg.create_session("mock", hooks);
  ASSERT_TRUE(s.has_value());
  EXPECT_TRUE(seen_sink);
  EXPECT_TRUE(seen_manager);
}

}  // namespace
}  // namespace tensorplate
