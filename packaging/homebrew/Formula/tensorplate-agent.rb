# SPDX-License-Identifier: Apache-2.0
class TensorplateAgent < Formula
  desc "Device agent and worker supervisor for TensorPlate"
  homepage "https://github.com/tensorplate/tensorplate"
  url "https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.0.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "Apache-2.0"

  depends_on "rust" => :build
  depends_on arch: :arm64
  depends_on macos: :tahoe
  depends_on "tensorplate-serving"

  def install
    system "cargo", "install", *std_cargo_args(path: "agent")
  end

  service do
    run [opt_bin/"tensorplate-agent", "--config", etc/"tensorplate/agent.json"]
    run_at_load true
    keep_alive successful_exit: false
    throttle_interval 5
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tensorplate-agent --version")
  end
end
