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

    config = buildpath/"packaging/homebrew/conf/agent.json.in"
    inreplace config, "@HOMEBREW_PREFIX@", HOMEBREW_PREFIX
    (etc/"tensorplate").install config => "agent.json"
    (share/"tensorplate/platform").install \
      "config/platform/rows",
      "config/platform/roadmap_targets"
  end

  def post_install
    [
      etc/"tensorplate",
      var/"tensorplate",
      var/"tensorplate/state",
      var/"tensorplate/bundles",
      var/"tensorplate/bundles/staging",
      var/"tensorplate/worker-configs",
      var/"log/tensorplate",
    ].each { |path| secure_directory(path, 0750) }
    secure_directory(var/"run/tensorplate", 0700)
    secure_file(etc/"tensorplate/agent.json", 0640)
    secure_log(var/"log/tensorplate/agent.log")
    secure_log(var/"log/tensorplate/agent.error.log")
  end

  service do
    run [opt_bin/"tensorplate-agent", "--config", etc/"tensorplate/agent.json"]
    environment_variables PATH:                         std_service_path_env,
                          PYTHONPATH:                   formula_opt_libexec("tensorplate-backend-python-pytorch"),
                          TP_BACKEND_DESCRIPTOR_DIR:    HOMEBREW_PREFIX/"share/tensorplate/backends",
                          TP_PLATFORM_REGISTRY_DIR:     HOMEBREW_PREFIX/"share/tensorplate/platform",
                          TP_PYTHON_PYTORCH_EXECUTABLE: formula_opt_libexec("pytorch")/"bin/python"
    working_dir var/"tensorplate"
    log_path var/"log/tensorplate/agent.log"
    error_log_path var/"log/tensorplate/agent.error.log"
    run_at_load true
    keep_alive successful_exit: false
    throttle_interval 5
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tensorplate-agent --version")
    assert_predicate etc/"tensorplate/agent.json", :file?
    assert_equal 0640, (etc/"tensorplate/agent.json").stat.mode & 0777
    assert_equal 0700, (var/"run/tensorplate").stat.mode & 0777
    assert_match "#{HOMEBREW_PREFIX}/var/run/tensorplate/agent.sock",
                 (etc/"tensorplate/agent.json").read
    assert_predicate share/"tensorplate/platform/rows/macos26-m1pro-16gb.json", :file?
    assert_predicate share/"tensorplate/platform/roadmap_targets/pkg-macos-notarized.json", :file?
  end

  private

  def secure_directory(path, mode)
    odie "TensorPlate path #{path} must not be a symlink" if path.symlink?

    path.mkpath
    path.chmod mode
    actual = path.stat.mode & 0777
    return if path.directory? && actual == mode

    odie "TensorPlate requires directory #{path} with mode #{format("%04o", mode)}; found #{format("%04o", actual)}"
  rescue SystemCallError => e
    odie "TensorPlate could not secure directory #{path}: #{e.message}"
  end

  def secure_file(path, mode)
    odie "TensorPlate path #{path} must be a regular file" unless path.file?
    odie "TensorPlate path #{path} must not be a symlink" if path.symlink?

    path.chmod mode
    actual = path.stat.mode & 0777
    return if actual == mode

    odie "TensorPlate requires file #{path} with mode #{format("%04o", mode)}; found #{format("%04o", actual)}"
  rescue SystemCallError => e
    odie "TensorPlate could not secure file #{path}: #{e.message}"
  end

  def secure_log(path)
    path.write("") unless path.exist?
    secure_file(path, 0640)
  end
end
