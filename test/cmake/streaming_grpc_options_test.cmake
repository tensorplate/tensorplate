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
  if(name STREQUAL MISSING_TARGET)
    continue()
  endif()
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
if(NOT MISSING_TARGET STREQUAL "gRPC::grpc_cpp_plugin")
  add_executable(gRPC::grpc_cpp_plugin IMPORTED)
  set_target_properties(gRPC::grpc_cpp_plugin PROPERTIES
    IMPORTED_CONFIGURATIONS "RELEASE;DEBUG"
    IMPORTED_LOCATION_RELEASE "${VCPKG_INSTALLED_DIR}/${TOOL_TRIPLET}/tools/grpc/grpc_cpp_plugin"
    IMPORTED_LOCATION_DEBUG "${VCPKG_INSTALLED_DIR}/${TOOL_TRIPLET}/tools/grpc/grpc_cpp_plugin")
  if(OUTSIDE_TARGET STREQUAL "gRPC::grpc_cpp_plugin")
    set_property(TARGET gRPC::grpc_cpp_plugin PROPERTY
      IMPORTED_LOCATION_DEBUG "/system/bin/grpc_cpp_plugin")
  endif()
endif()
]=])
file(WRITE "${root}/packages/ProtobufConfig.cmake" [=[
add_library(protobuf::libprotobuf STATIC IMPORTED)
set_target_properties(protobuf::libprotobuf PROPERTIES
  IMPORTED_LOCATION "${VCPKG_INSTALLED_DIR}/x64-linux/lib/protobuf.a")
if(MISSING_ARCHIVE)
  set_property(TARGET protobuf::libprotobuf PROPERTY IMPORTED_LOCATION "")
endif()
if(DOTDOT_ARCHIVE)
  set_property(TARGET protobuf::libprotobuf PROPERTY
    IMPORTED_LOCATION "${VCPKG_INSTALLED_DIR}/x64-linux/../../system/lib/protobuf.a")
endif()
if(NOT MISSING_TARGET STREQUAL "protobuf::protoc")
  add_executable(protobuf::protoc IMPORTED)
  set_target_properties(protobuf::protoc PROPERTIES
    IMPORTED_CONFIGURATIONS RELEASE
    IMPORTED_LOCATION_RELEASE "${VCPKG_INSTALLED_DIR}/${TOOL_TRIPLET}/tools/protobuf/protoc")
  if(OUTSIDE_TARGET STREQUAL "protobuf::protoc")
    set_property(TARGET protobuf::protoc PROPERTY IMPORTED_LOCATION_RELEASE "/system/bin/protoc")
  endif()
  if(MISSING_GENERATOR)
    set_property(TARGET protobuf::protoc PROPERTY IMPORTED_LOCATION_RELEASE "")
  endif()
  if(DOTDOT_GENERATOR)
    set_property(TARGET protobuf::protoc PROPERTY
      IMPORTED_LOCATION_RELEASE "${VCPKG_INSTALLED_DIR}/../system/bin/protoc")
  endif()
endif()
]=])

function(run_case name expected_error)
  cmake_parse_arguments(PARSE_ARGV 2 CASE "" "STATUS" "")
  execute_process(COMMAND "${CMAKE_COMMAND}" --fresh
    -S "${root}/source" -B "${root}/${name}"
    "-DSTREAMING_CMAKE=${STREAMING_CMAKE}"
    "-DgRPC_DIR=${root}/packages" "-DProtobuf_DIR=${root}/packages"
    "-DVCPKG_INSTALLED_DIR=${root}/installed" -DVCPKG_TARGET_TRIPLET=x64-linux
    -DTOOL_TRIPLET=x64-linux -DEXPECTED=ON ${CASE_UNPARSED_ARGUMENTS}
    RESULT_VARIABLE result OUTPUT_VARIABLE output ERROR_VARIABLE output)
  string(REGEX REPLACE "[ \t\r\n]+" " " normalized "${output}")
  if(expected_error STREQUAL "")
    if(NOT result EQUAL 0)
      message(FATAL_ERROR "${name} unexpectedly failed:\n${output}")
    endif()
  elseif(result EQUAL 0 OR NOT normalized MATCHES "${expected_error}")
    message(FATAL_ERROR "${name} did not fail for ${expected_error}:\n${output}")
  endif()
  if(CASE_STATUS AND NOT normalized MATCHES "${CASE_STATUS}")
    message(FATAL_ERROR "${name} did not report ${CASE_STATUS}:\n${output}")
  endif()
endfunction()

# Every unavailable dependency must be harmless by default and fatal on request.
function(run_unavailable name reason)
  run_case(${name}_default "" -DEXPECTED=OFF ${ARGN}
    STATUS "Streaming gRPC: default OFF.*${reason}")
  run_case(${name}_on "${reason}" -DTP_ENABLE_STREAMING_GRPC=ON ${ARGN})
endfunction()

run_case(installed_default "")
run_case(installed_on "" -DTP_ENABLE_STREAMING_GRPC=ON)
run_case(installed_off "" -DTP_ENABLE_STREAMING_GRPC=OFF -DEXPECTED=OFF -DFORBID_DISCOVERY=ON)
run_unavailable(absent "requires the streaming-grpc vcpkg manifest feature"
  -DCMAKE_DISABLE_FIND_PACKAGE_gRPC=TRUE)
run_unavailable(no_toolchain "requires the pinned vcpkg manifest toolchain"
  -DVCPKG_TARGET_TRIPLET=)
run_unavailable(no_install_dir "requires the pinned vcpkg manifest toolchain"
  -DVCPKG_INSTALLED_DIR=)
run_unavailable(no_protobuf "requires the pinned vcpkg protobuf package"
  -DCMAKE_DISABLE_FIND_PACKAGE_Protobuf=TRUE)
run_unavailable(missing_target "requires imported target"
  -DMISSING_TARGET=gRPC::gpr)
run_unavailable(shared_grpc "requires static libraries"
  -DBAD_TARGET=gRPC::grpc++ -DBAD_KIND=SHARED -DBAD_EXTENSION=so)
run_unavailable(shared_ssl "requires vcpkg static archives"
  -DBAD_TARGET=OpenSSL::SSL -DBAD_KIND=UNKNOWN -DBAD_EXTENSION=so)
run_unavailable(system_grpc_archive "requires vcpkg static archives"
  -DOUTSIDE_TARGET=gRPC::grpc++)
run_unavailable(system_debug_archive "requires vcpkg static archives"
  -DOUTSIDE_TARGET=OpenSSL::Crypto)
run_unavailable(missing_archive "has no imported static archive" -DMISSING_ARCHIVE=ON)
# The generators are host tools: another triplet of the same install tree is theirs to use.
run_case(host_triplet_generators "" -DTOOL_TRIPLET=x64-linux-release)
run_unavailable(missing_protoc "requires imported target protobuf::protoc"
  -DMISSING_TARGET=protobuf::protoc)
run_unavailable(missing_plugin "requires imported target gRPC::grpc_cpp_plugin"
  -DMISSING_TARGET=gRPC::grpc_cpp_plugin)
run_unavailable(system_protoc "requires vcpkg code generators: protobuf::protoc"
  -DOUTSIDE_TARGET=protobuf::protoc)
run_unavailable(system_plugin "requires vcpkg code generators: gRPC::grpc_cpp_plugin"
  -DOUTSIDE_TARGET=gRPC::grpc_cpp_plugin)
run_unavailable(missing_generator "code generator has no imported location"
  -DMISSING_GENERATOR=ON)
# A location that starts with the install tree's path and leaves it through "..".
run_unavailable(dotdot_archive "requires vcpkg static archives: protobuf::libprotobuf"
  -DDOTDOT_ARCHIVE=ON)
run_unavailable(dotdot_protoc "requires vcpkg code generators: protobuf::protoc"
  -DDOTDOT_GENERATOR=ON)
message(STATUS "Streaming configure policy: 38 cases passed")
