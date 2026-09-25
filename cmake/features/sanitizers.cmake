# tp_apply_sanitizers(target)
#
# Instruments a tp_* target on Clang/GCC:
#
#   TP_ENABLE_SANITIZERS=ON  AddressSanitizer and UndefinedBehaviorSanitizer
#   TP_ENABLE_TSAN=ON        ThreadSanitizer
#
# ThreadSanitizer cannot be combined with AddressSanitizer, so configure stops
# when both options are on; each needs its own build tree. No-op on MSVC and
# when both options are off.
#
# Sanitizers are opt-in CI gates; release builds do not enable them. The
# option check below also runs in script mode (cmake -D... -P this file), which
# is how test/cmake/sanitizer_options_test.cmake exercises it.

if(TP_ENABLE_SANITIZERS AND TP_ENABLE_TSAN)
  message(FATAL_ERROR
    "TP_ENABLE_TSAN and TP_ENABLE_SANITIZERS cannot both be ON: ThreadSanitizer "
    "cannot be combined with AddressSanitizer. Configure a separate build tree "
    "for each.")
endif()

function(tp_apply_sanitizers target)
  if(TP_ENABLE_SANITIZERS)
    set(_tp_sanitize address,undefined)
    set(_tp_sanitize_label "ASAN/UBSAN")
  elseif(TP_ENABLE_TSAN)
    set(_tp_sanitize thread)
    set(_tp_sanitize_label "TSAN")
  else()
    return()
  endif()
  if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang|AppleClang")
    target_compile_options(${target} PRIVATE
      -fsanitize=${_tp_sanitize}
      -fno-omit-frame-pointer
    )
    if(TP_ENABLE_SANITIZERS)
      target_compile_options(${target} PRIVATE -fno-sanitize-recover=undefined)
    endif()
    target_link_options(${target} PRIVATE
      -fsanitize=${_tp_sanitize}
    )
  else()
    message(WARNING
      "${_tp_sanitize_label} requested but compiler ${CMAKE_CXX_COMPILER_ID} "
      "does not have a supported configuration. Skipping for ${target}.")
  endif()
endfunction()
