import os

from conan import ConanFile
from conan.tools.cmake import CMake, CMakeDeps, CMakeToolchain, cmake_layout
from conan.tools.files import copy, get


class OtelCapiConan(ConanFile):
    name = "otel-capi"
    version = "0.1.0"
    license = "Apache-2.0"
    url = "https://github.com/daniel-white/otel_capi"
    homepage = "https://github.com/daniel-white/otel_capi"
    description = "C API for OpenTelemetry"
    settings = "os", "compiler", "build_type", "arch"
    package_type = "library"

    def layout(self):
        cmake_layout(self)

    def source(self):
        get(
            self,
            f"https://github.com/daniel-white/otel_capi/archive/refs/tags/v{self.version}.tar.gz",
            strip_root=True,
        )

    def generate(self):
        CMakeToolchain(self).generate()
        CMakeDeps(self).generate()

    def build(self):
        cmake = CMake(self)
        cmake.configure()
        cmake.build()

    def package(self):
        CMake(self).install()
        copy(
            self,
            "LICENSE",
            src=self.source_folder,
            dst=os.path.join(self.package_folder, "licenses"),
        )

    def package_info(self):
        self.cpp_info.set_property("cmake_file_name", "otel-capi")
        self.cpp_info.set_property("cmake_target_name", "otel-capi::otel-capi")
        self.cpp_info.set_property("pkg_config_name", "otel-capi")
        self.cpp_info.includedirs = ["include"]
        self.cpp_info.libs = ["otel_capi"]
        self.cpp_info.builddirs.append(os.path.join("lib", "cmake", "otel-capi"))
