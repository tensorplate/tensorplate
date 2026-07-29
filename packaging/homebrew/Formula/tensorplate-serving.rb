# SPDX-License-Identifier: Apache-2.0
class TensorplateServing < Formula
  desc "Serving worker for TensorPlate"
  homepage "https://github.com/tensorplate/tensorplate"
  url "https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.0.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "Apache-2.0"

  depends_on "cmake" => :build
  depends_on "ninja" => :build
  depends_on arch: :arm64
  depends_on macos: :tahoe
  depends_on "nlohmann-json"

  def install
    system "cmake", "-S", ".", "-B", "build-homebrew", "-G", "Ninja",
           *std_cmake_args,
           "-DTP_BUILD_TESTS=OFF",
           "-DTP_BUILD_EXAMPLES=OFF",
           "-DTP_ENABLE_TENSORRT=OFF",
           "-DTP_ENABLE_LIBTORCH=OFF"
    system "cmake", "--build", "build-homebrew", "--target", "tp_serving_worker"
    libexec.install "build-homebrew/serving_worker/tensorplate-serving"
  end

  test do
    assert_match version.to_s, shell_output("#{libexec}/tensorplate-serving --version")
  end
end
