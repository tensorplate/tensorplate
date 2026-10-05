# An explicit OFF must work even with an unusable system gRPC installation.
if(NOT DEFINED TP_ENABLE_STREAMING_GRPC OR TP_ENABLE_STREAMING_GRPC)
  find_package(gRPC CONFIG QUIET)
endif()
option(TP_ENABLE_STREAMING_GRPC
       "Build with the pinned static gRPC/protobuf streaming dependencies" ${gRPC_FOUND})

if(NOT TP_ENABLE_STREAMING_GRPC)
  return()
endif()

if(NOT gRPC_FOUND)
  message(FATAL_ERROR
    "TP_ENABLE_STREAMING_GRPC=ON requires the streaming-grpc vcpkg manifest feature.")
endif()
if(NOT VCPKG_TARGET_TRIPLET OR NOT VCPKG_INSTALLED_DIR)
  message(FATAL_ERROR "Streaming gRPC requires the pinned vcpkg manifest toolchain.")
endif()

find_package(Protobuf CONFIG REQUIRED)

# Triplet preference alone is insufficient: ports may ignore it. Check the
# actual imported archives, including OpenSSL, before accepting the link model.
set(_tp_streaming_prefix "${VCPKG_INSTALLED_DIR}/${VCPKG_TARGET_TRIPLET}")
foreach(_tp_target IN ITEMS gRPC::grpc++ gRPC::grpc gRPC::gpr
                            protobuf::libprotobuf OpenSSL::SSL OpenSSL::Crypto)
  if(NOT TARGET ${_tp_target})
    message(FATAL_ERROR "Streaming gRPC requires imported target ${_tp_target}.")
  endif()
  get_target_property(_tp_type ${_tp_target} TYPE)
  if(NOT _tp_type MATCHES "^(STATIC|UNKNOWN)_LIBRARY$")
    message(FATAL_ERROR "Streaming gRPC requires static libraries: ${_tp_target} is ${_tp_type}.")
  endif()
  get_target_property(_tp_configs ${_tp_target} IMPORTED_CONFIGURATIONS)
  set(_tp_locations IMPORTED_LOCATION)
  foreach(_tp_config IN LISTS _tp_configs)
    list(APPEND _tp_locations "IMPORTED_LOCATION_${_tp_config}")
  endforeach()
  set(_tp_has_archive OFF)
  foreach(_tp_property IN LISTS _tp_locations)
    get_target_property(_tp_location ${_tp_target} ${_tp_property})
    if(_tp_location)
      cmake_path(IS_PREFIX _tp_streaming_prefix "${_tp_location}" NORMALIZE _tp_in_prefix)
      if(NOT _tp_in_prefix OR NOT _tp_location MATCHES "\\.(a|lib)$")
        message(FATAL_ERROR "Streaming gRPC requires vcpkg static archives: ${_tp_target}.")
      endif()
      set(_tp_has_archive ON)
    endif()
  endforeach()
  if(NOT _tp_has_archive)
    message(FATAL_ERROR "Streaming gRPC target has no imported static archive: ${_tp_target}.")
  endif()
endforeach()
message(STATUS "Streaming gRPC: static archives verified for ${VCPKG_TARGET_TRIPLET}")
