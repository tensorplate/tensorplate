# tp_runtime - core C++20 inference runtime library.
#
# V01-E01-F02 only wires the CMake target. Implementation lands in
# V01-E02 (core types), V01-E03 (buffer plane), V01-E04 (execution session),
# V01-E05 (adapters), and V01-E06 (scheduler).

add_library(tp_runtime STATIC
  src/skeleton.cpp
)

add_library(tp::runtime ALIAS tp_runtime)

target_include_directories(tp_runtime
  PUBLIC
    $<BUILD_INTERFACE:${CMAKE_SOURCE_DIR}/include>
  PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/include
)

target_compile_features(tp_runtime PUBLIC cxx_std_20)

set_target_properties(tp_runtime PROPERTIES
  CXX_VISIBILITY_PRESET hidden
  VISIBILITY_INLINES_HIDDEN ON
)

tp_apply_warnings(tp_runtime)
tp_apply_sanitizers(tp_runtime)
