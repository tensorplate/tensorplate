# SPDX-License-Identifier: Apache-2.0
class TensorplateCli < Formula
  desc "Operator CLI for TensorPlate"
  homepage "https://github.com/tensorplate/tensorplate"
  url "https://github.com/tensorplate/tensorplate/archive/refs/tags/v0.0.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "Apache-2.0"

  depends_on "rust" => :build
  depends_on arch: :arm64
  depends_on macos: :tahoe

  # The original tensorplate formula owned this path. Allow the component
  # formula to take it over while that formula becomes the appliance meta-formula.
  link_overwrite "bin/tensorplate"

  def install
    system "cargo", "install", *std_cargo_args(path: "cli")

    config = buildpath/"packaging/homebrew/conf/cli.json.in"
    inreplace config, "@HOMEBREW_PREFIX@", HOMEBREW_PREFIX
    (etc/"tensorplate").install config => "cli.json"

    libexec.install bin/"tensorplate"
    (bin/"tensorplate").write <<~SH
      #!/bin/sh
      if [ "${TENSORPLATE_CLI_CONFIG+x}" != x ]; then
        export TENSORPLATE_CLI_CONFIG="#{etc}/tensorplate/cli.json"
      fi
      if [ "${TP_BACKEND_DESCRIPTOR_DIR+x}" != x ]; then
        export TP_BACKEND_DESCRIPTOR_DIR="#{HOMEBREW_PREFIX}/share/tensorplate/backends"
      fi
      if [ "${TP_PLATFORM_REGISTRY_DIR+x}" != x ]; then
        export TP_PLATFORM_REGISTRY_DIR="#{HOMEBREW_PREFIX}/share/tensorplate/platform"
      fi
      exec "#{libexec}/tensorplate" "$@"
    SH
    (bin/"tensorplate").chmod 0755
  end

  def post_install
    config_dir = etc/"tensorplate"
    target = config_dir
    odie "TensorPlate path #{config_dir} must not be a symlink" if config_dir.symlink?

    config_dir.mkpath
    config_dir.chmod 0750
    directory_mode = config_dir.stat.mode & 0777
    odie "TensorPlate path #{config_dir} must be a directory" unless config_dir.directory?
    if directory_mode != 0750
      odie "TensorPlate requires directory #{config_dir} with mode 0750; found #{format("%04o", directory_mode)}"
    end

    config = config_dir/"cli.json"
    target = config
    odie "TensorPlate path #{config} must be a regular file" unless config.file?
    odie "TensorPlate path #{config} must not be a symlink" if config.symlink?

    config.chmod 0644
    actual = config.stat.mode & 0777
    return if actual == 0644

    odie "TensorPlate requires file #{config} with mode 0644; found #{format("%04o", actual)}"
  rescue SystemCallError => e
    odie "TensorPlate could not secure #{target}: #{e.message}"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tensorplate version")
    assert_predicate etc/"tensorplate/cli.json", :file?
    assert_equal 0644, (etc/"tensorplate/cli.json").stat.mode & 0777
    assert_match "#{HOMEBREW_PREFIX}/var/run/tensorplate/agent.sock",
                 (etc/"tensorplate/cli.json").read
  end
end
