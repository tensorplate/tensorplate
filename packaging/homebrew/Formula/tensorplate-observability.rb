# SPDX-License-Identifier: Apache-2.0
class TensorplateObservability < Formula
  desc "Independent health monitor for TensorPlate"
  homepage "https://github.com/tensorplate/tensorplate"
  url "https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.0.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "Apache-2.0"

  depends_on "rust" => :build
  depends_on arch: :arm64
  depends_on macos: :tahoe

  def install
    system "cargo", "install", *std_cargo_args(path: "observability")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tensorplate-observability --version")
  end
end
