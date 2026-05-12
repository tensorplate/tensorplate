// SPDX-License-Identifier: Apache-2.0
//
// V01-E03-F04-T02: Shared ingress payload fixtures for vision and
// SmolVLA-style multi-input requests. Used by the F04 ingress tests and
// will be reused by the V01-E07 HTTP router tests when that work lands.

#pragma once

#include <cstddef>
#include <cstdint>
#include <span>
#include <string>
#include <vector>

#include "tensorplate/buffer/ingress.hpp"
#include "tensorplate/buffer/tensor_view.hpp"

namespace tensorplate::testing {

/// One named payload fixture. Owns the bytes so callers can take a
/// span without worrying about lifetime ordering.
struct PayloadFixture {
  std::string name;
  std::vector<std::byte> bytes;
  TensorView tensor;
};

/// Build a single-input vision payload sized to a small detector.
///
/// Default: 224x224x3 uint8 (NHWC), filled with a deterministic ramp
/// so equality tests can be exact.
inline std::vector<PayloadFixture> make_vision_fixture(std::int64_t height = 224,
                                                       std::int64_t width = 224,
                                                       std::int64_t channels = 3) {
  const std::size_t n = static_cast<std::size_t>(height) * static_cast<std::size_t>(width) *
                        static_cast<std::size_t>(channels);
  std::vector<std::byte> bytes(n);
  for (std::size_t i = 0; i < n; ++i) {
    bytes[i] = static_cast<std::byte>(i & 0xFF);
  }
  auto view = TensorView::create(DType::UInt8, {height, width, channels});
  // Tests use these fixtures synchronously; bail loudly on misconstruction.
  if (!view.has_value()) {
    std::abort();
  }
  std::vector<PayloadFixture> out;
  out.push_back(PayloadFixture{"image", std::move(bytes), view.value()});
  return out;
}

/// Build a SmolVLA-style multi-input fixture: two RGB camera images,
/// one float32 state vector, and one int64 instruction-token vector.
inline std::vector<PayloadFixture> make_smolvla_fixture() {
  std::vector<PayloadFixture> out;
  // Front camera: small RGB image so the test fits in a 1 MiB-ish pool.
  {
    const std::int64_t h = 64;
    const std::int64_t w = 64;
    const std::int64_t c = 3;
    std::vector<std::byte> b(static_cast<std::size_t>(h * w * c));
    for (std::size_t i = 0; i < b.size(); ++i)
      b[i] = static_cast<std::byte>(i & 0xFF);
    auto v = TensorView::create(DType::UInt8, {h, w, c});
    if (!v.has_value()) std::abort();
    out.push_back(PayloadFixture{"image_front", std::move(b), v.value()});
  }
  // Wrist camera: same shape, different bytes so equality checks can
  // distinguish the two.
  {
    const std::int64_t h = 64;
    const std::int64_t w = 64;
    const std::int64_t c = 3;
    std::vector<std::byte> b(static_cast<std::size_t>(h * w * c));
    for (std::size_t i = 0; i < b.size(); ++i)
      b[i] = static_cast<std::byte>((i * 7 + 13) & 0xFF);
    auto v = TensorView::create(DType::UInt8, {h, w, c});
    if (!v.has_value()) std::abort();
    out.push_back(PayloadFixture{"image_wrist", std::move(b), v.value()});
  }
  // State vector: float32, length 8.
  {
    const std::int64_t n = 8;
    std::vector<std::byte> b(static_cast<std::size_t>(n) * sizeof(float));
    for (std::size_t i = 0; i < b.size(); ++i)
      b[i] = static_cast<std::byte>(i);
    auto v = TensorView::create(DType::Float32, {n});
    if (!v.has_value()) std::abort();
    out.push_back(PayloadFixture{"state", std::move(b), v.value()});
  }
  // Instruction tokens: int64, length 16.
  {
    const std::int64_t n = 16;
    std::vector<std::byte> b(static_cast<std::size_t>(n) * sizeof(std::int64_t));
    for (std::size_t i = 0; i < b.size(); ++i)
      b[i] = static_cast<std::byte>(i + 100);
    auto v = TensorView::create(DType::Int64, {n});
    if (!v.has_value()) std::abort();
    out.push_back(PayloadFixture{"instruction_tokens", std::move(b), v.value()});
  }
  return out;
}

/// Translate a PayloadFixture vector into a vector of IngressInput
/// values pointing at the fixture's bytes. The IngressInput references
/// the fixture span; both must outlive the call to build_named_inputs.
inline std::vector<IngressInput> as_ingress_inputs(const std::vector<PayloadFixture>& fixtures) {
  std::vector<IngressInput> out;
  out.reserve(fixtures.size());
  for (const auto& f : fixtures) {
    out.push_back(IngressInput{f.name, std::span<const std::byte>(f.bytes), f.tensor});
  }
  return out;
}

}  // namespace tensorplate::testing
