# tp_runtime - core C++20 inference runtime library.
#
# Layered source list. V01-E05 introduces the backend registry, the
# adapter directory (TensorRT, LibTorch, Python/PyTorch sidecar), and
# the IPC framing helpers that the sidecar adapter rides on.

# V01-E05 adapter feature flags. Declared up front so the conditional
# source-list appends below can read them. TensorRT and LibTorch
# default OFF (host CI is clean of proprietary SDKs); the Python
# sidecar defaults ON because the runner is stdlib-only.
option(TP_ENABLE_TENSORRT
       "Build the TensorRT execution-backend adapter (V01-E05-F02)." OFF)
option(TP_ENABLE_LIBTORCH
       "Build the LibTorch native execution-backend adapter (V01-E05-F03)." OFF)
option(TP_ENABLE_PYTHON_PYTORCH_SIDECAR
       "Build the Python/PyTorch sidecar execution-backend adapter (V01-E05-F05)."
       ON)

set(_tp_runtime_sources
  src/skeleton.cpp
  src/core/error.cpp
  src/core/model_spec.cpp
  src/core/infer_request.cpp
  src/core/infer_result.cpp
  src/core/execution_session.cpp
  src/buffer/buffer_ref.cpp
  src/buffer/tensor_view.cpp
  src/buffer/buffer_manager.cpp
  src/buffer/cleanup.cpp
  src/buffer/ingress.cpp
  src/buffer/output.cpp
  # V01-E05-F01: backend capability + registry.
  src/backend/capability.cpp
  src/backend/registry.cpp
  src/backend/builtin.cpp
  # V01-E05-F04: Unix-socket framing helpers. Always built so the
  # codec and socket helpers can be unit-tested in T1 even when no
  # adapter consumes them yet.
  src/ipc/sidecar_codec.cpp
  src/ipc/unix_socket.cpp
)

if(TP_ENABLE_TENSORRT)
  list(APPEND _tp_runtime_sources
    src/adapters/tensorrt/tensorrt_session.cpp
  )
endif()

if(TP_ENABLE_LIBTORCH)
  list(APPEND _tp_runtime_sources
    src/adapters/libtorch/libtorch_session.cpp
  )
endif()

if(TP_ENABLE_PYTHON_PYTORCH_SIDECAR)
  list(APPEND _tp_runtime_sources
    src/adapters/python_pytorch/sidecar_process.cpp
    src/adapters/python_pytorch/python_pytorch_session.cpp
  )
endif()

add_library(tp_runtime STATIC ${_tp_runtime_sources})

add_library(tp::runtime ALIAS tp_runtime)

target_include_directories(tp_runtime
  PUBLIC
    $<BUILD_INTERFACE:${CMAKE_SOURCE_DIR}/include>
    $<BUILD_INTERFACE:${CMAKE_BINARY_DIR}/generated/include>
  PRIVATE
    $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/src>
)

target_compile_features(tp_runtime PUBLIC cxx_std_20)

if(TP_ENABLE_TENSORRT)
  target_compile_definitions(tp_runtime PUBLIC TP_ENABLE_TENSORRT=1)
  # Probe for the TensorRT and CUDA SDKs. When both are usable, expose
  # the headers/libraries and define TP_HAS_TENSORRT_SDK so the adapter
  # compiles its real execution path. Host CI builds with the flag on
  # but no SDK installed register the adapter and surface typed
  # Unsupported from `load` instead of failing the build.
  find_package(CUDAToolkit QUIET)
  find_path(TP_TENSORRT_INCLUDE_DIR NvInfer.h
    HINTS ENV TENSORRT_ROOT ENV TENSORRT_INSTALL_DIR
    PATH_SUFFIXES include)
  find_library(TP_TENSORRT_NVINFER_LIB nvinfer
    HINTS ENV TENSORRT_ROOT ENV TENSORRT_INSTALL_DIR
    PATH_SUFFIXES lib lib64 lib/x86_64-linux-gnu lib/aarch64-linux-gnu)
  if(TP_TENSORRT_INCLUDE_DIR AND TP_TENSORRT_NVINFER_LIB AND CUDAToolkit_FOUND)
    message(STATUS "TensorRT SDK detected; building real TensorRT adapter execution path")
    target_compile_definitions(tp_runtime PRIVATE TP_HAS_TENSORRT_SDK=1)
    target_include_directories(tp_runtime PRIVATE ${TP_TENSORRT_INCLUDE_DIR})
    target_link_libraries(tp_runtime PUBLIC ${TP_TENSORRT_NVINFER_LIB} CUDA::cudart)
  else()
    message(STATUS
      "TensorRT SDK not found; building TensorRT adapter without execution support "
      "(load() returns Unsupported). Set TENSORRT_ROOT / install CUDAToolkit to enable.")
  endif()
endif()
if(TP_ENABLE_LIBTORCH)
  target_compile_definitions(tp_runtime PUBLIC TP_ENABLE_LIBTORCH=1)
  # `Torch_DIR` points at a libtorch install (cmake/Torch). Without it
  # the adapter still compiles and surfaces typed `Unsupported` from
  # `load`; with it, the adapter executes TorchScript modules.
  find_package(Torch QUIET)
  if(Torch_FOUND)
    message(STATUS "LibTorch found; building real LibTorch adapter execution path")
    target_compile_definitions(tp_runtime PRIVATE TP_HAS_LIBTORCH_SDK=1)
    target_link_libraries(tp_runtime PUBLIC ${TORCH_LIBRARIES})
    # `set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} ${TORCH_CXX_FLAGS}")` is
    # the standard recipe; apply it on the target only to keep the
    # ABI scope localized.
    if(TORCH_CXX_FLAGS)
      separate_arguments(_tp_torch_flags UNIX_COMMAND "${TORCH_CXX_FLAGS}")
      target_compile_options(tp_runtime PRIVATE ${_tp_torch_flags})
    endif()
  else()
    message(STATUS
      "LibTorch not found; building LibTorch adapter without execution support "
      "(load() returns Unsupported). Set Torch_DIR to enable.")
  endif()
endif()
if(TP_ENABLE_PYTHON_PYTORCH_SIDECAR)
  target_compile_definitions(tp_runtime PUBLIC TP_ENABLE_PYTHON_PYTORCH_SIDECAR=1)
  # The sidecar adapter parses JSON sidecar headers via nlohmann::json
  # (header-only); v0.1.0 requires it via vcpkg. The link below is a
  # header-only INTERFACE target, so no runtime libs are pulled in.
  find_package(nlohmann_json CONFIG REQUIRED)
  target_link_libraries(tp_runtime PUBLIC nlohmann_json::nlohmann_json)
endif()

set_target_properties(tp_runtime PROPERTIES
  CXX_VISIBILITY_PRESET hidden
  VISIBILITY_INLINES_HIDDEN ON
)

tp_apply_warnings(tp_runtime)
tp_apply_sanitizers(tp_runtime)
