# T3 contract tests.
#
# Cross-adapter conformance harness that runs the V01-E04
# `ExecutionSession` lifecycle suite against every real backend
# compiled into this build of `tp_runtime`. Real-adapter coverage
# is gated on the V01-E05-F02 / F03 / F05 build flags; the
# `RealAdapterConformance` cases that cannot run on this host skip
# at runtime.

add_executable(tp_test_contract
  real_adapter_conformance_test.cpp
)

target_include_directories(tp_test_contract
  PRIVATE
    ${CMAKE_SOURCE_DIR}/test/mocks
    ${CMAKE_SOURCE_DIR}/test/contract
)

target_link_libraries(tp_test_contract
  PRIVATE
    tp::runtime
    GTest::gtest_main
)

target_compile_features(tp_test_contract PRIVATE cxx_std_20)

tp_apply_warnings(tp_test_contract)
tp_apply_sanitizers(tp_test_contract)

gtest_discover_tests(tp_test_contract
  PROPERTIES
    LABELS "T3"
  DISCOVERY_MODE PRE_TEST
)
