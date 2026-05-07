# tp_serving_worker - the tensorplate-serving data-plane process binary.
#
# V01-E01-F02 only wires the binary so the build graph and target names exist.
# The HTTP endpoint, request normalization, and runtime wiring land in
# V01-E07.

add_executable(tp_serving_worker
  src/main.cpp
)

add_executable(tp::serving_worker ALIAS tp_serving_worker)

target_link_libraries(tp_serving_worker PRIVATE tp::runtime)

target_compile_features(tp_serving_worker PRIVATE cxx_std_20)

set_target_properties(tp_serving_worker PROPERTIES
  OUTPUT_NAME tensorplate-serving
)

tp_apply_warnings(tp_serving_worker)
tp_apply_sanitizers(tp_serving_worker)
