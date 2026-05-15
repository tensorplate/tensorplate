// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F03: serialization (decode/encode) unit tests.

#include <gtest/gtest.h>

#include <string>

#include "tensorplate/serving/serialization.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::serving;

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

}  // namespace
