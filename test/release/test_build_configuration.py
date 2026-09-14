#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Black-box tests for the C++ configure step of the release builder.

The builder runs against a fixture checkout with stub dpkg, shellcheck,
cargo, cmake and clang++ on PATH. The stub cmake records the environment
compilers and every argument it is given and then fails, so each case stops
right after configure and nothing is compiled.
"""

from __future__ import annotations

import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

try:
    import yaml
except ImportError:  # pragma: no cover - exercised only on a bare host
    sys.exit(
        "FAIL: this suite needs PyYAML to read the release workflow.\n"
        "      Install the release tooling dependencies with:\n"
        "        python3 -m pip install -r tools/release/requirements.txt"
    )


REPO_ROOT = Path(__file__).resolve().parents[2]
BUILD_SCRIPT = REPO_ROOT / "tools/release/build-release-artifacts.sh"
PROFILE = "tools/release/amd64-build-profile.sh"
RELEASE_WORKFLOW = REPO_ROOT / ".github/workflows/release.yml"
FIXTURE_SOURCES = (
    "packaging/version.sh",
    "packaging/scripts/install.sh",
    "packaging/apt/tensorplate-archive-keyring.asc",
    PROFILE,
)

SNAPSHOT_VERSION = "0.2.1~dev.20260906.deadbeef1234"
SNAPSHOT_TAG = "snapshot-fixture-deadbeef1234"
RELEASE_TAG = "v0.2.1-rc.1"
RELEASE_DEB_VERSION = "0.2.1~rc.1"

# Variables that change what the builder hands cmake. Hosted runners set
# VCPKG_INSTALLATION_ROOT, which would add a toolchain file to every run.
SCRUBBED_ENVIRONMENT = (
    "BASH_ENV",
    "CC",
    "CDPATH",
    "CXX",
    "ENV",
    "TP_CMAKE_TOOLCHAIN_FILE",
    "TP_ENABLE_LIBTORCH",
    "TP_ENABLE_PYTHON_PYTORCH_SIDECAR",
    "TP_ENABLE_TENSORRT",
    "TP_JETSON_CC",
    "TP_JETSON_CXX",
    "TP_JETSON_SYSROOT",
    "TP_REQUIRE_TENSORRT_SDK",
    "TP_RUST_TARGET",
    "VCPKG_INSTALLATION_ROOT",
    "VCPKG_ROOT",
)

ARM64_SNAPSHOT_ARGS = [
    "-S",
    ".",
    "-B",
    "build/snapshot-arm64",
    "-G",
    "Ninja",
    "-DCMAKE_BUILD_TYPE=RelWithDebInfo",
    "-DTP_RUNTIME_VERSION_SUFFIX=dev.20260906.deadbeef1234",
    "-DTP_BUILD_TESTS=OFF",
    "-DTP_BUILD_EXAMPLES=OFF",
    "-DTP_ENABLE_SANITIZERS=OFF",
    "-DTP_ENABLE_TENSORRT=ON",
    "-DTP_REQUIRE_TENSORRT_SDK=ON",
    "-DTP_ENABLE_LIBTORCH=OFF",
    "-DTP_ENABLE_PYTHON_PYTORCH_SIDECAR=ON",
]

AMD64_SNAPSHOT_COMMON_ARGS = [
    "-S",
    ".",
    "-B",
    "build/snapshot-amd64",
    "-G",
    "Ninja",
    "-DCMAKE_BUILD_TYPE=RelWithDebInfo",
    "-DTP_RUNTIME_VERSION_SUFFIX=dev.20260906.deadbeef1234",
    "-DTP_BUILD_TESTS=OFF",
    "-DTP_BUILD_EXAMPLES=OFF",
    "-DTP_ENABLE_SANITIZERS=OFF",
]


def write_executable(path: Path, text: str) -> None:
    path.write_text(text)
    path.chmod(0o755)


def read_cmake_calls(log: Path) -> list[dict]:
    calls: list[dict] = []
    if not log.exists():
        return calls
    for line in log.read_text().splitlines():
        if line == "--call--":
            calls.append({"CC": None, "CXX": None, "args": []})
            continue
        key, _, value = line.partition("=")
        if key == "ARG":
            calls[-1]["args"].append(value)
        else:
            calls[-1][key] = value
    return calls


class BuildConfigurationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        self.repo = self.root / "fixture"
        for relative in FIXTURE_SOURCES:
            target = self.repo / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPO_ROOT / relative, target)
        (self.repo / "packaging/debian").mkdir(parents=True)
        self.changelog = self.repo / "packaging/debian/changelog"
        self.original_changelog = (
            "tensorplate (0.2.1-1) unstable; urgency=medium\n\n"
            "  * Fixture.\n\n"
            " -- Fixture <fixture@example.com>  Thu, 01 Jan 1970 00:00:00 +0000\n"
        )
        self.changelog.write_text(self.original_changelog)
        (self.repo / "packaging/VERSION").write_text("0.2.1\n")
        (self.repo / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 3.25)\n"
            "project(\n"
            "  TensorPlate\n"
            "  VERSION 0.2.1\n"
            "  LANGUAGES CXX\n"
            ")\n"
        )
        (self.repo / "Cargo.toml").write_text(
            "[workspace]\n"
            "members = []\n\n"
            "[workspace.package]\n"
            'version = "0.2.1"\n'
            'edition = "2021"\n'
        )
        subprocess.run(
            ["git", "init", "-q", "-b", "fixture"], cwd=self.repo, check=True
        )

        self.fake_bin = self.root / "bin"
        self.fake_bin.mkdir()
        self.cmake_log = self.root / "cmake.log"
        self.cargo_marker = self.root / "cargo-ran"
        write_executable(
            self.fake_bin / "dpkg",
            "#!/bin/sh\n"
            'if [ "${1:-}" = --print-architecture ]; then\n'
            "  printf '%s\\n' \"$FIXTURE_HOST_ARCH\"\n"
            "  exit 0\n"
            "fi\n"
            "exit 2\n",
        )
        write_executable(self.fake_bin / "shellcheck", "#!/bin/sh\nexit 0\n")
        write_executable(self.fake_bin / "clang++", "#!/bin/sh\nexit 0\n")
        write_executable(
            self.fake_bin / "cargo",
            '#!/bin/sh\n: >"$FIXTURE_CARGO_MARKER"\nexit 0\n',
        )
        write_executable(
            self.fake_bin / "cmake",
            "#!/bin/sh\n"
            "{\n"
            "  printf '%s\\n' --call--\n"
            '  if [ "${CC+set}" = set ]; then printf \'CC=%s\\n\' "$CC"; fi\n'
            '  if [ "${CXX+set}" = set ]; then printf \'CXX=%s\\n\' "$CXX"; fi\n'
            '  for arg in "$@"; do printf \'ARG=%s\\n\' "$arg"; done\n'
            '} >>"$FIXTURE_CMAKE_LOG"\n'
            'exit "${FIXTURE_CMAKE_STATUS:-97}"\n',
        )

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    # -- helpers -----------------------------------------------------------

    def environment(self, host_arch: str, extra: dict[str, str] | None) -> dict:
        env = os.environ.copy()
        for name in SCRUBBED_ENVIRONMENT:
            env.pop(name, None)
        env["PATH"] = f"{self.fake_bin}{os.pathsep}{env['PATH']}"
        env["FIXTURE_HOST_ARCH"] = host_arch
        env["FIXTURE_CMAKE_LOG"] = str(self.cmake_log)
        env["FIXTURE_CARGO_MARKER"] = str(self.cargo_marker)
        env.update(extra or {})
        return env

    def artifact_paths(self, directory: str = "artifacts", tag: str = SNAPSHOT_TAG):
        return [
            "--artifacts-dir",
            directory,
            "--manifest",
            f"{directory}/tensorplate-{tag}-artifacts.json",
            "--checksums",
            f"{directory}/SHA256SUMS",
        ]

    def run_builder(
        self,
        paths: list[str],
        *,
        arch: str,
        host_arch: str | None = None,
        env: dict[str, str] | None = None,
        release: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        if release:
            identity = [
                "--version",
                "0.2.1",
                "--tag",
                RELEASE_TAG,
                "--deb-version",
                RELEASE_DEB_VERSION,
                "--python-version",
                "0.2.1rc1",
                "--skip-tag-verify",
            ]
        else:
            identity = [
                "--snapshot",
                "--branch",
                "fixture",
                "--version",
                SNAPSHOT_VERSION,
                "--tag",
                SNAPSHOT_TAG,
            ]
        return subprocess.run(
            [str(BUILD_SCRIPT), *identity, *paths, "--arch", arch],
            cwd=self.repo,
            env=self.environment(host_arch or arch, env),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=60,
            check=False,
        )

    def profile(self) -> dict:
        """The profile's values, read by sourcing the real file in bash."""
        result = subprocess.run(
            [
                "bash",
                "--noprofile",
                "--norc",
                "-c",
                '. "$1" || exit 1\n'
                'printf "CC=%s\\n" "$TP_AMD64_CC"\n'
                'printf "CXX=%s\\n" "$TP_AMD64_CXX"\n'
                'for arg in "${TP_AMD64_CMAKE_ARGS[@]}"; do printf "ARG=%s\\n" "$arg"; done\n',
                "profile",
                str(REPO_ROOT / PROFILE),
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        values: dict = {"args": []}
        for line in result.stdout.splitlines():
            key, _, value = line.partition("=")
            if key == "ARG":
                values["args"].append(value)
            else:
                values[key] = value
        return values

    def assert_changelog_restored(self) -> None:
        self.assertEqual(self.changelog.read_text(), self.original_changelog)

    def assert_reached_configure(self, result) -> dict:
        calls = read_cmake_calls(self.cmake_log)
        self.assertEqual(len(calls), 1, result.stdout)
        self.assertTrue(self.cargo_marker.exists(), result.stdout)
        self.assert_changelog_restored()
        return calls[0]

    def assert_refused(self, result, *expected: str) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout)
        for text in expected:
            self.assertIn(text, result.stdout)
        # Refused before anything is compiled, not after an hour of it.
        self.assertFalse(self.cargo_marker.exists(), result.stdout)
        self.assertFalse(self.cmake_log.exists(), result.stdout)
        self.assert_changelog_restored()

    def run_release_step(self) -> dict:
        """Execute the release job's configure step with a stub cmake."""
        workflow = yaml.safe_load(RELEASE_WORKFLOW.read_text())
        steps = [
            step
            for step in workflow["jobs"]["build_packages_amd64"]["steps"]
            if step.get("name") == "Build amd64 serving worker"
        ]
        self.assertEqual(len(steps), 1, "release.yml must have one amd64 serving worker step")
        step = steps[0]
        body = step["run"]
        self.assertEqual(body.count("cmake --build"), 1, body)
        configure = body.split("cmake --build")[0]

        env = self.environment("amd64", {"FIXTURE_CMAKE_STATUS": "0"})
        expressions = {"DEB_VERSION": RELEASE_DEB_VERSION}
        for name, value in (step.get("env") or {}).items():
            if "${{" in str(value):
                self.assertIn(name, expressions, f"no fixture value for step env {name}")
                env[name] = expressions[name]
            else:
                env[name] = str(value)
        script = self.root / "release-step.sh"
        script.write_text(configure)
        # `shell: bash` on GitHub Actions runs exactly this.
        result = subprocess.run(
            ["bash", "--noprofile", "--norc", "-eo", "pipefail", str(script)],
            cwd=self.repo,
            env=env,
            text=True,
            capture_output=True,
            timeout=60,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = read_cmake_calls(self.cmake_log)
        self.assertEqual(len(calls), 1, calls)
        self.cmake_log.unlink()
        return calls[0]

    # -- arm64 is unchanged --------------------------------------------------

    def test_arm64_snapshot_configure_is_unchanged(self) -> None:
        result = self.run_builder(self.artifact_paths(), arch="arm64")
        call = self.assert_reached_configure(result)
        self.assertEqual(call["args"], ARM64_SNAPSHOT_ARGS)
        self.assertIsNone(call["CC"])
        self.assertIsNone(call["CXX"])

    def test_arm64_honours_backend_overrides(self) -> None:
        result = self.run_builder(
            self.artifact_paths(), arch="arm64", env={"TP_ENABLE_TENSORRT": "OFF"}
        )
        call = self.assert_reached_configure(result)
        expected = [
            "-DTP_ENABLE_TENSORRT=OFF" if arg == "-DTP_ENABLE_TENSORRT=ON" else arg
            for arg in ARM64_SNAPSHOT_ARGS
        ]
        self.assertEqual(call["args"], expected)

    def test_arm64_passes_the_callers_compilers_through(self) -> None:
        result = self.run_builder(
            self.artifact_paths(), arch="arm64", env={"CC": "gcc", "CXX": "g++"}
        )
        call = self.assert_reached_configure(result)
        self.assertEqual(call["args"], ARM64_SNAPSHOT_ARGS)
        self.assertEqual((call["CC"], call["CXX"]), ("gcc", "g++"))

    # -- amd64 takes the shared profile --------------------------------------

    def test_amd64_snapshot_configures_from_the_profile(self) -> None:
        profile = self.profile()
        # The documented form: the manifest and checksums take their defaults.
        result = self.run_builder(["--artifacts-dir", "artifacts"], arch="amd64")
        call = self.assert_reached_configure(result)
        self.assertEqual(call["args"], AMD64_SNAPSHOT_COMMON_ARGS + profile["args"])
        self.assertTrue(profile["CC"] and profile["CXX"], profile)
        self.assertEqual((call["CC"], call["CXX"]), (profile["CC"], profile["CXX"]))

        definitions = [arg.split("=", 1) for arg in call["args"] if arg.startswith("-D")]
        names = [name for name, _ in definitions]
        self.assertEqual(len(names), len(set(names)), f"a -D is given twice: {names}")
        values = dict(definitions)
        # A TensorRT adapter built without its SDK registers and fails only
        # at engine load; the profile must never ask for that.
        self.assertFalse(
            values.get("-DTP_ENABLE_TENSORRT") == "ON"
            and values.get("-DTP_REQUIRE_TENSORRT_SDK") == "OFF",
            values,
        )

    def test_amd64_configures_with_the_profile_compilers_over_the_callers(self) -> None:
        # The release step never reads CC or CXX from its environment, so a
        # snapshot does not either.
        profile = self.profile()
        result = self.run_builder(
            self.artifact_paths(), arch="amd64", env={"CC": "gcc", "CXX": "g++"}
        )
        call = self.assert_reached_configure(result)
        self.assertEqual((call["CC"], call["CXX"]), (profile["CC"], profile["CXX"]))

    def test_amd64_release_configure_matches_the_release_workflow(self) -> None:
        release = self.run_release_step()
        result = self.run_builder(
            self.artifact_paths(tag=RELEASE_TAG), arch="amd64", release=True
        )
        builder = self.assert_reached_configure(result)
        self.assertEqual(sorted(builder["args"]), sorted(release["args"]))
        self.assertIsNotNone(release["CC"])
        self.assertIsNotNone(release["CXX"])
        self.assertEqual((builder["CC"], builder["CXX"]), (release["CC"], release["CXX"]))

    def test_release_workflow_pins_a_dwarf_version_dwz_can_read(self) -> None:
        # jammy's dwz (0.14) cannot read DWARF 5's .debug_addr section and
        # dh_dwz turns that into a hard dpkg-buildpackage failure, which no
        # PR check reaches because the job runs only for releases. The
        # release and the builder read one profile, so the equality test
        # above cannot see a 4 -> 5 edit; this one pins the version range.
        release = self.run_release_step()
        self.assertTrue(
            any(
                re.search(r"^-DCMAKE_CXX_FLAGS=.*-gdwarf-[2-4]\b", arg)
                for arg in release["args"]
            ),
            release["args"],
        )

    # -- amd64 refusals --------------------------------------------------------

    def test_amd64_refuses_backend_overrides(self) -> None:
        # Each value disagrees with the profile. amd64 never reads these
        # variables, so one the builder stopped refusing would be ignored
        # without a word instead of changing the build.
        overrides = {
            "TP_ENABLE_TENSORRT": "ON",
            "TP_REQUIRE_TENSORRT_SDK": "ON",
            "TP_ENABLE_LIBTORCH": "ON",
            "TP_ENABLE_PYTHON_PYTORCH_SIDECAR": "OFF",
        }
        for name, value in overrides.items():
            with self.subTest(name=name):
                self.cmake_log.unlink(missing_ok=True)
                self.cargo_marker.unlink(missing_ok=True)
                result = self.run_builder(
                    self.artifact_paths(), arch="amd64", env={name: value}
                )
                self.assert_refused(result, f"{name} is set", PROFILE)

    def test_amd64_refuses_a_missing_profile_compiler(self) -> None:
        profile = self.repo / PROFILE
        text = profile.read_text()
        self.assertIn("TP_AMD64_CXX=clang++\n", text)
        profile.write_text(
            text.replace("TP_AMD64_CXX=clang++\n", "TP_AMD64_CXX=tp-fixture-missing-c++\n")
        )
        result = self.run_builder(self.artifact_paths(), arch="amd64")
        self.assert_refused(result, "tp-fixture-missing-c++")

    def test_amd64_refuses_a_tree_without_the_profile(self) -> None:
        (self.repo / PROFILE).unlink()
        result = self.run_builder(self.artifact_paths(), arch="amd64")
        self.assert_refused(result, f"cannot read {PROFILE}")

    def test_amd64_refuses_a_build_dir_configured_with_another_compiler(self) -> None:
        cache = self.repo / "build/snapshot-amd64/CMakeCache.txt"
        cache.parent.mkdir(parents=True)
        cache.write_text(
            "CMAKE_BUILD_TYPE:STRING=RelWithDebInfo\n"
            "CMAKE_CXX_COMPILER:FILEPATH=/usr/bin/c++\n"
            "TP_ENABLE_TENSORRT:BOOL=ON\n"
        )
        result = self.run_builder(self.artifact_paths(), arch="amd64")
        self.assert_refused(result, "/usr/bin/c++", "build/snapshot-amd64")

        # The same directory configured with the profile's compiler is reused.
        cache.write_text(
            "CMAKE_BUILD_TYPE:STRING=RelWithDebInfo\n"
            f"CMAKE_CXX_COMPILER:FILEPATH=/usr/bin/{self.profile()['CXX']}\n"
        )
        result = self.run_builder(self.artifact_paths(), arch="amd64")
        self.assert_reached_configure(result)

    def test_amd64_reuses_a_build_dir_whose_cache_names_no_compiler(self) -> None:
        # A first configure that stops before compiler detection, for example
        # because Ninja is not installed, leaves a cache like this one. CMake
        # still takes CXX from the environment on the next run.
        cache = self.repo / "build/snapshot-amd64/CMakeCache.txt"
        cache.parent.mkdir(parents=True)
        cache.write_text("CMAKE_MAKE_PROGRAM:FILEPATH=CMAKE_MAKE_PROGRAM-NOTFOUND\n")
        profile = self.profile()
        result = self.run_builder(self.artifact_paths(), arch="amd64")
        call = self.assert_reached_configure(result)
        self.assertEqual((call["CC"], call["CXX"]), (profile["CC"], profile["CXX"]))

    # -- manifest and checksums paths ------------------------------------------

    def test_refuses_a_manifest_name_the_installer_does_not_read(self) -> None:
        paths = self.artifact_paths()
        paths[3] = "artifacts/manifest.json"
        result = self.run_builder(paths, arch="arm64")
        self.assert_refused(
            result, "--manifest", f"artifacts/tensorplate-{SNAPSHOT_TAG}-artifacts.json"
        )

    def test_refuses_a_manifest_outside_the_artifacts_dir(self) -> None:
        (self.repo / "elsewhere").mkdir()
        for directory in ("elsewhere", "missing"):
            with self.subTest(directory=directory):
                paths = self.artifact_paths()
                paths[3] = f"{directory}/tensorplate-{SNAPSHOT_TAG}-artifacts.json"
                result = self.run_builder(paths, arch="arm64")
                self.assert_refused(
                    result,
                    "--manifest",
                    f"artifacts/tensorplate-{SNAPSHOT_TAG}-artifacts.json",
                )

    def test_refuses_a_checksums_name_the_installer_does_not_read(self) -> None:
        paths = self.artifact_paths()
        paths[5] = "artifacts/sums.txt"
        result = self.run_builder(paths, arch="arm64")
        self.assert_refused(result, "--checksums", "artifacts/SHA256SUMS")

    def test_refuses_checksums_outside_the_artifacts_dir(self) -> None:
        (self.repo / "elsewhere").mkdir()
        paths = self.artifact_paths()
        paths[5] = "elsewhere/SHA256SUMS"
        result = self.run_builder(paths, arch="arm64")
        self.assert_refused(result, "--checksums", "artifacts/SHA256SUMS")

    def test_manifest_and_checksums_default_into_the_artifacts_dir(self) -> None:
        result = self.run_builder(["--artifacts-dir", "artifacts"], arch="arm64")
        self.assert_reached_configure(result)

    def test_refuses_an_artifacts_dir_that_cannot_be_created(self) -> None:
        (self.repo / "not-a-dir").write_text("")
        result = self.run_builder(["--artifacts-dir", "not-a-dir/artifacts"], arch="arm64")
        self.assert_refused(result, "cannot create --artifacts-dir not-a-dir/artifacts")

    def test_accepts_the_artifacts_dir_through_another_spelling(self) -> None:
        real = self.repo / "real-artifacts"
        real.mkdir()
        (self.repo / "linked-artifacts").symlink_to(real)
        result = self.run_builder(
            [
                "--artifacts-dir",
                "linked-artifacts",
                "--manifest",
                f"{real}/tensorplate-{SNAPSHOT_TAG}-artifacts.json",
                "--checksums",
                "real-artifacts/SHA256SUMS",
            ],
            arch="arm64",
        )
        self.assert_reached_configure(result)

        self.cmake_log.unlink()
        self.cargo_marker.unlink()
        result = self.run_builder(
            [
                "--artifacts-dir",
                "artifacts",
                "--manifest",
                f"artifacts/../artifacts/tensorplate-{SNAPSHOT_TAG}-artifacts.json",
                "--checksums",
                "artifacts/../artifacts/SHA256SUMS",
            ],
            arch="arm64",
        )
        self.assert_reached_configure(result)

    def test_directory_comparison_ignores_an_exported_cdpath(self) -> None:
        # With CDPATH set, `cd` prints the directory it changed to, and a
        # command substitution around it returns that path twice.
        result = self.run_builder(
            [
                "--artifacts-dir",
                "artifacts",
                "--manifest",
                f"{self.repo}/artifacts/tensorplate-{SNAPSHOT_TAG}-artifacts.json",
                "--checksums",
                f"{self.repo}/artifacts/SHA256SUMS",
            ],
            arch="arm64",
            env={"CDPATH": ".:/"},
        )
        self.assert_reached_configure(result)


if __name__ == "__main__":
    unittest.main()
