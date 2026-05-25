// SPDX-License-Identifier: Apache-2.0
//
// Build a tiny TensorRT identity engine for E15 hardware validation.
// The network has one FP32 input named "image" with shape [1, 3, 4, 4]
// and one FP32 output named "features" with the same shape.

#include <NvInfer.h>

#include <cstdint>
#include <fstream>
#include <iostream>
#include <memory>
#include <string>

namespace {

class Logger final : public nvinfer1::ILogger {
 public:
  void log(Severity severity, const char* msg) noexcept override {
    if (severity <= Severity::kWARNING) {
      std::cerr << "tensorrt: " << msg << '\n';
    }
  }
};

template <typename T>
struct TrtDeleter {
  void operator()(T* ptr) const noexcept {
    if (ptr == nullptr) {
      return;
    }
#if NV_TENSORRT_MAJOR >= 10
    delete ptr;
#else
    ptr->destroy();
#endif
  }
};

template <typename T>
using TrtPtr = std::unique_ptr<T, TrtDeleter<T>>;

}  // namespace

int main(int argc, char** argv) {
  if (argc != 2) {
    std::cerr << "usage: trt_identity_engine <output.engine>\n";
    return 2;
  }

  Logger logger;
  TrtPtr<nvinfer1::IBuilder> builder{nvinfer1::createInferBuilder(logger)};
  if (!builder) {
    std::cerr << "failed to create TensorRT builder\n";
    return 1;
  }

  const auto flags =
      1U << static_cast<std::uint32_t>(nvinfer1::NetworkDefinitionCreationFlag::kEXPLICIT_BATCH);
  TrtPtr<nvinfer1::INetworkDefinition> network{builder->createNetworkV2(flags)};
  if (!network) {
    std::cerr << "failed to create TensorRT network\n";
    return 1;
  }

  auto* input = network->addInput("image", nvinfer1::DataType::kFLOAT, nvinfer1::Dims4{1, 3, 4, 4});
  if (input == nullptr) {
    std::cerr << "failed to add image input\n";
    return 1;
  }
  auto* identity = network->addIdentity(*input);
  if (identity == nullptr || identity->getOutput(0) == nullptr) {
    std::cerr << "failed to add identity layer\n";
    return 1;
  }
  identity->getOutput(0)->setName("features");
  network->markOutput(*identity->getOutput(0));

  TrtPtr<nvinfer1::IBuilderConfig> config{builder->createBuilderConfig()};
  if (!config) {
    std::cerr << "failed to create TensorRT builder config\n";
    return 1;
  }
  config->setMemoryPoolLimit(nvinfer1::MemoryPoolType::kWORKSPACE, 1U << 20);

  TrtPtr<nvinfer1::IHostMemory> plan{builder->buildSerializedNetwork(*network, *config)};
  if (!plan) {
    std::cerr << "failed to build serialized TensorRT engine\n";
    return 1;
  }

  std::ofstream out(argv[1], std::ios::binary);
  if (!out) {
    std::cerr << "failed to open output engine: " << argv[1] << '\n';
    return 1;
  }
  out.write(static_cast<const char*>(plan->data()), static_cast<std::streamsize>(plan->size()));
  if (!out) {
    std::cerr << "failed to write output engine\n";
    return 1;
  }
  return 0;
}
