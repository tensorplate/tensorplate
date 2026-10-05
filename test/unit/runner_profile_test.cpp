// SPDX-License-Identifier: Apache-2.0
//
// The sidecar launcher's reading of installed runner profiles, and the
// launch a deployment that selects one gets.

#include <gtest/gtest.h>

#if !defined(TP_ENABLE_PYTHON_PYTORCH_SIDECAR) || !TP_ENABLE_PYTHON_PYTORCH_SIDECAR
TEST(RunnerProfile, FeatureFlagDisabled) {
  GTEST_SKIP() << "TP_ENABLE_PYTHON_PYTORCH_SIDECAR=OFF";
}
#else

#include <sys/stat.h>
#include <sys/statvfs.h>
#include <unistd.h>

#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <memory>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/model_spec.hpp"

#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "adapters/python_pytorch/runner_profile.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"

namespace tensorplate::adapters::python_pytorch {
namespace {

namespace fs = std::filesystem;
using json = nlohmann::json;

const fs::path kSource{TP_SOURCE_DIR};
const char* const kPackagedDescriptor = "packaging/backend-metadata/python_pytorch.json";
const char* const kPackagedRoot = "/usr/lib/tensorplate/speech-runtime";
const char* const kFixtures = "protocol/rust/tests/fixtures/runner_profile_declarations";

std::string read_file(const fs::path& path) {
  std::ifstream in(path, std::ios::binary);
  EXPECT_TRUE(in.is_open()) << path;
  return {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
}

void write_file(const fs::path& path, const std::string& text) {
  fs::create_directories(path.parent_path());
  std::ofstream(path, std::ios::binary | std::ios::trunc) << text;
}

json read_json(const fs::path& path) {
  return json::parse(read_file(path));
}

std::string replace_all(std::string text, const std::string& from, const std::string& to) {
  for (std::size_t at = text.find(from); at != std::string::npos;
       at = text.find(from, at + to.size())) {
    text.replace(at, from.size(), to);
  }
  return text;
}

// A backend descriptor directory as a package install leaves it, removed
// with the test.
class InstalledTree {
 public:
  InstalledTree()
      : root_(fs::temp_directory_path() /
              ("tp-runner-profile-" + std::to_string(::getpid()) + "-" +
               ::testing::UnitTest::GetInstance()->current_test_info()->name())) {
    fs::remove_all(root_);
    write_file(descriptor(), read_file(kSource / kPackagedDescriptor));
  }
  InstalledTree(const InstalledTree&) = delete;
  InstalledTree& operator=(const InstalledTree&) = delete;
  ~InstalledTree() {
    std::error_code ignored;
    fs::remove_all(root_, ignored);
  }

  [[nodiscard]] const fs::path& root() const { return root_; }
  [[nodiscard]] fs::path descriptor() const {
    return root_ / "backends" / "python_pytorch" / "backend.json";
  }
  [[nodiscard]] fs::path declarations() const {
    return descriptor().parent_path() / "runner_profiles.d";
  }
  [[nodiscard]] fs::path environment_root() const { return root_ / "speech-runtime"; }

  void declare(const std::string& file_name, const std::string& text) const {
    write_file(declarations() / file_name, text);
  }

  // The packaged declaration of `profile`, with its environment under this
  // tree and an interpreter that exists.
  void install(const std::string& profile) const {
    const auto packaged =
        read_file(kSource / "packaging/backend-metadata/runner_profiles" / (profile + ".json"));
    declare(profile + ".json", replace_all(packaged, kPackagedRoot, environment_root().string()));
    const auto interpreter = environment_root() / "bin" / "python";
    write_file(interpreter, "#!/bin/sh\n");
    fs::permissions(interpreter, fs::perms::owner_all);
  }

 private:
  fs::path root_;
};

std::vector<std::string> ids_of(const std::vector<InstalledRunnerProfile>& profiles) {
  std::vector<std::string> ids;
  for (const auto& profile : profiles) {
    ids.push_back(profile.id);
  }
  return ids;
}

// Every install combination the Rust reader's fixture lists. A refusal for
// a package that is not installed is the one the launcher does not make.
TEST(RunnerProfileReader, MakesOfEachInstallCombinationWhatItsCaseSays) {
  const json cases = read_json(kSource / kFixtures / "cases.json")["cases"];
  int merged = 0;
  int refused = 0;
  int left_to_the_package_check = 0;
  for (const auto& c : cases) {
    SCOPED_TRACE(c["name"].get<std::string>());
    const InstalledTree tree;
    std::size_t declared = 0;
    if (c["declarations"].is_object()) {
      fs::create_directories(tree.declarations());
      for (const auto& [file_name, source] : c["declarations"].items()) {
        tree.declare(file_name, read_file(kSource / source.get<std::string>()));
        declared += fs::path(file_name).extension() == ".json" ? 1 : 0;
      }
    }

    const auto profiles = read_installed_runner_profiles(tree.descriptor());

    if (c.contains("runner_profiles")) {
      ASSERT_TRUE(profiles.has_value()) << profiles.error().message;
      EXPECT_EQ(ids_of(profiles.value()), c["runner_profiles"].get<std::vector<std::string>>());
      ++merged;
    } else if (c["refusal"]["variant"] == "PackageNotInstalled") {
      ASSERT_TRUE(profiles.has_value()) << profiles.error().message;
      EXPECT_EQ(profiles.value().size(), declared);
      ++left_to_the_package_check;
    } else {
      ASSERT_FALSE(profiles.has_value());
      EXPECT_EQ(profiles.error().code, Error::Code::ConfigInvalid);
      ++refused;
    }
  }
  EXPECT_GE(merged, 6);
  EXPECT_GE(refused, 2);
  EXPECT_GE(left_to_the_package_check, 3);
}

// One declaration beside the packaged descriptor, accepted or refused: the
// same documents the Rust reader's test runs.
TEST(RunnerProfileReader, AgreesWithTheRustReaderOnEveryDeclarationRule) {
  const json cases = read_json(kSource / kFixtures / "rule_cases.json")["cases"];
  int accepted = 0;
  int refused = 0;
  for (const auto& c : cases) {
    SCOPED_TRACE(c["name"].get<std::string>());
    const InstalledTree tree;
    tree.declare("case.json",
                 c.contains("text") ? c["text"].get<std::string>() : c["declaration"].dump());

    const auto profiles = read_installed_runner_profiles(tree.descriptor());

    EXPECT_EQ(profiles.has_value(), c["accepted"].get<bool>());
    if (!profiles.has_value()) {
      EXPECT_EQ(profiles.error().code, Error::Code::ConfigInvalid);
    }
    ++(c["accepted"].get<bool>() ? accepted : refused);
  }
  // The Rust reader's test pins the same two counts.
  EXPECT_EQ(accepted, 10);
  EXPECT_EQ(refused, 55);
}

// The Rust reader accepts a NUL inside a path; here it would end the path
// early where the launch hands it to the system.
TEST(RunnerProfileReader, RefusesANulInsideAPath) {
  const json base = read_json(kSource / kFixtures / "rule_cases.json")["cases"][0]["declaration"];
  for (const char* member : {"environment_root", "interpreter", "library_search_paths"}) {
    SCOPED_TRACE(member);
    json declaration = base;
    auto& profile = declaration["runner_profile"];
    const std::string root = profile["environment_root"].get<std::string>();
    if (std::string{member} == "environment_root") {
      profile["environment_root"] = root + std::string{"\0", 1};
      profile["interpreter"] = root + std::string{"\0/bin/python", 12};
      profile.erase("library_search_paths");
    } else if (std::string{member} == "interpreter") {
      profile["interpreter"] = root + "/bin/python" + std::string{"\0", 1} + "3";
    } else {
      profile["library_search_paths"] = {root + "/lib" + std::string{"\0", 1} + "64"};
    }
    const InstalledTree tree;
    tree.declare("case.json", declaration.dump());

    const auto profiles = read_installed_runner_profiles(tree.descriptor());

    ASSERT_FALSE(profiles.has_value());
    EXPECT_EQ(profiles.error().code, Error::Code::ConfigInvalid);
  }
}

// Written out of order: a directory lists its entries in no promised order.
TEST(RunnerProfileReader, MergesDeclarationsInFileNameOrder) {
  const InstalledTree tree;
  const std::string second = read_file(kSource / kFixtures / "second_faster_whisper.json");
  std::vector<std::string> expected;
  for (const int n : {5, 2, 7, 0, 3, 6, 1, 4}) {
    const std::string id = "profile_" + std::to_string(n);
    tree.declare(id + ".json", replace_all(second, "\"faster_whisper\"", "\"" + id + "\""));
  }
  for (int n = 0; n < 8; ++n) {
    expected.push_back("profile_" + std::to_string(n));
  }

  const auto profiles = read_installed_runner_profiles(tree.descriptor());

  ASSERT_TRUE(profiles.has_value()) << profiles.error().message;
  EXPECT_EQ(ids_of(profiles.value()), expected);
}

TEST(RunnerProfileReader, ReturnsTheDeclaredEnvironmentOfEachProfile) {
  const InstalledTree tree;
  for (const char* id : {"faster_whisper", "kokoro"}) {
    SCOPED_TRACE(id);
    const auto source =
        kSource / "packaging/backend-metadata/runner_profiles" / (std::string{id} + ".json");
    tree.declare(std::string{id} + ".json", read_file(source));
    const json declared = read_json(source)["runner_profile"];

    const auto environment = read_runner_environment(tree.descriptor(), id);

    ASSERT_TRUE(environment.has_value()) << environment.error().message;
    EXPECT_EQ(environment.value().interpreter, declared["interpreter"]);
    EXPECT_EQ(environment.value().environment_root, declared["environment_root"]);
    EXPECT_EQ(environment.value().library_search_paths,
              declared.value("library_search_paths", std::vector<std::string>{}));
    EXPECT_NE(environment.value().interpreter, environment.value().environment_root);
  }
}

TEST(RunnerProfileReader, ReadsTheDescriptorsOwnProfilesBeforeTheDeclared) {
  const InstalledTree tree;
  write_file(
      tree.descriptor(),
      read_file(kSource / "protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json"));
  tree.declare("parakeet.json",
               replace_all(read_file(kSource / kFixtures / "second_faster_whisper.json"),
                           "\"faster_whisper\"", "\"parakeet\""));

  const auto profiles = read_installed_runner_profiles(tree.descriptor());

  ASSERT_TRUE(profiles.has_value()) << profiles.error().message;
  EXPECT_EQ(ids_of(profiles.value()),
            (std::vector<std::string>{"faster_whisper", "kokoro", "parakeet"}));
}

TEST(RunnerProfileReader, RefusesAProfileTheDescriptorAndADeclarationBothName) {
  const InstalledTree tree;
  write_file(
      tree.descriptor(),
      read_file(kSource / "protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json"));
  tree.declare("second.json", read_file(kSource / kFixtures / "second_faster_whisper.json"));

  const auto profiles = read_installed_runner_profiles(tree.descriptor());

  ASSERT_FALSE(profiles.has_value());
  EXPECT_EQ(profiles.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(profiles.error().message, "runner profile is declared more than once");
}

TEST(RunnerProfileReader, RefusesADescriptorItCannotReadOrParse) {
  const InstalledTree tree;
  const std::vector<std::pair<std::string, std::string>> cases = {
      {"{", "backend descriptor is malformed"},
      {"[]", "backend descriptor is malformed"},
      {R"({"schema_version": "0.1"})", "backend descriptor is malformed"},
      {R"({"backend_name": "  "})", "backend descriptor is malformed"},
      {R"({"schema_version": "9.9", "backend_name": "python_pytorch"})",
       "backend descriptor is malformed"},
      {R"({"backend_name": "python_pytorch", "runner_profiles": {}})",
       "backend descriptor is malformed"},
      {R"({"backend_name": "python_pytorch", "runner_profiles": [{"id": "kokoro"}]})",
       "backend descriptor breaks a runner profile rule"},
  };
  for (const auto& [text, message] : cases) {
    SCOPED_TRACE(text);
    write_file(tree.descriptor(), text);
    const auto profiles = read_installed_runner_profiles(tree.descriptor());
    ASSERT_FALSE(profiles.has_value());
    EXPECT_EQ(profiles.error().code, Error::Code::ConfigInvalid);
    EXPECT_EQ(profiles.error().message, message);
  }

  const auto missing = read_installed_runner_profiles(tree.root() / "absent" / "backend.json");
  ASSERT_FALSE(missing.has_value());
  EXPECT_EQ(missing.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(missing.error().message, "backend descriptor could not be read");

  const std::string packaged = read_file(kSource / kPackagedDescriptor);
  write_file(tree.descriptor(), "\xEF\xBB\xBF" + packaged);
  const auto marked = read_installed_runner_profiles(tree.descriptor());
  ASSERT_FALSE(marked.has_value());
  EXPECT_EQ(marked.error().message, "backend descriptor is malformed");

  fs::remove(tree.descriptor());
  fs::create_directories(tree.descriptor());
  const auto directory = read_installed_runner_profiles(tree.descriptor());
  ASSERT_FALSE(directory.has_value());
  EXPECT_EQ(directory.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(directory.error().message, "backend descriptor could not be read");
}

TEST(RunnerProfileReader, RefusesAnIdTheDescriptorItselfListsTwice) {
  const InstalledTree tree;
  json descriptor =
      read_json(kSource / "protocol/rust/tests/fixtures/backend_descriptor_runner_profiles.json");
  descriptor["runner_profiles"].push_back(descriptor["runner_profiles"][0]);
  write_file(tree.descriptor(), descriptor.dump());

  const auto profiles = read_installed_runner_profiles(tree.descriptor());

  ASSERT_FALSE(profiles.has_value());
  EXPECT_EQ(profiles.error().code, Error::Code::ConfigInvalid);
  EXPECT_EQ(profiles.error().message, "runner profile is declared more than once");
}

// An entry named like a declaration that cannot be read refuses the list;
// it is never skipped, and never an exception.
TEST(RunnerProfileReader, RefusesADeclarationItCannotRead) {
  const std::string valid = read_file(kSource / kFixtures / "second_faster_whisper.json");
  {
    const InstalledTree tree;
    tree.declare("a.json", valid);
    fs::create_directories(tree.declarations() / "x.json");
    const auto profiles = read_installed_runner_profiles(tree.descriptor());
    ASSERT_FALSE(profiles.has_value()) << "a directory";
    EXPECT_EQ(profiles.error().code, Error::Code::ConfigInvalid);
    EXPECT_EQ(profiles.error().message, "runner profile declaration could not be read");
  }
  {
    const InstalledTree tree;
    tree.declare("a.json", valid);
    fs::create_symlink(tree.root() / "absent.json", tree.declarations() / "x.json");
    const auto profiles = read_installed_runner_profiles(tree.descriptor());
    ASSERT_FALSE(profiles.has_value()) << "a dangling link";
    EXPECT_EQ(profiles.error().message, "runner profile declaration could not be read");
  }
  {
    const InstalledTree tree;
    tree.declare("a.json", valid);
    tree.declare("x.json", "");
    const auto profiles = read_installed_runner_profiles(tree.descriptor());
    ASSERT_FALSE(profiles.has_value()) << "an empty file";
    EXPECT_EQ(profiles.error().message, "runner profile declaration is malformed");
  }
}

TEST(RunnerProfileReader, RefusesADeclarationDirectoryThatIsNotADirectory) {
  const InstalledTree tree;
  write_file(tree.declarations(), "{}");

  const auto profiles = read_installed_runner_profiles(tree.descriptor());

  ASSERT_FALSE(profiles.has_value());
  EXPECT_EQ(profiles.error().message, "runner profile declarations could not be listed");
}

// The message names neither the profile nor a path.
TEST(RunnerProfileReader, AProfileNoPackageDeclaresIsUnsupported) {
  const InstalledTree tree;
  tree.install("faster_whisper");

  const auto environment = read_runner_environment(tree.descriptor(), "kokoro");

  ASSERT_FALSE(environment.has_value());
  EXPECT_EQ(environment.error().code, Error::Code::Unsupported);
  EXPECT_EQ(environment.error().message, "runner profile is not installed");
  EXPECT_FALSE(environment.error().context.has_value());
}

class ScopedEnv {
 public:
  ScopedEnv(const char* name, const char* value) : name_(name) {
    if (const char* old = std::getenv(name); old != nullptr) {
      saved_ = old;
    }
    if (value != nullptr) {
      ::setenv(name, value, 1);
    } else {
      ::unsetenv(name);
    }
  }
  ScopedEnv(const ScopedEnv&) = delete;
  ScopedEnv& operator=(const ScopedEnv&) = delete;
  ~ScopedEnv() {
    if (saved_.has_value()) {
      ::setenv(name_, saved_->c_str(), 1);
    } else {
      ::unsetenv(name_);
    }
  }

 private:
  const char* name_;
  std::optional<std::string> saved_;
};

TEST(RunnerProfileReader, InstalledDescriptorFollowsThePackageChannelsDirectory) {
  {
    const ScopedEnv unset{"TP_BACKEND_DESCRIPTOR_DIR", nullptr};
    EXPECT_EQ(installed_descriptor_path().value(),
              "/usr/share/tensorplate/backends/python_pytorch/backend.json");
  }
  {
    const ScopedEnv empty{"TP_BACKEND_DESCRIPTOR_DIR", ""};
    EXPECT_EQ(installed_descriptor_path().value(),
              "/usr/share/tensorplate/backends/python_pytorch/backend.json");
  }
  {
    const ScopedEnv set{"TP_BACKEND_DESCRIPTOR_DIR", "/opt/channel/share/backends"};
    EXPECT_EQ(installed_descriptor_path().value(),
              "/opt/channel/share/backends/python_pytorch/backend.json");
  }
  {
    const ScopedEnv relative{"TP_BACKEND_DESCRIPTOR_DIR", "share/backends"};
    const auto path = installed_descriptor_path();
    ASSERT_FALSE(path.has_value());
    EXPECT_EQ(path.error().code, Error::Code::ConfigInvalid);
  }
}

RunnerEnvironment installed_environment(const InstalledTree& tree, const char* id) {
  tree.install(id);
  return read_runner_environment(tree.descriptor(), id).value();
}

TEST(RunnerLaunch, GivesTheSidecarTheProfilesInterpreterAndExactlyItsEnvironment) {
  const InstalledTree tree;
  auto environment = installed_environment(tree, "faster_whisper");
  const auto first = environment.library_search_paths.at(0);
  environment.library_search_paths.push_back(environment.environment_root + "/lib/second");

  const auto request = runner_launch_request(environment, tree.root());

  ASSERT_TRUE(request.has_value()) << request.error().message;
  EXPECT_EQ(request.value().python_exe, tree.environment_root() / "bin" / "python");
  EXPECT_EQ(request.value().environment,
            (std::vector<std::string>{
                "LD_LIBRARY_PATH=" + first + ":" + environment.environment_root + "/lib/second",
                "ORT_DISABLE_TELEMETRY=1",
                "TMPDIR=" + tree.root().string(),
            }));
  EXPECT_TRUE(request.value().extra_args.empty());
  EXPECT_TRUE(request.value().socket_path.empty());
}

// An empty value, so a search path the worker inherited does not reach it.
TEST(RunnerLaunch, AProfileWithoutSearchPathsClearsTheInheritedOne) {
  const InstalledTree tree;
  const auto environment = installed_environment(tree, "kokoro");
  ASSERT_TRUE(environment.library_search_paths.empty());

  const auto request = runner_launch_request(environment, tree.root());

  ASSERT_TRUE(request.has_value()) << request.error().message;
  EXPECT_EQ(request.value().environment.at(0), "LD_LIBRARY_PATH=");
}

TEST(RunnerLaunch, RefusesAnInterpreterThatIsNotAnExecutableFile) {
  const InstalledTree tree;
  const auto environment = installed_environment(tree, "kokoro");
  const fs::path interpreter{environment.interpreter};
  const auto refused = [&] {
    const auto request = runner_launch_request(environment, tree.root());
    return !request.has_value() && request.error().code == Error::Code::Unavailable &&
           request.error().message == "runner profile interpreter is not an executable file";
  };
  ASSERT_TRUE(runner_launch_request(environment, tree.root()).has_value());

  fs::permissions(interpreter, fs::perms::owner_read | fs::perms::owner_write);
  EXPECT_TRUE(refused()) << "not executable";

  fs::remove(interpreter);
  EXPECT_TRUE(refused()) << "absent";

  fs::create_directories(interpreter);
  EXPECT_TRUE(refused()) << "a directory";
}

TEST(RunnerLaunch, RefusesASearchPathTheLoaderWouldSplit) {
  const InstalledTree tree;
  for (const char* separator : {":", ";"}) {
    SCOPED_TRACE(separator);
    auto environment = installed_environment(tree, "faster_whisper");
    environment.library_search_paths = {environment.environment_root + "/lib" + separator + "x"};

    const auto request = runner_launch_request(environment, tree.root());

    ASSERT_FALSE(request.has_value());
    EXPECT_EQ(request.error().code, Error::Code::ConfigInvalid);
  }
}

const char* const kUnusableTempDir =
    "sidecar temporary directory is not writable or does not allow execution";

TEST(RunnerLaunch, RefusesATemporaryDirectoryThatIsAbsentOrAFile) {
  const InstalledTree tree;
  const auto environment = installed_environment(tree, "kokoro");
  // Writable and executable, so that only its not being a directory refuses it.
  write_file(tree.root() / "file", "");
  fs::permissions(tree.root() / "file", fs::perms::owner_all);
  for (const fs::path& temp_dir : {tree.root() / "absent", tree.root() / "file", fs::path{}}) {
    SCOPED_TRACE(temp_dir.string());
    const auto request = runner_launch_request(environment, temp_dir);
    ASSERT_FALSE(request.has_value());
    EXPECT_EQ(request.error().code, Error::Code::Unavailable);
    EXPECT_EQ(request.error().message, kUnusableTempDir);
  }
}

TEST(RunnerLaunch, RefusesATemporaryDirectoryItCannotWriteTo) {
  if (::geteuid() == 0) {
    GTEST_SKIP() << "root can write to any directory";
  }
  const InstalledTree tree;
  const auto environment = installed_environment(tree, "kokoro");
  const auto temp_dir = tree.root() / "read-only";
  fs::create_directories(temp_dir);
  fs::permissions(temp_dir, fs::perms::owner_read | fs::perms::owner_exec);

  const auto request = runner_launch_request(environment, temp_dir);

  ASSERT_FALSE(request.has_value());
  EXPECT_EQ(request.error().message, kUnusableTempDir);
}

// Writable, but nothing in it can be opened by name.
TEST(RunnerLaunch, RefusesATemporaryDirectoryItCannotSearch) {
  if (::geteuid() == 0) {
    GTEST_SKIP() << "root can search any directory";
  }
  const InstalledTree tree;
  const auto environment = installed_environment(tree, "kokoro");
  const auto temp_dir = tree.root() / "write-only";
  fs::create_directories(temp_dir);
  fs::permissions(temp_dir, fs::perms::owner_write);

  const auto request = runner_launch_request(environment, temp_dir);

  ASSERT_FALSE(request.has_value());
  EXPECT_EQ(request.error().message, kUnusableTempDir);
}

#ifdef ST_NOEXEC
// A directory this user can write to on a filesystem mounted noexec, if the
// machine has one.
std::optional<fs::path> writable_noexec_directory() {
  for (const fs::path& candidate : {fs::path{"/dev/shm"}, fs::path{"/run/lock"},
                                    fs::path{"/run/user"} / std::to_string(::getuid())}) {
    struct statvfs mount {};
    if (::access(candidate.c_str(), W_OK | X_OK) == 0 &&
        ::statvfs(candidate.c_str(), &mount) == 0 && (mount.f_flag & ST_NOEXEC) != 0) {
      return candidate;
    }
  }
  return std::nullopt;
}

TEST(RunnerLaunch, RefusesATemporaryDirectoryOnAFilesystemMountedNoexec) {
  const auto temp_dir = writable_noexec_directory();
  if (!temp_dir.has_value()) {
    GTEST_SKIP() << "no writable noexec filesystem on this machine";
  }
  const InstalledTree tree;
  const auto environment = installed_environment(tree, "kokoro");

  const auto request = runner_launch_request(environment, *temp_dir);

  ASSERT_FALSE(request.has_value());
  EXPECT_EQ(request.error().code, Error::Code::Unavailable);
  EXPECT_EQ(request.error().message, kUnusableTempDir);
}
#endif

// What the session hands the launcher, without starting a process.
struct Launches {
  std::vector<SidecarLaunchRequest> requests;

  SidecarLauncher launcher() {
    return [this](const SidecarLaunchRequest& request) -> Result<SidecarHandle> {
      requests.push_back(request);
      return unexpected(Error::make(Error::Code::Unavailable, "launch recorded"));
    };
  }
};

ModelSpec spec_selecting(std::optional<std::string> runner_profile) {
  return ModelSpec::create("m", ModelClass::Speech, "/bundle/entry.json", "python_pytorch",
                           PrecisionHint::Auto, std::nullopt, std::move(runner_profile))
      .value();
}

const char* const kConfiguredInterpreter = "/opt/from-the-environment/python";

std::unique_ptr<ExecutionSession> session_over(const fs::path& descriptor, Launches& launches) {
  PythonPytorchConfig config;
  config.python_exe = kConfiguredInterpreter;
  config.descriptor_path = descriptor;
  return make_python_pytorch_session(ExecutionSessionRuntimeHooks{}, std::move(config),
                                     launches.launcher());
}

TEST(PythonPytorchLaunch, ARunnerProfileIsStartedWithTheDescriptorsInterpreter) {
  const InstalledTree tree;
  tree.install("faster_whisper");
  tree.install("kokoro");
  // Short: the session binds its socket under it, within the socket path limit.
  const fs::path temp_dir = fs::temp_directory_path() / ("tp-tmp-" + std::to_string(::getpid()));
  fs::create_directories(temp_dir);
  const struct Removed {
    const fs::path& path;
    ~Removed() {
      std::error_code ignored;
      fs::remove_all(path, ignored);
    }
  } removed{temp_dir};
  const ScopedEnv worker_tmp{"TMPDIR", temp_dir.c_str()};
  Launches launches;
  auto session = session_over(tree.descriptor(), launches);

  const auto loaded = session->load(spec_selecting("faster_whisper"));

  ASSERT_FALSE(loaded.has_value());
  EXPECT_EQ(loaded.error().message, "launch recorded");
  ASSERT_EQ(launches.requests.size(), 1U);
  const auto& request = launches.requests.front();
  EXPECT_EQ(request.python_exe, tree.environment_root() / "bin" / "python");
  EXPECT_NE(request.python_exe, kConfiguredInterpreter);
  EXPECT_EQ(request.environment, (std::vector<std::string>{
                                     "LD_LIBRARY_PATH=" + tree.environment_root().string() +
                                         "/lib/python3.12/site-packages/nvidia/cublas/lib",
                                     "ORT_DISABLE_TELEMETRY=1",
                                     "TMPDIR=" + temp_dir.string(),
                                 }));
}

TEST(PythonPytorchLaunch, AModelThatSelectsNoProfileKeepsTheConfiguredInterpreter) {
  const InstalledTree tree;
  Launches launches;
  auto session = session_over(tree.root() / "absent" / "backend.json", launches);

  const auto loaded = session->load(spec_selecting(std::nullopt));

  ASSERT_FALSE(loaded.has_value());
  ASSERT_EQ(launches.requests.size(), 1U);
  EXPECT_EQ(launches.requests.front().python_exe, kConfiguredInterpreter);
  EXPECT_TRUE(launches.requests.front().environment.empty());
}

TEST(PythonPytorchLaunch, AProfileThatCannotBeResolvedNeverReachesTheLauncher) {
  const InstalledTree tree;
  tree.install("faster_whisper");
  Launches launches;

  auto not_installed = session_over(tree.descriptor(), launches);
  const auto unsupported = not_installed->load(spec_selecting("kokoro"));
  ASSERT_FALSE(unsupported.has_value());
  EXPECT_EQ(unsupported.error().code, Error::Code::Unsupported);

  auto no_descriptor = session_over(tree.root() / "absent" / "backend.json", launches);
  const auto unreadable = no_descriptor->load(spec_selecting("faster_whisper"));
  ASSERT_FALSE(unreadable.has_value());
  EXPECT_EQ(unreadable.error().code, Error::Code::ConfigInvalid);

  fs::remove(tree.environment_root() / "bin" / "python");
  auto no_interpreter = session_over(tree.descriptor(), launches);
  const auto unavailable = no_interpreter->load(spec_selecting("faster_whisper"));
  ASSERT_FALSE(unavailable.has_value());
  EXPECT_EQ(unavailable.error().code, Error::Code::Unavailable);

  EXPECT_TRUE(launches.requests.empty());
}

}  // namespace
}  // namespace tensorplate::adapters::python_pytorch

#endif  // TP_ENABLE_PYTHON_PYTORCH_SIDECAR
