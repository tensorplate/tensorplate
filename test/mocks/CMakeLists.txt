# Stand-ins with sources of their own. The header-only ones need no target:
# a test adds this directory to its include path.

# A session dispatch with no backend, for tests of a transport binding.
add_library(tp_synthetic_session_dispatch STATIC
  synthetic_session_dispatch.cpp
)
add_library(tp::synthetic_session_dispatch ALIAS tp_synthetic_session_dispatch)

target_include_directories(tp_synthetic_session_dispatch
  PUBLIC
    ${CMAKE_CURRENT_SOURCE_DIR}
    ${CMAKE_SOURCE_DIR}/runtime/src
)

target_link_libraries(tp_synthetic_session_dispatch
  PUBLIC
    tp::runtime
)

tp_apply_warnings(tp_synthetic_session_dispatch)
tp_apply_sanitizers(tp_synthetic_session_dispatch)
