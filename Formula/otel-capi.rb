class OtelCapi < Formula
  desc "C API for OpenTelemetry"
  homepage "https://github.com/daniel-white/otel_capi"
  version "0.1.0"
  license "Apache-2.0"

  url "https://github.com/daniel-white/otel_capi.git", tag: "v#{version}"

  depends_on "cmake" => :build
  depends_on "rust" => :build

  def install
    system "cmake", "-S", ".", "-B", "build/cmake", "-DCMAKE_INSTALL_PREFIX=#{prefix}"
    system "cmake", "--build", "build/cmake"
    system "cmake", "--install", "build/cmake"
  end

  test do
    (testpath / "test.c").write <<~C
      #include <otel_capi.h>
      int main(void) { return 0; }
    C
    system ENV.cc, "test.c", "-I#{include}", "-L#{lib}", "-lotel_capi", "-o", "test"
    system "./test"
  end
end
