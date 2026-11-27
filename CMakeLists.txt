cmake_minimum_required(VERSION 3.20)

file(STRINGS "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml" _version_line REGEX "^version = ")
string(REGEX REPLACE "^version = \"([^\"]+)\".*" "\\1" OTEL_CAPI_VERSION "${_version_line}")

project(otel-capi VERSION ${OTEL_CAPI_VERSION} LANGUAGES C)

include(GNUInstallDirs)
include(CMakePackageConfigHelpers)

find_program(CARGO_EXECUTABLE cargo REQUIRED)
file(MAKE_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/include")

add_custom_target(otel_capi ALL
  COMMAND "${CARGO_EXECUTABLE}" build --release --locked -p otel-capi
  COMMAND "${CARGO_EXECUTABLE}" build --release --locked -p otel-capi-grpc-exporter
  COMMAND "${CARGO_EXECUTABLE}" build --release --locked -p otel-capi-http-exporter
  COMMAND "${CARGO_EXECUTABLE}" build --release --locked -p otel-capi-stdout-exporter
  WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
  COMMENT "Building otel-capi with Cargo"
  VERBATIM)

set(PREFIX "${CMAKE_INSTALL_PREFIX}")
configure_file(
  "${CMAKE_CURRENT_SOURCE_DIR}/otel-capi.pc.in"
  "${CMAKE_CURRENT_BINARY_DIR}/otel-capi.pc"
  @ONLY)

set(OTEL_CAPI_INSTALL_CMAKEDIR "${CMAKE_INSTALL_LIBDIR}/cmake/otel-capi")
configure_package_config_file(
  "${CMAKE_CURRENT_SOURCE_DIR}/cmake/otel-capi-config.cmake.in"
  "${CMAKE_CURRENT_BINARY_DIR}/otel-capi-config.cmake"
  INSTALL_DESTINATION "${OTEL_CAPI_INSTALL_CMAKEDIR}"
  PATH_VARS CMAKE_INSTALL_INCLUDEDIR CMAKE_INSTALL_LIBDIR)
write_basic_package_version_file(
  "${CMAKE_CURRENT_BINARY_DIR}/otel-capi-config-version.cmake"
  VERSION ${PROJECT_VERSION}
  COMPATIBILITY SameMajorVersion)

install(DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/include/"
  DESTINATION "${CMAKE_INSTALL_INCLUDEDIR}"
  FILES_MATCHING PATTERN "otel_capi*.h")

set(_static "${CMAKE_STATIC_LIBRARY_PREFIX}otel_capi${CMAKE_STATIC_LIBRARY_SUFFIX}")
set(_shared "${CMAKE_SHARED_LIBRARY_PREFIX}otel_capi${CMAKE_SHARED_LIBRARY_SUFFIX}")
set(_grpc_exporter "${CMAKE_SHARED_LIBRARY_PREFIX}otel_capi_grpc_exporter${CMAKE_SHARED_LIBRARY_SUFFIX}")
set(_http_exporter "${CMAKE_SHARED_LIBRARY_PREFIX}otel_capi_http_exporter${CMAKE_SHARED_LIBRARY_SUFFIX}")
set(_stdout_exporter "${CMAKE_SHARED_LIBRARY_PREFIX}otel_capi_stdout_exporter${CMAKE_SHARED_LIBRARY_SUFFIX}")
set(_release_dir "${CMAKE_CURRENT_SOURCE_DIR}/target/release")
set(_install_libs "${_release_dir}/${_static}" "${_release_dir}/${_shared}"
  "${_release_dir}/${_grpc_exporter}" "${_release_dir}/${_http_exporter}"
  "${_release_dir}/${_stdout_exporter}")
if(WIN32)
  list(APPEND _install_libs "${_release_dir}/otel_capi.dll.lib"
    "${_release_dir}/otel_capi_grpc_exporter.dll.lib"
    "${_release_dir}/otel_capi_http_exporter.dll.lib"
    "${_release_dir}/otel_capi_stdout_exporter.dll.lib")
endif()

install(FILES ${_install_libs}
  DESTINATION "${CMAKE_INSTALL_LIBDIR}"
  OPTIONAL)

if(WIN32)
  install(FILES "${_release_dir}/otel_capi.dll"
    "${_release_dir}/otel_capi_grpc_exporter.dll"
    "${_release_dir}/otel_capi_stdout_exporter.dll"
    DESTINATION "${CMAKE_INSTALL_BINDIR}"
    OPTIONAL)
endif()

install(FILES "${CMAKE_CURRENT_BINARY_DIR}/otel-capi.pc"
  DESTINATION "${CMAKE_INSTALL_LIBDIR}/pkgconfig")

install(FILES
  "${CMAKE_CURRENT_BINARY_DIR}/otel-capi-config.cmake"
  "${CMAKE_CURRENT_BINARY_DIR}/otel-capi-config-version.cmake"
  DESTINATION "${OTEL_CAPI_INSTALL_CMAKEDIR}")

install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/LICENSE"
  DESTINATION "${CMAKE_INSTALL_DATAROOTDIR}/licenses/otel-capi")
