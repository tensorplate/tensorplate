# tp_example_buffer_plane - V01-E03 end-to-end developer smoke.

add_executable(tp_example_buffer_plane
  main.cpp
)

add_executable(tp::example_buffer_plane ALIAS tp_example_buffer_plane)

target_include_directories(tp_example_buffer_plane
  PRIVATE
    ${CMAKE_SOURCE_DIR}/test/mocks
)

target_link_libraries(tp_example_buffer_plane
  PRIVATE
    tp::runtime
)

target_compile_features(tp_example_buffer_plane PRIVATE cxx_std_20)

set_target_properties(tp_example_buffer_plane PROPERTIES
  OUTPUT_NAME tensorplate-example-buffer-plane
)

tp_apply_warnings(tp_example_buffer_plane)
tp_apply_sanitizers(tp_example_buffer_plane)
