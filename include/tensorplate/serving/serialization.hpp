// SPDX-License-Identifier: Apache-2.0
//
// V01-E07-F03: HTTP `/infer` and async-policy JSON serialization.
//
// The serving worker uses one normalized JSON shape for both single-
// input vision and LeRobot-compatible async VLA requests. The
// request/response schema is documented in
// `protocol/schemas/serving_infer_request.json` and
// `protocol/schemas/serving_infer_response.json`. Async result
// payloads share the response schema; only the route differs.
//
// Tensor payloads are base64-encoded raw bytes in the JSON envelope. A
// binary tensor envelope is also available for latency-sensitive local
// workloads; it keeps the same request/result metadata but stores tensor
// payloads as raw byte windows after a small header.

#pragma once

#include <cstddef>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/ingress.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"
#include "tensorplate/core/result.hpp"

namespace tensorplate::serving {

inline constexpr std::string_view kBinaryInferContentType =
    "application/vnd.tensorplate.infer.binary.v1";
inline constexpr std::string_view kBinaryInferMagic = "TPINFER1";
inline constexpr std::string_view kBinaryResultMagic = "TPRESULT1";

/// Decoded HTTP request envelope before buffer allocation.
struct DecodedInferRequest {
  std::string request_id;
  std::string endpoint;
  RequestMetadata metadata;
  std::optional<InferRequest::Duration> relative_deadline;

  /// Per-input descriptor. Bytes are owned by the decoder so the
  /// router can pass them to `build_named_inputs` without re-decoding.
  struct DecodedInput {
    std::string name;
    std::vector<std::byte> bytes;
    TensorView tensor;
  };
  std::vector<DecodedInput> inputs;
};

/// Parse a JSON `/infer` request envelope into a `DecodedInferRequest`.
///
/// Validation runs in order: envelope shape -> request_id / endpoint
/// non-empty -> inputs vector non-empty -> per-input dtype / shape /
/// layout parseable -> base64 payload decodes -> declared byte_size
/// matches the decoded payload. All validation errors are typed:
///   - ConfigInvalid: malformed envelope, missing required fields,
///     duplicate input names, illegal correlation/action-chunk
///     metadata.
///   - ShapeMismatch: tensor metadata or byte-size mismatch.
///   - Unsupported: unknown dtype, unknown layout, unknown
///     schema_version.
///
/// Decoder does **not** allocate buffer-plane storage; the router
/// takes the returned descriptors to `build_named_inputs` after the
/// HTTP-level limits are confirmed.
[[nodiscard]] Result<DecodedInferRequest> decode_infer_request(std::string_view body);

/// Parse a binary `/infer` request envelope into a `DecodedInferRequest`.
///
/// Wire format:
///   - ASCII magic `TPINFER1`
///   - little-endian uint32 metadata JSON byte length
///   - metadata JSON mirroring the v0.1 request envelope, except each input
///     uses `payload_offset` / `payload_size` instead of `payload_b64`
///   - concatenated raw payload bytes
[[nodiscard]] Result<DecodedInferRequest> decode_binary_infer_request(std::string_view body);

/// Render a successful `InferResult` to JSON. Outputs are base64-
/// encoded. The response carries request_id, correlation_id (when
/// set), timing fields, and the outputs vector.
[[nodiscard]] std::string render_infer_response(const InferResult& result,
                                                BufferManager& buffer_manager,
                                                std::optional<std::string_view> correlation_id);

/// Fallible variant used by HTTP routes. Returns a typed error if an
/// output buffer cannot be viewed while serializing a successful
/// result.
[[nodiscard]] Result<std::string> render_infer_response_checked(
    const InferResult& result, BufferManager& buffer_manager,
    std::optional<std::string_view> correlation_id);

/// Render a successful `InferResult` to the binary result envelope. Failures
/// should continue to use `render_error_response` so typed failures remain JSON.
[[nodiscard]] Result<std::string> render_binary_infer_response_checked(
    const InferResult& result, BufferManager& buffer_manager,
    std::optional<std::string_view> correlation_id);

/// Render a typed `Error` to the canonical error-response JSON shape.
/// Used for both `/infer` failures and async-policy errors.
[[nodiscard]] std::string render_error_response(std::string_view request_id,
                                                std::optional<std::string_view> correlation_id,
                                                const Error& error);

/// Build an `IngressInput` vector from decoded inputs. The returned
/// spans reference the bytes inside `decoded`; both must outlive the
/// call to `build_named_inputs`.
[[nodiscard]] std::vector<IngressInput> as_ingress_inputs(const DecodedInferRequest& decoded);

/// Base64-encode raw bytes. Stable lowercase alphabet (RFC 4648),
/// no line breaks. Public so async-result serialization and tests
/// can reuse it.
[[nodiscard]] std::string base64_encode(const std::byte* data, std::size_t len);

/// Base64-decode. Returns ConfigInvalid for malformed input.
[[nodiscard]] Result<std::vector<std::byte>> base64_decode(std::string_view text);

}  // namespace tensorplate::serving
