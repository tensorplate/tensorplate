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

if(TP_ENABLE_STREAMING_GRPC)
  # The stream envelope's golden frames, read from the source tree and decoded
  # with the bindings generated in this build.
  target_sources(tp_test_contract PRIVATE stream_envelope_conformance_test.cpp)
  target_link_libraries(tp_test_contract PRIVATE tp::stream_proto)
  target_compile_definitions(tp_test_contract PRIVATE TP_SOURCE_DIR="${CMAKE_SOURCE_DIR}")

  add_test(NAME contract.stream_frames_recorded
    COMMAND ${CMAKE_CURRENT_SOURCE_DIR}/fixtures/stream/v1/record.sh --check
            $<TARGET_FILE:protobuf::protoc>)
  set_tests_properties(contract.stream_frames_recorded PROPERTIES LABELS "T3")
  tp_uninstrumented_protobuf_test_properties(_tp_contract_test_properties)
endif()

tp_apply_warnings(tp_test_contract)
tp_apply_sanitizers(tp_test_contract)

gtest_discover_tests(tp_test_contract
  PROPERTIES
    LABELS "T3"
    ${_tp_contract_test_properties}
  DISCOVERY_MODE PRE_TEST
)
