// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/serialization.hpp"

#include <algorithm>
#include <array>
#include <cstddef>
#include <cstdint>
#include <nlohmann/json.hpp>
#include <span>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"

namespace tensorplate::serving {

namespace {

using json = nlohmann::json;

constexpr std::string_view kBase64Alphabet =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

constexpr std::array<std::int8_t, 256> build_base64_decode_table() {
  std::array<std::int8_t, 256> t{};
  t.fill(-1);
  for (std::int8_t i = 0; i < 64; ++i) {
    // constexpr-friendly indexing; .at() is not constexpr in C++20 on the
    // string_view path. Indices come from the fixed 64-char alphabet and
    // are statically known to fit. The corresponding clang-tidy check is
    // intentionally suppressed inside this constexpr table builder.
    // NOLINTBEGIN(cppcoreguidelines-pro-bounds-constant-array-index)
    t[static_cast<std::uint8_t>(kBase64Alphabet[static_cast<std::size_t>(i)])] = i;
    // NOLINTEND(cppcoreguidelines-pro-bounds-constant-array-index)
  }
  return t;
}

const std::array<std::int8_t, 256> kBase64DecodeTable = build_base64_decode_table();

Result<DType> parse_dtype(std::string_view name) {
  if (auto v = dtype_from_string(name); v.has_value()) {
    return *v;
  }
  return unexpected(Error::Code::Unsupported,
                    std::string{"infer request: unknown dtype "} + std::string(name));
}

Result<Layout> parse_layout(std::string_view name) {
  if (auto v = layout_from_string(name); v.has_value()) {
    return *v;
  }
  return unexpected(Error::Code::Unsupported,
                    std::string{"infer request: unknown layout "} + std::string(name));
}

std::string code_string(Error::Code code) {
  return std::string{to_string(code)};
}

}  // namespace

std::string base64_encode(const std::byte* data, std::size_t len) {
  std::string out;
  if (len == 0) {
    return out;
  }
  out.reserve(((len + 2) / 3) * 4);
  std::size_t i = 0;
  while (i + 3 <= len) {
    const auto a = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[i]));
    const auto b = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[i + 1]));
    const auto c = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[i + 2]));
    std::uint32_t v = (a << 16) | (b << 8) | c;
    out.push_back(kBase64Alphabet[(v >> 18) & 0x3F]);
    out.push_back(kBase64Alphabet[(v >> 12) & 0x3F]);
    out.push_back(kBase64Alphabet[(v >> 6) & 0x3F]);
    out.push_back(kBase64Alphabet[v & 0x3F]);
    i += 3;
  }
  if (i < len) {
    const auto a = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[i]));
    std::uint32_t v = a << 16;
    if (i + 1 < len) {
      v |= static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[i + 1])) << 8;
    }
    out.push_back(kBase64Alphabet[(v >> 18) & 0x3F]);
    out.push_back(kBase64Alphabet[(v >> 12) & 0x3F]);
    if (i + 1 < len) {
      out.push_back(kBase64Alphabet[(v >> 6) & 0x3F]);
    } else {
      out.push_back('=');
    }
    out.push_back('=');
  }
  return out;
}

Result<std::vector<std::byte>> base64_decode(std::string_view text) {
  // Strip whitespace.
  std::string compacted;
  compacted.reserve(text.size());
  for (char c : text) {
    if (c == '\r' || c == '\n' || c == ' ' || c == '\t') {
      continue;
    }
    compacted.push_back(c);
  }
  if (compacted.size() % 4 != 0) {
    return unexpected(Error::Code::ConfigInvalid, "base64: bad length");
  }
  std::vector<std::byte> out;
  out.reserve((compacted.size() / 4) * 3);
  for (std::size_t i = 0; i < compacted.size(); i += 4) {
    auto t0 = kBase64DecodeTable.at(static_cast<std::uint8_t>(compacted[i]));
    auto t1 = kBase64DecodeTable.at(static_cast<std::uint8_t>(compacted[i + 1]));
    auto t2 = compacted[i + 2] == '='
                  ? -2
                  : kBase64DecodeTable.at(static_cast<std::uint8_t>(compacted[i + 2]));
    auto t3 = compacted[i + 3] == '='
                  ? -2
                  : kBase64DecodeTable.at(static_cast<std::uint8_t>(compacted[i + 3]));
    if (t0 < 0 || t1 < 0 || (t2 < 0 && t2 != -2) || (t3 < 0 && t3 != -2)) {
      return unexpected(Error::Code::ConfigInvalid, "base64: invalid character");
    }
    std::uint32_t v =
        (static_cast<std::uint32_t>(t0) << 18) | (static_cast<std::uint32_t>(t1) << 12);
    if (t2 >= 0) {
      v |= static_cast<std::uint32_t>(t2) << 6;
    }
    if (t3 >= 0) {
      v |= static_cast<std::uint32_t>(t3);
    }
    out.push_back(static_cast<std::byte>((v >> 16) & 0xFF));
    if (t2 >= 0) {
      out.push_back(static_cast<std::byte>((v >> 8) & 0xFF));
    }
    if (t3 >= 0) {
      out.push_back(static_cast<std::byte>(v & 0xFF));
    }
  }
  return out;
}

// NOLINTNEXTLINE(readability-function-cognitive-complexity)
Result<DecodedInferRequest> decode_infer_request(std::string_view body) {
  json root;
  try {
    root = json::parse(body);
  } catch (const json::parse_error& e) {
    return unexpected(Error::Code::ConfigInvalid,
                      std::string{"infer request: JSON parse error: "} + e.what());
  }
  if (!root.is_object()) {
    return unexpected(Error::Code::ConfigInvalid, "infer request: JSON root must be an object");
  }
  if (root.contains("schema_version") && root["schema_version"].is_string() &&
      root["schema_version"].get<std::string>() != "0.1") {
    return unexpected(Error::Code::Unsupported,
                      std::string{"infer request: unsupported schema_version "} +
                          root["schema_version"].get<std::string>());
  }
  DecodedInferRequest out;
  if (!root.contains("request_id") || !root["request_id"].is_string() ||
      root["request_id"].get<std::string>().empty()) {
    return unexpected(Error::Code::ConfigInvalid, "infer request: request_id missing or empty");
  }
  out.request_id = root["request_id"].get<std::string>();
  if (!root.contains("endpoint") || !root["endpoint"].is_string() ||
      root["endpoint"].get<std::string>().empty()) {
    return unexpected(Error::Code::ConfigInvalid, "infer request: endpoint missing or empty");
  }
  out.endpoint = root["endpoint"].get<std::string>();
  if (!root.contains("inputs") || !root["inputs"].is_array() || root["inputs"].empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "infer request: inputs must be a non-empty array");
  }
  if (root.contains("metadata") && root["metadata"].is_object()) {
    const auto& m = root["metadata"];
    if (m.contains("correlation_id") && m["correlation_id"].is_string()) {
      auto v = m["correlation_id"].get<std::string>();
      if (v.empty()) {
        return unexpected(Error::Code::ConfigInvalid,
                          "infer request: metadata.correlation_id empty");
      }
      out.metadata.correlation_id = std::move(v);
    }
    if (m.contains("action_chunk_id") && m["action_chunk_id"].is_string()) {
      auto v = m["action_chunk_id"].get<std::string>();
      if (v.empty()) {
        return unexpected(Error::Code::ConfigInvalid,
                          "infer request: metadata.action_chunk_id empty");
      }
      out.metadata.action_chunk_id = std::move(v);
    }
    if (m.contains("action_chunk_sequence") && m["action_chunk_sequence"].is_number_integer()) {
      out.metadata.action_chunk_sequence = m["action_chunk_sequence"].get<std::int64_t>();
    }
    if (m.contains("stale_after_sequence") && m["stale_after_sequence"].is_number_integer()) {
      out.metadata.stale_after_sequence = m["stale_after_sequence"].get<std::int64_t>();
    }
    if (m.contains("extra") && m["extra"].is_object()) {
      for (auto it = m["extra"].begin(); it != m["extra"].end(); ++it) {
        if (!it.value().is_string()) {
          return unexpected(Error::Code::ConfigInvalid,
                            "infer request: metadata.extra values must be strings");
        }
        out.metadata.extra.emplace(it.key(), it.value().get<std::string>());
      }
    }
  }
  if (root.contains("deadline_ms") && root["deadline_ms"].is_number_integer()) {
    auto v = root["deadline_ms"].get<std::int64_t>();
    if (v <= 0) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: deadline_ms must be > 0");
    }
    out.relative_deadline = std::chrono::milliseconds{v};
  }
  for (const auto& item : root["inputs"]) {
    if (!item.is_object()) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: input must be an object");
    }
    if (!item.contains("name") || !item["name"].is_string()) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: input.name missing");
    }
    std::string name = item["name"].get<std::string>();
    if (name.empty()) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: input.name empty");
    }
    if (!item.contains("tensor") || !item["tensor"].is_object()) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: input.tensor missing");
    }
    const auto& t = item["tensor"];
    if (!t.contains("dtype") || !t["dtype"].is_string()) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: input.tensor.dtype missing");
    }
    auto dt = parse_dtype(t["dtype"].get<std::string>());
    if (!dt) {
      return unexpected(dt.error());
    }
    Layout layout = Layout::RowMajor;
    if (t.contains("layout") && t["layout"].is_string()) {
      auto l = parse_layout(t["layout"].get<std::string>());
      if (!l) {
        return unexpected(l.error());
      }
      layout = l.value();
    }
    if (!t.contains("shape") || !t["shape"].is_array() || t["shape"].empty()) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: input.tensor.shape missing");
    }
    std::vector<std::int64_t> shape;
    for (const auto& s : t["shape"]) {
      if (!s.is_number_integer()) {
        return unexpected(Error::Code::ConfigInvalid,
                          "infer request: input.tensor.shape entries must be integers");
      }
      shape.push_back(s.get<std::int64_t>());
    }
    std::size_t byte_offset = 0;
    std::size_t byte_size = 0;
    if (t.contains("byte_offset") && t["byte_offset"].is_number_integer()) {
      byte_offset = static_cast<std::size_t>(t["byte_offset"].get<std::int64_t>());
    }
    if (t.contains("byte_size") && t["byte_size"].is_number_integer()) {
      byte_size = static_cast<std::size_t>(t["byte_size"].get<std::int64_t>());
    }
    auto view = TensorView::create(dt.value(), shape, layout, byte_offset, byte_size);
    if (!view) {
      return unexpected(view.error());
    }
    if (!item.contains("payload_b64") || !item["payload_b64"].is_string()) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: input.payload_b64 missing");
    }
    auto bytes_r = base64_decode(item["payload_b64"].get<std::string>());
    if (!bytes_r) {
      return unexpected(bytes_r.error());
    }
    auto bytes = std::move(bytes_r).value();
    // Validate against the declared tensor window: payload must be at
    // least as large as offset + byte_size.
    const std::size_t expected = view.value().byte_offset() + view.value().byte_size();
    if (bytes.size() < expected) {
      return unexpected(Error::Code::ShapeMismatch, std::string{"infer request: payload bytes ("} +
                                                        std::to_string(bytes.size()) +
                                                        ") shorter than declared tensor window (" +
                                                        std::to_string(expected) + ")");
    }
    out.inputs.push_back(
        DecodedInferRequest::DecodedInput{std::move(name), std::move(bytes), view.value()});
  }
  return out;
}

std::vector<IngressInput> as_ingress_inputs(const DecodedInferRequest& decoded) {
  std::vector<IngressInput> out;
  out.reserve(decoded.inputs.size());
  for (const auto& in : decoded.inputs) {
    out.push_back(IngressInput{in.name, std::span<const std::byte>(in.bytes), in.tensor});
  }
  return out;
}

namespace {

nlohmann::json tensor_to_json(const TensorView& tv) {
  return nlohmann::json{
      {"dtype", std::string{to_string(tv.dtype())}},
      {"layout", std::string{to_string(tv.layout())}},
      {"shape", tv.shape()},
      {"byte_offset", tv.byte_offset()},
      {"byte_size", tv.byte_size()},
  };
}

}  // namespace

std::string render_infer_response(const InferResult& result, BufferManager& buffer_manager,
                                  std::optional<std::string_view> correlation_id) {
  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["request_id"] = result.request_id();
  if (correlation_id.has_value()) {
    j["correlation_id"] = std::string{*correlation_id};
  }
  if (result.is_success()) {
    j["status"] = "success";
    nlohmann::json outs = nlohmann::json::array();
    for (const auto& out : result.outputs()) {
      auto span_r = buffer_manager.view(out.buffer, out.tensor);
      std::string payload;
      if (span_r) {
        const auto& s = span_r.value();
        payload = base64_encode(s.data(), s.size());
      }
      nlohmann::json item;
      item["name"] = out.name;
      item["tensor"] = tensor_to_json(out.tensor);
      item["payload_b64"] = std::move(payload);
      if (out.semantic_tag.has_value()) {
        item["semantic_tag"] = *out.semantic_tag;
      }
      outs.push_back(std::move(item));
    }
    j["outputs"] = std::move(outs);
  } else {
    j["status"] = "failure";
    const auto& err = result.error();
    nlohmann::json je;
    je["schema_version"] = "0.1";
    je["code"] = code_string(err.code);
    je["message"] = err.message;
    if (err.context.has_value()) {
      je["context"] = *err.context;
    }
    j["error"] = std::move(je);
  }
  const auto& t = result.timing();
  if (t.queue_latency || t.execution_latency || t.total_latency) {
    nlohmann::json jt;
    if (t.queue_latency) {
      jt["queue_latency_ns"] = t.queue_latency->count();
    }
    if (t.execution_latency) {
      jt["execution_latency_ns"] = t.execution_latency->count();
    }
    if (t.total_latency) {
      jt["total_latency_ns"] = t.total_latency->count();
    }
    j["timing"] = std::move(jt);
  }
  return j.dump();
}

std::string render_error_response(std::string_view request_id,
                                  std::optional<std::string_view> correlation_id,
                                  const Error& error) {
  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["request_id"] = std::string{request_id};
  if (correlation_id.has_value()) {
    j["correlation_id"] = std::string{*correlation_id};
  }
  j["status"] = "failure";
  nlohmann::json je;
  je["schema_version"] = "0.1";
  je["code"] = code_string(error.code);
  je["message"] = error.message;
  if (error.context.has_value()) {
    je["context"] = *error.context;
  }
  j["error"] = std::move(je);
  return j.dump();
}

}  // namespace tensorplate::serving
