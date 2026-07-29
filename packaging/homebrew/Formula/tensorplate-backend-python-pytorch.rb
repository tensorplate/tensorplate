# SPDX-License-Identifier: Apache-2.0
class TensorplateBackendPythonPytorch < Formula
  desc "Python and PyTorch sidecar backend for TensorPlate"
  homepage "https://github.com/tensorplate/tensorplate"
  url "https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.0.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "Apache-2.0"

  depends_on arch: :arm64
  depends_on :macos
  depends_on "pytorch"

  def install
    libexec.install "backends/python_pytorch/src/tensorplate_pytorch_backend"

    pytorch_python = formula_opt_libexec("pytorch")/"bin/python"
    launcher = libexec/"bin/tensorplate-backend-python-pytorch"
    launcher.dirname.mkpath
    launcher.write <<~SH
      #!/bin/sh
      export PYTHONPATH="#{libexec}${PYTHONPATH:+:${PYTHONPATH}}"
      exec "#{pytorch_python}" -m tensorplate_pytorch_backend "$@"
    SH
    launcher.chmod 0755
    bin.install_symlink launcher
  end

  test do
    assert_match "usage:", shell_output("#{bin}/tensorplate-backend-python-pytorch --help")
    system formula_opt_libexec("pytorch")/"bin/python", "-c",
           "import torch; assert torch.backends.mps.is_built()"
  end
end
