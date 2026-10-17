// SPDX-License-Identifier: Apache-2.0
#if TP_HAS_TENSORRT_LOADER
#include <cuda_runtime.h>
#include <dlfcn.h>
#include <gtest/gtest.h>

#include <cstdlib>
#include <string>
#include <vector>

#include "adapters/tensorrt/sdk_loader.hpp"

namespace tensorplate::adapters::tensorrt {
namespace {

// Synthetic loader responses, not recordings of vendor behavior.
struct Fake {
  char cuda = 0;
  char infer = 0;
  std::string missing;
  int infer_version = NV_TENSORRT_VERSION;
  int cuda_version = CUDART_VERSION;
  cudaError_t version_status = cudaSuccess;
  int unsafe_calls = 0;
  std::vector<void*> closed;
  std::vector<std::string> symbols;
};
Fake& fake() {
  static Fake value;
  return value;
}
int32_t infer_version() noexcept {
  return fake().infer_version;
}
cudaError_t runtime_version(int* version) {
  *version = fake().cuda_version;
  return fake().version_status;
}
cudaError_t device_probe(int*) {
  ++fake().unsafe_calls;
  return cudaErrorInsufficientDriver;
}
void* factory(void*, int32_t) noexcept {
  ++fake().unsafe_calls;
  return nullptr;
}
void unused() {}
void* open_library(const char* name, int flags) {
  EXPECT_EQ(flags, RTLD_NOW | RTLD_LOCAL);
  if (fake().missing == name) {
    return nullptr;
  }
  if (std::string(name) == cuda_library_name()) {
    return &fake().cuda;
  }
  EXPECT_EQ(std::string(name), tensorrt_library_name());
  return &fake().infer;
}
void* symbol(void* handle, const char* name) {
  const std::string key{name};
  fake().symbols.push_back(key);
  EXPECT_EQ(handle, key.starts_with("cuda") ? &fake().cuda : &fake().infer);
  if (fake().missing == key) {
    return nullptr;
  }
  if (key == "getInferLibVersion") {
    return reinterpret_cast<void*>(&infer_version);
  }
  if (key == "cudaRuntimeGetVersion") {
    return reinterpret_cast<void*>(&runtime_version);
  }
  if (key == "cudaDriverGetVersion" || key == "cudaGetDeviceCount") {
    return reinterpret_cast<void*>(&device_probe);
  }
  if (key == "createInferRuntime_INTERNAL") {
    return reinterpret_cast<void*>(&factory);
  }
  return reinterpret_cast<void*>(&unused);
}
int close_library(void* handle) {
  fake().closed.push_back(handle);
  return 0;
}
char* loader_error() {
  static std::string text = "synthetic dependency load failure";
  return text.data();
}
const LibraryOps kOps{open_library, symbol, close_library, loader_error};
const std::vector<std::string> kSymbols{"createInferRuntime_INTERNAL",
                                        "getInferLibVersion",
                                        "cudaRuntimeGetVersion",
                                        "cudaDriverGetVersion",
                                        "cudaGetDeviceCount",
                                        "cudaGetErrorString",
                                        "cudaMalloc",
                                        "cudaFree",
                                        "cudaStreamCreate",
                                        "cudaStreamDestroy",
                                        "cudaMemcpyAsync",
                                        "cudaStreamSynchronize"};

class TensorRTLoader : public ::testing::Test {
 protected:
  void SetUp() override { fake() = {}; }
  void TearDown() override { EXPECT_EQ(fake().unsafe_calls, 0); }
  static void expect_unsupported(const Result<std::shared_ptr<const SdkLibraries>>& result,
                                 const std::string& name) {
    ASSERT_FALSE(result.has_value());
    EXPECT_EQ(result.error().code, Error::Code::Unsupported);
    EXPECT_NE(result.error().message.find(name), std::string::npos);
  }
  static void expect_closed() {
    EXPECT_EQ(fake().closed, (std::vector<void*>{&fake().infer, &fake().cuda}));
  }
};

TEST_F(TensorRTLoader, KeepsHandlesUntilLastOwnerAndResolvesEveryExport) {
  auto result = SdkLibraries::load(kOps);
  ASSERT_TRUE(result.has_value()) << result.error().message;
  EXPECT_EQ(fake().symbols, kSymbols);
  auto held = result.value();
  result.value().reset();
  EXPECT_TRUE(fake().closed.empty());
  int version = 0;
  EXPECT_EQ(held->functions().runtime_version(&version), cudaSuccess);
  EXPECT_EQ(version, CUDART_VERSION);
  held.reset();
  expect_closed();
}

TEST_F(TensorRTLoader, MissingLibrariesAreUnsupportedAndReleasePartialLoad) {
  const std::vector<std::string> names{cuda_library_name(), tensorrt_library_name()};
  for (const auto& name : names) {
    fake() = {};
    fake().missing = name;
    auto result = SdkLibraries::load(kOps);
    expect_unsupported(result, name);
    ASSERT_FALSE(result.has_value());
    EXPECT_NE(result.error().message.find(loader_error()), std::string::npos);
    EXPECT_TRUE(fake().symbols.empty());
    EXPECT_EQ(fake().closed,
              name == names.front() ? std::vector<void*>{} : std::vector<void*>{&fake().cuda});
  }
}

TEST_F(TensorRTLoader, EveryMissingExportIsUnsupportedAndClosesBothLibraries) {
  for (const auto& name : kSymbols) {
    fake() = {};
    fake().missing = name;
    auto result = SdkLibraries::load(kOps);
    ASSERT_FALSE(result.has_value()) << name;
    EXPECT_EQ(result.error().code, Error::Code::Unsupported);
    EXPECT_NE(result.error().message.find(name), std::string::npos);
    expect_closed();
  }
}

TEST_F(TensorRTLoader, RejectsEachRuntimeVersionMismatch) {
  for (const bool change_cuda : {false, true}) {
    fake() = {};
    (change_cuda ? fake().cuda_version : fake().infer_version) -= 1;
    auto result = SdkLibraries::load(kOps);
    ASSERT_FALSE(result.has_value());
    EXPECT_EQ(result.error().code, Error::Code::Unsupported);
    EXPECT_NE(result.error().message.find(change_cuda ? "CUDA" : "TensorRT"), std::string::npos);
    expect_closed();
  }
}

TEST_F(TensorRTLoader, FailedRuntimeVersionQueryIsLoadFailed) {
  fake().version_status = cudaErrorUnknown;
  auto result = SdkLibraries::load(kOps);
  ASSERT_FALSE(result.has_value());
  EXPECT_EQ(result.error().code, Error::Code::LoadFailed);
  EXPECT_EQ(result.error().message, "cudaRuntimeGetVersion failed");
  expect_closed();
}

TEST_F(TensorRTLoader, InstalledSdkLoadsWithoutCreatingRuntime) {
  if (std::getenv("TP_TEST_TENSORRT_SDK") == nullptr) {
    GTEST_SKIP() << "requires the pinned runtime packages; no GPU or driver required";
  }
  const auto is_mapped = [](const std::string& name) {
    void* handle = ::dlopen(name.c_str(), RTLD_NOLOAD | RTLD_NOW);
    if (handle != nullptr) {
      ::dlclose(handle);
      return true;
    }
    return false;
  };
  const std::vector<std::string> names{tensorrt_library_name(), cuda_library_name()};
  std::vector<bool> already_mapped;
  for (const auto& name : names) {
    already_mapped.push_back(is_mapped(name));
  }
  auto result = SdkLibraries::load();
  ASSERT_TRUE(result.has_value()) << result.error().message;
  EXPECT_EQ(result.value()->functions().infer_version(), NV_TENSORRT_VERSION);
  result.value().reset();
  for (std::size_t i = 0; i < names.size(); ++i) {
    if (!already_mapped[i]) {
      EXPECT_FALSE(is_mapped(names[i])) << names[i];
    }
  }
}

// Synthetic function-table responses exercise ordering independently of the driver.
TEST(TensorRTPreflight, GuardsEveryProbeBeforeCallingFactory) {
  struct Case {
    cudaError_t driver_status;
    int driver;
    cudaError_t device_status;
    int devices;
    std::string probe;
    std::vector<int> calls;
  };
  static Case current;
  static std::vector<int> calls;
  class Logger final : public nvinfer1::ILogger {
    void log(Severity, const char*) noexcept override {}
  } logger;
  SdkFunctions functions;
  functions.driver_version = [](int* value) {
    calls.push_back(1);
    *value = current.driver;
    return current.driver_status;
  };
  functions.device_count = [](int* value) {
    calls.push_back(2);
    *value = current.devices;
    return current.device_status;
  };
  functions.create_runtime = [](void*, int32_t version) noexcept -> void* {
    EXPECT_EQ(version, NV_TENSORRT_VERSION);
    calls.push_back(3);
    return nullptr;
  };
  for (const auto& item :
       {Case{cudaErrorUnknown, 12090, cudaSuccess, 1, "cudaDriverGetVersion", {1}},
        Case{cudaSuccess, 0, cudaSuccess, 1, "cudaDriverGetVersion", {1}},
        Case{cudaSuccess, 12090, cudaErrorInsufficientDriver, 1, "cudaGetDeviceCount", {1, 2}},
        Case{cudaSuccess, 12090, cudaSuccess, 0, "cudaGetDeviceCount", {1, 2}},
        Case{cudaSuccess, 12090, cudaSuccess, 1, "createInferRuntime", {1, 2, 3}}}) {
    current = item;
    calls.clear();
    SCOPED_TRACE(item.probe + " driver=" + std::to_string(item.driver) +
                 " devices=" + std::to_string(item.devices));
    const auto result = create_runtime(functions, logger);
    ASSERT_FALSE(result.has_value());
    EXPECT_EQ(result.error().code, Error::Code::LoadFailed);
    EXPECT_TRUE(result.error().message.starts_with(item.probe)) << result.error().message;
    EXPECT_EQ(calls, item.calls);
  }
}

TEST(TensorRTLoaderCache, RetainsSuccessfulOwnerAcrossSessions) {
  if (std::getenv("TP_TEST_TENSORRT_SDK") == nullptr) {
    GTEST_SKIP() << "requires the pinned runtime packages";
  }
  auto first = SdkLibraries::acquire();
  ASSERT_TRUE(first.has_value()) << first.error().message;
  const std::weak_ptr<const SdkLibraries> observer = first.value();
  first.value().reset();
  ASSERT_FALSE(observer.expired());
  auto second = SdkLibraries::acquire();
  ASSERT_TRUE(second.has_value()) << second.error().message;
  EXPECT_EQ(second.value(), observer.lock());
}

}  // namespace
}  // namespace tensorplate::adapters::tensorrt
#endif
