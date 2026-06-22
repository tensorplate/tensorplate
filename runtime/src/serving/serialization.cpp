// SPDX-License-Identifier: Apache-2.0

#include "tensorplate/serving/serialization.hpp"

#include <algorithm>
#include <array>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <limits>
#include <nlohmann/json.hpp>
#include <optional>
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

void append_u32_le(std::string& out, std::uint32_t value) {
  out.push_back(static_cast<char>(value & 0xFFU));
  out.push_back(static_cast<char>((value >> 8U) & 0xFFU));
  out.push_back(static_cast<char>((value >> 16U) & 0xFFU));
  out.push_back(static_cast<char>((value >> 24U) & 0xFFU));
}

std::uint32_t read_u32_le(std::string_view data, std::size_t offset) {
  const auto b0 = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[offset]));
  const auto b1 = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[offset + 1]));
  const auto b2 = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[offset + 2]));
  const auto b3 = static_cast<std::uint32_t>(static_cast<std::uint8_t>(data[offset + 3]));
  return b0 | (b1 << 8U) | (b2 << 16U) | (b3 << 24U);
}

Result<void> parse_optional_metadata_string(const json& metadata, std::string_view field,
                                            std::optional<std::string>& target) {
  const auto item = metadata.find(std::string{field});
  if (item == metadata.end() || !item->is_string()) {
    return Result<void>{};
  }
  auto value = item->get<std::string>();
  if (value.empty()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "infer request: metadata." + std::string{field} + " empty");
  }
  target = std::move(value);
  return Result<void>{};
}

void parse_optional_metadata_i64(const json& metadata, std::string_view field,
                                 std::optional<std::int64_t>& target) {
  const auto item = metadata.find(std::string{field});
  if (item != metadata.end() && item->is_number_integer()) {
    target = item->get<std::int64_t>();
  }
}

Result<void> parse_metadata_extra(const json& metadata, RequestMetadata& out) {
  const auto extra = metadata.find("extra");
  if (extra == metadata.end() || !extra->is_object()) {
    return Result<void>{};
  }
  for (auto it = extra->begin(); it != extra->end(); ++it) {
    if (!it.value().is_string()) {
      return unexpected(Error::Code::ConfigInvalid,
                        "infer request: metadata.extra values must be strings");
    }
    out.extra.emplace(it.key(), it.value().get<std::string>());
  }
  return Result<void>{};
}

Result<void> parse_metadata_object(const json& metadata, RequestMetadata& out) {
  auto correlation_id =
      parse_optional_metadata_string(metadata, "correlation_id", out.correlation_id);
  if (!correlation_id) {
    return unexpected(correlation_id.error());
  }
  auto action_chunk_id =
      parse_optional_metadata_string(metadata, "action_chunk_id", out.action_chunk_id);
  if (!action_chunk_id) {
    return unexpected(action_chunk_id.error());
  }
  parse_optional_metadata_i64(metadata, "action_chunk_sequence", out.action_chunk_sequence);
  parse_optional_metadata_i64(metadata, "stale_after_sequence", out.stale_after_sequence);
  return parse_metadata_extra(metadata, out);
}

Result<void> parse_request_metadata(const json& root, DecodedInferRequest& out) {
  const auto metadata = root.find("metadata");
  if (metadata != root.end() && metadata->is_object()) {
    auto parsed = parse_metadata_object(*metadata, out.metadata);
    if (!parsed) {
      return unexpected(parsed.error());
    }
  }
  const auto deadline = root.find("deadline_ms");
  if (deadline != root.end() && deadline->is_number_integer()) {
    auto v = deadline->get<std::int64_t>();
    if (v <= 0) {
      return unexpected(Error::Code::ConfigInvalid, "infer request: deadline_ms must be > 0");
    }
    out.relative_deadline = std::chrono::milliseconds{v};
  }
  return Result<void>{};
}

Result<TensorView> parse_tensor_view_json(const json& t) {
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
    const auto v = t["byte_offset"].get<std::int64_t>();
    if (v < 0) {
      return unexpected(Error::Code::ConfigInvalid,
                        "infer request: input.tensor.byte_offset must be >= 0");
    }
    byte_offset = static_cast<std::size_t>(v);
  }
  if (t.contains("byte_size") && t["byte_size"].is_number_integer()) {
    const auto v = t["byte_size"].get<std::int64_t>();
    if (v < 0) {
      return unexpected(Error::Code::ConfigInvalid,
                        "infer request: input.tensor.byte_size must be >= 0");
    }
    byte_size = static_cast<std::size_t>(v);
  }
  return TensorView::create(dt.value(), std::move(shape), layout, byte_offset, byte_size);
}

Result<DecodedInferRequest> parse_request_metadata_root(const json& root) {
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
  if (auto meta = parse_request_metadata(root, out); !meta) {
    return unexpected(meta.error());
  }
  return out;
}

Result<std::size_t> read_nonnegative_size(const json& item, std::string_view field) {
  if (!item.contains(std::string(field)) || !item[std::string(field)].is_number_integer()) {
    return unexpected(Error::Code::ConfigInvalid,
                      "infer request: input." + std::string(field) + " missing");
  }
  const auto value = item[std::string(field)].get<std::int64_t>();
  if (value < 0) {
    return unexpected(Error::Code::ConfigInvalid,
                      "infer request: input." + std::string(field) + " must be >= 0");
  }
  return static_cast<std::size_t>(value);
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
Result<DecodedInferRequest> decode_binary_infer_request(std::string_view body) {
  constexpr std::size_t kHeaderSize = kBinaryInferMagic.size() + sizeof(std::uint32_t);
  if (body.size() < kHeaderSize) {
    return unexpected(Error::Code::ConfigInvalid, "binary infer request: truncated header");
  }
  if (body.substr(0, kBinaryInferMagic.size()) != kBinaryInferMagic) {
    return unexpected(Error::Code::ConfigInvalid, "binary infer request: bad magic");
  }
  const std::uint32_t metadata_len = read_u32_le(body, kBinaryInferMagic.size());
  const std::size_t metadata_offset = kHeaderSize;
  if (metadata_len > body.size() - metadata_offset) {
    return unexpected(Error::Code::ConfigInvalid, "binary infer request: truncated metadata");
  }
  const auto metadata_text = body.substr(metadata_offset, static_cast<std::size_t>(metadata_len));
  const auto payload = body.substr(metadata_offset + static_cast<std::size_t>(metadata_len));

  json root;
  try {
    root = json::parse(metadata_text);
  } catch (const json::parse_error& e) {
    return unexpected(Error::Code::ConfigInvalid,
                      std::string{"binary infer request: metadata JSON parse error: "} + e.what());
  }
  auto decoded_r = parse_request_metadata_root(root);
  if (!decoded_r) {
    return unexpected(decoded_r.error());
  }
  auto out = std::move(decoded_r).value();

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
    auto view = parse_tensor_view_json(item["tensor"]);
    if (!view) {
      return unexpected(view.error());
    }
    auto offset_r = read_nonnegative_size(item, "payload_offset");
    if (!offset_r) {
      return unexpected(offset_r.error());
    }
    auto size_r = read_nonnegative_size(item, "payload_size");
    if (!size_r) {
      return unexpected(size_r.error());
    }
    const std::size_t offset = offset_r.value();
    const std::size_t size = size_r.value();
    if (offset > payload.size() || size > payload.size() - offset) {
      return unexpected(Error::Code::ShapeMismatch,
                        "binary infer request: input payload window exceeds body");
    }
    if (view.value().byte_size() >
        std::numeric_limits<std::size_t>::max() - view.value().byte_offset()) {
      return unexpected(Error::Code::ShapeMismatch,
                        "binary infer request: declared tensor window offset + size overflows");
    }
    const std::size_t expected = view.value().byte_offset() + view.value().byte_size();
    if (size < expected) {
      return unexpected(Error::Code::ShapeMismatch,
                        std::string{"binary infer request: payload bytes ("} +
                            std::to_string(size) + ") shorter than declared tensor window (" +
                            std::to_string(expected) + ")");
    }
    std::vector<std::byte> bytes(size);
    if (size > 0) {
      std::memcpy(bytes.data(), payload.data() + offset, size);
    }
    out.inputs.push_back(
        DecodedInferRequest::DecodedInput{std::move(name), std::move(bytes), view.value()});
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
    if (view.value().byte_size() >
        std::numeric_limits<std::size_t>::max() - view.value().byte_offset()) {
      return unexpected(Error::Code::ShapeMismatch,
                        "infer request: declared tensor window offset + size overflows");
    }
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

Result<std::string> render_infer_response_checked(const InferResult& result,
                                                  BufferManager& buffer_manager,
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
      if (!span_r) {
        return unexpected(span_r.error());
      }
      const auto& s = span_r.value();
      std::string payload = base64_encode(s.data(), s.size());
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

Result<std::string> render_binary_infer_response_checked(
    const InferResult& result, BufferManager& buffer_manager,
    std::optional<std::string_view> correlation_id) {
  if (result.is_failure()) {
    return unexpected(Error::Code::Internal,
                      "binary infer response renderer only supports success results");
  }

  nlohmann::json j;
  j["schema_version"] = "0.1";
  j["request_id"] = result.request_id();
  j["status"] = "success";
  if (correlation_id.has_value()) {
    j["correlation_id"] = std::string{*correlation_id};
  }

  std::string payload;
  nlohmann::json outs = nlohmann::json::array();
  for (const auto& out : result.outputs()) {
    auto span_r = buffer_manager.view(out.buffer, out.tensor);
    if (!span_r) {
      return unexpected(span_r.error());
    }
    const auto& s = span_r.value();
    const auto offset = payload.size();
    payload.append(reinterpret_cast<const char*>(s.data()), s.size());

    nlohmann::json item;
    item["name"] = out.name;
    item["tensor"] = tensor_to_json(out.tensor);
    item["payload_offset"] = offset;
    item["payload_size"] = s.size();
    if (out.semantic_tag.has_value()) {
      item["semantic_tag"] = *out.semantic_tag;
    }
    outs.push_back(std::move(item));
  }
  j["outputs"] = std::move(outs);

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

  const std::string metadata = j.dump();
  if (metadata.size() > std::numeric_limits<std::uint32_t>::max()) {
    return unexpected(Error::Code::Internal, "binary infer response metadata too large");
  }
  std::string out;
  out.reserve(kBinaryResultMagic.size() + sizeof(std::uint32_t) + metadata.size() + payload.size());
  out.append(kBinaryResultMagic.data(), kBinaryResultMagic.size());
  append_u32_le(out, static_cast<std::uint32_t>(metadata.size()));
  out.append(metadata);
  out.append(payload);
  return out;
}

std::string render_infer_response(const InferResult& result, BufferManager& buffer_manager,
                                  std::optional<std::string_view> correlation_id) {
  auto rendered = render_infer_response_checked(result, buffer_manager, correlation_id);
  if (rendered) {
    return std::move(rendered).value();
  }
  return render_error_response(result.request_id(), correlation_id, rendered.error());
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
