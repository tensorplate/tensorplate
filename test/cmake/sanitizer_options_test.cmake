# Script-mode test of the sanitizer options in cmake/features/sanitizers.cmake:
# ASan+UBSan together with TSan must stop configure for that reason, and each
# option on its own must not.
#
#   cmake -DSANITIZERS_CMAKE=<path to sanitizers.cmake> -P sanitizer_options_test.cmake

if(NOT SANITIZERS_CMAKE)
  message(FATAL_ERROR "set SANITIZERS_CMAKE to cmake/features/sanitizers.cmake")
endif()

function(run_case asan tsan out_result out_output)
  execute_process(
    COMMAND ${CMAKE_COMMAND} -DTP_ENABLE_SANITIZERS=${asan} -DTP_ENABLE_TSAN=${tsan}
            -P ${SANITIZERS_CMAKE}
    RESULT_VARIABLE result
    OUTPUT_VARIABLE output
    ERROR_VARIABLE output)
  set(${out_result} "${result}" PARENT_SCOPE)
  set(${out_output} "${output}" PARENT_SCOPE)
endfunction()

run_case(ON ON result output)
if(result EQUAL 0)
  message(FATAL_ERROR "ASan+UBSan with TSan was accepted:\n${output}")
endif()
if(NOT output MATCHES "TP_ENABLE_TSAN and TP_ENABLE_SANITIZERS cannot both be ON")
  message(FATAL_ERROR "ASan+UBSan with TSan failed for another reason:\n${output}")
endif()

foreach(pair "ON;OFF" "OFF;ON" "OFF;OFF")
  list(GET pair 0 asan)
  list(GET pair 1 tsan)
  run_case(${asan} ${tsan} result output)
  if(NOT result EQUAL 0)
    message(FATAL_ERROR
      "TP_ENABLE_SANITIZERS=${asan} TP_ENABLE_TSAN=${tsan} was refused:\n${output}")
  endif()
endforeach()
