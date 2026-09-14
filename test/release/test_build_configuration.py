#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Black-box tests for the C++ configure step of the release builder.

The builder runs against a fixture checkout with stub dpkg, shellcheck,
cargo and cmake on PATH. The stub cmake records the environment
compilers and every argument it is given and then fails, so each case stops
right after configure and nothing is compiled.
"""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
BUILD_SCRIPT = REPO_ROOT / "tools/release/build-release-artifacts.sh"
FIXTURE_SOURCES = (
    "packaging/version.sh",
    "packaging/scripts/install.sh",
    "packaging/apt/tensorplate-archive-keyring.asc",
)

SNAPSHOT_VERSION = "0.2.1~dev.20260906.deadbeef1234"
SNAPSHOT_TAG = "snapshot-fixture-deadbeef1234"

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

    def artifact_paths(self, directory: str = "artifacts"):
        return [
            "--artifacts-dir",
            directory,
            "--manifest",
            f"{directory}/tensorplate-{SNAPSHOT_TAG}-artifacts.json",
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
    ) -> subprocess.CompletedProcess[str]:
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
