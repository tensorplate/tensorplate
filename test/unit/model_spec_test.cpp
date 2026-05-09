// SPDX-License-Identifier: Apache-2.0
//
// V01-E02-F02-T01 / T03 unit coverage for tensorplate::ModelSpec.

#include "tensorplate/core/model_spec.hpp"

#include <gtest/gtest.h>

#include <optional>
#include <string>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/result.hpp"

namespace {

using tensorplate::Error;
using tensorplate::model_class_from_string;
using tensorplate::ModelClass;
using tensorplate::ModelSpec;
using tensorplate::precision_hint_from_string;
using tensorplate::PrecisionHint;
using tensorplate::Result;
using tensorplate::to_string;

TEST(ModelSpec, ClassNamesRoundTripViaSnakeCase) {
  for (auto cls : {ModelClass::Vision, ModelClass::Speech, ModelClass::Language, ModelClass::Vla,
                   ModelClass::Embedding, ModelClass::Custom}) {
    auto parsed = model_class_from_string(to_string(cls));
    ASSERT_TRUE(parsed.has_value());
    EXPECT_EQ(*parsed, cls);
  }
}

TEST(ModelSpec, ClassNamesAreLockedToWireFormat) {
  EXPECT_EQ(to_string(ModelClass::Vision), "vision");
  EXPECT_EQ(to_string(ModelClass::Vla), "vla");
  EXPECT_EQ(to_string(ModelClass::Custom), "custom");
}

TEST(ModelSpec, PrecisionNamesRoundTripViaSnakeCase) {
  for (auto h : {PrecisionHint::Auto, PrecisionHint::Fp32, PrecisionHint::Fp16,
                 PrecisionHint::BFloat16, PrecisionHint::Int8, PrecisionHint::Int4}) {
    auto parsed = precision_hint_from_string(to_string(h));
    ASSERT_TRUE(parsed.has_value());
    EXPECT_EQ(*parsed, h);
  }
}

TEST(ModelSpec, PrecisionNamesAreLockedToWireFormat) {
  EXPECT_EQ(to_string(PrecisionHint::Auto), "auto");
  EXPECT_EQ(to_string(PrecisionHint::Fp16), "fp16");
  EXPECT_EQ(to_string(PrecisionHint::BFloat16), "bfloat16");
}

TEST(ModelSpec, FromStringRejectsUnknownNames) {
  EXPECT_FALSE(model_class_from_string("does_not_exist").has_value());
  EXPECT_FALSE(precision_hint_from_string("fp9").has_value());
  // Casing matters; wire format is snake_case lowercase.
  EXPECT_FALSE(model_class_from_string("Vision").has_value());
}

TEST(ModelSpec, ValidConstructionPreservesAllFields) {
  auto r = ModelSpec::create("yolov8n", ModelClass::Vision, "models/yolov8n.engine", "tensorrt",
                             PrecisionHint::Fp16, std::optional<std::string>{"orin-nano-fp16"});
  ASSERT_TRUE(r.has_value());
  const auto& s = r.value();
  EXPECT_EQ(s.model_id(), "yolov8n");
  EXPECT_EQ(s.model_class(), ModelClass::Vision);
  EXPECT_EQ(s.artifact_path(), "models/yolov8n.engine");
  EXPECT_EQ(s.backend_hint(), "tensorrt");
  EXPECT_EQ(s.precision_hint(), PrecisionHint::Fp16);
  ASSERT_TRUE(s.profile_id().has_value());
  EXPECT_EQ(*s.profile_id(), "orin-nano-fp16");
}

TEST(ModelSpec, ValidConstructionWithoutProfileId) {
  auto r = ModelSpec::create("smolvla-450m", ModelClass::Vla, "models/smolvla.pt", "python_pytorch",
                             PrecisionHint::Auto, std::nullopt);
  ASSERT_TRUE(r.has_value());
  EXPECT_FALSE(r.value().profile_id().has_value());
  EXPECT_EQ(r.value().precision_hint(), PrecisionHint::Auto);
}

TEST(ModelSpec, RejectsEmptyModelId) {
  auto r = ModelSpec::create("", ModelClass::Vision, "p", "tensorrt", PrecisionHint::Auto, {});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ModelSpec, RejectsEmptyArtifactPath) {
  auto r = ModelSpec::create("id", ModelClass::Vision, "", "tensorrt", PrecisionHint::Auto, {});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ModelSpec, RejectsEmptyBackendHint) {
  auto r = ModelSpec::create("id", ModelClass::Vision, "p", "", PrecisionHint::Auto, {});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ModelSpec, RejectsPresentButEmptyProfileId) {
  auto r = ModelSpec::create("id", ModelClass::Vision, "p", "tensorrt", PrecisionHint::Auto,
                             std::optional<std::string>{""});
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ModelSpec, EqualityComparesAllFields) {
  auto a = ModelSpec::create("id", ModelClass::Vision, "p", "tensorrt", PrecisionHint::Fp16, {});
  auto b = ModelSpec::create("id", ModelClass::Vision, "p", "tensorrt", PrecisionHint::Fp16, {});
  auto c = ModelSpec::create("id2", ModelClass::Vision, "p", "tensorrt", PrecisionHint::Fp16, {});
  ASSERT_TRUE(a.has_value() && b.has_value() && c.has_value());
  EXPECT_EQ(a.value(), b.value());
  EXPECT_NE(a.value(), c.value());
}

}  // namespace
