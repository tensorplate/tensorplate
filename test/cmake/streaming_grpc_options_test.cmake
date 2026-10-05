# Synthetic imported targets isolate configure policy from installed packages.
if(NOT STREAMING_CMAKE)
  message(FATAL_ERROR "set STREAMING_CMAKE to cmake/features/streaming_grpc.cmake")
endif()
set(root "${CMAKE_CURRENT_BINARY_DIR}/streaming-options")
file(MAKE_DIRECTORY "${root}/source" "${root}/packages")
file(WRITE "${root}/source/CMakeLists.txt" [=[
cmake_minimum_required(VERSION 3.25)
project(StreamingOptions NONE)
include("${STREAMING_CMAKE}")
if(NOT "${TP_ENABLE_STREAMING_GRPC}" STREQUAL "${EXPECTED}")
  message(FATAL_ERROR "unexpected streaming option: ${TP_ENABLE_STREAMING_GRPC}")
endif()
]=])
file(WRITE "${root}/packages/gRPCConfig.cmake" [=[
if(FORBID_DISCOVERY)
  message(FATAL_ERROR "explicit OFF discovered gRPC")
endif()
set(gRPC_FOUND TRUE)
foreach(name gRPC::grpc++ gRPC::grpc gRPC::gpr OpenSSL::SSL OpenSSL::Crypto)
  set(kind STATIC)
  set(extension a)
  if(name STREQUAL BAD_TARGET)
    set(kind "${BAD_KIND}")
    set(extension "${BAD_EXTENSION}")
  endif()
  add_library(${name} ${kind} IMPORTED)
  set_target_properties(${name} PROPERTIES
    IMPORTED_CONFIGURATIONS "RELEASE;DEBUG"
    IMPORTED_LOCATION_RELEASE "${VCPKG_INSTALLED_DIR}/x64-linux/lib/library.${extension}"
    IMPORTED_LOCATION_DEBUG "${VCPKG_INSTALLED_DIR}/x64-linux/debug/lib/library.${extension}")
  if(name STREQUAL OUTSIDE_TARGET)
    set_property(TARGET ${name} PROPERTY IMPORTED_LOCATION_DEBUG "/system/lib/library.a")
  endif()
endforeach()
]=])
file(WRITE "${root}/packages/ProtobufConfig.cmake" [=[
add_library(protobuf::libprotobuf STATIC IMPORTED)
set_target_properties(protobuf::libprotobuf PROPERTIES
  IMPORTED_LOCATION "${VCPKG_INSTALLED_DIR}/x64-linux/lib/protobuf.a")
if(MISSING_ARCHIVE)
  set_property(TARGET protobuf::libprotobuf PROPERTY IMPORTED_LOCATION "")
endif()
]=])

function(run_case name expected_error)
  execute_process(COMMAND "${CMAKE_COMMAND}" --fresh
    -S "${root}/source" -B "${root}/${name}"
    "-DSTREAMING_CMAKE=${STREAMING_CMAKE}"
    "-DgRPC_DIR=${root}/packages" "-DProtobuf_DIR=${root}/packages"
    "-DVCPKG_INSTALLED_DIR=${root}/installed" -DVCPKG_TARGET_TRIPLET=x64-linux
    -DEXPECTED=ON ${ARGN}
    RESULT_VARIABLE result OUTPUT_VARIABLE output ERROR_VARIABLE output)
  string(REGEX REPLACE "[ \t\r\n]+" " " normalized "${output}")
  if(expected_error STREQUAL "")
    if(NOT result EQUAL 0)
      message(FATAL_ERROR "${name} unexpectedly failed:\n${output}")
    endif()
  elseif(result EQUAL 0 OR NOT normalized MATCHES "${expected_error}")
    message(FATAL_ERROR "${name} did not fail for ${expected_error}:\n${output}")
  endif()
endfunction()

run_case(absent_default "" -DCMAKE_DISABLE_FIND_PACKAGE_gRPC=TRUE -DEXPECTED=OFF)
run_case(absent_on "requires the streaming-grpc vcpkg manifest feature"
  -DCMAKE_DISABLE_FIND_PACKAGE_gRPC=TRUE -DTP_ENABLE_STREAMING_GRPC=ON)
run_case(installed_default "")
run_case(installed_on "" -DTP_ENABLE_STREAMING_GRPC=ON)
run_case(installed_off "" -DTP_ENABLE_STREAMING_GRPC=OFF -DEXPECTED=OFF -DFORBID_DISCOVERY=ON)
run_case(no_toolchain "requires the pinned vcpkg manifest toolchain" -DVCPKG_TARGET_TRIPLET=)
run_case(shared_grpc "requires static libraries"
  -DBAD_TARGET=gRPC::grpc++ -DBAD_KIND=SHARED -DBAD_EXTENSION=so)
run_case(shared_ssl "requires vcpkg static archives"
  -DBAD_TARGET=OpenSSL::SSL -DBAD_KIND=UNKNOWN -DBAD_EXTENSION=so)
run_case(system_debug_archive "requires vcpkg static archives" -DOUTSIDE_TARGET=OpenSSL::Crypto)
run_case(missing_archive "has no imported static archive" -DMISSING_ARCHIVE=ON)
