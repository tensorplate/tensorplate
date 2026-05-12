// SPDX-License-Identifier: Apache-2.0
//
// V01-E03 end-to-end developer smoke. Walks the public buffer-plane API
// in the order a real serving worker will:
//
//   raw bytes (vision + VLA fixtures)
//     -> BufferManager ingress copy
//     -> BufferRef + TensorView per named input
//     -> InferRequest
//     -> mock policy execution (read inputs, write output)
//     -> InferResult
//     -> cancellation/timeout/error cleanup demos
//     -> pressure-event subscriber demo
//
// The program exits 0 on success and a non-zero exit code on the first
// buffer-plane invariant violation. It is meant for `make example` and
// CI smoke runs; it is NOT part of the shipping runtime.

#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/buffer/buffer_manager.hpp"
#include "tensorplate/buffer/cleanup.hpp"
#include "tensorplate/buffer/ingress.hpp"
#include "tensorplate/buffer/output.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/infer_result.hpp"

#include "ingress_fixtures.hpp"

namespace {

using namespace tensorplate;

#define EXAMPLE_CHECK(cond, msg)                                            \
  do {                                                                      \
    if (!(cond)) {                                                          \
      std::fprintf(stderr, "buffer-plane example FAILED: %s (%s:%d)\n",     \
                   (msg), __FILE__, __LINE__);                              \
      std::exit(1);                                                         \
    }                                                                       \
  } while (0)

void log_accounting(const BufferManager& mgr, const char* tag) {
  const auto snap = mgr.accounting();
  std::printf("[%s] pool=%s active=%zu in_use=%zu high_water=%zu pressure=%s\n", tag,
              snap.pool_name.c_str(), snap.active_count, snap.in_use_bytes, snap.high_water_bytes,
              std::string(to_string(snap.pressure)).c_str());
}

// Mock "policy" execution. Reads one named input through BufferManager,
// writes a deterministic ramp into an allocated action output buffer,
// and returns an InferResult.
Result<InferResult> run_mock_policy(BufferManager& mgr, const InferRequest& req) {
  RequestBufferGuard input_guard(mgr, req);

  auto action_view = TensorView::create(DType::Float32, {4, 7});
  if (!action_view) return unexpected(std::move(action_view).error());
  std::vector<OutputDescriptor> descs;
  descs.push_back({"action_chunk", action_view.value(),
                   std::optional<std::string>{"action_chunk"}, 0});

  auto outs = build_named_outputs(mgr, descs);
  if (!outs) return unexpected(std::move(outs).error());

  // Touch one named input through the safe view path so we exercise
  // the bounds-checked accessor.
  for (const auto& in : req.inputs()) {
    if (in.name == "state") {
      auto bytes = mgr.view(in.buffer, in.tensor);
      if (!bytes) {
        (void)release_partial_outputs(mgr, outs.value());
        return unexpected(std::move(bytes).error());
      }
      break;
    }
  }

  auto out_bytes = mgr.data(outs.value().front().buffer);
  if (!out_bytes) {
    (void)release_partial_outputs(mgr, outs.value());
    return unexpected(std::move(out_bytes).error());
  }
  for (std::size_t i = 0; i < out_bytes.value().size(); ++i) {
    out_bytes.value()[i] = static_cast<std::byte>(i & 0xFF);
  }

  auto result = InferResult::create_success(std::string(req.request_id()), std::move(outs).value());
  if (!result) {
    (void)release_partial_outputs(mgr, outs.value());
    return unexpected(std::move(result).error());
  }
  // Inputs released by the guard on return; outputs travel with the
  // result and must be released by the caller after inspection.
  return result;
}

}  // namespace

int main() {
  std::printf("== tensorplate buffer-plane example ==\n");

  // 1. Manager with conservative limits so the pressure demo crosses.
  BufferManagerConfig cfg;
  cfg.pool_name = "example";
  cfg.capacity_bytes = 4ULL * 1024ULL * 1024ULL;
  cfg.max_buffer_bytes = 1ULL * 1024ULL * 1024ULL;
  cfg.warning_threshold = 0.5;
  cfg.critical_threshold = 0.9;
  auto mgr_r = BufferManager::create(std::move(cfg));
  EXAMPLE_CHECK(mgr_r.has_value(), "BufferManager::create");
  auto mgr = std::move(mgr_r).value();
  log_accounting(*mgr, "init");

  // 2. SmolVLA-style multi-input request: ingress copy + InferRequest.
  {
    const auto fixtures = testing::make_smolvla_fixture();
    auto inputs = build_named_inputs(*mgr, testing::as_ingress_inputs(fixtures));
    EXAMPLE_CHECK(inputs.has_value(), "build_named_inputs(smolvla)");
    auto req = InferRequest::create("smolvla-1", "/policy", std::move(inputs).value());
    EXAMPLE_CHECK(req.has_value(), "InferRequest::create");
    log_accounting(*mgr, "after-ingress");

    auto result = run_mock_policy(*mgr, req.value());
    EXAMPLE_CHECK(result.has_value(), "run_mock_policy");
    EXAMPLE_CHECK(result.value().is_success(), "InferResult::is_success");
    EXAMPLE_CHECK(result.value().outputs().size() == 1, "result.outputs.size == 1");
    EXAMPLE_CHECK(result.value().outputs().front().name == "action_chunk", "output name");

    // Release the output buffer that the result handed back.
    EXAMPLE_CHECK(mgr->release(result.value().outputs().front().buffer).has_value(),
                  "release output");
    log_accounting(*mgr, "after-cleanup");
    EXAMPLE_CHECK(mgr->accounting().active_count == 0, "active_count == 0 after policy");
  }

  // 3. Cancellation cleanup: build a request, cancel it, ensure inputs go away.
  {
    const auto fixtures = testing::make_vision_fixture(32, 32, 3);
    auto inputs = build_named_inputs(*mgr, testing::as_ingress_inputs(fixtures));
    EXAMPLE_CHECK(inputs.has_value(), "build_named_inputs(vision)");
    auto req = InferRequest::create("vision-cancel", "/policy", std::move(inputs).value());
    EXAMPLE_CHECK(req.has_value(), "InferRequest::create(cancel)");

    auto report = release_request_buffers(*mgr, req.value());
    EXAMPLE_CHECK(report.clean(), "cancellation cleanup is clean");
    EXAMPLE_CHECK(report.buffers_released == 1, "cancellation released input");
    EXAMPLE_CHECK(mgr->accounting().active_count == 0, "active_count == 0 after cancel");
    std::printf("[cancel] released %zu buffers\n", report.buffers_released);
  }

  // 4. Double-release returns a typed error and does not corrupt state.
  {
    auto h = mgr->allocate(128);
    EXAMPLE_CHECK(h.has_value(), "allocate(128)");
    EXAMPLE_CHECK(mgr->release(h.value()).has_value(), "first release");
    auto second = mgr->release(h.value());
    EXAMPLE_CHECK(!second.has_value(), "second release fails");
    EXAMPLE_CHECK(second.error().code == Error::Code::Internal,
                  "double release Error::Code::Internal");
    std::printf("[double-release] diagnosed as %s\n",
                std::string(to_string(second.error().code)).c_str());
  }

  // 5. Pressure events as the pool fills and drains.
  {
    int transitions = 0;
    auto sub = mgr->subscribe_pressure([&transitions](const BufferPressureEvent& e) {
      ++transitions;
      std::printf("[pressure] %s -> %s (in_use=%zu, capacity=%zu)\n",
                  std::string(to_string(e.previous)).c_str(),
                  std::string(to_string(e.current)).c_str(), e.in_use_bytes, e.capacity_bytes);
    });

    std::vector<BufferRef> handles;
    for (int i = 0; i < 60; ++i) {
      auto h = mgr->allocate(64 * 1024);
      if (h.has_value()) handles.push_back(h.value());
    }
    for (auto& h : handles) {
      EXAMPLE_CHECK(mgr->release(h).has_value(), "release pressure-test handle");
    }
    mgr->unsubscribe_pressure(sub);
    EXAMPLE_CHECK(transitions >= 2, "at least one threshold crossing observed");
    std::printf("[pressure] observed %d transitions\n", transitions);
  }

  std::printf("OK\n");
  return 0;
}
