# T2 integration tests.
#
# Cross-layer tests that exercise multiple parts of the runtime together
# with mocks substituted at hardware or process boundaries. Required on
# every PR when a change crosses layer lines.

add_executable(tp_test_integration
  buffer_plane_e2e_test.cpp
  sidecar_socket_e2e_test.cpp
  python_pytorch_adapter_test.cpp
  scheduler_smolvla_test.cpp
  # V01-E07-F08: full serving worker end-to-end coverage.
  serving_e2e_test.cpp
)

target_include_directories(tp_test_integration
  PRIVATE
    ${CMAKE_SOURCE_DIR}/test/mocks
)

target_link_libraries(tp_test_integration
  PRIVATE
    tp::runtime
    GTest::gtest_main
)

target_compile_features(tp_test_integration PRIVATE cxx_std_20)

tp_apply_warnings(tp_test_integration)
tp_apply_sanitizers(tp_test_integration)

gtest_discover_tests(tp_test_integration
  PROPERTIES
    LABELS "T2"
  DISCOVERY_MODE PRE_TEST
)
