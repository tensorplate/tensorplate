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
  # Logical-session lifecycle and cancel acknowledgement on the real clock.
  session_lifecycle_test.cpp
  # The worker binary and the control descriptor it inherits as fd 0.
  control_descriptor_test.cpp
  worker_signal_test.cpp
  # What --version says about streaming support, in both build flavours.
  worker_version_test.cpp
)

target_include_directories(tp_test_integration
  PRIVATE
    ${CMAKE_SOURCE_DIR}/test/mocks
    ${CMAKE_SOURCE_DIR}/runtime/src
)

target_link_libraries(tp_test_integration
  PRIVATE
    tp::runtime
    GTest::gtest_main
)

# Lets the adapter test stage the packaged backend descriptor and read
# bundle fixtures from the source tree.
target_compile_definitions(tp_test_integration PRIVATE TP_SOURCE_DIR="${CMAKE_SOURCE_DIR}")

# The adapter test runs the serving worker binary against a bundle fixture
# and compares its stderr with the fixture the agent's deploy tests replay.
if(TARGET tp_serving_worker)
  add_dependencies(tp_test_integration tp_serving_worker)
  target_compile_definitions(tp_test_integration
    PRIVATE
      TP_SERVING_WORKER_BINARY="$<TARGET_FILE:tp_serving_worker>"
      TP_ENABLE_STREAMING_GRPC=$<BOOL:${TP_ENABLE_STREAMING_GRPC}>
  )
endif()

target_compile_features(tp_test_integration PRIVATE cxx_std_20)

tp_apply_warnings(tp_test_integration)
tp_apply_sanitizers(tp_test_integration)

gtest_discover_tests(tp_test_integration
  PROPERTIES
    LABELS "T2"
  DISCOVERY_MODE PRE_TEST
)
