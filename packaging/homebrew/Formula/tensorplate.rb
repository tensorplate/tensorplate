# SPDX-License-Identifier: Apache-2.0
class Tensorplate < Formula
  desc "Complete TensorPlate appliance for macOS"
  homepage "https://github.com/tensorplate/tensorplate"
  url "https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.0.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "Apache-2.0"

  depends_on arch: :arm64
  depends_on :macos
  depends_on "tensorplate-agent"
  depends_on "tensorplate-backend-python-pytorch"
  depends_on "tensorplate-cli"
  depends_on "tensorplate-observability"
  depends_on "tensorplate-serving"

  def install
    (pkgshare/"components.txt").write <<~EOS
      tensorplate-agent #{version}
      tensorplate-serving #{version}
      tensorplate-cli #{version}
      tensorplate-observability #{version}
      tensorplate-backend-python-pytorch #{version}
    EOS
  end

  def caveats
    <<~EOS
      This formula installs the complete TensorPlate appliance. The operator
      command is provided by the tensorplate-cli component formula.
    EOS
  end

  test do
    assert_match "tensorplate-cli #{version}", (pkgshare/"components.txt").read
    assert_predicate formula_opt_bin("tensorplate-cli")/"tensorplate", :executable?
  end
end
