# T1 unit tests for tp_runtime.

add_executable(tp_test_unit
  version_test.cpp
  error_test.cpp
  result_test.cpp
  model_spec_test.cpp
)

target_link_libraries(tp_test_unit
  PRIVATE
    tp::runtime
    GTest::gtest_main
)

target_compile_features(tp_test_unit PRIVATE cxx_std_20)

tp_apply_warnings(tp_test_unit)
tp_apply_sanitizers(tp_test_unit)

gtest_discover_tests(tp_test_unit
  PROPERTIES
    LABELS "T1"
  DISCOVERY_MODE PRE_TEST
)
