# tp_runtime - core C++20 inference runtime library.
#
# Layered source list. V01-E05 introduces the backend registry, the
# adapter directory (TensorRT, LibTorch, Python/PyTorch sidecar), and
# the IPC framing helpers that the sidecar adapter rides on.

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
)

# V01-E05 adapter feature flags. Off by default so host CI builds clean
# without proprietary SDKs. When ON, the adapter source file registers
# its factory through `tensorplate::register_builtin_backends()`.
option(TP_ENABLE_TENSORRT
       "Build the TensorRT execution-backend adapter (V01-E05-F02)." OFF)
option(TP_ENABLE_LIBTORCH
       "Build the LibTorch native execution-backend adapter (V01-E05-F03)." OFF)
option(TP_ENABLE_PYTHON_PYTORCH_SIDECAR
       "Build the Python/PyTorch sidecar execution-backend adapter (V01-E05-F05)."
       OFF)

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
endif()
if(TP_ENABLE_LIBTORCH)
  target_compile_definitions(tp_runtime PUBLIC TP_ENABLE_LIBTORCH=1)
endif()
if(TP_ENABLE_PYTHON_PYTORCH_SIDECAR)
  target_compile_definitions(tp_runtime PUBLIC TP_ENABLE_PYTHON_PYTORCH_SIDECAR=1)
endif()

set_target_properties(tp_runtime PROPERTIES
  CXX_VISIBILITY_PRESET hidden
  VISIBILITY_INLINES_HIDDEN ON
)

tp_apply_warnings(tp_runtime)
tp_apply_sanitizers(tp_runtime)
