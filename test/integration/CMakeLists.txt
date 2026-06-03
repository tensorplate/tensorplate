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
  # Socket write helpers suppress SIGPIPE on peer close.
  socket_sigpipe_test.cpp
  # Route dispatch releases routes_mutex before invoking handlers.
  http_route_concurrency_test.cpp
  # HTTP server binds the loopback family the host literal names.
  http_server_bind_test.cpp
  # Header-size limit is enforced before a completed header block is
  # accepted, including the read chunk that carries the terminator.
  http_header_limit_test.cpp
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
