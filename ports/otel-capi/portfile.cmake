vcpkg_from_git(
    OUT_SOURCE_PATH SOURCE_PATH
    URL "https://github.com/daniel-white/otel_capi.git"
    REF "v${VERSION}"
)

vcpkg_cmake_configure(SOURCE_PATH "${SOURCE_PATH}")
vcpkg_cmake_build()
vcpkg_cmake_install()
vcpkg_cmake_config_fixup(
    PACKAGE_NAME otel-capi
    CONFIG_PATH "lib/cmake/otel-capi")
vcpkg_fixup_pkgconfig()

file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/debug")
vcpkg_install_copyright(FILE_LIST "${SOURCE_PATH}/LICENSE")
