# tp_apply_sanitizers(target)
#
# When TP_ENABLE_SANITIZERS=ON, instrument a tp_* target with AddressSanitizer
# and UndefinedBehaviorSanitizer on Clang/GCC. No-op on MSVC and when the
# option is off.
#
# Sanitizers are an opt-in CI gate; release builds do not enable them.

function(tp_apply_sanitizers target)
  if(NOT TP_ENABLE_SANITIZERS)
    return()
  endif()
  if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang|AppleClang")
    target_compile_options(${target} PRIVATE
      -fsanitize=address,undefined
      -fno-omit-frame-pointer
      -fno-sanitize-recover=undefined
    )
    target_link_options(${target} PRIVATE
      -fsanitize=address,undefined
    )
  else()
    message(WARNING
      "TP_ENABLE_SANITIZERS=ON but compiler ${CMAKE_CXX_COMPILER_ID} "
      "does not have a supported ASAN/UBSAN configuration. Skipping for "
      "${target}.")
  endif()
endfunction()
