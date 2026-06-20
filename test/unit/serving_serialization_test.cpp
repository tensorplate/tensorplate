// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F03: serialization (decode/encode) unit tests.

#include <gtest/gtest.h>

#include <cstddef>
#include <cstdint>
#include <nlohmann/json.hpp>
#include <string>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/serving/serialization.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::serving;

void append_u32_le(std::string& out, std::uint32_t value) {
  out.push_back(static_cast<char>(value & 0xFFU));
  out.push_back(static_cast<char>((value >> 8U) & 0xFFU));
  out.push_back(static_cast<char>((value >> 16U) & 0xFFU));
  out.push_back(static_cast<char>((value >> 24U) & 0xFFU));
}

std::string make_binary_infer_body(const nlohmann::json& metadata, const std::string& payload) {
  const std::string meta = metadata.dump();
  std::string body;
  body.append(kBinaryInferMagic.data(), kBinaryInferMagic.size());
  append_u32_le(body, static_cast<std::uint32_t>(meta.size()));
  body.append(meta);
  body.append(payload);
  return body;
}

TEST(ServingSerialization, Base64RoundTrip) {
  std::string s = "TensorPlate";
  std::vector<std::byte> bytes(s.size());
  for (size_t i = 0; i < s.size(); ++i) {
    bytes[i] = static_cast<std::byte>(s[i]);
  }
  auto enc = base64_encode(bytes.data(), bytes.size());
  auto dec_r = base64_decode(enc);
  ASSERT_TRUE(dec_r);
  auto dec = std::move(dec_r).value();
  ASSERT_EQ(dec.size(), bytes.size());
  for (size_t i = 0; i < bytes.size(); ++i) {
    EXPECT_EQ(dec[i], bytes[i]);
  }
}

TEST(ServingSerialization, Base64RejectsInvalidLength) {
  auto r = base64_decode("AAA");
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingSerialization, DecodeInferRequestMinimal) {
  const std::string body = R"({
    "request_id": "req-1",
    "endpoint": "default",
    "inputs": [
      {
        "name": "image",
        "tensor": {"dtype": "uint8", "shape": [2, 2], "byte_size": 4},
        "payload_b64": "AAECAw=="
      }
    ]
  })";
  auto r = decode_infer_request(body);
  ASSERT_TRUE(r) << r.error().message;
  auto d = std::move(r).value();
  EXPECT_EQ(d.request_id, "req-1");
  EXPECT_EQ(d.endpoint, "default");
  ASSERT_EQ(d.inputs.size(), 1U);
  EXPECT_EQ(d.inputs[0].name, "image");
  EXPECT_EQ(d.inputs[0].bytes.size(), 4U);
}

TEST(ServingSerialization, DecodeInferRequestRejectsEmptyId) {
  const std::string body = R"({"request_id": "", "endpoint": "x", "inputs": [{}]})";
  auto r = decode_infer_request(body);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingSerialization, DecodeInferRequestRejectsBadDtype) {
  const std::string body = R"({
    "request_id": "r", "endpoint": "e", "inputs": [
      {"name": "x", "tensor": {"dtype": "wibble", "shape": [1]}, "payload_b64": ""}
    ]
  })";
  auto r = decode_infer_request(body);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

TEST(ServingSerialization, DecodeInferRequestRejectsShortPayload) {
  // Declares byte_size 8 but supplies only 4 bytes.
  const std::string body = R"({
    "request_id": "r", "endpoint": "e", "inputs": [
      {"name": "x",
       "tensor": {"dtype": "uint8", "shape": [8], "byte_size": 8},
       "payload_b64": "AAECAw=="}
    ]
  })";
  auto r = decode_infer_request(body);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
}

TEST(ServingSerialization, DecodeInferRequestCarriesMetadataAndDeadline) {
  const std::string body = R"({
    "request_id": "r", "endpoint": "e",
    "metadata": {"correlation_id": "cid-1", "action_chunk_id": "c", "action_chunk_sequence": 7},
    "deadline_ms": 100,
    "inputs": [
      {"name": "x", "tensor": {"dtype": "uint8", "shape": [4], "byte_size": 4},
       "payload_b64": "AAECAw=="}
    ]
  })";
  auto r = decode_infer_request(body);
  ASSERT_TRUE(r) << r.error().message;
  auto d = std::move(r).value();
  ASSERT_TRUE(d.metadata.correlation_id.has_value());
  EXPECT_EQ(*d.metadata.correlation_id, "cid-1");
  EXPECT_EQ(*d.metadata.action_chunk_id, "c");
  EXPECT_EQ(*d.metadata.action_chunk_sequence, 7);
  ASSERT_TRUE(d.relative_deadline.has_value());
  EXPECT_EQ(d.relative_deadline->count(), 100);
}

TEST(ServingSerialization, DecodeBinaryInferRequestMinimal) {
  nlohmann::json body;
  body["schema_version"] = "0.1";
  body["request_id"] = "bin-1";
  body["endpoint"] = "default";
  body["inputs"] =
      nlohmann::json::array({{{"name", "image"},
                              {"tensor", {{"dtype", "uint8"}, {"shape", {2, 2}}, {"byte_size", 4}}},
                              {"payload_offset", 0},
                              {"payload_size", 4}}});
  auto r = decode_binary_infer_request(make_binary_infer_body(body, std::string{"\0\1\2\3", 4}));
  ASSERT_TRUE(r) << r.error().message;
  auto d = std::move(r).value();
  EXPECT_EQ(d.request_id, "bin-1");
  ASSERT_EQ(d.inputs.size(), 1U);
  EXPECT_EQ(d.inputs[0].bytes.size(), 4U);
  EXPECT_EQ(std::to_integer<unsigned int>(d.inputs[0].bytes[3]), 3U);
}

TEST(ServingSerialization, DecodeBinaryInferRequestAcceptsMultiInput) {
  nlohmann::json body;
  body["schema_version"] = "0.1";
  body["request_id"] = "bin-multi";
  body["endpoint"] = "default";
  body["inputs"] =
      nlohmann::json::array({{{"name", "image"},
                              {"tensor", {{"dtype", "uint8"}, {"shape", {2}}, {"byte_size", 2}}},
                              {"payload_offset", 0},
                              {"payload_size", 2}},
                             {{"name", "meta"},
                              {"tensor", {{"dtype", "int32"}, {"shape", {1}}, {"byte_size", 4}}},
                              {"payload_offset", 2},
                              {"payload_size", 4}}});
  auto r =
      decode_binary_infer_request(make_binary_infer_body(body, std::string{"\1\2\3\4\5\6", 6}));
  ASSERT_TRUE(r) << r.error().message;
  auto d = std::move(r).value();
  ASSERT_EQ(d.inputs.size(), 2U);
  EXPECT_EQ(d.inputs[0].name, "image");
  EXPECT_EQ(d.inputs[0].bytes.size(), 2U);
  EXPECT_EQ(d.inputs[1].name, "meta");
  EXPECT_EQ(d.inputs[1].bytes.size(), 4U);
  EXPECT_EQ(std::to_integer<unsigned int>(d.inputs[1].bytes[3]), 6U);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsTruncatedHeader) {
  auto r = decode_binary_infer_request("TPINFER");
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsBadMagic) {
  auto r = decode_binary_infer_request("not-binary");
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsTruncatedMetadata) {
  std::string body;
  body.append(kBinaryInferMagic.data(), kBinaryInferMagic.size());
  append_u32_le(body, 10);
  body.append("{}");

  auto r = decode_binary_infer_request(body);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsMalformedMetadata) {
  std::string body;
  body.append(kBinaryInferMagic.data(), kBinaryInferMagic.size());
  append_u32_le(body, 1);
  body.append("{");

  auto r = decode_binary_infer_request(body);
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsShortPayload) {
  nlohmann::json body;
  body["request_id"] = "short";
  body["endpoint"] = "default";
  body["inputs"] =
      nlohmann::json::array({{{"name", "image"},
                              {"tensor", {{"dtype", "uint8"}, {"shape", {4}}, {"byte_size", 4}}},
                              {"payload_offset", 0},
                              {"payload_size", 2}}});
  auto r = decode_binary_infer_request(make_binary_infer_body(body, std::string{"\0\1", 2}));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsInvalidOffset) {
  nlohmann::json body;
  body["request_id"] = "offset";
  body["endpoint"] = "default";
  body["inputs"] =
      nlohmann::json::array({{{"name", "image"},
                              {"tensor", {{"dtype", "uint8"}, {"shape", {1}}, {"byte_size", 1}}},
                              {"payload_offset", -1},
                              {"payload_size", 1}}});
  auto r = decode_binary_infer_request(make_binary_infer_body(body, std::string{"\0", 1}));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ConfigInvalid);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsPayloadOverrun) {
  nlohmann::json body;
  body["request_id"] = "overrun";
  body["endpoint"] = "default";
  body["inputs"] =
      nlohmann::json::array({{{"name", "image"},
                              {"tensor", {{"dtype", "uint8"}, {"shape", {1}}, {"byte_size", 1}}},
                              {"payload_offset", 3},
                              {"payload_size", 2}}});
  auto r = decode_binary_infer_request(make_binary_infer_body(body, std::string{"\0\1\2\3", 4}));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsBadDtype) {
  nlohmann::json body;
  body["request_id"] = "dtype";
  body["endpoint"] = "default";
  body["inputs"] =
      nlohmann::json::array({{{"name", "image"},
                              {"tensor", {{"dtype", "wibble"}, {"shape", {1}}, {"byte_size", 1}}},
                              {"payload_offset", 0},
                              {"payload_size", 1}}});
  auto r = decode_binary_infer_request(make_binary_infer_body(body, std::string{"\0", 1}));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

TEST(ServingSerialization, DecodeBinaryInferRequestRejectsBadLayout) {
  nlohmann::json body;
  body["request_id"] = "layout";
  body["endpoint"] = "default";
  body["inputs"] = nlohmann::json::array(
      {{{"name", "image"},
        {"tensor", {{"dtype", "uint8"}, {"layout", "strided"}, {"shape", {1}}, {"byte_size", 1}}},
        {"payload_offset", 0},
        {"payload_size", 1}}});
  auto r = decode_binary_infer_request(make_binary_infer_body(body, std::string{"\0", 1}));
  ASSERT_FALSE(r);
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);
}

TEST(ServingSerialization, RenderBinaryInferResponseRoundTripsMetadataAndPayload) {
  BufferManagerConfig cfg;
  auto manager_r = BufferManager::create(cfg);
  ASSERT_TRUE(manager_r);
  auto manager = std::move(manager_r).value();

  auto view = TensorView::create(DType::UInt8, {4}, Layout::RowMajor);
  ASSERT_TRUE(view);
  auto output =
      build_named_output(*manager, OutputDescriptor{"actions", view.value(), "action_chunk"});
  ASSERT_TRUE(output);
  auto data = manager->data(output.value().buffer);
  ASSERT_TRUE(data);
  for (std::size_t i = 0; i < data.value().size(); ++i) {
    data.value()[i] = static_cast<std::byte>(i + 1);
  }
  auto result = InferResult::create_success("bin-result", {std::move(output).value()});
  ASSERT_TRUE(result);

  auto rendered = render_binary_infer_response_checked(result.value(), *manager, "cid-bin");
  ASSERT_TRUE(rendered) << rendered.error().message;
  const std::string& wire = rendered.value();
  ASSERT_EQ(wire.substr(0, kBinaryResultMagic.size()),
            std::string(kBinaryResultMagic.data(), kBinaryResultMagic.size()));
  const auto meta_len_offset = kBinaryResultMagic.size();
  const auto meta_len =
      static_cast<std::uint32_t>(static_cast<unsigned char>(wire[meta_len_offset])) |
      (static_cast<std::uint32_t>(static_cast<unsigned char>(wire[meta_len_offset + 1])) << 8U) |
      (static_cast<std::uint32_t>(static_cast<unsigned char>(wire[meta_len_offset + 2])) << 16U) |
      (static_cast<std::uint32_t>(static_cast<unsigned char>(wire[meta_len_offset + 3])) << 24U);
  const auto meta_start = meta_len_offset + sizeof(std::uint32_t);
  auto meta = nlohmann::json::parse(wire.substr(meta_start, meta_len));
  EXPECT_EQ(meta["request_id"], "bin-result");
  EXPECT_EQ(meta["correlation_id"], "cid-bin");
  EXPECT_EQ(meta["outputs"][0]["payload_offset"], 0);
  EXPECT_EQ(meta["outputs"][0]["payload_size"], 4);
  const auto payload_start = meta_start + meta_len;
  ASSERT_EQ(wire.size() - payload_start, 4U);
  EXPECT_EQ(static_cast<unsigned char>(wire[payload_start + 3]), 4U);
}

TEST(ServingSerialization, CheckedRenderFailsIfOutputBufferWasReleased) {
  BufferManagerConfig cfg;
  auto manager_r = BufferManager::create(cfg);
  ASSERT_TRUE(manager_r);
  auto manager = std::move(manager_r).value();

  auto view = TensorView::create(DType::UInt8, {4}, Layout::RowMajor);
  ASSERT_TRUE(view);
  auto output = build_named_output(*manager, OutputDescriptor{"actions", view.value(), {}});
  ASSERT_TRUE(output);
  auto result = InferResult::create_success("req-released", {std::move(output).value()});
  ASSERT_TRUE(result);
  ASSERT_TRUE(manager->release(result.value().outputs()[0].buffer));

  auto rendered = render_infer_response_checked(result.value(), *manager, "cid-released");
  ASSERT_FALSE(rendered);
  EXPECT_EQ(rendered.error().code, Error::Code::Internal);
}

}  // namespace
