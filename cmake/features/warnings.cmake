# tp_apply_warnings(target [PRIVATE|PUBLIC|INTERFACE])
#
# Apply the project-wide warning set to a tp_* target. Defaults to PRIVATE
# scope so warning flags do not leak through INTERFACE consumers.
#
# Honors TP_WARNINGS_AS_ERRORS to flip warnings to errors at CI time.

function(tp_apply_warnings target)
  set(_options "")
  set(_one_value "SCOPE")
  set(_multi_value "")
  cmake_parse_arguments(_TPW "${_options}" "${_one_value}" "${_multi_value}" ${ARGN})
  if(NOT _TPW_SCOPE)
    set(_TPW_SCOPE PRIVATE)
  endif()

  if(MSVC)
    target_compile_options(${target} ${_TPW_SCOPE} /W4 /permissive- /utf-8)
    if(TP_WARNINGS_AS_ERRORS)
      target_compile_options(${target} ${_TPW_SCOPE} /WX)
    endif()
  else()
    target_compile_options(${target} ${_TPW_SCOPE}
      -Wall
      -Wextra
      -Wpedantic
      -Wshadow
      -Wnon-virtual-dtor
      -Wold-style-cast
      -Wcast-align
      -Woverloaded-virtual
      -Wdouble-promotion
      -Wformat=2
    )
    if(TP_WARNINGS_AS_ERRORS)
      target_compile_options(${target} ${_TPW_SCOPE} -Werror)
    endif()
  endif()
endfunction()
