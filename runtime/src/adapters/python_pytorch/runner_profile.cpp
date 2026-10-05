// SPDX-License-Identifier: Apache-2.0
//
// Runner profile reader for the sidecar launcher. The rules are the ones
// `protocol/rust/src/backend_descriptor.rs` applies to the same files; the
// messages are fixed and name no path or profile, like the sidecar's own.

#include "runner_profile.hpp"

#include <sys/statvfs.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cstddef>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <nlohmann/json.hpp>
#include <optional>
#include <set>
#include <span>
#include <sstream>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include "tensorplate/core/error.hpp"

#include "python_pytorch_session.hpp"

namespace tensorplate::adapters::python_pytorch {

namespace {

using json = nlohmann::json;

constexpr std::string_view kSchemaVersion = "0.1";
constexpr const char* kDescriptorDirEnv = "TP_BACKEND_DESCRIPTOR_DIR";
constexpr std::string_view kDescriptorDir = "/usr/share/tensorplate/backends";
constexpr std::string_view kDescriptorFile = "backend.json";
constexpr std::string_view kDeclarationDir = "runner_profiles.d";

constexpr std::array<std::string_view, 8> kComputeTypes = {
    "float32",      "float16",      "bfloat16",      "int8",
    "int8_float32", "int8_float16", "int8_bfloat16", "int16",
};

Error invalid(std::string message) {
  return Error::make(Error::Code::ConfigInvalid, std::move(message));
}

// Copies through the stream buffer, which reports a failed read in the
// stream's state; reading through iterators throws on one.
std::optional<std::string> read_text(const std::filesystem::path& path) {
  std::error_code ec;
  std::ifstream in(path, std::ios::binary);
  if (!std::filesystem::is_regular_file(path, ec) || !in.is_open()) {
    return std::nullopt;
  }
  std::ostringstream out;
  out << in.rdbuf();
  if (out.fail()) {
    const bool empty = std::filesystem::is_empty(path, ec) && !ec;
    return empty ? std::optional<std::string>{std::string{}} : std::nullopt;
  }
  return std::move(out).str();
}

// What the Rust reader's JSON library refuses and this one would read: a
// byte order mark, and anything after a NUL.
json parse_document(std::string_view text) {
  if (text.starts_with("\xEF\xBB\xBF") || text.find('\0') != std::string_view::npos) {
    text = {};
  }
  return json::parse(text, nullptr, /*allow_exceptions=*/false);
}

bool is_snake_identifier(std::string_view id) {
  if (id.empty() || id.front() == '_' || id.back() == '_' ||
      id.find("__") != std::string_view::npos) {
    return false;
  }
  return std::all_of(id.begin(), id.end(), [](char c) {
    return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_';
  });
}

// The non-empty segments of an absolute path with no `.` or `..` segment,
// and no NUL, checked on the text; nullopt for any other path.
std::optional<std::vector<std::string_view>> normalized_segments(std::string_view path) {
  if (path.empty() || path.front() != '/' || path.find('\0') != std::string_view::npos) {
    return std::nullopt;
  }
  std::vector<std::string_view> segments;
  std::size_t start = 0;
  while (start < path.size()) {
    const std::size_t end = std::min(path.find('/', start), path.size());
    const std::string_view segment = path.substr(start, end - start);
    if (segment == "." || segment == "..") {
      return std::nullopt;
    }
    if (!segment.empty()) {
      segments.push_back(segment);
    }
    start = end + 1;
  }
  return segments;
}

bool starts_with(const std::vector<std::string_view>& path,
                 const std::vector<std::string_view>& root) {
  return path.size() >= root.size() && std::equal(root.begin(), root.end(), path.begin());
}

bool has_only_keys(const json& object, std::span<const std::string_view> keys) {
  for (auto it = object.begin(); it != object.end(); ++it) {
    if (std::find(keys.begin(), keys.end(), it.key()) == keys.end()) {
      return false;
    }
  }
  return true;
}

// A list of distinct strings; nullopt when `value` is anything else.
std::optional<std::vector<std::string>> distinct_strings(const json& value) {
  if (!value.is_array()) {
    return std::nullopt;
  }
  std::vector<std::string> out;
  std::set<std::string> seen;
  for (const auto& item : value) {
    if (!item.is_string() || !seen.insert(item.get<std::string>()).second) {
      return std::nullopt;
    }
    out.push_back(item.get<std::string>());
  }
  return out;
}

// Only Unicode White_Space, which is what Rust's `str::trim` removes.
bool is_blank(std::string_view text) {
  static constexpr std::array<std::string_view, 25> kWhiteSpace = {
      "\t",           "\n",           "\v",
      "\f",           "\r",           " ",
      "\xC2\x85",     "\xC2\xA0",     "\xE1\x9A\x80",
      "\xE2\x80\x80", "\xE2\x80\x81", "\xE2\x80\x82",
      "\xE2\x80\x83", "\xE2\x80\x84", "\xE2\x80\x85",
      "\xE2\x80\x86", "\xE2\x80\x87", "\xE2\x80\x88",
      "\xE2\x80\x89", "\xE2\x80\x8A", "\xE2\x80\xA8",
      "\xE2\x80\xA9", "\xE2\x80\xAF", "\xE2\x81\x9F",
      "\xE3\x80\x80",
  };
  while (!text.empty()) {
    const auto* space = std::find_if(kWhiteSpace.begin(), kWhiteSpace.end(),
                                     [&](std::string_view s) { return text.starts_with(s); });
    if (space == kWhiteSpace.end()) {
      return false;
    }
    text.remove_prefix(space->size());
  }
  return true;
}

std::optional<InstalledRunnerProfile> parse_profile(const json& entry) {
  static constexpr std::array<std::string_view, 6> kKeys = {
      "id", "interpreter", "environment_root", "library_search_paths", "packages", "compute_types",
  };
  if (!entry.is_object() || !has_only_keys(entry, kKeys)) {
    return std::nullopt;
  }
  for (const char* key : {"id", "interpreter", "environment_root"}) {
    if (!entry.contains(key) || !entry[key].is_string()) {
      return std::nullopt;
    }
  }
  InstalledRunnerProfile profile;
  profile.id = entry["id"].get<std::string>();
  profile.environment.interpreter = entry["interpreter"].get<std::string>();
  profile.environment.environment_root = entry["environment_root"].get<std::string>();
  const auto root = normalized_segments(profile.environment.environment_root);
  const auto interpreter = normalized_segments(profile.environment.interpreter);
  if (!is_snake_identifier(profile.id) || !root.has_value() || !interpreter.has_value() ||
      *interpreter == *root || !starts_with(*interpreter, *root)) {
    return std::nullopt;
  }
  if (entry.contains("library_search_paths")) {
    auto paths = distinct_strings(entry["library_search_paths"]);
    if (!paths.has_value()) {
      return std::nullopt;
    }
    std::set<std::vector<std::string_view>> seen;
    for (const auto& dir : *paths) {
      const auto segments = normalized_segments(dir);
      if (!segments.has_value() || !starts_with(*segments, *root) ||
          !seen.insert(*segments).second) {
        return std::nullopt;
      }
    }
    profile.environment.library_search_paths = std::move(*paths);
  }
  const auto packages =
      entry.contains("packages") ? distinct_strings(entry["packages"]) : std::nullopt;
  if (!packages.has_value() || packages->empty() ||
      std::any_of(packages->begin(), packages->end(), is_blank)) {
    return std::nullopt;
  }
  const auto compute_types =
      entry.contains("compute_types") ? distinct_strings(entry["compute_types"]) : std::nullopt;
  if (!compute_types.has_value() || compute_types->empty() ||
      !std::all_of(compute_types->begin(), compute_types->end(), [](const std::string& name) {
        return std::find(kComputeTypes.begin(), kComputeTypes.end(), name) != kComputeTypes.end();
      })) {
    return std::nullopt;
  }
  return profile;
}

// An absent `schema_version` passes only where the schema defaults it.
bool has_known_schema_version(const json& document, bool required) {
  if (!document.contains("schema_version")) {
    return !required;
  }
  const auto& version = document["schema_version"];
  return version.is_string() && version.get<std::string>() == kSchemaVersion;
}

std::optional<std::string> backend_name_of(const json& document) {
  if (!document.contains("backend_name") || !document["backend_name"].is_string()) {
    return std::nullopt;
  }
  auto name = document["backend_name"].get<std::string>();
  if (is_blank(name)) {
    return std::nullopt;
  }
  return name;
}

std::optional<InstalledRunnerProfile> parse_declaration(const json& document,
                                                        const std::string& backend_name) {
  static constexpr std::array<std::string_view, 4> kKeys = {
      "$schema",
      "schema_version",
      "backend_name",
      "runner_profile",
  };
  if (!document.is_object() || !has_only_keys(document, kKeys) ||
      !has_known_schema_version(document, /*required=*/true)) {
    return std::nullopt;
  }
  if (document.contains("$schema") && !document["$schema"].is_string()) {
    return std::nullopt;
  }
  if (backend_name_of(document) != backend_name || !document.contains("runner_profile")) {
    return std::nullopt;
  }
  return parse_profile(document["runner_profile"]);
}

// The `*.json` entries of `directory` in file name order; none when it does
// not exist, nullopt when it cannot be listed.
std::optional<std::vector<std::filesystem::path>> declaration_files(
    const std::filesystem::path& directory) {
  std::error_code ec;
  std::filesystem::directory_iterator it{directory, ec};
  if (ec == std::errc::no_such_file_or_directory) {
    return std::vector<std::filesystem::path>{};
  }
  if (ec) {
    return std::nullopt;
  }
  std::vector<std::filesystem::path> files;
  for (const std::filesystem::directory_iterator end; it != end; it.increment(ec)) {
    if (it->path().extension() == ".json") {
      files.push_back(it->path());
    }
  }
  if (ec) {
    return std::nullopt;
  }
  std::sort(files.begin(), files.end());
  return files;
}

// Appends the descriptor's own profiles, then the declared ones, to
// `profiles`. Returns why the list is refused, or null.
const char* list_profiles(const std::filesystem::path& descriptor_path,
                          std::vector<InstalledRunnerProfile>& profiles) {
  const auto text = read_text(descriptor_path);
  if (!text.has_value()) {
    return "backend descriptor could not be read";
  }
  const json descriptor = parse_document(*text);
  const auto backend_name =
      descriptor.is_object() ? backend_name_of(descriptor) : std::optional<std::string>{};
  if (!backend_name.has_value() || !has_known_schema_version(descriptor, /*required=*/false)) {
    return "backend descriptor is malformed";
  }

  std::set<std::string> ids;
  if (descriptor.contains("runner_profiles")) {
    const auto& own = descriptor["runner_profiles"];
    if (!own.is_array()) {
      return "backend descriptor is malformed";
    }
    for (const auto& entry : own) {
      auto profile = parse_profile(entry);
      if (!profile.has_value()) {
        return "backend descriptor breaks a runner profile rule";
      }
      if (!ids.insert(profile->id).second) {
        return "runner profile is declared more than once";
      }
      profiles.push_back(std::move(*profile));
    }
  }

  const auto files = declaration_files(descriptor_path.parent_path() / kDeclarationDir);
  if (!files.has_value()) {
    return "runner profile declarations could not be listed";
  }
  for (const auto& file : *files) {
    const auto declaration_text = read_text(file);
    if (!declaration_text.has_value()) {
      return "runner profile declaration could not be read";
    }
    auto profile = parse_declaration(parse_document(*declaration_text), *backend_name);
    if (!profile.has_value()) {
      return "runner profile declaration is malformed";
    }
    if (!ids.insert(profile->id).second) {
      return "runner profile is declared more than once";
    }
    profiles.push_back(std::move(*profile));
  }
  return nullptr;
}

}  // namespace

Result<std::filesystem::path> installed_descriptor_path() {
  std::filesystem::path directory{kDescriptorDir};
  if (const char* value = std::getenv(kDescriptorDirEnv); value != nullptr && *value != '\0') {
    directory = value;
    if (!directory.is_absolute()) {
      return unexpected(invalid("TP_BACKEND_DESCRIPTOR_DIR must be an absolute path"));
    }
  }
  return directory / kBackendName / kDescriptorFile;
}

Result<std::vector<InstalledRunnerProfile>> read_installed_runner_profiles(
    const std::filesystem::path& descriptor_path) {
  std::vector<InstalledRunnerProfile> profiles;
  if (const char* refusal = list_profiles(descriptor_path, profiles); refusal != nullptr) {
    return unexpected(invalid(refusal));
  }
  return profiles;
}

Result<RunnerEnvironment> read_runner_environment(const std::filesystem::path& descriptor_path,
                                                  std::string_view id) {
  std::vector<InstalledRunnerProfile> profiles;
  if (const char* refusal = list_profiles(descriptor_path, profiles); refusal != nullptr) {
    return unexpected(invalid(refusal));
  }
  for (auto& profile : profiles) {
    if (profile.id == id) {
      return std::move(profile.environment);
    }
  }
  return unexpected(Error::make(Error::Code::Unsupported, "runner profile is not installed"));
}

Result<SidecarLaunchRequest> runner_launch_request(const RunnerEnvironment& environment,
                                                   const std::filesystem::path& temp_dir) {
  std::error_code ec;
  if (!std::filesystem::is_regular_file(environment.interpreter, ec) ||
      ::access(environment.interpreter.c_str(), X_OK) != 0) {
    return unexpected(Error::make(Error::Code::Unavailable,
                                  "runner profile interpreter is not an executable file"));
  }
  bool temp_dir_usable =
      std::filesystem::is_directory(temp_dir, ec) && ::access(temp_dir.c_str(), W_OK | X_OK) == 0;
#ifdef ST_NOEXEC
  struct statvfs fs {};
  temp_dir_usable =
      temp_dir_usable && ::statvfs(temp_dir.c_str(), &fs) == 0 && (fs.f_flag & ST_NOEXEC) == 0;
#endif
  if (!temp_dir_usable) {
    return unexpected(
        Error::make(Error::Code::Unavailable,
                    "sidecar temporary directory is not writable or does not allow execution"));
  }

  std::string search_path;
  for (const auto& dir : environment.library_search_paths) {
    // The dynamic loader splits the variable on both characters.
    if (dir.find_first_of(":;") != std::string::npos) {
      return unexpected(invalid("runner profile library search path cannot be expressed"));
    }
    search_path += search_path.empty() ? dir : ":" + dir;
  }

  SidecarLaunchRequest request;
  request.python_exe = environment.interpreter;
  request.environment = {
      "LD_LIBRARY_PATH=" + search_path,
      "ORT_DISABLE_TELEMETRY=1",
      "TMPDIR=" + temp_dir.string(),
  };
  return request;
}

}  // namespace tensorplate::adapters::python_pytorch
