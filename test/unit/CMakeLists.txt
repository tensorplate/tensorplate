# T1 unit tests for tp_runtime.

add_executable(tp_test_unit
  version_test.cpp
  error_test.cpp
  result_test.cpp
  model_spec_test.cpp
  buffer_ref_test.cpp
  tensor_view_test.cpp
  buffer_manager_test.cpp
  buffer_cleanup_test.cpp
  buffer_ingress_test.cpp
  buffer_output_test.cpp
  buffer_pressure_test.cpp
  infer_request_test.cpp
  infer_result_test.cpp
  job_value_objects_test.cpp
  execution_session_interface_test.cpp
  execution_session_lifecycle_test.cpp
  execution_session_validation_test.cpp
  execution_session_infer_test.cpp
  execution_session_async_test.cpp
  execution_session_events_test.cpp
  execution_session_conformance_test.cpp
  backend_capability_test.cpp
  backend_registry_test.cpp
  backend_builtin_test.cpp
  tensorrt_adapter_test.cpp
  libtorch_adapter_test.cpp
  sidecar_codec_test.cpp
  worker_control_codec_test.cpp
  sidecar_process_test.cpp
  runner_profile_test.cpp
  scheduler_interface_test.cpp
  scheduler_fifo_test.cpp
  scheduler_deadline_test.cpp
  scheduler_cancellation_test.cpp
  scheduler_metrics_test.cpp
  scheduler_pressure_test.cpp
  scheduler_in_flight_test.cpp
  scheduler_schema_test.cpp
  # V01-E07: serving worker.
  serving_config_test.cpp
  serving_serialization_test.cpp
  serving_pipeline_test.cpp
  serving_health_metrics_test.cpp
  serving_async_store_test.cpp
  serving_error_status_test.cpp
  session_state_machine_test.cpp
  session_limits_test.cpp
  session_timers_test.cpp
  session_manager_test.cpp
  session_credits_test.cpp
  session_output_queue_test.cpp
  session_tombstones_test.cpp
  session_finalize_test.cpp
  session_race_test.cpp
  session_dispatch_test.cpp
  control_channel_test.cpp
)

target_include_directories(tp_test_unit
  PRIVATE
    ${CMAKE_SOURCE_DIR}/test/mocks
    ${CMAKE_SOURCE_DIR}/test/contract
    ${CMAKE_SOURCE_DIR}/runtime/src
)

target_link_libraries(tp_test_unit
  PRIVATE
    tp::runtime
    tp::synthetic_session_dispatch
    GTest::gtest_main
)

# Lets schema drift tests read protocol/schemas/ from the source tree.
target_compile_definitions(tp_test_unit PRIVATE TP_SOURCE_DIR="${CMAKE_SOURCE_DIR}")
target_compile_definitions(tp_test_unit PRIVATE
  TP_ENABLE_STREAMING_GRPC=$<BOOL:${TP_ENABLE_STREAMING_GRPC}>)

target_compile_features(tp_test_unit PRIVATE cxx_std_20)

tp_apply_warnings(tp_test_unit)
tp_apply_sanitizers(tp_test_unit)

gtest_discover_tests(tp_test_unit
  PROPERTIES
    LABELS "T1"
  DISCOVERY_MODE PRE_TEST
)
