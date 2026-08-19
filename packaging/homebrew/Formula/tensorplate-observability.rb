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

    config = buildpath/"packaging/homebrew/conf/observability.json.in"
    inreplace config, "@HOMEBREW_PREFIX@", HOMEBREW_PREFIX
    (etc/"tensorplate").install config => "observability.json"
  end

  def post_install
    [
      etc/"tensorplate",
      var/"tensorplate",
      var/"tensorplate/state",
      var/"log/tensorplate",
    ].each { |path| secure_directory(path) }
    secure_file(etc/"tensorplate/observability.json", 0640)
    secure_log(var/"log/tensorplate/observability.log")
    secure_log(var/"log/tensorplate/observability.error.log")
    secure_log(var/"log/tensorplate/events.ndjson")
  end

  service do
    run [opt_bin/"tensorplate-observability", "--config", etc/"tensorplate/observability.json"]
    environment_variables TP_PLATFORM_REGISTRY_DIR: HOMEBREW_PREFIX/"share/tensorplate/platform"
    working_dir var/"tensorplate"
    log_path var/"log/tensorplate/observability.log"
    error_log_path var/"log/tensorplate/observability.error.log"
    run_at_load true
    keep_alive successful_exit: false
    throttle_interval 5
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/tensorplate-observability --version")
    assert_predicate etc/"tensorplate/observability.json", :file?
    assert_equal 0640, (etc/"tensorplate/observability.json").stat.mode & 0777
    assert_equal 0640, (var/"log/tensorplate/events.ndjson").stat.mode & 0777
    assert_match "#{HOMEBREW_PREFIX}/var/log/tensorplate/events.ndjson",
                 (etc/"tensorplate/observability.json").read
  end

  private

  def secure_directory(path)
    odie "TensorPlate path #{path} must not be a symlink" if path.symlink?

    path.mkpath
    path.chmod 0750
    actual = path.stat.mode & 0777
    return if path.directory? && actual == 0750

    odie "TensorPlate requires directory #{path} with mode 0750; found #{format("%04o", actual)}"
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
