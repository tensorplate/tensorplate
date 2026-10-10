#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Black-box tests for the C++ configure step of the release builder.

The builder runs against a fixture checkout with stub dpkg, shellcheck,
cargo, cmake and clang++ on PATH. The stub cmake records the environment
compilers and every argument it is given and then fails, so each case stops
right after configure and nothing is compiled.

ReleaseStagingTests run the builder past configure to the end, with the
build steps stubbed, to see which names its packages are published under.

RunnerVcpkgProvisioningTests run the release runner's control script against
a local git repository and a stub vcpkg, to see what it calls a ready runner.
"""

from __future__ import annotations

import contextlib
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import pwd
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
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
sys.path.insert(0, str(Path(__file__).resolve().parent))

from test_artifact_identity import (  # noqa: E402
    DPKG_DEB_STUB,
    SPEECH_RUNTIME_PACKAGES,
    fixture_control,
    github_served_name,
)
BUILD_SCRIPT = REPO_ROOT / "tools/release/build-release-artifacts.sh"
PROFILE = "tools/release/amd64-build-profile.sh"
WORKFLOWS = REPO_ROOT / ".github/workflows"
RELEASE_WORKFLOW = WORKFLOWS / "release.yml"
VCPKG_ACTION_USE = "./.github/actions/release-vcpkg"
VCPKG_ACTION = REPO_ROOT / VCPKG_ACTION_USE / "action.yml"
CACHE_WARMER = WORKFLOWS / "release-dependencies.yml"
SOURCE_INSTALL = REPO_ROOT / "packaging/scripts/build-install-from-source.sh"
CLOSURE_CHECK = REPO_ROOT / "tools/release/assert-static-streaming-closure.sh"
FIXTURE_SOURCES = (
    "packaging/version.sh",
    "packaging/scripts/install.sh",
    "packaging/apt/tensorplate-archive-keyring.asc",
    "tools/release/stage-debian-changelog.sh",
    PROFILE,
)

SNAPSHOT_VERSION = "0.2.1~dev.20260906.deadbeef1234"
SNAPSHOT_TAG = "snapshot-fixture-deadbeef1234"
RELEASE_TAG = "v0.2.1-rc.1"
RELEASE_DEB_VERSION = "0.2.1~rc.1"

# Variables that change what the builder hands cmake. The fixture supplies
# its own VCPKG_ROOT; the host's vcpkg must not reach a run.
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
    "TP_VCPKG_BINARY_ONLY",
    "VCPKG_INSTALLATION_ROOT",
    "VCPKG_ROOT",
)

# "{vcpkg_root}" is the fixture's checkout; BuilderFixture.pinned fills it in.
VCPKG_TOOLCHAIN_ARG = "-DCMAKE_TOOLCHAIN_FILE={vcpkg_root}/scripts/buildsystems/vcpkg.cmake"
BINARY_ONLY_ARG = "-DVCPKG_INSTALL_OPTIONS=--only-binarycaching"

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
    "-DTP_ENABLE_STREAMING_GRPC=ON",
    "-DTP_ENABLE_PYTHON_PYTORCH_SIDECAR=ON",
    "-DVCPKG_MANIFEST_FEATURES=streaming-grpc",
    "-DVCPKG_TARGET_TRIPLET=arm64-linux",
    VCPKG_TOOLCHAIN_ARG,
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

# What the amd64 profile adds for the streaming feature, in its order.
STREAMING_PREFIXES = ("-DTP_ENABLE_STREAMING_GRPC=", "-DVCPKG_", "-DCMAKE_TOOLCHAIN_FILE=")
AMD64_STREAMING_ARGS = [
    "-DTP_ENABLE_STREAMING_GRPC=ON",
    "-DVCPKG_MANIFEST_FEATURES=streaming-grpc",
    "-DVCPKG_TARGET_TRIPLET=x64-linux",
    VCPKG_TOOLCHAIN_ARG,
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


class BuilderFixture(unittest.TestCase):
    """The fixture checkout and stubs every builder case runs against."""

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

        # A vcpkg checkout as far as the builder looks: its toolchain file.
        self.vcpkg_root = self.root / "vcpkg"
        (self.vcpkg_root / "scripts/buildsystems").mkdir(parents=True)
        (self.vcpkg_root / "scripts/buildsystems/vcpkg.cmake").write_text("")

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

    def environment(self, host_arch: str, extra: dict | None) -> dict:
        """The builder's environment; an extra value of None removes the variable."""
        env = os.environ.copy()
        for name in SCRUBBED_ENVIRONMENT:
            env.pop(name, None)
        env["PATH"] = f"{self.fake_bin}{os.pathsep}{env['PATH']}"
        env["FIXTURE_HOST_ARCH"] = host_arch
        env["FIXTURE_CMAKE_LOG"] = str(self.cmake_log)
        env["FIXTURE_CARGO_MARKER"] = str(self.cargo_marker)
        env["VCPKG_ROOT"] = str(self.vcpkg_root)
        env.update(extra or {})
        return {name: value for name, value in env.items() if value is not None}

    def pinned(self, args: list[str]) -> list[str]:
        return [arg.replace("{vcpkg_root}", str(self.vcpkg_root)) for arg in args]

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
        env: dict | None = None,
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

    def profile(self, *arguments: str, env: dict | None = None, refused: bool = False):
        """The profile's values, read by sourcing the real file in bash."""
        result = subprocess.run(
            [
                "bash",
                "--noprofile",
                "--norc",
                "-c",
                # `.` with no argument hands the file the caller's own.
                'file="$1"; shift; . "$file" "$@" || exit 1\n'
                'printf "CC=%s\\n" "$TP_AMD64_CC"\n'
                'printf "CXX=%s\\n" "$TP_AMD64_CXX"\n'
                'for arg in "${TP_AMD64_CMAKE_ARGS[@]}"; do printf "ARG=%s\\n" "$arg"; done\n',
                "profile",
                str(REPO_ROOT / PROFILE),
                *arguments,
            ],
            env=self.environment("amd64", env),
            text=True,
            capture_output=True,
            check=False,
        )
        if refused:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            return result.stderr
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

    def execute_release_step(self, extra: dict | None = None):
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

        env = self.environment("amd64", {"FIXTURE_CMAKE_STATUS": "0", **(extra or {})})
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
        return subprocess.run(
            ["bash", "--noprofile", "--norc", "-eo", "pipefail", str(script)],
            cwd=self.repo,
            env=env,
            text=True,
            capture_output=True,
            timeout=60,
            check=False,
        )

    def run_release_step(self, extra: dict | None = None) -> dict:
        result = self.execute_release_step(extra)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = read_cmake_calls(self.cmake_log)
        self.assertEqual(len(calls), 1, calls)
        self.cmake_log.unlink()
        return calls[0]


class BuildConfigurationTests(BuilderFixture):
    # -- arm64 pins its release defaults ------------------------------------

    def test_arm64_snapshot_pins_configure_arguments(self) -> None:
        result = self.run_builder(self.artifact_paths(), arch="arm64")
        call = self.assert_reached_configure(result)
        self.assertEqual(call["args"], self.pinned(ARM64_SNAPSHOT_ARGS))
        self.assertIsNone(call["CC"])
        self.assertIsNone(call["CXX"])

    def test_arm64_honours_backend_overrides(self) -> None:
        result = self.run_builder(
            self.artifact_paths(), arch="arm64", env={"TP_ENABLE_TENSORRT": "OFF"}
        )
        call = self.assert_reached_configure(result)
        expected = [
            "-DTP_ENABLE_TENSORRT=OFF" if arg == "-DTP_ENABLE_TENSORRT=ON" else arg
            for arg in self.pinned(ARM64_SNAPSHOT_ARGS)
        ]
        self.assertEqual(call["args"], expected)

    def test_arm64_passes_the_callers_compilers_through(self) -> None:
        result = self.run_builder(
            self.artifact_paths(), arch="arm64", env={"CC": "gcc", "CXX": "g++"}
        )
        call = self.assert_reached_configure(result)
        self.assertEqual(call["args"], self.pinned(ARM64_SNAPSHOT_ARGS))
        self.assertEqual((call["CC"], call["CXX"]), ("gcc", "g++"))

    def test_arm64_takes_an_explicit_toolchain_file_in_place_of_the_checkouts(self) -> None:
        expected = ARM64_SNAPSHOT_ARGS[:-1] + ["-DCMAKE_TOOLCHAIN_FILE=/elsewhere/toolchain.cmake"]
        for root in (None, str(self.vcpkg_root)):
            with self.subTest(vcpkg_root=root):
                explicit = {"TP_CMAKE_TOOLCHAIN_FILE": "/elsewhere/toolchain.cmake"}
                explicit["VCPKG_ROOT"] = root
                result = self.run_builder(self.artifact_paths(), arch="arm64", env=explicit)
                self.assertEqual(self.assert_reached_configure(result)["args"], expected)
                self.cmake_log.unlink()

    def test_arm64_forbids_vcpkg_to_build_when_binary_only_is_1(self) -> None:
        only = {"TP_VCPKG_BINARY_ONLY": "1"}
        result = self.run_builder(self.artifact_paths(), arch="arm64", env=only)
        call = self.assert_reached_configure(result)
        self.assertEqual(call["args"], self.pinned(ARM64_SNAPSHOT_ARGS) + [BINARY_ONLY_ARG])

    def test_a_binary_only_value_other_than_0_or_1_is_refused(self) -> None:
        said = {"TP_VCPKG_BINARY_ONLY": "true"}
        for arch in ("arm64", "amd64"):
            with self.subTest(arch=arch):
                result = self.run_builder(self.artifact_paths(), arch=arch, env=said)
                self.assert_refused(result, "TP_VCPKG_BINARY_ONLY must be 0 or 1")
        result = self.execute_release_step(said)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("TP_VCPKG_BINARY_ONLY must be 0 or 1", result.stderr)
        self.assertFalse(self.cmake_log.exists(), result.stdout)

    def test_a_binary_only_configure_that_fails_points_at_the_cold_cache(self) -> None:
        hint = "the binary cache is cold or was built with another compiler"
        for only in ("1", "0", None):
            said = {"TP_VCPKG_BINARY_ONLY": only, "FIXTURE_CMAKE_STATUS": "97"}
            failed = {"release step": self.execute_release_step(said)}
            for arch in ("arm64", "amd64"):
                failed[arch] = self.run_builder(self.artifact_paths(), arch=arch, env=said)
            for writer, result in failed.items():
                with self.subTest(writer=writer, binary_only=only):
                    text = result.stdout + (result.stderr or "")
                    self.assertNotEqual(result.returncode, 0, text)
                    hints = [line for line in text.splitlines() if hint in line]
                    self.assertEqual(len(hints), 1 if only == "1" else 0, text)
                    for line in hints:
                        self.assertRegex(line, r"^(::error::|error: ).*docs/release/runbook\.md")

    def test_without_streaming_configures_the_feature_off_and_reads_no_vcpkg(self) -> None:
        off = [arg for arg in ARM64_SNAPSHOT_ARGS if not arg.startswith(STREAMING_PREFIXES)]
        off.insert(-1, "-DTP_ENABLE_STREAMING_GRPC=OFF")
        common = [arg for arg in self.profile()["args"] if not arg.startswith(STREAMING_PREFIXES)]
        self.assertEqual(
            [arg for arg in self.profile("--without-streaming", env={"VCPKG_ROOT": None})["args"]
             if arg not in common], ["-DTP_ENABLE_STREAMING_GRPC=OFF"])
        for root in (None, str(self.vcpkg_root)):
            for arch in ("arm64", "amd64"):
                with self.subTest(arch=arch, vcpkg_root=root):
                    result = self.run_builder(
                        [*self.artifact_paths(), "--without-streaming"], arch=arch,
                        env={"VCPKG_ROOT": root},
                    )
                    args = self.assert_reached_configure(result)["args"]
                    self.cmake_log.unlink()
                    streaming = [arg for arg in args if arg.startswith(STREAMING_PREFIXES)]
                    self.assertEqual(streaming, ["-DTP_ENABLE_STREAMING_GRPC=OFF"])
                    if arch == "arm64":
                        self.assertEqual(args, off)

    def test_without_streaming_is_a_flag_and_nothing_else_asks_for_it(self) -> None:
        refused = {
            "TP_VCPKG_BINARY_ONLY": {"TP_VCPKG_BINARY_ONLY": "1"},
            "TP_CMAKE_TOOLCHAIN_FILE": {"TP_CMAKE_TOOLCHAIN_FILE": "/elsewhere/toolchain.cmake"},
        }
        for arch in ("arm64", "amd64"):
            for name, env in refused.items():
                with self.subTest(arch=arch, combined_with=name):
                    paths = [*self.artifact_paths(), "--without-streaming"]
                    result = self.run_builder(paths, arch=arch, env=env)
                    self.assert_refused(result, "--without-streaming", name)
        cross = self.run_builder(
            [*self.artifact_paths(), "--without-streaming"], arch="arm64", host_arch="amd64"
        )
        self.assert_refused(cross, "--without-streaming", "cross-build")
        for argument in ("--with-streaming", "--without-streaming extra"):
            self.assertIn("--without-streaming", self.profile(*argument.split(), refused=True))
        only = self.profile("--without-streaming", env={"TP_VCPKG_BINARY_ONLY": "1"}, refused=True)
        self.assertIn("TP_VCPKG_BINARY_ONLY", only)
        paths = [*self.artifact_paths(tag=RELEASE_TAG), "--without-streaming"]
        release = self.run_builder(paths, arch="arm64", release=True)
        self.assert_refused(release, "--without-streaming", "--snapshot")
        smoke = REPO_ROOT / "test/packaging/verify_cpu_only_smoke.sh"
        for path in (*(REPO_ROOT / ".github").rglob("*"), smoke):
            if path.is_file():
                self.assertNotIn("without-streaming", path.read_text(), path)
        # The smoke sources the profile with its own arguments, so it takes none.
        argued = subprocess.run(
            ["bash", str(smoke), "--without-streaming"], env={"PATH": os.environ["PATH"]},
            text=True, capture_output=True, timeout=30,
        )
        self.assertEqual((argued.returncode, "takes no argument" in argued.stderr), (1, True))
        checked = re.search(r"assert-static-streaming-closure\.sh[^|]*", smoke.read_text()).group()
        self.assertIn("--cmake-cache build/release/CMakeCache.txt", checked)

    def test_no_environment_variable_selects_the_build_without_streaming(self) -> None:
        said = {"TP_AMD64_STREAMING": "OFF", "WITHOUT_STREAMING": "1", "TP_WITHOUT_STREAMING": "1"}
        on = {"amd64": AMD64_STREAMING_ARGS,
              "arm64": [arg for arg in ARM64_SNAPSHOT_ARGS if arg.startswith(STREAMING_PREFIXES)]}
        calls = {"profile": self.profile(env=said)["args"]}
        for arch in on:
            result = self.run_builder(self.artifact_paths(), arch=arch, env=said)
            calls[arch] = self.assert_reached_configure(result)["args"]
            self.cmake_log.unlink()
        for writer, args in calls.items():
            streaming = [arg for arg in args if arg.startswith(STREAMING_PREFIXES)]
            self.assertEqual(streaming, self.pinned(on.get(writer, on["amd64"])), writer)

    def test_a_build_dir_configured_with_the_other_streaming_choice_is_refused(self) -> None:
        for arch in ("arm64", "amd64"):
            cache = self.repo / f"build/snapshot-{arch}/CMakeCache.txt"
            cache.parent.mkdir(parents=True)
            for cached, flag in (("ON", ["--without-streaming"]), ("OFF", [])):
                with self.subTest(arch=arch, cached=cached):
                    cache.write_text(f"TP_ENABLE_STREAMING_GRPC:BOOL={cached}\n")
                    result = self.run_builder([*self.artifact_paths(), *flag], arch=arch)
                    self.assert_refused(result, f"TP_ENABLE_STREAMING_GRPC={cached}", "--build-dir")
                    # The same directory is reused for the choice it was configured with.
                    kept = [] if flag else ["--without-streaming"]
                    result = self.run_builder([*self.artifact_paths(), *kept], arch=arch)
                    self.assert_reached_configure(result)
                    self.cmake_log.unlink()
                    self.cargo_marker.unlink()

    def test_the_cache_warmer_configures_as_the_release_step_does_and_proves_its_cache(self):
        release = self.run_release_step()
        jobs = yaml.safe_load(CACHE_WARMER.read_text())["jobs"]
        steps = jobs["warm"]["steps"]
        configures = [step for step in steps if f". {PROFILE}" in (step.get("run") or "")]
        self.assertEqual(configures, steps[-2:], "the proof must be the job's last step")
        proof = {"TP_VCPKG_BINARY_ONLY": "1",
                 "VCPKG_BINARY_SOURCES": "clear;files,${{ runner.temp }}/vcpkg-archives,read"}
        self.assertEqual([step.get("env") for step in configures], [None, proof])
        # The second job configures as the action's restore mode leaves the environment.
        configures.append(jobs["restore"]["steps"][-1])
        calls = []
        for step, exported in zip(configures, ({}, {}, {"TP_VCPKG_BINARY_ONLY": "1"})):
            self.assertNotIn("cmake --build", step["run"])
            env = self.environment(
                "amd64", {"FIXTURE_CMAKE_STATUS": "0", "RUNNER_TEMP": "/t", **exported})
            result = run_step(step, self.repo, **{**env, "VCPKG_BINARY_SOURCES": "proof"})
            self.assertEqual(result.returncode, 0, result.stderr)
            calls.append(read_cmake_calls(self.cmake_log)[-1])
        for call, only in zip(calls, ([], [BINARY_ONLY_ARG], [BINARY_ONLY_ARG])):
            streaming = [arg for arg in call["args"] if arg.startswith(STREAMING_PREFIXES)]
            self.assertEqual(streaming, self.pinned(AMD64_STREAMING_ARGS) + only)
            self.assertEqual((call["CC"], call["CXX"]), (release["CC"], release["CXX"]))
        directories = [call["args"][call["args"].index("-B") + 1] for call in calls]
        self.assertEqual(len(set(directories)), 3, "each configure needs a directory of its own")

    def test_a_cross_build_adds_only_the_chainload_file(self) -> None:
        for name in ("tensorplate-agent", "tensorplate-observability", "tensorplate"):
            binary = self.repo / "target/aarch64-unknown-linux-gnu/release" / name
            binary.parent.mkdir(parents=True, exist_ok=True)
            write_executable(binary, "#!/bin/sh\n")
        cross = {"TP_JETSON_SYSROOT": "/sysroot", "TP_JETSON_CC": "cc", "TP_JETSON_CXX": "c++"}
        result = self.run_builder(self.artifact_paths(), arch="arm64", host_arch="amd64", env=cross)
        call = self.assert_reached_configure(result)
        chainload = "-DVCPKG_CHAINLOAD_TOOLCHAIN_FILE={}/cmake/toolchains/aarch64-jetson.cmake"
        expected = self.pinned(ARM64_SNAPSHOT_ARGS) + [chainload.format(self.repo.resolve())]
        self.assertEqual(call["args"], expected)

    # -- no build without the pinned checkout ---------------------------------

    def test_a_build_without_a_vcpkg_checkout_is_refused_before_cmake(self) -> None:
        missing = {"unset": None, "empty": "", "no toolchain file": str(self.root)}
        for label, value in missing.items():
            for arch in ("arm64", "amd64"):
                with self.subTest(vcpkg_root=label, arch=arch):
                    env = {"VCPKG_ROOT": value}
                    result = self.run_builder(self.artifact_paths(), arch=arch, env=env)
                    self.assert_refused(result, "VCPKG_ROOT")
            with self.subTest(vcpkg_root=label, writer="release step"):
                result = self.execute_release_step({"VCPKG_ROOT": value})
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("VCPKG_ROOT", result.stderr)
                self.assertFalse(self.cmake_log.exists(), result.stdout)

    def test_a_hosted_images_own_vcpkg_is_not_taken_for_the_pinned_checkout(self) -> None:
        image = {"VCPKG_ROOT": None, "VCPKG_INSTALLATION_ROOT": str(self.vcpkg_root)}
        for arch in ("arm64", "amd64"):
            with self.subTest(arch=arch):
                result = self.run_builder(self.artifact_paths(), arch=arch, env=image)
                self.assert_refused(result, "VCPKG_ROOT")

    # -- amd64 takes the shared profile --------------------------------------

    def test_amd64_snapshot_configures_from_the_profile(self) -> None:
        profile = self.profile()
        # The documented form: the manifest and checksums take their defaults.
        result = self.run_builder(["--artifacts-dir", "artifacts"], arch="amd64")
        call = self.assert_reached_configure(result)
        # The stub cmake fails configure; the builder stops there and says so.
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("error: C++ configure failed", result.stdout)
        self.assertEqual(call["args"], AMD64_SNAPSHOT_COMMON_ARGS + profile["args"])
        self.assertTrue(profile["CC"] and profile["CXX"], profile)
        self.assertEqual((call["CC"], call["CXX"]), (profile["CC"], profile["CXX"]))
        streaming = [arg for arg in call["args"] if arg.startswith(STREAMING_PREFIXES)]
        self.assertEqual(streaming, self.pinned(AMD64_STREAMING_ARGS))

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
        for binary_only in (None, "1"):
            with self.subTest(binary_only=binary_only):
                extra = {"TP_VCPKG_BINARY_ONLY": binary_only}
                release = self.run_release_step(extra)
                self.cargo_marker.unlink(missing_ok=True)
                result = self.run_builder(
                    self.artifact_paths(tag=RELEASE_TAG), arch="amd64", release=True, env=extra
                )
                builder = self.assert_reached_configure(result)
                self.cmake_log.unlink()
                self.assertEqual(sorted(builder["args"]), sorted(release["args"]))
                # Only a run told to may refuse to build what the cache lacks.
                for call in (builder, release):
                    self.assertEqual(call["args"].count(BINARY_ONLY_ARG), 1 if binary_only else 0)
                self.assertTrue(release["CC"] and release["CXX"], release)
                compilers = (release["CC"], release["CXX"])
                self.assertEqual((builder["CC"], builder["CXX"]), compilers)

    def test_release_workflow_builds_streaming_through_the_pinned_checkout(self) -> None:
        release = self.run_release_step()
        streaming = [arg for arg in release["args"] if arg.startswith(STREAMING_PREFIXES)]
        self.assertEqual(streaming, self.pinned(AMD64_STREAMING_ARGS))

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
            # It would be a second toolchain file beside the profile's.
            "TP_CMAKE_TOOLCHAIN_FILE": "/elsewhere/toolchain.cmake",
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


RUNNER_CONTROL = REPO_ROOT / "tools/release/jetson-runner-control.sh"


# dpkg-buildpackage as the release build runs it: every package built
# from packaging/debian, written to the repository's parent directory under
# the name dpkg gives it, at the version the (staged) changelog carries.
FAKE_BUILD_DEB = r"""#!/usr/bin/env bash
set -euo pipefail
version="$(sed -n '1s/^tensorplate (\([^)]*\)).*/\1/p' packaging/debian/changelog)"
[[ -n "$version" ]] || { echo "build-deb stand-in: no version in the changelog" >&2; exit 1; }
for spec in tensorplate-common:all tensorplate-backend-python-pytorch:all \
            tensorplate-apt-source:all tensorplate-agent:arm64 tensorplate-serving:arm64 \
            tensorplate-observability:arm64 tensorplate-cli:arm64 tensorplate:arm64; do
  package="${spec%%:*}" arch="${spec##*:}"
  printf 'Package: %s\nVersion: %s\nArchitecture: %s\nDescription: built\n' \
    "$package" "$version" "$arch" >"../${package}_${version}_${arch}.deb"
done
"""

SECONDARY_PACKAGES = (
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate",
)


def modern_bash() -> bool:
    """Whether the bash on PATH is one the builder runs under (4+: mapfile)."""
    result = subprocess.run(
        ["bash", "-c", "echo ${BASH_VERSINFO[0]}"], capture_output=True, text=True, check=False
    )
    return result.returncode == 0 and result.stdout.strip().isdigit() \
        and int(result.stdout.strip()) >= 4


class ReleaseStagingTests(BuilderFixture):
    """The release build, run to the end, publishes packages under served names.

    Compiling, the packaging suite and dpkg-buildpackage are stubbed; the
    collection, staging, manifest and verification are the builder's and
    the release driver's own. A dpkg-deb stand-in reads each package's
    control fields, as the driver does with the real one.
    """

    @classmethod
    def setUpClass(cls) -> None:
        if modern_bash():
            return
        message = ("the bash on PATH is older than 4, and the builder collects packages "
                   "with mapfile; the release build's staging NOT verified end to end here")
        if os.environ.get("CI") == "true":
            raise AssertionError(message)
        raise unittest.SkipTest(message)

    def setUp(self) -> None:
        super().setUp()
        write_executable(self.fake_bin / "dpkg-deb", DPKG_DEB_STUB)
        for relative, text in (
            ("packaging/scripts/build-deb.sh", FAKE_BUILD_DEB),
            ("test/packaging/run.sh", "#!/bin/sh\nexit 0\n"),
            # Where the C++ build leaves the serving worker.
            ("build/release/serving_worker/tensorplate-serving", "#!/bin/sh\nexit 0\n"),
        ):
            target = self.repo / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            write_executable(target, text)
        driver = self.repo / "tools/release/tensorplate-release.sh"
        shutil.copy2(REPO_ROOT / "tools/release/tensorplate-release.sh", driver)
        identity = ["-c", "user.name=TensorPlate Tests", "-c", "user.email=tests@tensorplate.invalid"]
        for step in (["git", "add", "-A"], ["git", *identity, "commit", "-qm", "fixture"]):
            subprocess.run(step, cwd=self.repo, check=True, capture_output=True)

    def build(self, tag: str, deb_version: str, python_version: str, *extra: str):
        """Run the release build for tag, with the amd64 set and SDK staged."""
        # The amd64 runtime set, which the release job moves into the
        # repository's parent before the build, as dpkg named it.
        for package in SECONDARY_PACKAGES:
            (self.root / f"{package}_{deb_version}-1_amd64.deb").write_text(
                fixture_control(package, f"{deb_version}-1", "amd64", "amd64 job")
            )
        sdk = self.root / "sdk"
        sdk.mkdir()
        (sdk / f"tensorplate_python-{python_version}-py3-none-any.whl").write_text("wheel\n")
        (sdk / f"tensorplate_python-{python_version}.tar.gz").write_text("sdist\n")
        result = subprocess.run(
            [str(BUILD_SCRIPT), "--version", "0.2.1", "--tag", tag,
             "--deb-version", deb_version, "--python-version", python_version,
             "--skip-tag-verify", "--artifacts-dir", "artifacts", "--arch", "arm64",
             "--sdk-dist-dir", str(sdk), *extra],
            cwd=self.repo,
            env=self.environment("arm64", {"FIXTURE_CMAKE_STATUS": "0"}),
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            timeout=120, check=False,
        )
        return result, self.repo / "artifacts"

    def expected_packages(self, deb_version: str) -> list[tuple[str, str]]:
        packages = [("tensorplate-common", "all"), ("tensorplate-backend-python-pytorch", "all"),
                    ("tensorplate-apt-source", "all")]
        packages += [(package, "arm64") for package in SECONDARY_PACKAGES]
        packages += [(package, "amd64") for package in SECONDARY_PACKAGES]
        packages += [(package, "amd64") for package in self.speech_runtime]
        return sorted(packages)

    # The family this case expects the build to publish; none by default.
    speech_runtime: tuple[str, ...] = ()

    def stage_speech_runtime(self, deb_version: str, packages=SPEECH_RUNTIME_PACKAGES) -> None:
        """What the speech runtime job leaves in the repository's parent."""
        for package in packages:
            (self.root / f"{package}_{deb_version}-1_amd64.deb").write_text(
                fixture_control(package, f"{deb_version}-1", "amd64", "speech runtime job")
            )

    def assert_published_as(self, artifacts: Path, tag: str, deb_version: str) -> None:
        served = sorted(
            f"{package}_{github_served_name(deb_version)}-1_{arch}.deb"
            for package, arch in self.expected_packages(deb_version)
        )
        self.assertEqual(sorted(path.name for path in artifacts.glob("*.deb")), served)
        # Every name the signed list carries is one GitHub serves unchanged,
        # and names a file the set holds.
        listed = [line.split(maxsplit=1)[1]
                  for line in (artifacts / "SHA256SUMS").read_text().splitlines() if line]
        self.assertEqual([name for name in listed if github_served_name(name) != name], [])
        self.assertEqual(sorted(listed), sorted(
            path.name for path in artifacts.iterdir() if path.name != "SHA256SUMS"
        ))
        manifest = json.loads((artifacts / f"tensorplate-{tag}-artifacts.json").read_text())
        for artifact in manifest["artifacts"]:
            if artifact.get("package"):
                # Recorded at the package's own version, tilde and all.
                self.assertEqual(artifact["version"], f"{deb_version}-1", artifact)

    def test_a_candidate_build_publishes_its_packages_under_the_names_github_serves(self) -> None:
        result, artifacts = self.build(RELEASE_TAG, RELEASE_DEB_VERSION, "0.2.1rc1")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("manifest verified", result.stdout)
        self.assert_published_as(artifacts, RELEASE_TAG, RELEASE_DEB_VERSION)
        # dpkg named them with the tilde; only the staged copies differ.
        self.assertTrue((self.root / "tensorplate-agent_0.2.1~rc.1-1_arm64.deb").is_file())
        self.assertTrue((artifacts / "tensorplate-agent_0.2.1.rc.1-1_arm64.deb").is_file())
        self.assert_changelog_restored()

    def test_a_final_build_publishes_its_packages_under_the_names_dpkg_gave_them(self) -> None:
        result, artifacts = self.build("v0.2.1", "0.2.1", "0.2.1")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assert_published_as(artifacts, "v0.2.1", "0.2.1")
        self.assertEqual(
            sorted(path.name for path in artifacts.glob("*.deb")),
            sorted(path.name for path in self.root.glob("*.deb")),
        )


    def test_a_build_asked_for_the_speech_runtime_publishes_the_whole_family(self) -> None:
        self.stage_speech_runtime(RELEASE_DEB_VERSION)
        self.speech_runtime = SPEECH_RUNTIME_PACKAGES
        result, artifacts = self.build(
            RELEASE_TAG, RELEASE_DEB_VERSION, "0.2.1rc1", "--with-speech-runtime")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(len(list(artifacts.glob("*.deb"))), 21)
        self.assert_published_as(artifacts, RELEASE_TAG, RELEASE_DEB_VERSION)

    def test_a_build_not_asked_for_it_leaves_a_staged_family_out(self) -> None:
        # A rehearsal's packages can outlive it on a persistent runner.
        self.stage_speech_runtime(RELEASE_DEB_VERSION)
        result, artifacts = self.build(RELEASE_TAG, RELEASE_DEB_VERSION, "0.2.1rc1")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(len(list(artifacts.glob("*.deb"))), 13)
        self.assert_published_as(artifacts, RELEASE_TAG, RELEASE_DEB_VERSION)

    def test_a_build_asked_for_it_refuses_an_incomplete_family(self) -> None:
        self.stage_speech_runtime(RELEASE_DEB_VERSION, SPEECH_RUNTIME_PACKAGES[:-1])
        # Another build's copy of the missing package does not stand in for it.
        self.stage_speech_runtime("0.2.1", SPEECH_RUNTIME_PACKAGES[-1:])
        result, artifacts = self.build(
            RELEASE_TAG, RELEASE_DEB_VERSION, "0.2.1rc1", "--with-speech-runtime")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            f"expected exactly one {SPEECH_RUNTIME_PACKAGES[-1]}_{RELEASE_DEB_VERSION}-*_amd64.deb",
            result.stdout,
        )
        self.assertFalse((artifacts / "SHA256SUMS").exists(), result.stdout)


class ReleaseStagingRouteTests(unittest.TestCase):
    """Where ReleaseStagingTests cannot run: packages reach the artifacts
    directory only through stage_release_debs, read from the builder's
    source. Weaker than running it, and it runs everywhere."""

    def test_the_build_stages_its_packages_only_through_stage_release_debs(self) -> None:
        source = BUILD_SCRIPT.read_text()
        body = re.search(r"(?ms)^stage_release_debs\(\) \{\n(.*?)^\}\n", source)
        self.assertIsNotNone(body, "build-release-artifacts.sh defines no stage_release_debs")
        outside = source.replace(body.group(0), "")
        calls = re.findall(r"(?m)^.*\bstage_release_debs\b.*$", outside)
        self.assertEqual(calls, ['stage_release_debs "$ARTIFACTS_DIR" "${debs[@]}"'], calls)
        # After the collector has gathered every package, before anything
        # records them.
        call = outside.index(calls[0])
        self.assertGreater(call, outside.rindex('debs+=("${matches[0]}")'))
        self.assertLess(call, outside.index('note "generating manifest and checksums"'))
        # And no other copy, move, install or link of a package anywhere.
        movers = [
            line for line in outside.splitlines()
            if re.search(r"^\s*(cp|mv|install|ln|rsync)\b", line)
            and re.search(r"\.deb|\bdebs\b|\$\{?deb\b", line)
        ]
        self.assertEqual(movers, [])


# Stands in for vcpkg's bootstrap script: records how it was called and
# installs the stub tool, as the real one leaves `vcpkg` in its checkout.
VCPKG_BOOTSTRAP_STUB = r"""#!/bin/sh
root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
{
  echo "--bootstrap--"
  for arg in "$@"; do echo "ARG=$arg"; done
  echo "VCPKG_FORCE_SYSTEM_BINARIES=${VCPKG_FORCE_SYSTEM_BINARIES-<unset>}"
} >>"$FAKE_VCPKG_LOG"
case "${FAKE_BOOTSTRAP:-}" in
  fail) exit 1 ;;
  no-tool) exit 0 ;;
esac
cp "$root/scripts/vcpkg-tool.sh" "$root/vcpkg"
chmod 755 "$root/vcpkg"
"""

# Stands in for the vcpkg tool. `install` simulates the binary cache: an
# archive is named for the package, the triplet and the checkout's commit and
# holds the package's name; a package without an intact one is built, and
# archived when the cache is writable; with --only-binarycaching a package
# without an intact one fails the install. A proof can be made to leave a
# file in the checkout, to move it to another commit, to fail with the cache
# directory closed to writing behind it, or to edit the manifest it reads:
# how the file is laid out, or what it says.
VCPKG_TOOL_STUB = r"""#!/bin/sh
root=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
if [ "${1:-}" = version ]; then
  [ -z "${FAKE_VCPKG_VERSION_FAILS:-}" ] || exit 1
  echo "vcpkg package management program version 2099-01-01-fixture"
  echo
  echo "See LICENSE.txt for license information."
  exit 0
fi
{
  echo "--install--"
  for arg in "$@"; do echo "ARG=$arg"; done
  echo "VCPKG_BINARY_SOURCES=${VCPKG_BINARY_SOURCES-<unset>}"
  echo "VCPKG_FORCE_SYSTEM_BINARIES=${VCPKG_FORCE_SYSTEM_BINARIES-<unset>}"
  echo "VCPKG_MAX_CONCURRENCY=${VCPKG_MAX_CONCURRENCY-<unset>}"
} >>"$FAKE_VCPKG_LOG"
[ "${1:-}" = install ] || { echo "fixture vcpkg: unexpected command" >&2; exit 2; }
only_binary=no
install_root=
manifest_root=
triplet=
feature=
for arg in "$@"; do
  case "$arg" in
    --only-binarycaching) only_binary=yes ;;
    --x-install-root=*) install_root=${arg#*=} ;;
    --x-manifest-root=*) manifest_root=${arg#*=} ;;
    --triplet=*) triplet=${arg#*=} ;;
    --x-feature=*) feature=${arg#*=} ;;
  esac
done
[ -f "$manifest_root/vcpkg.json" ] || { echo "fixture vcpkg: no manifest" >&2; exit 2; }
[ -d "$install_root" ] && [ -n "$triplet" ] || { echo "fixture vcpkg: bad arguments" >&2; exit 2; }
case "${VCPKG_BINARY_SOURCES-}" in
  "clear;files,"*",readwrite") access=readwrite ;;
  "clear;files,"*",read") access=read ;;
  *) echo "fixture vcpkg: unexpected VCPKG_BINARY_SOURCES" >&2; exit 2 ;;
esac
archives=${VCPKG_BINARY_SOURCES#clear;files,}
archives=${archives%,*}
if [ "$only_binary" = no ]; then
  case "${FAKE_VCPKG_BUILD:-}" in
    fail) echo "fixture vcpkg: build failed" >&2; exit 1 ;;
    hang) echo "$$" >"$FAKE_VCPKG_LOG.pid"; sleep 20; exit 1 ;;
    deaf) echo "$$" >"$FAKE_VCPKG_LOG.pid"; trap '' TERM; while :; do sleep 1; done ;;
    linger) echo "$$" >"$FAKE_VCPKG_LOG.pid"; trap 'sleep 1; exit 1' TERM; sleep 20; exit 1 ;;
  esac
elif [ "${FAKE_VCPKG_EDITS_MANIFEST:-}" = layout ]; then
  echo >>"$manifest_root/vcpkg.json"
elif [ "${FAKE_VCPKG_EDITS_MANIFEST:-}" = content ]; then
  sed -i 's/"fixture"/"another-fixture"/' "$manifest_root/vcpkg.json"
elif [ "${FAKE_VCPKG_EDITS_MANIFEST:-}" = beside ]; then
  echo '{}' >"$manifest_root/vcpkg-configuration.json"
fi
if [ "$only_binary" = yes ] && [ -n "${FAKE_VCPKG_PROOF_CLOSES_CACHE:-}" ]; then
  chmod 555 "$(dirname "$install_root")"
  echo "fixture vcpkg: the proof failed and closed the cache directory" >&2
  exit 1
fi
commit=$(cat "$root/.git/HEAD")
packages="gtest nlohmann-json"
[ "$feature" = streaming-grpc ] && packages="$packages grpc protobuf"
for package in $packages; do
  archive="$archives/$triplet/$package-$commit.zip"
  if [ -f "$archive" ] && [ "$(cat "$archive")" = "$package" ]; then
    echo "RESTORED=$package" >>"$FAKE_VCPKG_LOG"
  elif [ "$only_binary" = yes ]; then
    echo "MISSING=$package" >>"$FAKE_VCPKG_LOG"
    echo "fixture vcpkg: $package is not in the binary cache" >&2
    exit 1
  else
    echo "BUILT=$package" >>"$FAKE_VCPKG_LOG"
    if [ "$access" = readwrite ] && [ "${FAKE_VCPKG_UNCACHED:-}" != "$package" ]; then
      mkdir -p "$archives/$triplet"
      echo "$package" >"$archive"
    fi
  fi
done
if [ "$only_binary" = yes ]; then
  [ -z "${FAKE_VCPKG_PROOF_LEAVES:-}" ] || echo left >"$root/$FAKE_VCPKG_PROOF_LEAVES"
  [ -z "${FAKE_VCPKG_PROOF_MOVES_CHECKOUT:-}" ] ||
    git -C "$root" checkout --quiet --detach "$FAKE_VCPKG_PROOF_MOVES_CHECKOUT"
fi
[ -z "${FAKE_VCPKG_LOCKS_ROOT:-}" ] || {
  mkdir -p "$install_root/locked/kept"
  chmod 555 "$install_root/locked"
}
echo installed >"$install_root/marker"
"""

def provisioning_host_gap() -> str | None:
    """What this host lacks of the Linux userland the commands run on, if anything."""
    if not sys.platform.startswith("linux"):
        return f"the runner control script provisions a Linux runner and this is {sys.platform}"
    tools = ("bash", "git", "timeout", "flock", "sha256sum", "find", "python3")
    missing = [tool for tool in tools if shutil.which(tool) is None]
    if missing:
        return f"no {', '.join(missing)} on PATH"
    probe = subprocess.run(
        ["find", os.devnull, "-printf", ""], capture_output=True, check=False
    )
    return None if probe.returncode == 0 else "the find on PATH has no -printf"


def provisioning_skip_reason() -> str | None:
    """Why the provisioning commands cannot be run here, if they cannot.

    Being root is one reason: the commands refuse it. A host that lacks what
    they run on is the other, except under CI=true, where the cases run and
    RunnerHost fails them: a hosted run must not pass by skipping.
    """
    if os.geteuid() == 0:
        return "provisioning refuses root, so its cases run as an ordinary user"
    gap = provisioning_host_gap()
    if gap and os.environ.get("CI") != "true":
        return f"{gap}; the runner's vcpkg provisioning NOT verified here"
    return None


def uid_zero_prefix(case: unittest.TestCase) -> tuple:
    """A command prefix under which the shell really is uid 0, or skip the case.

    A user namespace maps uid 0 to the caller, so nothing here is privileged.
    """
    as_root = ("unshare", "--user", "--map-root-user")
    probe = subprocess.run(
        [*as_root, "bash", "-c", "echo $EUID"], capture_output=True, text=True, check=False
    )
    if probe.returncode != 0 or probe.stdout.strip() != "0":
        case.skipTest("no unprivileged user namespace to be uid 0 in")
    return as_root


def read_vcpkg_log(log: Path) -> list[dict]:
    """The calls the stub bootstrap and tool recorded, in order."""
    calls: list[dict] = []
    if not log.exists():
        return calls
    for line in log.read_text().splitlines():
        if line in ("--bootstrap--", "--install--"):
            calls.append(
                {"kind": line.strip("-"), "args": [], "built": [], "restored": [], "missing": []}
            )
            continue
        key, _, value = line.partition("=")
        if key == "ARG":
            calls[-1]["args"].append(value)
        elif key in ("BUILT", "RESTORED", "MISSING"):
            calls[-1][key.lower()].append(value)
        else:
            calls[-1][key] = value
    return calls


class RunnerHost:
    """A machine for the runner control script to provision, in a temp dir.

    Holds a checkout with a copy of the script and a `vcpkg.json`, a local
    git repository standing in for the vcpkg remote, and recording stubs for
    everything else the script reaches. Nothing here touches the network,
    needs root, or runs the real vcpkg.
    """

    PACKAGES = ["gtest", "nlohmann-json", "grpc", "protobuf"]
    SERVICE = "actions.runner.tensorplate-tensorplate.ubuntu.service"

    def __init__(self, case: unittest.TestCase) -> None:
        self.case = case
        gap = provisioning_host_gap()
        if gap:
            case.fail(f"{gap}; the runner's vcpkg provisioning NOT verified here")
        self.tmp = Path(tempfile.mkdtemp()).resolve()
        case.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.user = pwd.getpwuid(os.geteuid()).pw_name
        self.real_git = shutil.which("git")

        self.repo = self.tmp / "repo"
        self.script = self.repo / "tools/release/jetson-runner-control.sh"
        self.script.parent.mkdir(parents=True)
        shutil.copy(RUNNER_CONTROL, self.script)
        self.manifest = self.repo / "vcpkg.json"

        self.checkout = self.tmp / "runner/vcpkg"
        self.cache = self.tmp / "runner/cache"
        self.archives = self.cache / "archives"
        self.stamp = self.cache / "provisioned.stamp"
        self.lock = self.cache / "provision.lock"
        self.tool = self.checkout / "vcpkg"
        self.sudoers = self.tmp / "sudoers"
        self.apt_wrapper = self.tmp / "tensorplate-apt"

        self.vcpkg_log = self.tmp / "vcpkg.log"
        self.git_log = self.tmp / "git.log"
        self.sudo_log = self.tmp / "sudo.log"
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        self.write_stubs()
        case.addCleanup(self.kill_recorded_stub)

        self.remote = self.tmp / "vcpkg-remote"
        self.baseline = self.create_remote()
        self.write_manifest(self.baseline)

    def write_stubs(self) -> None:
        write_executable(
            self.bin / "git",
            "#!/bin/sh\n"
            'echo "git $*" >>"$FAKE_GIT_LOG"\n'
            # One subcommand can be made to do nothing, or to fail.
            'for arg in "$@"; do\n'
            '  [ "$arg" = "${FAKE_GIT_SKIPS:-}" ] && exit 0\n'
            '  [ "$arg" = "${FAKE_GIT_FAILS:-}" ] && exit 128\n'
            "done\n"
            f'exec "{self.real_git}" "$@"\n',
        )
        write_executable(
            self.bin / "sudo", '#!/bin/sh\necho "sudo $*" >>"$FAKE_SUDO_LOG"\nexit 1\n'
        )
        write_executable(self.bin / "systemctl", "#!/bin/sh\nexit 0\n")
        write_executable(
            self.bin / "uname",
            "#!/bin/sh\n"
            '[ "$*" = "-m" ] || exit 2\n'
            'echo "$FAKE_MACHINE"\n',
        )
        write_executable(
            self.bin / "c++",
            "#!/bin/sh\n"
            # Run without --version a compiler reports no version and fails.
            '[ "$*" = "--version" ] || exit 2\n'
            'echo "fixture-c++ (Fixture 1.0) 13.2.0"\n'
            'echo "second line"\n',
        )
        write_executable(
            self.bin / "id",
            "#!/bin/sh\n"
            'case "$*" in\n'
            '  -u) echo "$FAKE_ID_UID" ;;\n'
            '  -un) echo "$FAKE_ID_NAME" ;;\n'
            "  *) exit 2 ;;\n"
            "esac\n",
        )

    def git(self, cwd: Path, *args: str) -> str:
        """Run the real git for the fixture's own setup, outside the recording."""
        result = subprocess.run(
            [
                self.real_git,
                "-c", "user.name=fixture",
                "-c", "user.email=fixture@example.invalid",
                "-c", "init.defaultBranch=main",
                "-c", "commit.gpgsign=false",
                *args,
            ],
            cwd=cwd,
            capture_output=True,
            text=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"},
        )
        self.case.assertEqual(result.returncode, 0, f"git {args}: {result.stderr}")
        return result.stdout.strip()

    def create_remote(self) -> str:
        """The stand-in vcpkg repository; returns its first commit."""
        self.remote.mkdir()
        (self.remote / "scripts").mkdir()
        # vcpkg ignores its own tool and build directories the same way, which
        # is why a bootstrapped checkout still counts as unmodified.
        (self.remote / ".gitignore").write_text("/vcpkg\n/buildtrees/\n/packages/\n")
        # The empty file vcpkg knows its own root by.
        (self.remote / ".vcpkg-root").write_text("")
        write_executable(self.remote / "bootstrap-vcpkg.sh", VCPKG_BOOTSTRAP_STUB)
        write_executable(self.remote / "scripts/vcpkg-tool.sh", VCPKG_TOOL_STUB)
        self.git(self.remote, "init", "--quiet")
        self.git(
            self.remote,
            "add", "--", ".gitignore", ".vcpkg-root", "bootstrap-vcpkg.sh", "scripts",
        )
        self.git(self.remote, "commit", "--quiet", "-m", "fixture vcpkg")
        return self.git(self.remote, "rev-parse", "HEAD")

    def commit_remote(self, name: str) -> str:
        """Add a commit to the stand-in remote; returns it."""
        (self.remote / name).write_text(name + "\n")
        self.git(self.remote, "add", "--", name)
        self.git(self.remote, "commit", "--quiet", "-m", name)
        return self.git(self.remote, "rev-parse", "HEAD")

    def write_manifest(self, baseline: str | None, **members: object) -> None:
        manifest: dict = {"name": "fixture", "version-string": "1.0.0"}
        if baseline is not None:
            manifest["builtin-baseline"] = baseline
        manifest["features"] = {"streaming-grpc": {"dependencies": ["grpc", "protobuf"]}}
        manifest.update(members)
        self.manifest.write_text(json.dumps(manifest, indent=2) + "\n")

    def environment(self, overrides: dict) -> dict:
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("TP_JETSON_RUNNER_", "VCPKG_", "FAKE_", "GIT_"))
            and key != "CXX"
        }
        env.update(
            PATH=f"{self.bin}{os.pathsep}{env['PATH']}",
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_NOSYSTEM="1",
            # A zone away from UTC, so a stamp in local time cannot pass for UTC.
            TZ="FIX-9",
            CXX=str(self.bin / "c++"),
            FAKE_MACHINE="aarch64",
            FAKE_VCPKG_LOG=str(self.vcpkg_log),
            FAKE_GIT_LOG=str(self.git_log),
            FAKE_SUDO_LOG=str(self.sudo_log),
            TP_JETSON_RUNNER_USER=self.user,
            TP_JETSON_RUNNER_DIR=str(self.tmp / "actions-runner"),
            TP_JETSON_RUNNER_SUDOERS_FILE=str(self.sudoers),
            TP_JETSON_RUNNER_APT_WRAPPER=str(self.apt_wrapper),
            TP_JETSON_RUNNER_TIMEOUT=shutil.which("timeout"),
            TP_JETSON_RUNNER_FLOCK=shutil.which("flock"),
            TP_JETSON_RUNNER_VCPKG_DIR=str(self.checkout),
            TP_JETSON_RUNNER_VCPKG_CACHE_DIR=str(self.cache),
            TP_JETSON_RUNNER_VCPKG_GIT_URL=str(self.remote),
        )
        for key, value in overrides.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = str(value)
        return env

    def start(self, *args: str, prefix: tuple = (), **overrides: object) -> subprocess.Popen:
        """Start the script's copy in a session of its own, for `finish` to collect."""
        return subprocess.Popen(
            [*prefix, "bash", str(self.script), *args],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            cwd=self.tmp,
            env=self.environment(overrides),
            start_new_session=True,
        )

    def finish(self, process: subprocess.Popen, bound: float = 60):
        """Collect a started run; one that outlives `bound` is killed and fails."""
        try:
            stdout, stderr = process.communicate(timeout=bound)
        except subprocess.TimeoutExpired:
            for group in (process.pid, self.recorded_stub_group()):
                if group is not None and group != os.getpgid(0):
                    try:
                        os.killpg(group, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                pass
            self.case.fail(f"{' '.join(process.args)} was still running after {bound}s")
        return subprocess.CompletedProcess(process.args, process.returncode, stdout, stderr)

    def run(self, *args: str, prefix: tuple = (), bound: float = 60, **overrides: object):
        """Run the script's copy; a run that outlives `bound` is killed and fails."""
        return self.finish(self.start(*args, prefix=prefix, **overrides), bound)

    def recorded_stub(self) -> int | None:
        """The process id a hanging stub tool recorded, once it has."""
        try:
            return int(Path(f"{self.vcpkg_log}.pid").read_text())
        except (FileNotFoundError, ValueError):
            return None

    def recorded_stub_group(self) -> int | None:
        """The process group of that stub, while it is still running."""
        pid = self.recorded_stub()
        try:
            return None if pid is None else os.getpgid(pid)
        except ProcessLookupError:
            return None

    def kill_recorded_stub(self) -> None:
        """No case leaves a stub tool running behind it, however it ended."""
        group = self.recorded_stub_group()
        if group is not None and group != os.getpgid(0):
            os.killpg(group, signal.SIGKILL)

    def provision(self, **overrides: object):
        result = self.run("provision-vcpkg", **overrides)
        self.case.assertEqual(
            result.returncode, 0, f"stdout={result.stdout}\nstderr={result.stderr}"
        )
        return result

    def head(self) -> str:
        return (self.checkout / ".git/HEAD").read_text().strip()

    def clone_checkout(self) -> None:
        """Put a checkout of the remote's branch where the script expects one."""
        self.checkout.parent.mkdir(parents=True, exist_ok=True)
        self.git(self.tmp, "clone", "--quiet", str(self.remote), str(self.checkout))

    def git_calls(self) -> list[str]:
        return self.git_log.read_text().splitlines() if self.git_log.exists() else []

    def vcpkg_calls(self) -> list[dict]:
        return read_vcpkg_log(self.vcpkg_log)

    def installs(self) -> list[dict]:
        return [call for call in self.vcpkg_calls() if call["kind"] == "install"]

    def forget_calls(self) -> None:
        for log in (self.vcpkg_log, self.git_log):
            log.unlink(missing_ok=True)

    def stamp_fields(self) -> dict:
        return dict(line.split("=", 1) for line in self.stamp.read_text().splitlines())

    def cache_entries(self) -> list[str]:
        """What the cache root holds; a leftover install root would show here."""
        return sorted(path.name for path in self.cache.iterdir())

    def archive_names(self) -> list[str]:
        return sorted(
            str(path.relative_to(self.archives))
            for path in self.archives.rglob("*")
            if path.is_file()
        )


class SelfHostedRunnerPrivilegeTests(unittest.TestCase):
    """The self-hosted job may only sudo binaries the runner's grant names.

    `jetson-runner-control.sh` installs a deliberately narrow sudoers file:
    the runner account gets NOPASSWD on `apt-get` and `install` and nothing
    else. A step that sudoes anything else asks for a password no one can
    type, and the job dies mid-build -- which is how the first `v0.2.1-rc.1`
    build failed, on a step written for hosted runners that had never run on
    this one. The grant and the workflow live in different files and nothing
    bound them together, so this does.
    """

    def granted_binaries(self) -> set:
        """Binary basenames the control script's sudoers line allows."""
        text = RUNNER_CONTROL.read_text()
        line = re.search(
            r"NOPASSWD: %s, %s\\n' \"\$RUNNER_USER\" \"\$(\w+)\" \"\$(\w+)\"", text
        )
        self.assertIsNotNone(
            line,
            "could not read the NOPASSWD grant from jetson-runner-control.sh; "
            "if its shape changed, update this test rather than deleting it",
        )
        granted = set()
        for var in line.groups():
            default = re.search(rf'^{var}="\$\{{TP_[A-Z_]+:-([^}}]+)\}}"', text, re.M)
            self.assertIsNotNone(default, f"no default path for {var}")
            granted.add(PurePosixPath(default.group(1)).name)
        return granted

    def self_hosted_job(self) -> dict:
        workflow = yaml.safe_load(RELEASE_WORKFLOW.read_text())
        jobs = {
            name: job
            for name, job in workflow["jobs"].items()
            if "self-hosted" in (job.get("runs-on") or [])
        }
        self.assertEqual(
            len(jobs), 1, f"expected exactly one self-hosted job, found {sorted(jobs)}"
        )
        return next(iter(jobs.values()))


    # Repository helper scripts the job runs. A step that shells out to one
    # of these elevates whatever the helper elevates, so scanning only the
    # inline `run:` bodies reports a job as safe while it still cannot run.
    HELPERS = ("tools/ci/apt-get.sh", "tools/release/assert-static-streaming-closure.sh")

    def elevated_commands(self, body: str, step_name: str):
        """Yield (where, binary) for every sudo in a body and in helpers it calls."""
        for where, text in self.sources(body, step_name):
            # Comments discuss `sudo tee` by name; scanning them would make
            # the guard fire on prose rather than on what is run.
            code = "\n".join(line.split("#", 1)[0] for line in text.splitlines())
            # `sudo` then any flags, then the command it elevates. The shell
            # array indirection helpers use expands to sudo, so match that too.
            for match in re.finditer(
                r"(?:\bsudo\s+"
                r"|\$\{privileged\[@\]\+\"?\$\{privileged\[@\]\}\"?\}\s+"
                r"|\"\$\{privileged\[@\]\}\"\s+)"
                r"((?:-\S+\s+)*)(\S+)",
                code,
            ):
                token = match.group(2)
                # `"$APT_GET"` and friends: resolve a variable to its default.
                var = re.fullmatch(r'"?\$\{?(\w+)\}?"?', token)
                if var:
                    token = self.script_default(var.group(1)) or token
                yield where, PurePosixPath(token.strip('"')).name

    def sources(self, body: str, step_name: str):
        """The step body, plus any repository helper script it invokes."""
        yield step_name, body
        for helper in self.HELPERS:
            if helper in body:
                text = (REPO_ROOT / helper).read_text()
                yield f"{step_name} -> {helper}", self.wrapper_branch(text)

    @staticmethod
    def wrapper_branch(text: str) -> str:
        """Drop the fallback half of `if ... -x "$APT_WRAPPER"` blocks.

        On the constrained runner the wrapper is installed, so that branch is
        what runs and the `else` is unreachable there. Scanning both would
        report `timeout` as an offender on a job that never reaches it, and
        the honest question this guard asks is what the RUNNER elevates.
        """
        out, skipping, depth = [], False, 0
        for line in text.splitlines(keepends=True):
            stripped = line.strip()
            if skipping:
                if stripped == "fi" and depth == 0:
                    skipping = False
                    out.append(line)
                    continue
                if stripped.startswith("if "):
                    depth += 1
                elif stripped == "fi":
                    depth -= 1
                continue
            if stripped == "else" and out and 'APT_WRAPPER"' in "".join(out[-8:]):
                skipping, depth = True, 0
                continue
            out.append(line)
        return "".join(out)

    def script_default(self, name: str):
        """Resolve VAR="${TP_X:-/usr/bin/y}" in any scanned helper."""
        for helper in self.HELPERS:
            text = (REPO_ROOT / helper).read_text()
            hit = re.search(
                rf'^(?:readonly\s+)?{name}="\$\{{[A-Z_]+:-([^}}]+)\}}"', text, re.M
            )
            if hit:
                return hit.group(1)
        return None

    def test_the_self_hosted_job_sudoes_only_granted_binaries(self):
        granted = self.granted_binaries()
        self.assertTrue(granted, "the grant parsed as empty")
        offenders = []
        for step in self.self_hosted_job()["steps"]:
            body = step.get("run") or ""
            for where, command in self.elevated_commands(body, step.get("name")):
                if command not in granted:
                    offenders.append((where, command))
        self.assertEqual(
            offenders,
            [],
            "the self-hosted job sudoes binaries the runner grant does not "
            f"allow (granted: {sorted(granted)}). Either use a granted binary "
            "or widen the grant in jetson-runner-control.sh deliberately: "
            f"{offenders}",
        )

    # The runner's vcpkg provisioning must not need the grant, let alone widen
    # it: the cases below hold the grant to its two binaries and hold the
    # provisioning commands to no elevation at all.

    def test_the_generated_grant_names_exactly_the_wrapper_and_install(self):
        tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        sourceable = tmp / "control.sh"
        sourceable.write_text(RUNNER_CONTROL.read_text().replace('\nmain "$@"\n', "\n"))
        visudo = tmp / "visudo"
        write_executable(visudo, "#!/bin/sh\nexit 0\n")
        wrapper = tmp / "tensorplate-apt"
        write_executable(wrapper, "#!/bin/sh\nexit 0\n")
        # Drops the ownership flags so the file can be written unprivileged.
        install = tmp / "install"
        write_executable(
            install,
            '#!/bin/sh\nwhile [ "$#" -gt 2 ]; do shift; done\ncp "$1" "$2"\n',
        )
        sudoers = tmp / "sudoers"
        result = subprocess.run(
            ["bash", "-c", f'source "{sourceable}"; write_sudoers'],
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "TP_JETSON_RUNNER_USER": "fixture-runner",
                "TP_JETSON_RUNNER_SUDOERS_FILE": str(sudoers),
                "TP_JETSON_RUNNER_VISUDO": str(visudo),
                "TP_JETSON_RUNNER_INSTALL": str(install),
                "TP_JETSON_RUNNER_APT_WRAPPER": str(wrapper),
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        rules = [
            line for line in sudoers.read_text().splitlines()
            if line.strip() and not line.startswith("#")
        ]
        self.assertEqual(
            rules, [f"fixture-runner ALL=(root) NOPASSWD: {wrapper}, {install}"]
        )

    # Functions that elevate or write what the grant is made of. Nothing the
    # provisioning commands reach may be one of them.
    PRIVILEGED_FUNCTIONS = {
        "require_root",
        "install_service_if_needed",
        "ensure_runner_path",
        "write_apt_wrapper",
        "remove_apt_wrapper",
        "write_sudoers",
        "remove_sudoers",
        "print_sudoers_status",
        "cmd_on",
        "cmd_off",
        "cmd_status",
    }
    PROVISIONING_ENTRY_POINTS = ("cmd_provision_vcpkg", "cmd_vcpkg_env", "print_vcpkg_status")
    # The one place provisioning may spell `sudo`: the command it tells the
    # operator to use, a printf format the script prints and never runs.
    OPERATOR_HINT = "'sudo -u %s -H %s%s provision-vcpkg'"
    ELEVATION = re.compile(
        r"\b(?:sudo|su|pkexec|doas|runuser|setpriv|chown|chgrp|setcap|eval|source)\b"
        r"|\$\{?(?:INSTALL|VISUDO|APT_WRAPPER|APT_GET|DPKG_BIN|SUDOERS_FILE)\b"
    )

    def provisioning_functions(self) -> dict:
        """Every function the provisioning commands can reach, by name."""
        text = RUNNER_CONTROL.read_text()
        functions = dict(re.findall(r"(?ms)^(\w+)\(\) \{\n(.*?)^\}\n", text))
        reached: dict = {}
        pending = list(self.PROVISIONING_ENTRY_POINTS)
        while pending:
            name = pending.pop()
            if name in reached:
                continue
            self.assertIn(name, functions, f"jetson-runner-control.sh defines no {name}")
            reached[name] = functions[name]
            pending.extend(
                other for other in functions
                if re.search(rf"(?<![\w-]){re.escape(other)}(?![\w-])", functions[name])
            )
        return reached

    def test_the_provisioning_commands_reach_no_elevation(self):
        reached = self.provisioning_functions()
        self.assertEqual(sorted(set(reached) & self.PRIVILEGED_FUNCTIONS), [])
        hints = 0
        for name, body in reached.items():
            hints += body.count(self.OPERATOR_HINT)
            found = self.ELEVATION.findall(body.replace(self.OPERATOR_HINT, ""))
            self.assertEqual(found, [], f"{name} elevates or touches the grant")
        self.assertEqual(hints, 1, "the operator hint is spelled once, in one function")
        # What python3 runs for them is no function of the script. It may
        # import what reads and hashes a manifest, and nothing that could
        # start a process.
        program = re.search(
            r"(?ms)^readonly MANIFEST_FACTS='\n(.*?)^'\n", RUNNER_CONTROL.read_text()
        )
        self.assertIsNotNone(program, "jetson-runner-control.sh defines no MANIFEST_FACTS")
        self.assertEqual(self.ELEVATION.findall(program.group(1)), [])
        self.assertEqual(
            re.findall(r"(?m)^\s*(?:import|from)\s+(\S+)", program.group(1)),
            ["hashlib", "json", "sys"],
        )
        self.assertEqual(re.findall(r"__import__|importlib|\bexec\b", program.group(1)), [])
        # And the commands are dispatched to those functions and nothing else.
        main = re.search(r"(?ms)^main\(\) \{\n(.*?)^\}\n", RUNNER_CONTROL.read_text()).group(1)
        arms = dict(re.findall(r"(?ms)^    ([\w|-]+)\)\s*(.*?)\s*;;", main))
        commands = ("provision-vcpkg", "vcpkg-env", "status")
        self.assertEqual(
            {name: arms.get(name, "").split() for name in commands},
            {
                "provision-vcpkg": ["shift", "cmd_provision_vcpkg", '"$@"'],
                "vcpkg-env": ["shift", "cmd_vcpkg_env", '"$@"'],
                "status": ["cmd_status", "print_vcpkg_status"],
            },
        )

    def assert_sudo_was_never_run(self, host: RunnerHost) -> None:
        self.assertFalse(
            host.sudo_log.exists(),
            host.sudo_log.read_text() if host.sudo_log.exists() else "",
        )

    @unittest.skipIf(provisioning_skip_reason(), provisioning_skip_reason())
    def test_provisioning_never_runs_sudo_and_leaves_the_grant_alone(self):
        host = RunnerHost(self)
        # Stand-ins for the installed grant: provisioning must not rewrite them.
        host.sudoers.write_text("fixture grant\n")
        host.apt_wrapper.write_text("fixture wrapper\n")

        fresh = host.run("provision-vcpkg")
        again = host.run("provision-vcpkg")
        check = host.run("provision-vcpkg", "--check")
        env = host.run("vcpkg-env")
        for result in (fresh, again, check, env):
            self.assertEqual(
                result.returncode, 0, f"stdout={result.stdout}\nstderr={result.stderr}"
            )
        # The refusals name a sudo command for the operator; they must not run it.
        other = host.run("provision-vcpkg", TP_JETSON_RUNNER_USER="fixture-other-account")
        root = host.run(
            "provision-vcpkg",
            TP_JETSON_RUNNER_ID=host.bin / "id",
            FAKE_ID_UID="0",
            FAKE_ID_NAME="root",
        )
        for result in (other, root):
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("sudo -u", result.stderr)

        self.assert_sudo_was_never_run(host)
        self.assertEqual(host.sudoers.read_text(), "fixture grant\n")
        self.assertEqual(host.apt_wrapper.read_text(), "fixture wrapper\n")
        # Control: the recording sudo is the one those runs would have reached.
        subprocess.run(
            ["bash", "-c", "sudo control"], env=host.environment({}), check=False
        )
        self.assertEqual(host.sudo_log.read_text(), "sudo control\n")

    @unittest.skipIf(provisioning_skip_reason(), provisioning_skip_reason())
    def test_the_id_override_cannot_stand_in_for_the_runner_account_as_root(self):
        """The override exists so the root refusal can be tested unprivileged.

        It may only add a refusal. Here the shell really is uid 0, in a user
        namespace that maps it to the caller, and the override claims the
        runner account: the command must still refuse.
        """
        as_root = uid_zero_prefix(self)
        host = RunnerHost(self)
        result = host.run(
            "provision-vcpkg",
            prefix=as_root,
            TP_JETSON_RUNNER_ID=host.bin / "id",
            FAKE_ID_UID="1000",
            FAKE_ID_NAME=host.user,
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("never as root", result.stderr)
        self.assertFalse(host.checkout.exists())
        self.assertEqual(host.vcpkg_calls(), [])
        self.assert_sudo_was_never_run(host)


RUNNER_APT_HELPER = REPO_ROOT / "tools/ci/apt-get.sh"


class RunnerAllowlistExecutionTests(unittest.TestCase):
    """Run the real dependency-install path against the real allowlist.

    A static scan of the helper is only as good as its regexes, and one
    spelling it does not recognise makes it pass while the job cannot run.
    This drives `tools/ci/apt-get.sh` for real, through a `sudo` that permits
    exactly what the runner's sudoers file permits, so any spelling that
    elevates a denied binary fails here the way it fails on the device.
    """

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        self.log = self.tmp / "ran.log"

    def stub(self, name: str, body: str) -> Path:
        path = self.bin / name
        path.write_text("#!/bin/sh\n" + body)
        path.chmod(0o755)
        return path

    def build_stubs(self):
        """Recording stand-ins for every binary the path can reach."""
        # `timeout <flags> <bound> <cmd> <args>`: record, then run the command.
        self.stub("timeout", f'echo "timeout $*" >>"{self.log}"\nshift 3\nexec "$@"\n')
        self.stub("apt-get", f'echo "apt-get $*" >>"{self.log}"\nexit 0\n')
        self.stub("dpkg", f'echo "dpkg $*" >>"{self.log}"\nexit 0\n')
        # The generator installs the wrapper root-owned; drop the ownership
        # flags so it can run unprivileged without changing what it writes.
        self.stub(
            "install",
            'args=""\n'
            'while [ "$#" -gt 2 ]; do\n'
            '  case "$1" in -o|-g) shift 2 ;; *) args="$args $1"; shift ;; esac\n'
            'done\n'
            '# shellcheck disable=SC2086\n'
            'exec /usr/bin/install $args "$1" "$2"\n',
        )

    def generate_wrapper(self) -> Path:
        """Produce the wrapper with the real jetson-runner-control.sh code."""
        wrapper = self.tmp / "tensorplate-apt"
        # `main "$@"` runs on source; strip it so the function can be called.
        sourceable = self.tmp / "control.sh"
        sourceable.write_text(
            RUNNER_CONTROL.read_text().replace('\nmain "$@"\n', "\n")
        )
        result = subprocess.run(
            ["bash", "-c", f'source "{sourceable}"; write_apt_wrapper'],
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "TP_JETSON_RUNNER_APT_WRAPPER": str(wrapper),
                "TP_JETSON_RUNNER_TIMEOUT": str(self.bin / "timeout"),
                "TP_JETSON_RUNNER_DPKG": str(self.bin / "dpkg"),
                "TP_JETSON_RUNNER_APT_GET": str(self.bin / "apt-get"),
                "TP_JETSON_RUNNER_INSTALL": str(self.bin / "install"),
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(wrapper.exists(), result.stderr)
        return wrapper

    def allowlist_sudo(self, *permitted: Path) -> Path:
        """A `sudo` that permits exactly these paths, as the sudoers file does."""
        cases = "".join(f'  "{p}") shift; exec "{p}" "$@" ;;\n' for p in permitted)
        return self.stub(
            "sudo",
            "case \"$1\" in\n"
            + cases
            + 'esac\n'
            'echo "sudo: a password is required" >&2\n'
            "exit 1\n",
        )

    def run_helper(self, helper: Path, wrapper: Path, sudo: Path, *args):
        return subprocess.run(
            ["bash", str(helper), *args],
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "SUDO_BIN": str(sudo),
                "APT_WRAPPER_BIN": str(wrapper),
                "APT_GET_BIN": str(self.bin / "apt-get"),
                "APT_ATTEMPTS": "1",
                "APT_BOUND_SECONDS": "240",
            },
        )

    def test_the_dependency_install_path_runs_under_the_runner_allowlist(self):
        self.build_stubs()
        wrapper = self.generate_wrapper()
        sudo = self.allowlist_sudo(wrapper, self.bin / "install")

        result = self.run_helper(RUNNER_APT_HELPER, wrapper, sudo, "update")

        self.assertEqual(
            result.returncode, 0, f"stdout={result.stdout}\nstderr={result.stderr}"
        )
        ran = self.log.read_text()
        self.assertIn("apt-get update", ran, ran)
        self.assertIn("timeout -k 30 240", ran, "the bound must still wrap apt")

    def test_the_wrapper_refuses_a_subcommand_the_release_path_does_not_use(self):
        self.build_stubs()
        wrapper = self.generate_wrapper()
        result = subprocess.run(
            [str(wrapper), "240", "source", "tensorplate"],
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("refusing apt-get 'source'", result.stderr)
        self.assertNotIn("apt-get source", self.log.read_text() if self.log.exists() else "")

    def test_configure_pending_is_reachable_through_the_allowlist(self):
        self.build_stubs()
        wrapper = self.generate_wrapper()
        sudo = self.allowlist_sudo(wrapper, self.bin / "install")
        result = subprocess.run(
            [str(sudo), str(wrapper), "configure-pending"],
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("dpkg --configure -a", self.log.read_text())

    def test_elevating_timeout_directly_is_denied_by_the_allowlist(self):
        """The regression this exists for, in the shape the device sees it."""
        self.build_stubs()
        wrapper = self.generate_wrapper()
        sudo = self.allowlist_sudo(wrapper, self.bin / "install")

        # The helper as it was before the wrapper: sudo elevates `timeout`.
        mutated = self.tmp / "apt-get-direct.sh"
        text = RUNNER_APT_HELPER.read_text()
        opening = '  if ((${#privileged[@]})) && [ -x "$APT_WRAPPER" ]; then'
        try:
            start = text.index(opening)
            end = text.index("  fi", text.index("timeout -k 30", start)) + len("  fi")
        except ValueError:
            self.fail(
                "tools/ci/apt-get.sh no longer routes the bounded call through the "
                "wrapper, so this test cannot build the denied shape from it. If the "
                "helper was restructured deliberately, re-point this mutation at the "
                "new shape -- do not delete it; the static guard alone was what let "
                "an unrecognised spelling pass."
            )
        mutated.write_text(
            text[:start]
            + '  "${privileged[@]}" timeout -k 30 "$BOUND_SECONDS" "$APT_GET" "$@" || status=$?'
            + text[end:]
        )

        result = self.run_helper(mutated, wrapper, sudo, "update")

        self.assertNotEqual(
            result.returncode, 0, "elevating timeout must be denied, not silently allowed"
        )
        self.assertIn("a password is required", result.stderr)


@unittest.skipIf(provisioning_skip_reason(), provisioning_skip_reason())
class RunnerVcpkgProvisioningTests(unittest.TestCase):
    """`provision-vcpkg`, `vcpkg-env` and the vcpkg part of `status`.

    A release job will trust what these report, and a cold build of the
    streaming dependencies has no place in one, so "ready" has to mean the
    cache was shown to hold every package for this checkout's manifest. Each
    case runs the real script on a RunnerHost.
    """

    TRIPLET = "arm64-linux"

    def setUp(self):
        self.host = RunnerHost(self)

    def install_args(self, host: RunnerHost, call: dict, *extra: str) -> list[str]:
        """The arguments one install must have had, around its own install root."""
        root = next(a for a in call["args"] if a.startswith("--x-install-root="))
        return [
            "install",
            f"--vcpkg-root={host.checkout}",
            f"--x-manifest-root={host.repo}",
            root,
            f"--triplet={self.TRIPLET}",
            f"--host-triplet={self.TRIPLET}",
            "--x-feature=streaming-grpc",
            *extra,
        ]

    def assert_refused(self, result, reason: str) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(reason, result.stderr)

    OWN_VERSION_KEYS = ("version", "version-string", "version-semver", "version-date")

    def content_digest(self, manifest: str | bytes) -> str:
        """The dependencies digest of a manifest, worked out apart from the script.

        What the manifest says without the four keys that name its own
        version, as the one JSON text that sorted keys, no blanks and ASCII
        alone give.
        """
        said = json.loads(manifest)
        for key in self.OWN_VERSION_KEYS:
            said.pop(key, None)
        canonical = json.dumps(said, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
        return hashlib.sha256(canonical.encode("ascii")).hexdigest()

    def listing_sha256(self, host: RunnerHost) -> str:
        """What a stamp records of the cache: the digest of its sorted file names."""
        names = "".join(f"{name}\n" for name in host.archive_names())
        return hashlib.sha256(names.encode()).hexdigest()

    def assert_nothing_was_started(self, host: RunnerHost | None = None) -> None:
        """The refusal came before the checkout, the tool or the cache changed."""
        host = host or self.host
        self.assertEqual(host.git_calls(), [])
        self.assertEqual(host.vcpkg_calls(), [])
        self.assertFalse(host.checkout.exists())
        self.assertFalse(host.cache.exists())

    def test_a_fresh_runner_is_cloned_bootstrapped_warmed_proved_and_stamped(self):
        host = self.host
        before = time.time()
        result = host.provision()

        self.assertIn(f"git clone -- {host.remote} {host.checkout}", host.git_calls())
        self.assertFalse([call for call in host.git_calls() if " fetch " in call])
        self.assertEqual(host.head(), host.baseline)

        bootstrap, warm, proof = host.vcpkg_calls()
        self.assertEqual(bootstrap["kind"], "bootstrap")
        self.assertEqual(bootstrap["args"], ["-disableMetrics"])
        self.assertEqual(bootstrap["VCPKG_FORCE_SYSTEM_BINARIES"], "1")

        self.assertEqual(warm["args"], self.install_args(host, warm))
        self.assertEqual(warm["VCPKG_BINARY_SOURCES"], f"clear;files,{host.archives},readwrite")
        self.assertEqual(warm["built"], host.PACKAGES)
        self.assertEqual(proof["args"], self.install_args(host, proof, "--only-binarycaching"))
        self.assertEqual(proof["VCPKG_BINARY_SOURCES"], f"clear;files,{host.archives},read")
        self.assertEqual((proof["built"], proof["restored"]), ([], host.PACKAGES))
        for call in (warm, proof):
            self.assertEqual(call["VCPKG_FORCE_SYSTEM_BINARIES"], "1")
            # No job count of the script's own.
            self.assertEqual(call["VCPKG_MAX_CONCURRENCY"], "<unset>")
        # Two different throwaway roots under the cache root, both gone again.
        roots = [Path(call["args"][3].split("=", 1)[1]) for call in (warm, proof)]
        self.assertNotEqual(roots[0], roots[1])
        self.assertEqual([root.parent for root in roots], [host.cache, host.cache])
        self.assertEqual(
            host.cache_entries(), ["archives", "provision.lock", "provisioned.stamp"]
        )
        self.assertEqual(host.lock.read_bytes(), b"")

        stamp = host.stamp_fields()
        recorded = datetime.datetime.strptime(
            stamp.pop("provisioned_at"), "%Y-%m-%dT%H:%M:%SZ"
        ).replace(tzinfo=datetime.timezone.utc)
        self.assertLessEqual(abs(recorded.timestamp() - before), 120)
        # The digest is of what the manifest says without its own version: its
        # other members as one JSON text, the keys sorted and no blanks.
        manifest = host.manifest.read_text()
        self.assertEqual(
            list(json.loads(manifest)), ["name", "version-string", "builtin-baseline", "features"]
        )
        canonical = (
            '{"builtin-baseline":"%s",'
            '"features":{"streaming-grpc":{"dependencies":["grpc","protobuf"]}},'
            '"name":"fixture"}' % host.baseline
        )
        self.assertEqual(
            stamp,
            {
                "baseline": host.baseline,
                "dependencies_sha256": hashlib.sha256(canonical.encode()).hexdigest(),
                "triplet": self.TRIPLET,
                "feature": "streaming-grpc",
                "vcpkg_version": "vcpkg package management program version 2099-01-01-fixture",
                "compiler": "fixture-c++ (Fixture 1.0) 13.2.0",
                "archives_sha256": self.listing_sha256(host),
            },
        )
        self.assertEqual(len(host.archive_names()), 4)
        steps = ("vcpkg baseline", "cloning", "checking out", "bootstrapping", "building")
        for step in (*steps, "proving", "provisioned"):
            self.assertIn(f"==> {step}", result.stdout)

    def test_a_second_run_changes_nothing_it_does_not_have_to(self):
        host = self.host
        host.provision()
        archives = host.archive_names()
        host.forget_calls()

        host.provision()

        self.assertFalse(
            [call for call in host.git_calls() if " clone " in call or " fetch " in call]
        )
        bootstrap, warm, proof = host.vcpkg_calls()
        self.assertEqual(bootstrap["kind"], "bootstrap")
        self.assertEqual((warm["built"], warm["restored"]), ([], host.PACKAGES))
        self.assertEqual(proof["restored"], host.PACKAGES)
        self.assertEqual(host.archive_names(), archives)
        self.assertEqual(host.head(), host.baseline)
        self.assertEqual(host.stamp_fields()["baseline"], host.baseline)

    def test_the_compiler_is_c_plus_plus_on_path_unless_cxx_names_another(self):
        host = self.host
        write_executable(
            host.bin / "other-c++",
            '#!/bin/sh\n[ "$*" = "--version" ] || exit 2\necho "other-c++ 1.0"\n',
        )
        host.provision(CXX=None)
        self.assertEqual(host.stamp_fields()["compiler"], "fixture-c++ (Fixture 1.0) 13.2.0")
        host.provision(CXX=host.bin / "other-c++")
        self.assertEqual(host.stamp_fields()["compiler"], "other-c++ 1.0")

    def test_the_operators_concurrency_reaches_vcpkg_as_set(self):
        self.host.provision(VCPKG_MAX_CONCURRENCY="3")
        self.assertEqual(
            [call["VCPKG_MAX_CONCURRENCY"] for call in self.host.installs()], ["3", "3"]
        )

    def test_an_amd64_runner_gets_the_x64_triplet(self):
        self.host.provision(FAKE_MACHINE="x86_64")
        self.assertEqual(
            [call["args"][4:6] for call in self.host.installs()],
            [["--triplet=x64-linux", "--host-triplet=x64-linux"]] * 2,
        )
        self.assertEqual(self.host.stamp_fields()["triplet"], "x64-linux")

    def test_a_machine_without_a_triplet_is_refused(self):
        result = self.host.run("provision-vcpkg", FAKE_MACHINE="riscv64")
        self.assert_refused(result, "no vcpkg triplet")
        self.assert_nothing_was_started()

    def test_a_checkout_at_another_commit_is_moved_to_the_baseline(self):
        host = self.host
        later = host.commit_remote("later-port")
        host.clone_checkout()
        self.assertNotEqual(later, host.baseline)
        self.assertEqual(host.git(host.checkout, "rev-parse", "HEAD"), later)

        host.provision()

        self.assertEqual(host.head(), host.baseline)
        self.assertFalse(
            [call for call in host.git_calls() if " clone " in call or " fetch " in call]
        )
        self.assertEqual(host.stamp_fields()["baseline"], host.baseline)

    def test_a_baseline_the_checkout_lacks_is_fetched(self):
        host = self.host
        host.clone_checkout()
        # From the configured URL, whatever the checkout calls its origin.
        host.git(host.checkout, "remote", "set-url", "origin", str(host.tmp / "moved-away"))
        newer = host.commit_remote("newer-port")
        host.write_manifest(newer)

        result = host.provision()

        self.assertEqual(host.head(), newer)
        self.assertEqual(
            [call for call in host.git_calls() if " fetch " in call],
            [
                f"git -C {host.checkout} fetch -- {host.remote} "
                "+refs/heads/*:refs/remotes/origin/*"
            ],
        )
        self.assertIn(
            f"==> fetching {host.remote}: the checkout does not hold {newer}", result.stdout
        )
        self.assertEqual(host.stamp_fields()["baseline"], newer)

    def test_a_baseline_the_remote_lacks_is_refused(self):
        host = self.host
        host.write_manifest("0123456789abcdef0123456789abcdef01234567")
        result = host.run("provision-vcpkg")
        self.assert_refused(result, "is not a commit of")
        self.assertEqual(host.vcpkg_calls(), [])
        self.assertFalse(host.stamp.exists())

    def checkout_changes(self) -> tuple:
        """Ways a checkout stops being the commit it is at."""

        def modified(host):
            (host.checkout / "bootstrap-vcpkg.sh").write_text("#!/bin/sh\nexit 0\n")

        def untracked(host):
            (host.checkout / "ports-local").write_text("an overlay nobody reviewed\n")

        def untracked_and_hidden_by_configuration(host):
            untracked(host)
            host.git(host.checkout, "config", "status.showUntrackedFiles", "no")

        return modified, untracked, untracked_and_hidden_by_configuration

    def test_a_checkout_with_local_changes_is_refused(self):
        for change in self.checkout_changes():
            with self.subTest(change=change.__name__):
                host = RunnerHost(self)
                host.commit_remote("later-port")
                host.clone_checkout()
                head = host.head()
                change(host)
                result = host.run("provision-vcpkg")
                self.assert_refused(result, "local modifications or untracked files")
                self.assertEqual(host.head(), head)
                self.assertEqual(host.vcpkg_calls(), [])
                # The run held the lock while it looked, and made nothing else.
                self.assertEqual(host.cache_entries(), ["provision.lock"])

    def test_a_checkout_git_cannot_inspect_is_refused(self):
        host = self.host
        host.provision()
        host.forget_calls()
        for arguments in (("provision-vcpkg",), ("provision-vcpkg", "--check")):
            with self.subTest(arguments=arguments):
                result = host.run(*arguments, FAKE_GIT_FAILS="status")
                self.assert_refused(result, "cannot inspect")
                self.assertEqual(host.vcpkg_calls(), [])

    def test_a_directory_that_is_not_a_checkout_is_not_replaced(self):
        host = self.host
        host.checkout.mkdir(parents=True)
        (host.checkout / "notes").write_text("kept\n")
        result = host.run("provision-vcpkg")
        self.assert_refused(result, "is not a git checkout")
        self.assertEqual([p.name for p in host.checkout.iterdir()], ["notes"])
        self.assertEqual(host.vcpkg_calls(), [])

    def test_a_checkout_of_something_other_than_vcpkg_is_not_moved(self):
        host = self.host
        host.checkout.mkdir(parents=True)
        host.git(host.checkout, "init", "--quiet")
        (host.checkout / "notes").write_text("kept\n")
        host.git(host.checkout, "add", "--", "notes")
        host.git(host.checkout, "commit", "--quiet", "-m", "not vcpkg")
        head = host.head()

        result = host.run("provision-vcpkg")

        self.assert_refused(result, "is not a vcpkg checkout")
        self.assertEqual(host.head(), head)
        self.assertEqual((host.checkout / "notes").read_text(), "kept\n")
        # git was asked for its variable names and not run in that repository.
        self.assertEqual(
            (host.git_calls(), host.vcpkg_calls()), (["git rev-parse --local-env-vars"], [])
        )
        self.assertEqual(host.cache_entries(), ["provision.lock"])

    NOT_A_MANIFEST = (
        "python3 could not read {manifest} (exit {status}): a manifest is one JSON object in "
        "UTF-8 with no byte order mark that names no key twice and holds no number other than "
        "an integer"
    )
    PYTHON_FAILED = "python3 failed (exit {status}) before it answered for {manifest}"
    CONFIGURED_BESIDE = (
        "{configuration} is not covered by the dependencies digest and vcpkg would read it; "
        "move what it says into {manifest} as its vcpkg-configuration member"
    )
    NO_BASELINE = "{manifest} does not name a builtin-baseline of 40 lowercase hex digits"
    NO_DIGEST = "cannot compute the dependencies digest of {manifest}"
    UNREADABLE = "cannot read {manifest}; run this command from a repository checkout"
    NO_PYTHON = "missing required command: python3, which reads {manifest}"

    def reason_for(self, host: RunnerHost, reason: str) -> str:
        return reason.format(
            manifest=host.manifest,
            status=1,
            configuration=host.manifest.with_name("vcpkg-configuration.json"),
        )

    def assert_provisions_nothing(self, host: RunnerHost, reason: str, **overrides) -> None:
        """A runner that was never provisioned is not provisioned from this manifest."""
        said = self.reason_for(host, reason)
        result = host.run("provision-vcpkg", bound=10, **overrides)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((result.stdout, result.stderr), ("", f"error: {said}\n"))
        self.assert_nothing_was_started(host)

    def assert_answers_for_nothing(self, host: RunnerHost, reason: str, **overrides) -> None:
        """A runner that was ready takes no key from this manifest: it says why, and no more."""
        said = self.reason_for(host, reason)
        stamp = host.stamp.read_bytes()
        env = host.run("vcpkg-env", bound=10, **overrides)
        self.assertNotEqual(env.returncode, 0)
        self.assertEqual(
            (env.stdout, env.stderr),
            ("", f"error: the runner's vcpkg is not ready for this checkout: {said}\n"),
        )
        check = host.run("provision-vcpkg", "--check", bound=10, **overrides)
        self.assertNotEqual(check.returncode, 0)
        self.assertEqual((check.stdout, check.stderr), ("", f"error: {said}\n"))
        lines = self.status(host, bound=10, **overrides)
        self.assertEqual(
            (lines[1], lines[7]),
            (f"vcpkg_baseline: unreadable ({said})", f"vcpkg_cache_ready: no ({said})"),
        )
        self.assertEqual((host.git_calls(), host.vcpkg_calls()), ([], []))
        self.assertEqual(host.stamp.read_bytes(), stamp)

    def unusable_manifests(self, host: RunnerHost) -> dict:
        """Each manifest no key may be taken from: its bytes, and the reason to give.

        Where an object names a key twice, the second is what the runner was
        provisioned for: a reader that let the last one win would call the
        runner ready.
        """
        usable = host.manifest.read_text()
        name = '"name": "fixture"'
        own = '"version-string": "1.0.0"'
        baseline = f'"builtin-baseline": "{host.baseline}"'
        dependencies = '"dependencies": ["grpc", "protobuf"]'
        override = '"version": "1.60.0"'
        for member in (name, own, baseline, dependencies, override):
            self.assertEqual(usable.count(member), 1, member)
        self.assertTrue(usable.endswith("}]}"), usable)
        other = "0123456789abcdef0123456789abcdef01234567"
        good = host.baseline

        def twice(member: str, first: str) -> bytes:
            return usable.replace(member, f"{first}, {member}").encode()

        def baseline_of(value: str) -> bytes:
            return usable.replace(baseline, f'"builtin-baseline": {value}').encode()

        not_a_manifest = {
            "cut short": usable[: len(usable) // 2].encode(),
            "empty": b"",
            "with a trailing comma": (usable[:-1] + ",}").encode(),
            "with a comment": b"// the manifest\n" + usable.encode(),
            "in single quotes": usable.replace('"', "'").encode(),
            "followed by a second document": (usable + usable).encode(),
            "an array": f"[{usable}]".encode(),
            "a string": json.dumps(usable).encode(),
            "null": b"null",
            "a name given twice": twice(name, '"name": "another-fixture"'),
            "a name given twice with one value": twice(name, name),
            "a baseline given twice": twice(baseline, f'"builtin-baseline": "{other}"'),
            "its own version given twice": twice(own, '"version-string": "0.9.0"'),
            "a feature's dependencies given twice": twice(dependencies, '"dependencies": ["grpc"]'),
            "an override's version given twice": twice(override, '"version": "1.59.0"'),
            "a byte that is not UTF-8": usable.encode().replace(b"fixture", b"fix\xffture"),
            "UTF-16": usable.encode("utf-16-le"),
            "UTF-16 behind a byte order mark": usable.encode("utf-16"),
            "UTF-8 behind a byte order mark": b"\xef\xbb\xbf" + usable.encode(),
            "a surrogate pair encoded as UTF-8": usable.encode().replace(
                b"fixture", b"fix\xed\xa0\x80\xed\xb0\x80ture"
            ),
            "a line break inside a string": usable.replace(name, '"name": "fix\nture"').encode(),
            "a tab inside a string": usable.replace(name, '"name": "fix\tture"').encode(),
            "lists nested deeper than python reads": usable.replace(
                name, f'{name}, "nested": {"[" * self.TOO_DEEP}{"]" * self.TOO_DEEP}'
            ).encode(),
            "its own version not a number": usable.replace(own, '"version-string": NaN').encode(),
            "its own version without end": usable.replace(
                own, '"version-string": -Infinity'
            ).encode(),
            "its own version a fraction": usable.replace(own, '"version-string": 1.5').encode(),
            "a number with an exponent": usable.replace(
                own, f'{own}, "port-version": 1e0'
            ).encode(),
        }
        no_baseline = {
            "no baseline": usable.replace(f"{baseline}, ", "").encode(),
            "a baseline in upper case": baseline_of(f'"{good.upper()}"'),
            "an abbreviated baseline": baseline_of(f'"{good[:39]}"'),
            "a baseline one digit long": baseline_of(f'"{good}0"'),
            "a baseline that is not hex": baseline_of(f'"{good[:39]}g"'),
            "a branch name for a baseline": baseline_of('"master"'),
            "an empty baseline": baseline_of('""'),
            "a baseline behind a blank": baseline_of(f'" {good}"'),
            "a baseline before a carriage return": baseline_of(f'"{good}\\r"'),
            "a baseline before a line break": baseline_of(f'"{good}\\n"'),
            "a baseline before a null character": baseline_of(f'"{good}\\u0000"'),
            "a baseline in quotes of its own": baseline_of(f'"\\"{good}\\""'),
            "a baseline that is a number": baseline_of("123"),
            "a baseline that is null": baseline_of("null"),
            "a baseline in a list": baseline_of(f'["{good}"]'),
            "a baseline in an object": baseline_of(f'{{"commit": "{good}"}}'),
        }
        self.assertNotIn(baseline.encode(), no_baseline["no baseline"])
        return {
            **{case: (text, self.NOT_A_MANIFEST) for case, text in not_a_manifest.items()},
            **{case: (text, self.NO_BASELINE) for case, text in no_baseline.items()},
        }

    TOO_DEEP = 100000

    def readable_neighbours(self, host: RunnerHost) -> dict:
        """What stands next to four of those manifests and is a manifest: other content, read.

        Each differs from its neighbour above by the one thing that is refused there.
        """
        usable = host.manifest.read_text()
        name = '"name": "fixture"'
        self.assertEqual(usable.count(name), 1)
        return {
            "U+10000 in UTF-8": usable.replace(name, '"name": "fix\U00010000ture"').encode(),
            "a line break as an escape": usable.replace(name, '"name": "fix\\nture"').encode(),
            "a tab as an escape": usable.replace(name, '"name": "fix\\tture"').encode(),
            "lists nested ten deep": usable.replace(
                name, f'{name}, "nested": {"[" * 10}{"]" * 10}'
            ).encode(),
        }

    def with_an_override(self, host: RunnerHost) -> RunnerHost:
        """The host with a manifest on one line that also holds an override."""
        host.write_manifest(host.baseline, overrides=[{"name": "grpc", "version": "1.60.0"}])
        host.manifest.write_text(json.dumps(json.loads(host.manifest.read_text())))
        return host

    def test_a_manifest_no_key_can_be_taken_from_is_refused(self):
        """Not JSON, not an object, a key named twice, not UTF-8, no full baseline.

        A guess at any of them would let a runner answer ready for a manifest
        it was not provisioned for.
        """
        ready = self.with_an_override(self.host)
        ready.provision()
        usable = ready.manifest.read_bytes()
        cases = self.unusable_manifests(ready)
        self.assertEqual(len(cases), 43)
        for case, (manifest, reason) in cases.items():
            with self.subTest(manifest=case):
                ready.forget_calls()
                ready.manifest.write_bytes(manifest)
                self.assert_answers_for_nothing(ready, reason)
                # A runner of its own for each: one that was provisioned from
                # its manifest after all must not fail the cases after it.
                fresh = self.with_an_override(RunnerHost(self))
                fresh.manifest.write_bytes(self.unusable_manifests(fresh)[case][0])
                self.assert_provisions_nothing(fresh, reason)
        # Control: nothing but its manifest stood in the runner's way.
        ready.manifest.write_bytes(usable)
        self.assert_ready(ready)
        # Control: what the last of them were refused for is the one thing
        # each holds. Without it the manifest is read, as another one.
        other = f"provisioned for another dependencies digest than {ready.manifest} has now"
        for case, manifest in self.readable_neighbours(ready).items():
            with self.subTest(manifest=case):
                ready.manifest.write_bytes(manifest)
                lines = self.status(ready)
                self.assertEqual(
                    (lines[1], lines[7]),
                    (f"vcpkg_baseline: {ready.baseline}", f"vcpkg_cache_ready: no ({other})"),
                )

    def test_a_manifest_that_cannot_be_read_is_refused_and_never_waited_on(self):
        """A pipe where the manifest should be would be waited on for good."""

        def gone(host):
            host.manifest.unlink()

        def closed_to_this_account(host):
            host.manifest.chmod(0)

        def a_directory(host):
            host.manifest.unlink()
            host.manifest.mkdir()

        def a_pipe(host):
            host.manifest.unlink()
            os.mkfifo(host.manifest)

        for change in (gone, closed_to_this_account, a_directory, a_pipe):
            with self.subTest(manifest=change.__name__):
                fresh, ready = RunnerHost(self), self.provisioned()
                change(fresh)
                self.assert_provisions_nothing(fresh, self.UNREADABLE)
                change(ready)
                self.assert_answers_for_nothing(ready, self.UNREADABLE)

    def test_a_configuration_file_beside_the_manifest_is_refused(self):
        """vcpkg reads a vcpkg-configuration.json beside vcpkg.json: registries, overlay ports.

        Those decide what is built, and the dependencies digest is of
        vcpkg.json alone: with such a file a runner would answer ready for
        packages its proof never restored. Nothing is read from the file, so
        whatever is there by that name is a refusal.
        """
        registry = {"default-registry": {"kind": "builtin", "baseline": "1" * 40}}

        def a_file(beside):
            beside.write_text(json.dumps(registry))

        def an_empty_file(beside):
            beside.write_text("")

        def a_directory(beside):
            beside.mkdir()

        def a_link_to_nothing(beside):
            beside.symlink_to("no-such-file.json")

        for put in (a_file, an_empty_file, a_directory, a_link_to_nothing):
            with self.subTest(configuration=put.__name__):
                fresh, ready = RunnerHost(self), self.provisioned()
                for host, refuses in (
                    (fresh, self.assert_provisions_nothing),
                    (ready, self.assert_answers_for_nothing),
                ):
                    beside = host.repo / "vcpkg-configuration.json"
                    put(beside)
                    refuses(host, self.CONFIGURED_BESIDE)
                    # Control: that file was all that stood in the way.
                    if beside.is_dir():
                        beside.rmdir()
                    else:
                        beside.unlink()
                self.assert_ready(ready)
                fresh.provision()

    def test_a_configuration_file_that_appears_during_the_build_is_not_stamped(self):
        host = self.host
        result = host.run("provision-vcpkg", FAKE_VCPKG_EDITS_MANIFEST="beside")
        self.assert_refused(result, self.reason_for(host, self.CONFIGURED_BESIDE))
        self.assertEqual(len(host.installs()), 2)
        self.assertFalse(host.stamp.exists())

    def test_another_account_is_refused_with_the_command_to_use(self):
        host = self.host
        for arguments in (("provision-vcpkg",), ("provision-vcpkg", "--check")):
            with self.subTest(arguments=arguments):
                result = host.run(*arguments, TP_JETSON_RUNNER_USER="fixture-other-account")
                self.assertNotEqual(result.returncode, 0, result.stdout)
                # The far side of sudo has a clean environment: without the
                # variable there, the command would refuse again.
                self.assertEqual(
                    result.stderr.strip().rpartition("; use: ")[2],
                    "sudo -u fixture-other-account -H "
                    "env TP_JETSON_RUNNER_USER=fixture-other-account "
                    f"{host.script} {' '.join(arguments)}",
                )
                self.assert_nothing_was_started()
                # The default account needs no variable, and gets none.
                result = host.run(
                    *arguments,
                    TP_JETSON_RUNNER_USER=None,
                    TP_JETSON_RUNNER_ID=host.bin / "id",
                    FAKE_ID_UID="1000",
                    FAKE_ID_NAME="fixture-other-account",
                )
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertEqual(
                    result.stderr.strip().rpartition("; use: ")[2],
                    f"sudo -u gha-runner -H {host.script} {' '.join(arguments)}",
                )
                self.assertEqual((host.git_calls(), host.vcpkg_calls()), ([], []))
        # Who is asking is settled before anything is read, the manifest included.
        host.manifest.unlink()
        result = host.run("provision-vcpkg", TP_JETSON_RUNNER_USER="fixture-other-account")
        self.assert_refused(result, "runs as the runner account fixture-other-account")
        self.assertNotIn("cannot read", result.stderr)

    def test_root_is_refused_with_the_command_to_use(self):
        host = self.host
        result = host.run(
            "provision-vcpkg",
            TP_JETSON_RUNNER_ID=host.bin / "id",
            FAKE_ID_UID="0",
            FAKE_ID_NAME=host.user,
        )
        self.assert_refused(result, "never as root")
        self.assertIn(
            f"sudo -u {host.user} -H env TP_JETSON_RUNNER_USER={host.user} "
            f"{host.script} provision-vcpkg",
            result.stderr,
        )
        self.assert_nothing_was_started()

    def test_an_unanswered_identity_is_refused(self):
        host = self.host
        for answers in ({"FAKE_ID_UID": "1000"}, {"FAKE_ID_NAME": host.user}):
            with self.subTest(answers=sorted(answers)):
                failing = host.bin / "id-failing"
                # Answers one question and fails the other.
                write_executable(
                    failing,
                    "#!/bin/sh\n"
                    'case "$*" in\n'
                    '  -u) [ -n "${FAKE_ID_UID:-}" ] && echo "$FAKE_ID_UID" ;;\n'
                    '  -un) [ -n "${FAKE_ID_NAME:-}" ] && echo "$FAKE_ID_NAME" ;;\n'
                    "esac\n",
                )
                result = host.run("provision-vcpkg", TP_JETSON_RUNNER_ID=failing, **answers)
                self.assert_refused(result, "cannot read the current user")
                self.assert_nothing_was_started()

    def test_unknown_arguments_are_refused(self):
        # On a ready runner, where each command would otherwise succeed.
        host = self.provisioned()
        for arguments, reason in (
            (("provision-vcpkg", "--force"), "no argument other than --check"),
            (("provision-vcpkg", "--check", "--check"), "no argument other than --check"),
            (("provision-vcpkg", "--check", "extra"), "no argument other than --check"),
            (("vcpkg-env", "--check"), "takes no arguments"),
        ):
            with self.subTest(arguments=arguments):
                result = host.run(*arguments)
                self.assert_refused(result, reason)
                self.assertEqual(result.stdout, "")
                self.assertEqual((host.git_calls(), host.vcpkg_calls()), ([], []))

    def test_what_provisioning_needs_is_checked_before_anything_changes(self):
        host = self.host
        missing = host.tmp / "missing"
        write_executable(host.bin / "silent", "#!/bin/sh\nexit 0\n")
        bound = "TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT"
        cases = {
            "no compiler": ({"CXX": missing}, "no C++ compiler"),
            "compiler without a version": ({"CXX": host.bin / "silent"}, "no C++ compiler"),
            "no timeout": ({"TP_JETSON_RUNNER_TIMEOUT": missing}, "timeout not found"),
            "no flock": ({"TP_JETSON_RUNNER_FLOCK": missing}, "flock not found"),
            "zero bound": ({bound: "0"}, "whole number of seconds"),
            "bound with a unit": ({bound: "90m"}, "whole number of seconds"),
            "negative bound": ({bound: "-1"}, "whole number of seconds"),
            "relative checkout": ({"TP_JETSON_RUNNER_VCPKG_DIR": "runner/vcpkg"}, "absolute paths"),
            "cache with a comma": (
                {"TP_JETSON_RUNNER_VCPKG_CACHE_DIR": host.tmp / "ca,che"}, "absolute paths"),
            "checkout with a semicolon": (
                {"TP_JETSON_RUNNER_VCPKG_DIR": host.tmp / "vc;pkg"}, "absolute paths"),
            "cache with an empty component": (
                {"TP_JETSON_RUNNER_VCPKG_CACHE_DIR": f"{host.tmp}//cache"}, "absolute paths"),
            "cache that is only separators": (
                {"TP_JETSON_RUNNER_VCPKG_CACHE_DIR": "//"}, "absolute paths"),
            "cache with a trailing separator": (
                {"TP_JETSON_RUNNER_VCPKG_CACHE_DIR": f"{host.tmp}/cache/"}, "absolute paths"),
            "checkout through a parent": (
                {"TP_JETSON_RUNNER_VCPKG_DIR": f"{host.tmp}/runner/../vcpkg"}, "absolute paths"),
            "checkout through itself": (
                {"TP_JETSON_RUNNER_VCPKG_DIR": f"{host.tmp}/./vcpkg"}, "absolute paths"),
        }
        for name, (overrides, reason) in cases.items():
            with self.subTest(case=name):
                result = host.run("provision-vcpkg", **overrides)
                self.assert_refused(result, reason)
                self.assert_nothing_was_started()
                self.assertEqual(
                    sorted(p.name for p in host.tmp.iterdir()), ["bin", "repo", "vcpkg-remote"]
                )

    def test_an_answer_python_does_not_give_in_full_is_no_key(self):
        """What bash takes from python3 is a commit id and a SHA-256, each whole, or nothing."""
        ready = self.provisioned()
        digest = ready.stamp_fields()["dependencies_sha256"]

        def answered_by_a_stand_in(host: RunnerHost) -> RunnerHost:
            write_executable(
                host.bin / "python3",
                "#!/bin/sh\n"
                'printf "%s" "${FAKE_PYTHON_ANSWER-}"\n'
                'exit "${FAKE_PYTHON_EXIT:-0}"\n',
            )
            return host

        answered_by_a_stand_in(ready)
        # Control: the stand-in is what the commands ask, and a whole answer is taken.
        self.assert_ready(ready, FAKE_PYTHON_ANSWER=f'"{ready.baseline}"\n{digest}\n')
        ready.forget_calls()
        # Status 1 is the program's own refusal; any other is not about the manifest.
        not_read, failed = self.NOT_A_MANIFEST, self.PYTHON_FAILED
        answers = {
            "a whole answer and exit status 1": ('"{baseline}"\n{digest}\n', 1, not_read),
            "nothing and exit status 2": ("", 2, failed.replace("{status}", "2")),
            "a whole answer and the exit status of a kill": (
                '"{baseline}"\n{digest}\n', 137, failed.replace("{status}", "137")),
            "nothing": ("", 0, self.NO_BASELINE),
            "the digest alone": ("{digest}\n", 0, self.NO_BASELINE),
            "the digest first": ('{digest}\n"{baseline}"\n', 0, self.NO_BASELINE),
            "both on one line": ('"{baseline}" {digest}\n', 0, self.NO_BASELINE),
            "a baseline that is not JSON text": ("{baseline}\n{digest}\n", 0, self.NO_BASELINE),
            "something before the baseline": ('x"{baseline}"\n{digest}\n', 0, self.NO_BASELINE),
            "something after the baseline": ('"{baseline}"x\n{digest}\n', 0, self.NO_BASELINE),
            "the baseline alone": ('"{baseline}"\n', 0, self.NO_DIGEST),
            "a digest one digit short": ('"{baseline}"\n{short}\n', 0, self.NO_DIGEST),
            "a digest one digit long": ('"{baseline}"\n{digest}0\n', 0, self.NO_DIGEST),
            "a digest in upper case": ('"{baseline}"\n{upper}\n', 0, self.NO_DIGEST),
            "a digest that is not hex": ('"{baseline}"\n{other}\n', 0, self.NO_DIGEST),
            "a digest in quotes": ('"{baseline}"\n"{digest}"\n', 0, self.NO_DIGEST),
            "a blank line between the two": ('"{baseline}"\n\n{digest}\n', 0, self.NO_DIGEST),
            "a third line": ('"{baseline}"\n{digest}\nmore\n', 0, self.NO_DIGEST),
        }
        self.assertEqual(len(answers), 18)
        self.assertNotEqual(digest.upper(), digest)
        for case, (answer, status, reason) in answers.items():
            with self.subTest(python=case):
                ready.forget_calls()
                # A runner of its own for each, as for the manifests above.
                fresh = answered_by_a_stand_in(RunnerHost(self))
                for host, refuses in (
                    (ready, self.assert_answers_for_nothing),
                    (fresh, self.assert_provisions_nothing),
                ):
                    refuses(
                        host,
                        reason,
                        FAKE_PYTHON_ANSWER=answer.format(
                            baseline=host.baseline, digest=digest, short=digest[:63],
                            upper=digest.upper(), other="g" * 64,
                        ),
                        FAKE_PYTHON_EXIT=status,
                    )

    def path_without_python(self, host: RunnerHost) -> tuple:
        """A PATH that has every program of this one but python, and where to add one."""
        elsewhere = host.tmp / "path"
        elsewhere.mkdir()
        for directory in os.get_exec_path():
            if not os.path.isdir(directory):
                continue
            for program in Path(directory).iterdir():
                link = elsewhere / program.name
                if not program.name.startswith("python") and not os.path.lexists(link):
                    link.symlink_to(program)
        return f"{host.bin}{os.pathsep}{elsewhere}", elsewhere / "python3"

    def test_without_python_no_key_is_taken(self):
        fresh, ready = self.host, self.provisioned()
        for host, refuses in (
            (fresh, self.assert_provisions_nothing),
            (ready, self.assert_answers_for_nothing),
        ):
            path, python = self.path_without_python(host)
            self.assertIsNone(shutil.which("python3", path=path))
            refuses(host, self.NO_PYTHON, PATH=path)
            # Control: python3 was all that PATH lacked.
            python.symlink_to(shutil.which("python3"))
            if host is ready:
                self.assert_ready(ready, PATH=path)
            else:
                host.provision(PATH=path)

    def test_python_reads_the_manifest_apart_from_the_callers_directory_and_environment(self):
        """`python3 -c` takes its modules from the caller's directory first, and from PYTHONPATH.

        A json or hashlib found there would answer for the manifest in place
        of python's own.
        """
        planted = "raise SystemExit('this module is not python\\'s own')\n"
        for place in ("the directory the command is run from", "PYTHONPATH"):
            with self.subTest(planted_in=place):
                host = self.provisioned()
                directory = host.tmp if place != "PYTHONPATH" else host.tmp / "planted"
                directory.mkdir(exist_ok=True)
                for module in ("json.py", "hashlib.py"):
                    (directory / module).write_text(planted)
                overrides = {"PYTHONPATH": directory} if place == "PYTHONPATH" else {}
                # Control: python3 run the plain way does find the planted module.
                plain = subprocess.run(
                    ["python3", "-c", "import json"],
                    cwd=host.tmp, env=host.environment(overrides),
                    capture_output=True, text=True, check=False,
                )
                self.assertIn("not python's own", plain.stderr)
                self.assert_ready(host, **overrides)
                check = host.run("provision-vcpkg", "--check", **overrides)
                self.assertEqual(check.returncode, 0, check.stderr)

    def test_python_does_not_load_what_the_account_keeps_in_its_user_site(self):
        """A runner account keeps what `pip install --user` puts in its home between jobs.

        A .pth file or a usercustomize module there runs in every python3 the
        account starts, before the program does. -I leaves the user site out.
        """
        planted = {
            "a .pth file": ("planted.pth", "import os; os._exit(3)\n"),
            "a usercustomize module": ("usercustomize.py", "import os\nos._exit(3)\n"),
        }
        for case, (name, code) in planted.items():
            with self.subTest(planted=case):
                host = self.provisioned()
                overrides = {
                    "HOME": host.tmp / "home", "PYTHONUSERBASE": None, "PYTHONNOUSERSITE": None,
                }

                def plain(program: str):
                    return subprocess.run(
                        ["python3", "-c", program],
                        cwd=host.tmp, env=host.environment(overrides),
                        capture_output=True, text=True, check=False,
                    )

                asked = plain(
                    "import site; print(site.ENABLE_USER_SITE, site.getusersitepackages())"
                )
                self.assertEqual(asked.returncode, 0, asked.stderr)
                enabled, _, user_site = asked.stdout.strip().partition(" ")
                if enabled != "True":
                    reason = "the python3 on PATH has no user site; its isolation NOT verified here"
                    if os.environ.get("CI") == "true":
                        self.fail(reason)
                    self.skipTest(reason)
                self.assertTrue(Path(user_site).is_relative_to(host.tmp / "home"), user_site)
                Path(user_site).mkdir(parents=True)
                (Path(user_site) / name).write_text(code)
                # Control: python3 run the plain way does run what was planted.
                self.assertEqual(plain("pass").returncode, 3)
                self.assert_ready(host, **overrides)
                check = host.run("provision-vcpkg", "--check", **overrides)
                self.assertEqual(check.returncode, 0, check.stderr)

    def test_a_tool_that_does_not_report_its_version_is_not_built_with(self):
        host = self.host
        result = host.run("provision-vcpkg", FAKE_VCPKG_VERSION_FAILS="1")
        self.assert_refused(result, "cannot read the version")
        self.assertEqual(host.installs(), [])
        self.assertFalse(host.stamp.exists())

    def test_a_failed_bootstrap_leaves_no_stamp(self):
        host = self.host
        result = host.run("provision-vcpkg", FAKE_BOOTSTRAP="fail")
        self.assert_refused(result, "bootstrap failed")
        self.assertEqual(host.installs(), [])
        self.assertFalse(host.stamp.exists())

    def test_a_bootstrap_that_makes_no_tool_cannot_pass_on_an_older_one(self):
        host = self.host
        host.provision()
        self.assertTrue(host.tool.exists())
        host.forget_calls()

        result = host.run("provision-vcpkg", FAKE_BOOTSTRAP="no-tool")

        self.assert_refused(result, "left no executable")
        self.assertEqual(host.installs(), [])
        self.assertFalse(host.stamp.exists())

    def test_a_checkout_git_did_not_move_is_not_built_from(self):
        host = self.host
        host.commit_remote("later-port")
        host.clone_checkout()
        result = host.run("provision-vcpkg", FAKE_GIT_SKIPS="checkout")
        self.assert_refused(result, "after the checkout")
        self.assertEqual(host.vcpkg_calls(), [])
        self.assertFalse(host.stamp.exists())

        # The stamp of an earlier run is gone before the move is attempted.
        host.provision()
        host.forget_calls()
        host.write_manifest(host.commit_remote("newer-port"))
        result = host.run("provision-vcpkg", FAKE_GIT_SKIPS="checkout")
        self.assert_refused(result, "after the checkout")
        self.assertEqual(host.vcpkg_calls(), [])
        self.assertFalse(host.stamp.exists())

    def test_a_checkout_git_could_not_write_in_full_is_not_built_from(self):
        """git checkout exits 0 when it could not create a file of the commit."""
        host = self.host
        host.provision()
        host.forget_calls()
        (host.remote / "scripts/port.cmake").write_text("a port of the new baseline\n")
        host.git(host.remote, "add", "--", "scripts/port.cmake")
        host.git(host.remote, "commit", "--quiet", "-m", "a port")
        host.write_manifest(host.git(host.remote, "rev-parse", "HEAD"))
        scripts = host.checkout / "scripts"
        scripts.chmod(0o555)
        self.addCleanup(scripts.chmod, 0o755)

        result = host.run("provision-vcpkg")

        self.assertIn("unable to create file scripts/port.cmake", result.stderr)
        self.assert_refused(result, "local modifications or untracked files")
        self.assertNotIn("==> bootstrapping", result.stdout)
        self.assertEqual(host.vcpkg_calls(), [])
        self.assertFalse(host.stamp.exists())

    def test_a_checkout_that_changed_under_the_build_is_not_stamped(self):
        """The build runs for hours in a checkout the account can write."""
        for change in ("a file left in it", "moved to another commit"):
            with self.subTest(change=change):
                host = RunnerHost(self)
                later = host.commit_remote("later-port")
                if change == "a file left in it":
                    overrides = {"FAKE_VCPKG_PROOF_LEAVES": "ports-local"}
                    reason = "local modifications or untracked files"
                else:
                    overrides = {"FAKE_VCPKG_PROOF_MOVES_CHECKOUT": later}
                    reason = f"is {later} after the build, not {host.baseline}"
                result = host.run("provision-vcpkg", **overrides)
                self.assert_refused(result, reason)
                self.assertEqual(len(host.installs()), 2)
                self.assertNotIn("==> provisioned", result.stdout)
                self.assertFalse(host.stamp.exists())
                self.assertEqual(host.cache_entries(), ["archives", "provision.lock"])

    def test_git_is_asked_about_the_vcpkg_checkout_whatever_the_environment_names(self):
        """A job's environment can hold GIT_DIR, and git puts it before -C."""
        host = self.host
        other = host.tmp / "other"
        other.mkdir()
        host.git(other, "init", "--quiet")
        (other / "kept").write_text("kept\n")
        host.git(other, "add", "--", "kept")
        host.git(other, "commit", "--quiet", "-m", "another repository")
        other_head = host.git(other, "rev-parse", "HEAD")
        elsewhere = {"GIT_DIR": other / ".git", "GIT_WORK_TREE": other}

        result = host.run("provision-vcpkg", **elsewhere)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(host.head(), host.baseline)
        self.assertEqual(host.git(other, "rev-parse", "HEAD"), other_head)
        self.assertEqual(host.git(other, "status", "--porcelain", "--ignored"), "")
        self.assertEqual(sorted(p.name for p in other.iterdir()), [".git", "kept"])

        ready = host.run("vcpkg-env", **elsewhere)
        self.assertEqual(ready.returncode, 0, ready.stderr)
        self.assertEqual(len(ready.stdout.splitlines()), 3)
        (host.checkout / "ports-local").write_text("an overlay nobody reviewed\n")
        host.forget_calls()
        for arguments in (("vcpkg-env",), ("provision-vcpkg", "--check")):
            with self.subTest(arguments=arguments):
                result = host.run(*arguments, **elsewhere)
                self.assertEqual(result.stdout, "")
                self.assert_refused(result, "local modifications or untracked files")
                self.assertEqual(host.vcpkg_calls(), [])
        (host.checkout / "ports-local").unlink()
        # And without git's own list of those variables nothing is asked at all.
        stamp = host.stamp.read_bytes()
        for arguments in (("vcpkg-env",), ("provision-vcpkg", "--check"), ("provision-vcpkg",)):
            with self.subTest(arguments=arguments, git="cannot list them"):
                result = host.run(*arguments, FAKE_GIT_FAILS="rev-parse")
                self.assertNotIn("VCPKG_ROOT", result.stdout)
                self.assert_refused(result, "cannot ask git which environment variables")
                self.assertEqual(host.vcpkg_calls(), [])
                self.assertEqual(host.stamp.read_bytes(), stamp)

    def test_a_failed_build_leaves_no_stamp_and_no_install_root(self):
        host = self.host
        result = host.run("provision-vcpkg", FAKE_VCPKG_BUILD="fail")
        self.assert_refused(result, "the vcpkg build failed (exit 1)")
        self.assertEqual(len(host.installs()), 1)
        self.assertFalse(host.stamp.exists())
        self.assertEqual(host.cache_entries(), ["archives", "provision.lock"])

    def test_a_failed_proof_leaves_no_stamp_and_removes_the_one_before(self):
        host = self.host
        # The build succeeds without archiving one package, so only the proof
        # can tell that the cache is incomplete.
        result = host.run("provision-vcpkg", FAKE_VCPKG_UNCACHED="grpc")
        self.assert_refused(result, "the binary-only install failed (exit 1): a package of")
        warm, proof = host.installs()
        self.assertEqual(warm["built"], host.PACKAGES)
        self.assertEqual(proof["missing"], ["grpc"])
        self.assertFalse(host.stamp.exists())

        host.provision()
        self.assertTrue(host.stamp.exists())
        (host.archives / self.TRIPLET / f"grpc-{host.baseline}.zip").unlink()
        result = host.run("provision-vcpkg", FAKE_VCPKG_UNCACHED="grpc")
        self.assert_refused(result, "the binary-only install failed (exit 1)")
        # Provisioning removed the stamp before it built; the proof has none to remove.
        self.assertNotIn("Removed the stamp", result.stderr)
        self.assertFalse(host.stamp.exists())
        self.assertEqual(host.cache_entries(), ["archives", "provision.lock"])
        self.assertEqual(host.run("vcpkg-env").stdout, "")

    def test_a_refused_run_keeps_the_stamp_of_the_state_it_did_not_change(self):
        host = self.host
        host.provision()
        stamp = host.stamp.read_bytes()
        result = host.run("provision-vcpkg", CXX=host.tmp / "missing")
        self.assert_refused(result, "no C++ compiler")
        self.assertEqual(host.stamp.read_bytes(), stamp)
        # Nothing changed, so the runner is what the stamp says: ready.
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

        (host.checkout / "ports-local").write_text("an overlay nobody reviewed\n")
        result = host.run("provision-vcpkg")
        self.assert_refused(result, "local modifications or untracked files")
        self.assertEqual(host.stamp.read_bytes(), stamp)
        # The stamp stands, and the checkout it describes is no longer there
        # to use: the refusal is repeated to a job that asks.
        env = host.run("vcpkg-env")
        self.assert_refused(env, "local modifications or untracked files")
        self.assertEqual(env.stdout, "")

    def test_a_cache_that_cannot_be_listed_is_not_stamped(self):
        """The stamp records what the cache held; a guess at it is no record."""
        host = self.host
        closed = host.archives / "closed"
        closed.mkdir(parents=True)
        closed.chmod(0)
        self.addCleanup(closed.chmod, 0o755)
        result = host.run("provision-vcpkg")
        self.assert_refused(result, "cannot list the binary cache")
        self.assertEqual(len(host.installs()), 2)
        self.assertFalse(host.stamp.exists())

    def test_an_install_root_that_cannot_be_removed_is_reported_and_fails_nothing(self):
        host = self.host

        def unlock():
            for locked in host.cache.glob("install.*/locked"):
                locked.chmod(0o755)

        self.addCleanup(unlock)
        result = host.run("provision-vcpkg", FAKE_VCPKG_LOCKS_ROOT="1")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(host.stamp.exists())
        left = [name for name in host.cache_entries() if name.startswith("install.")]
        self.assertEqual(len(left), 2, host.cache_entries())
        for name in left:
            self.assertIn(f"warning: could not remove {host.cache / name}", result.stderr)
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

    def test_a_manifest_that_changes_during_the_build_is_not_stamped(self):
        host = self.host
        result = host.run("provision-vcpkg", FAKE_VCPKG_EDITS_MANIFEST="content")
        self.assert_refused(result, "changed while the cache was being built")
        self.assertEqual(len(host.installs()), 2)
        self.assertFalse(host.stamp.exists())

    def test_a_manifest_only_laid_out_another_way_during_the_build_is_stamped(self):
        host = self.host
        before = host.manifest.read_bytes()
        host.provision(FAKE_VCPKG_EDITS_MANIFEST="layout")
        self.assertEqual(host.manifest.read_bytes(), before + b"\n")
        self.assertEqual(
            host.stamp_fields()["dependencies_sha256"], self.content_digest(before)
        )
        self.assert_ready(host)

    def test_a_hung_build_is_stopped_at_the_bound_and_leaves_no_stamp(self):
        host = self.host
        started = time.monotonic()
        result = host.run(
            "provision-vcpkg",
            bound=10,
            FAKE_VCPKG_BUILD="hang",
            TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT="1",
        )
        self.assertLess(time.monotonic() - started, 10)
        self.assert_refused(result, "did not finish within 1s")
        self.assertFalse(host.stamp.exists())
        self.assertEqual(host.cache_entries(), ["archives", "provision.lock"])
        # The stub itself is gone, not left building behind the script.
        pid = int(Path(f"{host.vcpkg_log}.pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_a_build_that_ignores_the_stop_is_killed_and_reported_as_out_of_time(self):
        """Slow by design: timeout waits a fixed 30 seconds before it kills."""
        host = self.host
        started = time.monotonic()
        result = host.run(
            "provision-vcpkg",
            bound=50,
            FAKE_VCPKG_BUILD="deaf",
            TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT="1",
        )
        self.assertGreaterEqual(time.monotonic() - started, 30)
        self.assert_refused(result, "did not finish within 1s")
        self.assertFalse(host.stamp.exists())
        self.assertEqual(host.cache_entries(), ["archives", "provision.lock"])
        with self.assertRaises(ProcessLookupError):
            os.kill(host.recorded_stub(), 0)

    def test_a_build_the_system_kills_before_the_bound_is_a_failed_build(self):
        host = self.host
        process = host.start("provision-vcpkg", FAKE_VCPKG_BUILD="hang")
        self.wait_for_the_stub(host)
        # timeout and the stub tool, as the kernel's out-of-memory killer would.
        os.killpg(host.recorded_stub_group(), signal.SIGKILL)
        result = host.finish(process, bound=30)
        self.assert_refused(result, "the vcpkg build failed (exit 137)")
        self.assertFalse(host.stamp.exists())

    def wait_for_the_stub(self, host: RunnerHost) -> int:
        """Wait until the hanging stub tool is running; returns its process id."""
        deadline = time.monotonic() + 30
        while host.recorded_stub() is None:
            self.assertLess(time.monotonic(), deadline, "the stub tool never started")
            time.sleep(0.02)
        return host.recorded_stub()

    def test_a_signal_stops_the_build_along_with_the_script(self):
        """Ctrl-C, a hangup and a TERM: none may leave the build running behind.

        timeout puts vcpkg in a process group of its own, so the terminal's
        Ctrl-C reaches the script and not the build, as it does here. The
        stub tool takes a second to stop, and the script ends only once it
        has, whatever further signal arrives meanwhile.
        """
        for name, to_group, second in (
            ("SIGINT", True, None),
            ("SIGTERM", False, None),
            ("SIGHUP", False, None),
            ("SIGTERM", False, "SIGHUP"),
        ):
            with self.subTest(signal=name, second=second):
                host = RunnerHost(self)
                number = getattr(signal, name)
                process = host.start("provision-vcpkg", FAKE_VCPKG_BUILD="linger")
                stub = self.wait_for_the_stub(host)
                self.assertNotEqual(os.getpgid(stub), os.getpgid(process.pid))
                if to_group:
                    os.killpg(process.pid, number)
                else:
                    os.kill(process.pid, number)
                if second:
                    time.sleep(0.3)
                    if process.poll() is None:
                        os.kill(process.pid, getattr(signal, second))
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    pass
                ended = process.poll() is not None
                left_running = host.recorded_stub_group() is not None
                host.kill_recorded_stub()
                result = host.finish(process, bound=5)

                self.assertTrue(ended, f"the script ignored {name}")
                self.assertFalse(left_running, "the stub tool outlived the script")
                self.assertEqual(result.returncode, 128 + number, result.stderr)
                self.assertIn(f"error: stopped by {name}", result.stderr)
                self.assertNotIn("==> proving", result.stdout)
                self.assertFalse(host.stamp.exists())
                self.assertEqual(host.cache_entries(), ["archives", "provision.lock"])

    def hang_on_any_install(self, host: RunnerHost) -> None:
        """Make the bootstrapped stub hang on the proof too: --check runs nothing else."""
        tool = host.tool.read_text().replace('if [ "$only_binary" = no ]; then', "if true; then")
        self.assertNotEqual(tool, host.tool.read_text())
        host.tool.write_text(tool)

    def test_a_hung_proof_is_stopped_at_the_bound_and_takes_the_stamp_with_it(self):
        host = self.host
        host.provision()
        self.hang_on_any_install(host)
        result = host.run(
            "provision-vcpkg",
            "--check",
            bound=10,
            FAKE_VCPKG_BUILD="hang",
            TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT="1",
        )
        self.assert_refused(result, "did not finish within 1s and was stopped. Removed the stamp")
        pid = int(Path(f"{host.vcpkg_log}.pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)
        # A restore that does not end is no proof either.
        self.assertFalse(host.stamp.exists())
        self.assertEqual(host.run("vcpkg-env").stdout, "")

    def test_a_check_stopped_by_a_signal_leaves_the_stamp(self):
        """The operator stopped it: the proof neither passed nor failed."""
        host = self.host
        host.provision()
        stamp = (host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns)
        self.hang_on_any_install(host)
        process = host.start("provision-vcpkg", "--check", FAKE_VCPKG_BUILD="linger")
        self.wait_for_the_stub(host)
        os.kill(process.pid, signal.SIGTERM)
        result = host.finish(process, bound=20)
        self.assertEqual(result.returncode, 128 + signal.SIGTERM, result.stderr)
        self.assertIn("error: stopped by SIGTERM", result.stderr)
        self.assertNotIn("Removed the stamp", result.stderr)
        self.assertEqual((host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns), stamp)
        self.assertEqual(
            host.cache_entries(), ["archives", "provision.lock", "provisioned.stamp"]
        )
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

    LOCKED = "another provision-vcpkg is running, or the build of one that was killed still is"

    @contextlib.contextmanager
    def lock_held(self, host: RunnerHost):
        """Hold the provisioning lock, as a run that is under way does."""
        with host.lock.open("a") as handle:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            yield

    def assert_lock_is_free(self, host: RunnerHost) -> None:
        """Nothing holds the lock; a holder that is ending gets a moment to go."""
        deadline = time.monotonic() + 5
        while True:
            try:
                with self.lock_held(host):
                    return
            except BlockingIOError:
                self.assertLess(time.monotonic(), deadline, "the provisioning lock is still held")
                time.sleep(0.02)

    def test_a_second_run_is_refused_while_the_first_builds(self):
        host = self.host
        first = host.start("provision-vcpkg", FAKE_VCPKG_BUILD="hang")
        self.wait_for_the_stub(host)
        calls = (host.git_calls(), host.vcpkg_calls())
        entries = host.cache_entries()

        second = host.run("provision-vcpkg", bound=10)

        self.assert_refused(
            second,
            f"error: {self.LOCKED}: {host.lock} is locked. Wait for it to end, or stop that "
            f"build, and run this command again; 'fuser -v {host.lock}' lists what holds the "
            "lock\n",
        )
        self.assertEqual(second.stdout, "")
        # The refused run touched nothing, and the first is still building.
        self.assertEqual((host.git_calls(), host.vcpkg_calls()), calls)
        self.assertEqual(host.cache_entries(), entries)
        self.assertIsNone(first.poll())
        # The first run fails, here by a signal, and the lock goes with it.
        os.kill(first.pid, signal.SIGTERM)
        ended = host.finish(first, bound=20)
        self.assertEqual(ended.returncode, 128 + signal.SIGTERM, ended.stderr)
        self.assert_lock_is_free(host)
        host.provision()

    def test_a_build_left_behind_by_a_killed_command_keeps_the_lock(self):
        """Killed outright, the command cannot stop its build, which goes on writing."""
        host = self.host
        first = host.start("provision-vcpkg", FAKE_VCPKG_BUILD="hang")
        stub = self.wait_for_the_stub(host)
        os.kill(first.pid, signal.SIGKILL)
        self.assertEqual(first.wait(timeout=10), -signal.SIGKILL)
        os.kill(stub, 0)

        second = host.run("provision-vcpkg", bound=10)

        self.assert_refused(second, f"error: {self.LOCKED}: {host.lock} is locked")
        self.assertEqual(len(host.installs()), 1)
        os.kill(stub, 0)
        # With the build stopped the lock is free, and the next run provisions.
        host.kill_recorded_stub()
        first.communicate(timeout=10)
        self.assert_lock_is_free(host)
        host.provision()
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

    def test_the_lock_is_free_again_however_a_run_ended(self):
        host = self.host
        host.provision()
        self.assert_lock_is_free(host)
        failed = host.run("provision-vcpkg", FAKE_VCPKG_BUILD="fail")
        self.assert_refused(failed, "the vcpkg build failed")
        self.assert_lock_is_free(host)
        host.provision()
        (host.archives / self.TRIPLET / f"grpc-{host.baseline}.zip").write_text("")
        check = host.run("provision-vcpkg", "--check")
        self.assert_refused(check, "the binary-only install failed")
        self.assert_lock_is_free(host)
        host.provision()
        self.assertEqual(host.run("provision-vcpkg", "--check").returncode, 0)
        self.assert_lock_is_free(host)

    def test_a_held_lock_refuses_provisioning_and_the_check_and_nothing_else(self):
        host = self.provisioned()
        stamp = (host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns)
        with self.lock_held(host):
            for arguments in (("provision-vcpkg",), ("provision-vcpkg", "--check")):
                with self.subTest(arguments=arguments):
                    result = host.run(*arguments, bound=10)
                    self.assert_refused(result, f"error: {self.LOCKED}: {host.lock} is locked")
                    self.assertEqual(result.stdout, "")
                    self.assertEqual((host.git_calls(), host.vcpkg_calls()), ([], []))
                    self.assertEqual(
                        (host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns), stamp
                    )
            # Reading takes no lock: a job and the operator are answered meanwhile.
            env = host.run("vcpkg-env", bound=10)
            self.assertEqual(env.returncode, 0, env.stderr)
            self.assertEqual(len(env.stdout.splitlines()), 3)
            status = host.run("status", bound=10)
            self.assertEqual(status.returncode, 0, status.stderr)
            self.assertIn("vcpkg_cache_ready: yes", status.stdout.splitlines())
        self.assertEqual(host.run("provision-vcpkg", "--check").returncode, 0)

    def test_a_lock_that_cannot_be_taken_is_a_refusal_whatever_the_reason(self):
        host = self.host
        write_executable(host.bin / "flock-failing", "#!/bin/sh\nexit 66\n")
        (host.tmp / "taken").write_text("a file where the cache directory would go\n")
        closed = host.tmp / "closed"
        closed.mkdir(mode=0o555)
        self.addCleanup(closed.chmod, 0o755)
        cases = {
            "flock fails and the lock is not held": (
                {"TP_JETSON_RUNNER_FLOCK": host.bin / "flock-failing"},
                f"error: cannot lock {host.lock} ({host.bin / 'flock-failing'} exit 66)",
            ),
            "the lock file cannot be opened": (
                {"TP_JETSON_RUNNER_VCPKG_CACHE_DIR": closed},
                f"error: cannot open the lock at {closed / 'provision.lock'}",
            ),
            "the cache directory cannot be made": (
                {"TP_JETSON_RUNNER_VCPKG_CACHE_DIR": host.tmp / "taken/cache"},
                f"error: cannot create {host.tmp / 'taken/cache'}",
            ),
        }
        # Said in POSIX mode too, where bash ends a script at a redirection
        # that fails on `exec` or on another special builtin.
        for name, (overrides, reason) in cases.items():
            for posix in (None, "1"):
                with self.subTest(case=name, POSIXLY_CORRECT=posix):
                    result = host.run("provision-vcpkg", POSIXLY_CORRECT=posix, **overrides)
                    self.assert_refused(result, reason)
                    self.assertEqual(result.stdout, "")
                    self.assertEqual((host.git_calls(), host.vcpkg_calls()), ([], []))
                    self.assertFalse(host.checkout.exists())

    def provisioned(self) -> RunnerHost:
        host = RunnerHost(self)
        host.provision()
        host.forget_calls()
        return host

    def test_check_passes_on_a_good_cache_and_changes_nothing(self):
        host = self.provisioned()
        stamp = (host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns)
        archives = host.archive_names()

        result = host.run("provision-vcpkg", "--check")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("==> the vcpkg checkout and binary cache are ready", result.stdout)
        (proof,) = host.vcpkg_calls()
        self.assertEqual(proof["args"], self.install_args(host, proof, "--only-binarycaching"))
        self.assertEqual(proof["VCPKG_BINARY_SOURCES"], f"clear;files,{host.archives},read")
        self.assertEqual(proof["restored"], host.PACKAGES)
        self.assertEqual((host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns), stamp)
        self.assertEqual(host.archive_names(), archives)
        self.assertEqual(
            host.cache_entries(), ["archives", "provision.lock", "provisioned.stamp"]
        )
        self.assertEqual(
            host.git_calls(),
            [
                "git rev-parse --local-env-vars",
                f"git -C {host.checkout} status --porcelain --untracked-files=all",
            ],
        )

    def test_check_fails_once_an_archive_is_gone_and_leaves_the_stamp(self):
        host = self.provisioned()
        stamp = (host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns)
        (host.archives / self.TRIPLET / f"protobuf-{host.baseline}.zip").unlink()

        result = host.run("provision-vcpkg", "--check")

        # Seen from the names alone, before any install: refused, and unchanged.
        self.assert_refused(result, "does not hold exactly the archives")
        self.assertEqual(host.installs(), [])
        self.assertEqual((host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns), stamp)
        self.assertEqual(
            host.cache_entries(), ["archives", "provision.lock", "provisioned.stamp"]
        )

    def test_check_fails_on_an_archive_that_does_not_restore_and_removes_the_stamp(self):
        """What `status` and `vcpkg-env` cannot see, and the proof exists for.

        The proof ran and failed, so the stamp that says the cache restores is
        wrong, and goes: the runner stops answering as ready there and then.
        """
        host = self.provisioned()
        (host.archives / self.TRIPLET / f"protobuf-{host.baseline}.zip").write_text("")
        archives = host.archive_names()
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

        result = host.run("provision-vcpkg", "--check")

        self.assert_refused(
            result,
            "error: the binary-only install failed (exit 1): a package of streaming-grpc "
            f"for {self.TRIPLET} is missing from the cache, or vcpkg could not run or write "
            f"under {host.cache}; see its output above. Removed the stamp at {host.stamp}: "
            "the runner is no longer reported as ready until provision-vcpkg succeeds again",
        )
        self.assertEqual(host.installs()[0]["missing"], ["protobuf"])
        self.assertFalse(host.stamp.exists())
        # Nothing but the stamp went: the archives are there to provision from.
        self.assertEqual(host.cache_entries(), ["archives", "provision.lock"])
        self.assertEqual(host.archive_names(), archives)
        env = host.run("vcpkg-env")
        self.assertEqual(env.stdout, "")
        self.assert_refused(env, f"not provisioned: no stamp at {host.stamp}")
        self.assertIn(
            f"vcpkg_cache_ready: no (not provisioned: no stamp at {host.stamp})",
            self.status(host),
        )
        again = host.run("provision-vcpkg", "--check")
        self.assert_refused(again, "not provisioned")
        self.assertEqual(len(host.installs()), 1)
        # Provisioning again is what makes the runner ready again.
        host.provision()
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

    def test_check_does_not_call_a_tool_that_cannot_run_a_missing_package(self):
        host = self.provisioned()
        write_executable(host.tool, "#!/bin/sh\necho 'not vcpkg' >&2\nexit 3\n")
        result = host.run("provision-vcpkg", "--check")
        self.assert_refused(
            result,
            "error: the binary-only install failed (exit 3): a package of streaming-grpc "
            f"for {self.TRIPLET} is missing from the cache, or vcpkg could not run or write "
            f"under {host.cache}; see its output above. Removed the stamp",
        )
        # Whichever it was, the cache was not shown to restore.
        self.assertFalse(host.stamp.exists())

    def test_a_stamp_a_failed_check_cannot_remove_is_emptied(self):
        """The directory closed behind the proof; the stamp is still this account's to write."""
        host = self.provisioned()
        self.addCleanup(host.cache.chmod, 0o755)
        result = host.run("provision-vcpkg", "--check", FAKE_VCPKG_PROOF_CLOSES_CACHE="1")
        self.assert_refused(
            result,
            f"see its output above. Could not remove the stamp at {host.stamp} and emptied it "
            "instead: the runner is no longer reported as ready until provision-vcpkg succeeds "
            "again",
        )
        self.assertNotIn("Removed the stamp", result.stderr)
        self.assertEqual(host.stamp.read_bytes(), b"")
        # An empty stamp records nothing, so the runner stops answering as ready.
        reason = f"the stamp at {host.stamp} does not record baseline exactly once"
        env = host.run("vcpkg-env")
        self.assertEqual(env.stdout, "")
        self.assert_refused(env, reason)
        self.assertIn(f"vcpkg_cache_ready: no ({reason})", self.status(host))
        host.cache.chmod(0o755)
        host.provision()
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

    def test_a_stamp_a_failed_check_can_neither_remove_nor_empty_is_said_to_stand(self):
        for posix in (None, "1"):
            with self.subTest(POSIXLY_CORRECT=posix):
                host = self.provisioned()
                self.addCleanup(host.cache.chmod, 0o755)
                host.stamp.chmod(0o444)
                stamp = host.stamp.read_bytes()
                result = host.run(
                    "provision-vcpkg", "--check",
                    FAKE_VCPKG_PROOF_CLOSES_CACHE="1", POSIXLY_CORRECT=posix,
                )
                self.assert_refused(
                    result,
                    f"see its output above. The stamp at {host.stamp} could be neither removed "
                    "nor emptied, so status and vcpkg-env still report the runner as ready. It "
                    "is not: remove the stamp by hand",
                )
                self.assertNotIn("Removed the stamp", result.stderr)
                self.assertNotIn("emptied it instead", result.stderr)
                self.assertEqual(host.stamp.read_bytes(), stamp)

    def test_a_check_refused_before_its_proof_changes_nothing(self):
        host = self.provisioned()
        stamp = (host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns)
        entries = host.cache_entries()
        refusals = {
            "another account": (
                {"TP_JETSON_RUNNER_USER": "fixture-other-account"}, "runs as the runner account"),
            "a checkout git cannot inspect": ({"FAKE_GIT_FAILS": "status"}, "cannot inspect"),
            "no timeout": ({"TP_JETSON_RUNNER_TIMEOUT": host.tmp / "missing"}, "timeout not found"),
            "another machine": ({"FAKE_MACHINE": "x86_64"}, "another triplet"),
        }
        for name, (overrides, reason) in refusals.items():
            with self.subTest(refusal=name):
                result = host.run("provision-vcpkg", "--check", **overrides)
                self.assert_refused(result, reason)
                self.assertEqual(host.vcpkg_calls(), [])
                self.assertEqual((host.stamp.read_bytes(), host.stamp.stat().st_mtime_ns), stamp)
                self.assertEqual(host.cache_entries(), entries)
        self.assertEqual(host.run("vcpkg-env").returncode, 0)

    def test_check_fails_on_a_stale_or_modified_checkout_without_installing(self):
        def moved(host):
            later = host.commit_remote("later-port")
            host.git(host.checkout, "fetch", "--quiet", "origin")
            host.git(host.checkout, "checkout", "--quiet", "--detach", later)
            return "not at the baseline"

        def modified(host):
            (host.checkout / "ports-local").write_text("an overlay nobody reviewed\n")
            return "local modifications or untracked files"

        def never_provisioned(host):
            shutil.rmtree(host.cache)
            return "not provisioned"

        for change in (moved, modified, never_provisioned):
            with self.subTest(change=change.__name__):
                host = self.provisioned()
                stamp = host.stamp.read_bytes()
                reason = change(host)
                result = host.run("provision-vcpkg", "--check")
                self.assert_refused(result, reason)
                self.assertEqual(host.vcpkg_calls(), [])
                self.assertEqual(host.cache.exists(), change is not never_provisioned)
                # Refused before the proof: the stamp is not this command's to remove.
                if change is not never_provisioned:
                    self.assertEqual(host.stamp.read_bytes(), stamp)

    def test_vcpkg_env_is_exactly_three_lines_on_a_ready_runner(self):
        host = self.provisioned()
        result = host.run("vcpkg-env")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            f"VCPKG_ROOT={host.checkout}\n"
            f"VCPKG_BINARY_SOURCES=clear;files,{host.archives},read\n"
            "VCPKG_FORCE_SYSTEM_BINARIES=1\n",
        )
        self.assertEqual(result.stderr, "")
        self.assertEqual(host.vcpkg_calls(), [])

    def test_vcpkg_env_refuses_a_compiler_other_than_the_one_the_cache_was_built_with(self):
        host = self.provisioned()
        write_executable(
            host.bin / "other-c++",
            '#!/bin/sh\n[ "$*" = "--version" ] || exit 2\necho "other-c++ 1.0"\n',
        )
        ready = host.run("vcpkg-env")
        self.assertEqual((ready.returncode, len(ready.stdout.splitlines())), (0, 3), ready.stderr)
        other = host.run("vcpkg-env", CXX=host.bin / "other-c++")
        self.assertEqual((other.returncode, other.stdout), (1, ""), other.stderr)
        for line in ("fixture-c++ (Fixture 1.0) 13.2.0", "other-c++ 1.0"):
            self.assertIn(f"'{line}'", other.stderr)
        missing = host.run("vcpkg-env", CXX=host.tmp / "missing")
        self.assertEqual((missing.returncode, missing.stdout), (1, ""), missing.stderr)
        self.assertIn("no C++ compiler", missing.stderr)
        # The stamp's line is compared as text, never as a pattern.
        self.rewrite_stamp(host, "compiler", "fixture-c++ (Fixture 1.0) *")
        pattern = host.run("vcpkg-env")
        self.assertEqual((pattern.returncode, pattern.stdout), (1, ""), pattern.stderr)
        write_executable(
            host.bin / "other-c++", '#!/bin/sh\nprintf "other\\033[2Jc++\\n"\n'
        )
        self.rewrite_stamp(host, "compiler", "stamped\x1b[2Jc++")
        shown = host.run("vcpkg-env", CXX=host.bin / "other-c++")
        self.assertEqual((shown.returncode, shown.stdout), (1, ""), shown.stderr)
        for line in ("'stamped?[2Jc++'", "'other?[2Jc++'"):
            self.assertIn(line, shown.stderr)
        self.assertNotIn("\x1b", shown.stderr)

    STAMP_KEYS = (
        "baseline", "dependencies_sha256", "triplet", "feature", "vcpkg_version", "compiler",
        "provisioned_at", "archives_sha256",
    )

    def rewrite_stamp(self, host: RunnerHost, key: str, *values: str) -> None:
        """Replace one key's line in the stamp with a line for each value."""
        lines = []
        for line in host.stamp.read_text().splitlines():
            if line.startswith(f"{key}="):
                lines.extend(f"{key}={value}" for value in values)
            else:
                lines.append(line)
        host.stamp.write_text("\n".join(lines) + "\n")

    def not_ready_cases(self) -> dict:
        """Each way a runner is not ready: a change, the reason, extra environment.

        Every change breaks one condition and leaves the others true, so each
        check is shown to refuse on its own.
        """
        other = "0123456789abcdef0123456789abcdef01234567"

        def no_stamp(host):
            host.stamp.unlink()

        def stamp_for_another_baseline(host):
            self.rewrite_stamp(host, "baseline", other)

        def stamp_naming_two_baselines(host):
            self.rewrite_stamp(host, "baseline", other, host.baseline)

        def stamp_without(key):
            def change(host):
                self.rewrite_stamp(host, key)

            change.__name__ = f"stamp_without_{key}"
            return change

        def stamp_closed_to_this_account(host):
            host.stamp.chmod(0)

        def stamp_empty(host):
            host.stamp.write_text("")

        def stamp_for_other_dependencies(host):
            self.rewrite_stamp(host, "dependencies_sha256", "0" * 64)

        def stamp_with_the_digest_under_another_key(host):
            stamp = host.stamp.read_text()
            self.assertEqual(stamp.count("\ndependencies_sha256="), 1)
            host.stamp.write_text(
                stamp.replace("\ndependencies_sha256=", "\ndependencies_digest=")
            )

        def stamp_for_another_triplet(host):
            self.rewrite_stamp(host, "triplet", "x64-linux")

        def stamp_for_another_feature(host):
            self.rewrite_stamp(host, "feature", "tensorrt-adapter")

        def manifest_with_another_baseline(host):
            host.write_manifest(host.commit_remote("later-port"))

        def checkout_moved(host):
            later = host.commit_remote("later-port")
            host.git(host.checkout, "fetch", "--quiet", "origin")
            host.git(host.checkout, "checkout", "--quiet", "--detach", later)

        def checkout_on_a_branch_at_the_baseline(host):
            host.git(host.checkout, "checkout", "--quiet", "-B", "pinned", host.baseline)

        def checkout_gone(host):
            shutil.rmtree(host.checkout)

        def tool_gone(host):
            host.tool.unlink()

        def tool_not_executable(host):
            host.tool.chmod(0o644)

        def tool_is_a_directory(host):
            host.tool.unlink()
            host.tool.mkdir()

        def archives_gone(host):
            shutil.rmtree(host.archives)

        def archives_emptied(host):
            shutil.rmtree(host.archives)
            host.archives.mkdir()

        def archive_removed(host):
            (host.archives / self.TRIPLET / f"grpc-{host.baseline}.zip").unlink()

        def archive_added(host):
            (host.archives / "stray.zip").write_text("not from the proof\n")

        def cache_path_with_a_comma(host):
            shutil.copytree(host.cache, host.tmp / "ca,che")

        def checkout_path_with_a_semicolon(host):
            shutil.copytree(host.checkout, host.tmp / "vc;pkg", symlinks=True)

        return {
            no_stamp: ("not provisioned", {}),
            stamp_for_another_baseline: ("another vcpkg baseline", {}),
            stamp_naming_two_baselines: ("does not record baseline exactly once", {}),
            **{
                stamp_without(key): (f"does not record {key} exactly once", {})
                for key in self.STAMP_KEYS
            },
            stamp_closed_to_this_account: ("cannot read a stamp", {}),
            stamp_empty: ("does not record baseline exactly once", {}),
            stamp_for_other_dependencies: ("another dependencies digest", {}),
            stamp_with_the_digest_under_another_key: (
                "does not record dependencies_sha256 exactly once", {}),
            stamp_for_another_triplet: ("another triplet", {}),
            stamp_for_another_feature: ("another manifest feature", {}),
            manifest_with_another_baseline: ("another vcpkg baseline", {}),
            checkout_moved: ("not at the baseline", {}),
            checkout_on_a_branch_at_the_baseline: ("is not-detached", {}),
            checkout_gone: ("is absent", {}),
            tool_gone: ("no executable vcpkg tool", {}),
            tool_not_executable: ("no executable vcpkg tool", {}),
            tool_is_a_directory: ("no executable vcpkg tool", {}),
            archives_gone: ("cannot list the binary cache", {}),
            archives_emptied: ("does not hold exactly the archives", {}),
            archive_removed: ("does not hold exactly the archives", {}),
            archive_added: ("does not hold exactly the archives", {}),
            "another_machine": ("another triplet", {"FAKE_MACHINE": "x86_64"}),
            "machine_without_a_triplet": ("no vcpkg triplet", {"FAKE_MACHINE": "riscv64"}),
            cache_path_with_a_comma: (
                "absolute paths", {"TP_JETSON_RUNNER_VCPKG_CACHE_DIR": "{tmp}/ca,che"}),
            checkout_path_with_a_semicolon: (
                "absolute paths", {"TP_JETSON_RUNNER_VCPKG_DIR": "{tmp}/vc;pkg"}),
        }

    def test_vcpkg_env_prints_nothing_unless_the_runner_is_ready_for_this_checkout(self):
        for change, (reason, overrides) in self.not_ready_cases().items():
            name = change if isinstance(change, str) else change.__name__
            with self.subTest(case=name):
                host = self.provisioned()
                self.assertEqual(host.run("vcpkg-env").returncode, 0)
                if not isinstance(change, str):
                    change(host)
                overrides = {k: v.format(tmp=host.tmp) for k, v in overrides.items()}

                result = host.run("vcpkg-env", **overrides)

                self.assertEqual(result.stdout, "")
                self.assert_refused(result, reason)
                status = host.run("status", **overrides)
                self.assertEqual(status.returncode, 0, status.stderr)
                ready = [
                    line for line in status.stdout.splitlines()
                    if line.startswith("vcpkg_cache_ready:")
                ]
                self.assertEqual(len(ready), 1, status.stdout)
                self.assertTrue(ready[0].startswith("vcpkg_cache_ready: no ("), ready[0])
                self.assertIn(reason, ready[0])

    def full_manifest(self, host: RunnerHost) -> dict:
        """A manifest with every kind of member the digest has to follow.

        Some of it only looks like the manifest's own version: a `version` in
        an override and in a dependency, and a feature of that name.
        """
        return {
            "name": "fixture",
            "version-string": "1.0.0",
            "builtin-baseline": host.baseline,
            "description": "Gr\u00f6\u00dfe \u5c3a\u5bf8 \U0001f9ca, a \" quote, } and ] in it",
            "homepage": "https://example.invalid/fixture",
            "dependencies": ["gtest", {"name": "fmt", "version>=": "10.1.0"}, "nlohmann-json"],
            "features": {
                "streaming-grpc": {
                    "description": "The stream transport.",
                    "dependencies": ["grpc", "protobuf"],
                },
                "version": {
                    "description": "A feature that happens to have this name.",
                    "dependencies": [{"name": "zlib", "version": "1.3"}],
                },
                "plain": {"description": "A feature that depends on nothing.", "dependencies": []},
            },
            "overrides": [{"name": "grpc", "version": "1.60.0"}],
        }

    def provisioned_with(self, manifest: dict) -> tuple:
        """A runner provisioned for that manifest, and the digest its stamp records."""
        host = self.host
        host.manifest.write_bytes(json.dumps(manifest, indent=2, ensure_ascii=False).encode())
        host.provision()
        host.forget_calls()
        recorded = host.stamp_fields()["dependencies_sha256"]
        self.assertEqual(recorded, self.content_digest(host.manifest.read_bytes()))
        return host, recorded

    def assert_ready(self, host: RunnerHost, **overrides) -> None:
        env = host.run("vcpkg-env", **overrides)
        self.assertEqual(env.returncode, 0, env.stderr)
        self.assertEqual(len(env.stdout.splitlines()), 3)
        self.assertIn("vcpkg_cache_ready: yes", self.status(host, **overrides))

    def assert_still_proved(self, host: RunnerHost, stamp: bytes) -> None:
        """The proof passes for the manifest as it is now, and the stamp is as it was."""
        check = host.run("provision-vcpkg", "--check")
        self.assertEqual(check.returncode, 0, check.stderr)
        self.assertEqual(len(host.installs()), 1)
        self.assertEqual(host.stamp.read_bytes(), stamp)

    def test_the_digest_is_of_what_the_manifest_says_in_ascii_with_sorted_keys(self):
        """One manifest worked out by hand, with characters outside ASCII in it."""
        host = self.host
        host.manifest.write_bytes(
            (
                '{"name": "fixture", "version": "1.0.0", "builtin-baseline": "%s",\n'
                ' "description": "Gr\u00f6\u00dfe \U0001f9ca", "dependencies": ["b", "a"],\n'
                ' "port-version": 9007199254740993}\n'
                % host.baseline
            ).encode()
        )
        host.provision()
        canonical = (
            '{"builtin-baseline":"%s","dependencies":["b","a"],'
            '"description":"Gr\\u00f6\\u00dfe \\ud83e\\uddca","name":"fixture",'
            '"port-version":9007199254740993}' % host.baseline
        )
        self.assertEqual(
            host.stamp_fields()["dependencies_sha256"],
            hashlib.sha256(canonical.encode("ascii")).hexdigest(),
        )
        # An integer is the digits it is written with: 2**53 + 1 above, and
        # 2**53 here, which a reader that took numbers for floats could not
        # tell from it.
        said = host.manifest.read_text()
        self.assertEqual(said.count("9007199254740993"), 1)
        host.manifest.write_text(said.replace("9007199254740993", "9007199254740992"))
        self.assert_refused(
            host.run("vcpkg-env"),
            f"provisioned for another dependencies digest than {host.manifest} has now",
        )

    def test_a_new_version_alone_leaves_the_runner_ready(self):
        """A release rewrites the project's own version, and vcpkg builds nothing from it."""
        manifest = self.full_manifest(self.host)
        host, _ = self.provisioned_with(manifest)
        stamp = host.stamp.read_bytes()
        rest = {key: value for key, value in manifest.items() if key != "version-string"}
        versions = {
            **{f"a new {key}": {key: "2.0.0-rc.1"} for key in self.OWN_VERSION_KEYS},
            "none": {},
            "two at once": {"version": "2.0.0", "version-date": "2026-10-06"},
            "one that is not a string": {"version-string": 2},
            "one that is an object": {"version-semver": {"major": 2, "version": "nested"}},
        }
        self.assertEqual(len(versions), 8)
        for case, own in versions.items():
            with self.subTest(version=case):
                # First or last among the members: where it stands says nothing either.
                for said in ({**own, **rest}, {**rest, **own}):
                    host.manifest.write_bytes(json.dumps(said, indent=2).encode())
                    self.assert_ready(host)
        self.assert_still_proved(host, stamp)

    def test_another_layout_of_the_same_manifest_leaves_the_runner_ready(self):
        """Blanks, line breaks, the order of keys and how a string is escaped say nothing."""
        manifest = self.full_manifest(self.host)
        host, _ = self.provisioned_with(manifest)
        stamp = host.stamp.read_bytes()

        def reversed_keys(value):
            if isinstance(value, dict):
                return {key: reversed_keys(value[key]) for key in reversed(list(value))}
            if isinstance(value, list):
                return [reversed_keys(item) for item in value]
            return value

        def written(indent=None, **options) -> str:
            return json.dumps(manifest, indent=indent, ensure_ascii=False, **options)

        escaped = json.dumps(manifest, indent=2, ensure_ascii=True)
        self.assertIn("\\u00f6", escaped)
        self.assertEqual(escaped.count("/"), 3)
        layouts = {
            "on one line": written(),
            "without a blank": written(separators=(",", ":")),
            "indented by four": written(4) + "\n",
            "indented by tabs": written("\t") + "\n",
            "with a blank before each colon": written(2, separators=(",", " : ")),
            "with its keys sorted": written(2, sort_keys=True),
            "with its keys in reverse": json.dumps(
                reversed_keys(manifest), indent=2, ensure_ascii=False
            ),
            "with carriage returns": written(2).replace("\n", "\r\n"),
            "between blank lines": "\n\n" + written(2) + "\n\n\n",
            "as the release driver writes it": json.dumps(manifest, indent=2) + "\n",
            "with its strings escaped": escaped.replace("/", "\\/"),
        }
        self.assertEqual(len(layouts), 11)
        self.assertEqual(list(json.loads(layouts["with its keys in reverse"]))[0], "overrides")
        self.assertEqual(len(set(layouts.values())), len(layouts))
        for case, layout in layouts.items():
            with self.subTest(layout=case):
                self.assertEqual(json.loads(layout), manifest)
                host.manifest.write_bytes(layout.encode())
                self.assert_ready(host)
        self.assert_still_proved(host, stamp)

    # What a manifest may hold at its top level beside the full manifest above:
    # vcpkg's other members, one it does not know, and four whose names only
    # come close to an own-version key. None of them is left out of the digest.
    OTHER_TOP_LEVEL_MEMBERS = {
        "default-features": ["streaming-grpc"],
        "supports": "linux",
        "maintainers": ["A maintainer of the fixture"],
        "summary": "A fixture.",
        "documentation": "https://example.invalid/fixture/docs",
        "license": "Apache-2.0",
        "port-version": 1,
        "vcpkg-configuration": {"default-registry": {"kind": "builtin", "baseline": "1" * 40}},
        "$schema": "https://example.invalid/vcpkg.schema.json",
        "$comment": "A remark.",
        "a-member-vcpkg-does-not-know": True,
        "version>=": "1.0.0",
        "Version": "1.0.0",
        "version_string": "1.0.0",
        "versions": "1.0.0",
    }

    def test_a_manifest_that_says_something_else_is_another_one(self):
        """Everything but its own version is what vcpkg builds from, or may be.

        That holds for a `version` that is not the manifest's own: an
        override's and a dependency's decide what is built.
        """
        manifest = self.full_manifest(self.host)
        host, recorded = self.provisioned_with(manifest)
        stamp = host.stamp.read_bytes()
        streaming = ("features", "streaming-grpc", "dependencies")

        def said(path: tuple, change) -> dict:
            """The manifest with `change` applied to the member at `path`."""
            copy = json.loads(json.dumps(manifest))
            parent = copy
            for step in path[:-1]:
                parent = parent[step]
            if path:
                parent[path[-1]] = change(parent[path[-1]])
            else:
                copy = change(copy)
            return copy

        changes = {
            "a dependency added": said(("dependencies",), lambda old: [*old, "zstd"]),
            "a dependency removed": said(("dependencies",), lambda old: old[:-1]),
            "the dependencies in another order": said(("dependencies",), lambda old: old[::-1]),
            "a dependency's minimum version": said(
                ("dependencies", 1, "version>="), lambda old: "10.2.0"),
            "a feature's dependency added": said(streaming, lambda old: [*old, "abseil"]),
            "a feature's dependency removed": said(streaming, lambda old: old[:1]),
            "a feature's dependency replaced": said(streaming, lambda old: ["grpc", "upb"]),
            "a feature added": said(("features",), lambda old: {**old, "extra": {}}),
            "a feature removed": said(
                ("features",), lambda old: {"streaming-grpc": old["streaming-grpc"]}),
            "an override added": said(
                ("overrides",), lambda old: [*old, {"name": "protobuf", "version": "25.1"}]),
            "an override removed": said(("overrides",), lambda old: []),
            "an override's version": said(("overrides", 0, "version"), lambda old: "1.61.0"),
            "the version of a dependency of the feature named version": said(
                ("features", "version", "dependencies", 0, "version"), lambda old: "1.3.1"),
            "an own-version key below the top": said(
                ("overrides", 0), lambda old: {**old, "version-string": "1.60.0"}),
            "the name": said(("name",), lambda old: "another-fixture"),
            "the description": said(("description",), lambda old: old + "."),
            "the letters of the name in another case": said(("name",), lambda old: "Fixture"),
            **{
                f"a top-level {member} added": said(
                    (), lambda old, member=member, value=value: {**old, member: value})
                for member, value in self.OTHER_TOP_LEVEL_MEMBERS.items()
            },
            # vcpkg builds the same without it, and its own formatter drops it.
            "an empty list of dependencies removed": said(
                ("features", "plain"), lambda old: {"description": old["description"]}),
            "a member removed": said((), lambda old: {k: old[k] for k in old if k != "homepage"}),
            "a string become a number": said(
                ("overrides", 0, "version"), lambda old: 1),
            "a list become its one item": said(streaming, lambda old: old[0]),
        }
        self.assertEqual(len(self.OTHER_TOP_LEVEL_MEMBERS), 15)
        self.assertEqual(set(self.OTHER_TOP_LEVEL_MEMBERS) & set(manifest), set())
        self.assertEqual(len(changes), 36)
        reason = f"provisioned for another dependencies digest than {host.manifest} has now"
        digests = {recorded}
        for case, changed in changes.items():
            with self.subTest(changed=case):
                self.assertNotEqual(changed, manifest)
                self.assertEqual(changed["builtin-baseline"], host.baseline)
                host.manifest.write_bytes(json.dumps(changed, indent=2).encode())
                digests.add(self.content_digest(host.manifest.read_bytes()))
                env = host.run("vcpkg-env")
                self.assertNotEqual(env.returncode, 0)
                self.assertEqual(
                    (env.stdout, env.stderr),
                    ("", f"error: the runner's vcpkg is not ready for this checkout: {reason}\n"),
                )
                self.assertEqual(self.status(host)[7], f"vcpkg_cache_ready: no ({reason})")
                check = host.run("provision-vcpkg", "--check")
                self.assert_refused(check, reason)
        self.assertEqual(len(digests), len(changes) + 1)
        self.assertEqual((host.git_calls(), host.vcpkg_calls()), ([], []))
        self.assertEqual(host.stamp.read_bytes(), stamp)

    def test_another_baseline_is_another_digest_as_well(self):
        """The baseline is compared by itself, and it is part of what the manifest says."""
        host = self.host
        host.provision()
        first = host.stamp_fields()
        later = host.commit_remote("later-port")
        host.write_manifest(later)
        host.provision()
        second = host.stamp_fields()
        self.assertEqual((first["baseline"], second["baseline"]), (host.baseline, later))
        self.assertNotEqual(first["dependencies_sha256"], second["dependencies_sha256"])
        self.assertEqual(
            second["dependencies_sha256"], self.content_digest(host.manifest.read_bytes())
        )

    RELEASE_DRIVER = REPO_ROOT / "tools/release/tensorplate-release.sh"
    PREPARED_FILES = (
        "CMakeLists.txt", "Cargo.toml", "Cargo.lock", "vcpkg.json", "CHANGELOG.md",
        "packaging/VERSION", "packaging/debian/changelog", "packaging/scripts/install.sh",
    )

    def prepared_manifest(self, manifest: str, version: str) -> str:
        """A vcpkg.json as the release driver prepares it for a version.

        The driver's own `prepare --execute`, run in a throwaway repository on
        copies of the files it prepares, with `manifest` for its vcpkg.json.
        """
        host = self.host
        sandbox = Path(tempfile.mkdtemp(dir=host.tmp, prefix="prepare."))
        for name in self.PREPARED_FILES:
            (sandbox / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(REPO_ROOT / name, sandbox / name)
        (sandbox / "vcpkg.json").write_text(manifest)
        host.git(sandbox, "init", "--quiet", "--initial-branch=prepare")
        host.git(sandbox, "add", "--", *self.PREPARED_FILES)
        host.git(sandbox, "commit", "--quiet", "-m", "the files prepare writes")
        result = subprocess.run(
            [
                str(self.RELEASE_DRIVER), "prepare", "--version", version,
                "--prep-branch", "prepare", "--execute", "--confirm", f"PREPARE-v{version}",
            ],
            cwd=sandbox,
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
            env={
                **{key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_NOSYSTEM": "1",
            },
        )
        self.assertEqual(result.returncode, 0, f"stdout={result.stdout}\nstderr={result.stderr}")
        return (sandbox / "vcpkg.json").read_text()

    def test_the_release_drivers_rewrite_of_this_repositorys_manifest_leaves_the_runner_ready(
        self,
    ):
        """`prepare` parses vcpkg.json, sets the release's version and writes all of it back.

        So it lays the file out its own way, whatever way that was before.
        Neither the version nor the layout is anything vcpkg builds from, and
        a runner provisioned before a release is prepared is ready after it.
        """
        committed = (REPO_ROOT / "vcpkg.json").read_text()
        said = json.loads(committed)
        version = "97.98.99"
        self.assertNotEqual(said["version-string"], version)
        # Only the baseline is the fixture's: its stand-in for vcpkg has no
        # commit of the real one.
        host = self.host
        pinned = said["builtin-baseline"]
        self.assertEqual(committed.count(pinned), 1)
        host.manifest.write_text(committed.replace(pinned, host.baseline))
        host.provision()
        stamp = host.stamp.read_bytes()
        host.forget_calls()

        on_one_line = json.dumps(said, separators=(",", ":"))
        for case, before in (("as committed", committed), ("on one line", on_one_line)):
            with self.subTest(manifest=case):
                prepared = self.prepared_manifest(before, version)

                self.assertEqual(json.loads(prepared), {**said, "version-string": version})
                host.manifest.write_text(prepared.replace(pinned, host.baseline))
                self.assert_ready(host)
        # The driver wrote the one-line manifest out over many: a layout the
        # runner was not provisioned from, and the same key.
        self.assertEqual(len(on_one_line.splitlines()), 1)
        self.assertGreater(len(prepared.splitlines()), len(said))
        self.assert_still_proved(host, stamp)

    def test_vcpkg_env_prints_nothing_for_a_checkout_modified_after_the_proof(self):
        """A port edited after the proof would be built in the job that used it."""

        def moved_under_a_head_that_still_names_the_baseline(host):
            later = host.commit_remote("later-port")
            host.git(host.checkout, "fetch", "--quiet", "origin")
            host.git(host.checkout, "checkout", "--quiet", "--detach", later)
            (host.checkout / ".git/HEAD").write_text(host.baseline + "\n")

        for change in (
            *self.checkout_changes(), moved_under_a_head_that_still_names_the_baseline
        ):
            with self.subTest(change=change.__name__):
                host = self.provisioned()
                change(host)
                result = host.run("vcpkg-env")
                self.assertEqual(result.stdout, "")
                self.assert_refused(result, "local modifications or untracked files")
                self.assertEqual(host.vcpkg_calls(), [])
        with self.subTest(change="git cannot say"):
            host = self.provisioned()
            result = host.run("vcpkg-env", FAKE_GIT_FAILS="status")
            self.assertEqual(result.stdout, "")
            self.assert_refused(result, "cannot inspect")

    def test_vcpkg_env_does_not_ask_git_as_root(self):
        """Root must not run git in a checkout another account can write."""
        as_root = uid_zero_prefix(self)
        host = self.provisioned()
        result = host.run("vcpkg-env", prefix=as_root)
        self.assertEqual(result.stdout, "")
        self.assert_refused(result, "does not do that as root")
        self.assertEqual(host.git_calls(), [])

    def test_a_head_that_is_not_a_regular_file_is_unreadable_and_never_waited_on(self):
        """`status` is run by root, and the runner account owns the checkout."""
        host = self.provisioned()
        head = host.checkout / ".git/HEAD"
        head.unlink()
        os.mkfifo(head)
        status = host.run("status", bound=10)
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertEqual(
            status.stdout.splitlines()[9:11],
            ["vcpkg_commit: unreadable", "vcpkg_checkout: unknown"],
        )
        env = host.run("vcpkg-env", bound=10)
        self.assertEqual(env.stdout, "")
        self.assert_refused(env, "is unreadable, not at the baseline")

    def test_a_stamp_that_is_not_a_regular_file_is_unreadable_and_never_waited_on(self):
        """The same for the stamp: root's `status` must not block on a pipe."""
        host = self.provisioned()
        host.stamp.unlink()
        os.mkfifo(host.stamp)
        reason = f"cannot read a stamp at {host.stamp} as this account"
        status = host.run("status", bound=10)
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertEqual(status.stdout.splitlines()[14:], [f"vcpkg_cache_ready: no ({reason})"])
        env = host.run("vcpkg-env", bound=10)
        self.assertEqual(env.stdout, "")
        self.assert_refused(env, reason)

    def test_on_and_off_still_end_with_the_runner_status_lines_only(self):
        functions = dict(
            re.findall(r"(?ms)^(\w+)\(\) \{\n(.*?)^\}\n", RUNNER_CONTROL.read_text())
        )
        for name in ("cmd_on", "cmd_off", "cmd_status"):
            self.assertNotIn("vcpkg", functions[name].lower(), name)
        # And run: `off` needs to be uid 0, here in a namespace that grants nothing.
        as_root = uid_zero_prefix(self)
        host = self.provisioned()
        result = host.run("off", prefix=as_root)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            [
                f"==> runner service is not installed: {host.SERVICE}",
                *self.runner_status_lines(host),
            ],
        )
        self.assertTrue(host.stamp.exists())

    def runner_status_lines(self, host: RunnerHost) -> list[str]:
        """What `status` printed for this host before it knew about vcpkg."""
        return [
            f"runner_user: {host.user}",
            f"runner_dir: {host.tmp / 'actions-runner'}",
            f"runner_service: {host.SERVICE}",
            "required_labels: self-hosted,linux,ARM64,tensorplate-release",
            "runner_configured: no",
            "service_installed: no",
            f"sudoers: disabled ({host.sudoers} absent)",
        ]

    def status(self, host: RunnerHost, **overrides) -> list[str]:
        result = host.run("status", **overrides)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        lines = result.stdout.splitlines()
        self.assertEqual(lines[:7], self.runner_status_lines(host))
        return lines[7:]

    def test_status_reports_a_runner_nothing_was_provisioned_on(self):
        host = self.host
        self.assertEqual(
            self.status(host),
            [
                f"vcpkg_root: {host.checkout}",
                f"vcpkg_baseline: {host.baseline}",
                "vcpkg_commit: absent",
                "vcpkg_checkout: absent",
                "vcpkg_tool: absent",
                f"vcpkg_binary_cache: {host.archives}",
                "vcpkg_cache_archives: 0",
                f"vcpkg_cache_ready: no (not provisioned: no stamp at {host.stamp})",
            ],
        )
        self.assertEqual(host.git_calls(), [])
        self.assertFalse(host.cache.exists())

    def test_status_reports_a_ready_runner_with_what_its_stamp_records(self):
        host = self.provisioned()
        stamp = host.stamp_fields()
        self.assertEqual(
            self.status(host),
            [
                f"vcpkg_root: {host.checkout}",
                f"vcpkg_baseline: {host.baseline}",
                f"vcpkg_commit: {host.baseline}",
                "vcpkg_checkout: current",
                "vcpkg_tool: present",
                f"vcpkg_binary_cache: {host.archives}",
                "vcpkg_cache_archives: 4",
                "vcpkg_cache_ready: yes",
                f"vcpkg_stamp_baseline: {host.baseline}",
                f"vcpkg_stamp_triplet: {self.TRIPLET}",
                "vcpkg_stamp_compiler: fixture-c++ (Fixture 1.0) 13.2.0",
                f"vcpkg_stamp_provisioned_at: {stamp['provisioned_at']}",
            ],
        )
        # Read from the files: git would refuse root a checkout it does not own.
        self.assertEqual(host.git_calls(), [])
        self.assertEqual(host.vcpkg_calls(), [])

    def test_status_reports_a_stale_checkout_and_one_left_on_a_branch(self):
        host = self.provisioned()
        later = host.commit_remote("later-port")
        host.git(host.checkout, "fetch", "--quiet", "origin")
        host.git(host.checkout, "checkout", "--quiet", "--detach", later)
        self.assertEqual(
            self.status(host)[2:4], [f"vcpkg_commit: {later}", "vcpkg_checkout: stale"]
        )
        host.git(host.checkout, "checkout", "--quiet", "-B", "pinned", host.baseline)
        self.assertEqual(
            self.status(host)[2:4], ["vcpkg_commit: not-detached", "vcpkg_checkout: stale"]
        )
        (host.checkout / ".git/HEAD").write_text("neither a commit nor a ref\n")
        self.assertEqual(
            self.status(host)[2:4], ["vcpkg_commit: unreadable", "vcpkg_checkout: unknown"]
        )

    def test_status_does_not_call_absent_what_this_account_cannot_see(self):
        """`status` run without sudo by an account the runner's home is closed to."""
        host = self.provisioned()
        home = host.checkout.parent
        home.chmod(0)
        self.addCleanup(home.chmod, 0o755)
        self.assertEqual(
            self.status(host),
            [
                f"vcpkg_root: {host.checkout}",
                f"vcpkg_baseline: {host.baseline}",
                "vcpkg_commit: unreadable",
                "vcpkg_checkout: unknown",
                "vcpkg_tool: unknown",
                f"vcpkg_binary_cache: {host.archives}",
                "vcpkg_cache_archives: unreadable",
                f"vcpkg_cache_ready: no (cannot read a stamp at {host.stamp} as this account)",
            ],
        )

    def test_status_reports_a_tool_that_cannot_run_as_absent(self):
        host = self.provisioned()
        host.tool.chmod(0o644)
        self.assertEqual(self.status(host)[4], "vcpkg_tool: absent")

    def test_status_works_when_the_manifest_cannot_be_read(self):
        host = self.provisioned()
        host.manifest.write_text('{"name": "fixture"}\n')
        lines = self.status(host)
        self.assertEqual(
            lines[1:4],
            [
                f"vcpkg_baseline: unreadable ({host.manifest} does not name a "
                "builtin-baseline of 40 lowercase hex digits)",
                f"vcpkg_commit: {host.baseline}",
                "vcpkg_checkout: unknown",
            ],
        )
        self.assertTrue(lines[7].startswith("vcpkg_cache_ready: no ("), lines[7])
        # What the stamp records is still shown: it is the runner's history.
        self.assertEqual(lines[8], f"vcpkg_stamp_baseline: {host.baseline}")

    def test_status_says_so_when_a_stamp_lacks_a_field(self):
        host = self.provisioned()
        self.rewrite_stamp(host, "compiler")
        self.assertIn("vcpkg_stamp_compiler: unreadable", self.status(host))

    def test_a_stamp_key_is_read_by_its_whole_name(self):
        host = self.provisioned()
        with host.stamp.open("a") as stamp:
            stamp.write("compiler_note=a line this script did not write\n")
        self.assertEqual(host.run("vcpkg-env").returncode, 0)
        self.assertIn(
            "vcpkg_stamp_compiler: fixture-c++ (Fixture 1.0) 13.2.0", self.status(host)
        )

    def test_status_shows_what_a_stamp_holds_as_text(self):
        """The runner account writes the stamp; root's terminal shows it."""
        host = self.provisioned()
        self.rewrite_stamp(host, "compiler", "c++\x1b[2J\rvcpkg_cache_ready: yes\x07")
        lines = self.status(host)
        self.assertIn("vcpkg_stamp_compiler: c++?[2J?vcpkg_cache_ready: yes?", lines)
        self.assertEqual(len([line for line in lines if line.startswith("vcpkg_cache_")]), 2)

    def test_status_counts_a_cache_reached_through_a_link(self):
        host = self.provisioned()
        elsewhere = host.tmp / "elsewhere"
        host.archives.rename(elsewhere)
        host.archives.symlink_to(elsewhere)
        self.assertEqual(
            self.status(host)[6:8], ["vcpkg_cache_archives: 4", "vcpkg_cache_ready: yes"]
        )

    def from_a_closed_directory(self, host: RunnerHost) -> tuple:
        """A prefix that starts a command in a directory its account cannot enter."""
        closed = host.tmp / "closed"
        closed.mkdir()
        self.addCleanup(closed.chmod, 0o755)
        return ("sh", "-c", 'cd "$0" && chmod 0 . && exec "$@"', str(closed))

    def test_provisioning_stamps_the_cache_from_a_directory_the_account_cannot_enter(self):
        host = self.host
        result = host.run("provision-vcpkg", prefix=self.from_a_closed_directory(host))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(host.stamp_fields()["archives_sha256"], self.listing_sha256(host))
        self.assertEqual(host.cache_entries(), ["archives", "provision.lock", "provisioned.stamp"])

    def test_check_passes_from_a_directory_the_account_cannot_enter(self):
        host = self.provisioned()
        stamp = host.stamp.read_bytes()
        result = host.run("provision-vcpkg", "--check", prefix=self.from_a_closed_directory(host))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(host.installs()), 1)
        self.assertEqual(host.stamp.read_bytes(), stamp)
        self.assertEqual(host.cache_entries(), ["archives", "provision.lock", "provisioned.stamp"])

    def test_vcpkg_env_answers_from_a_directory_the_account_cannot_enter(self):
        host = self.provisioned()
        expected = host.run("vcpkg-env").stdout
        result = host.run("vcpkg-env", prefix=self.from_a_closed_directory(host))
        self.assertEqual((result.returncode, result.stderr), (0, ""))
        self.assertEqual((result.stdout, len(expected.splitlines())), (expected, 3))

    def test_status_lists_the_cache_from_a_directory_the_account_cannot_enter(self):
        host = self.provisioned()
        lines = self.status(host, prefix=self.from_a_closed_directory(host))
        self.assertEqual(lines[6:8], ["vcpkg_cache_archives: 4", "vcpkg_cache_ready: yes"])

    def test_the_script_read_from_standard_input_keeps_its_runner_commands(self):
        """`bash -s -- off < script`: there is no file to find a checkout from."""
        host = self.provisioned()
        no_checkout = "this script was not started from its file in a repository checkout"

        def piped(*arguments, **overrides):
            return subprocess.run(
                ["bash", "-s", "--", *arguments],
                input=host.script.read_text(),
                capture_output=True,
                text=True,
                cwd=host.tmp,
                env=host.environment(overrides),
                timeout=60,
                check=False,
            )

        status = piped("status")
        self.assertEqual((status.returncode, status.stderr), (0, ""))
        lines = status.stdout.splitlines()
        self.assertEqual(lines[:7], self.runner_status_lines(host))
        self.assertEqual(
            lines[7:9],
            [
                f"vcpkg_root: {host.checkout}",
                f"vcpkg_baseline: unreadable (cannot read a vcpkg.json: {no_checkout})",
            ],
        )
        self.assertEqual(
            lines[14], f"vcpkg_cache_ready: no (cannot read a vcpkg.json: {no_checkout})"
        )
        # `off` gets as far as its own first check.
        self.assertEqual(piped("off").stderr, "error: run this command with sudo\n")
        # Said before the account is looked at: the command to use that a
        # refusal names would have no script in it.
        for arguments in (("provision-vcpkg",), ("provision-vcpkg", "--check")):
            for account in (host.user, "fixture-other-account"):
                with self.subTest(arguments=arguments, account=account):
                    result = piped(*arguments, TP_JETSON_RUNNER_USER=account)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(
                        (result.stdout, result.stderr), ("", f"error: {no_checkout}\n")
                    )
        env = piped("vcpkg-env")
        self.assertEqual(env.stdout, "")
        self.assert_refused(env, no_checkout)
        self.assertEqual((host.git_calls(), host.vcpkg_calls()), ([], []))

    def test_the_directories_default_to_the_runner_accounts_home(self):
        user = "fixture-runner"
        result = self.host.run(
            "status",
            TP_JETSON_RUNNER_USER=user,
            TP_JETSON_RUNNER_VCPKG_DIR=None,
            TP_JETSON_RUNNER_VCPKG_CACHE_DIR=None,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertIn(f"vcpkg_root: /home/{user}/vcpkg", lines)
        self.assertIn(
            f"vcpkg_binary_cache: /home/{user}/.cache/tensorplate-vcpkg/archives", lines
        )

    def test_help_documents_the_commands_and_their_settings(self):
        result = self.host.run("help")
        self.assertEqual(result.returncode, 0, result.stderr)
        user = "$TP_JETSON_RUNNER_USER"
        for expected in (
            "  jetson-runner-control.sh provision-vcpkg [--check]",
            "  jetson-runner-control.sh vcpkg-env",
            f"  TP_JETSON_RUNNER_VCPKG_DIR            default: /home/{user}/vcpkg",
            "  TP_JETSON_RUNNER_VCPKG_CACHE_DIR      "
            f"default: /home/{user}/.cache/tensorplate-vcpkg",
            "  TP_JETSON_RUNNER_VCPKG_GIT_URL        "
            "default: https://github.com/microsoft/vcpkg.git",
            "  TP_JETSON_RUNNER_VCPKG_BUILD_TIMEOUT  default: 21600 (seconds per vcpkg run)",
        ):
            self.assertIn(expected, result.stdout.splitlines())


def run_step(step: dict, cwd: Path, **values: str) -> subprocess.CompletedProcess:
    """Run a step's script as `shell: bash` does; values stand for its expressions."""
    env = {"PATH": os.environ["PATH"], **values}
    for name, value in (step.get("env") or {}).items():
        if "${{" not in str(value):
            env[name] = str(value)
        elif name not in values:
            raise AssertionError(f"no fixture value for step env {name}")
    return subprocess.run(
        ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
        cwd=cwd, env=env, text=True, capture_output=True, timeout=60, check=False,
    )


def evaluate(expression: object, context: dict) -> object:
    """What the runner makes of an expression of ==, !=, && and || over dotted names."""
    if "${{" not in str(expression):
        return expression
    body = str(expression).strip().removeprefix("${{").removesuffix("}}")
    body = body.replace("&&", " and ").replace("||", " or ")
    body = re.sub(r"[A-Za-z_]+(?:\.[A-Za-z_]+)+", lambda name: repr(context[name.group()]), body)
    return eval(body, {"__builtins__": {}})  # noqa: S307 - the repository's own expressions


class ReleaseWorkflowVcpkgTests(unittest.TestCase):
    """Where each job that builds the release configuration gets its vcpkg."""

    PUBLISHES = "needs.meta.outputs.publish == 'true'"

    def test_release_checks_install_their_declared_python_dependencies_first(self):
        job = self.job("apt-lifecycle.yml", "script-checks")
        install = self.only_step(job, "Install release tooling dependencies")
        self.assertIn("python3 -m pip install --quiet -r tools/release/requirements.txt",
                      job["steps"][install]["run"])
        self.assertLess(install, self.only_step(job, "Release driver checks"))

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.root = Path(self.temp_dir.name)
        self.github_env = self.root / "github-env"

    def job(self, workflow: str, name: str) -> dict:
        return yaml.safe_load((WORKFLOWS / workflow).read_text())["jobs"][name]

    def only_step(self, job: dict, text: str) -> int:
        """The position of the one step whose action, name or script holds text."""
        found = [
            index for index, step in enumerate(job["steps"])
            if any(text in str(step.get(key, "")) for key in ("uses", "name", "run"))
        ]
        self.assertEqual(len(found), 1, f"expected one step with {text!r}")
        return found[0]

    def action_steps(self) -> dict:
        action = yaml.safe_load(VCPKG_ACTION.read_text())
        self.assertEqual(action["runs"]["using"], "composite")
        return {step.get("id"): step for step in action["runs"]["steps"]}

    def tools(self, clang: str | None, binary: str | None = "fixture compiler") -> str:
        """A PATH of what the action's first step runs, with a clang of that version line."""
        tools = self.root / "path"
        shutil.rmtree(tools, ignore_errors=True)
        tools.mkdir()
        for tool in ("bash", "python3", "sha256sum", "readlink"):
            (tools / tool).symlink_to(shutil.which(tool))
        if clang:
            write_executable(tools / "clang", f"#!/bin/sh\necho '{clang}'\necho 'Target: x'\n")
        if binary:
            # Reached through a link, as the alternatives system leaves clang++.
            (self.root / "clang-15").write_text(binary)
            (tools / "clang++").symlink_to(self.root / "clang-15")
        return str(tools)

    def mode(self, workflow: str, name: str, **context: str) -> object:
        """The action's mode in a job, for a run on develop unless context says otherwise."""
        job = self.job(workflow, name)
        use = job["steps"][self.only_step(job, VCPKG_ACTION_USE)]
        self.assertEqual(list(use["with"]), ["mode"])
        given = {"publish": "false", "event_name": "workflow_dispatch", "ref_type": "branch",
                 "ref_name": "develop", **context}
        values = {f"github.{key}": value for key, value in given.items()}
        values["needs.meta.outputs.publish"] = given["publish"]
        values["github.event.repository.default_branch"] = given.get("default_branch", "develop")
        for key, value in job.get("env", {}).items():
            values[f"env.{key}"] = evaluate(value, values)
        return evaluate(use["with"]["mode"], values)

    def appended(self, step: dict, **values: str) -> tuple:
        self.github_env.write_text("")
        result = run_step(step, self.root, GITHUB_ENV=str(self.github_env), **values)
        return result, self.github_env.read_text()

    def test_the_arm64_job_takes_its_vcpkg_from_the_runner_before_it_builds(self):
        job = self.job("release.yml", "build_packages")
        asks = self.only_step(job, "jetson-runner-control.sh vcpkg-env")
        build = self.only_step(job, "build-release-artifacts.sh")
        self.assertLess(asks, build)
        self.assertEqual(job["steps"][build]["env"].get("TP_VCPKG_BINARY_ONLY"), "1")

        control = self.root / "tools/release/jetson-runner-control.sh"
        control.parent.mkdir(parents=True)
        ready = "VCPKG_ROOT=/runner/vcpkg\nVCPKG_BINARY_SOURCES=clear;files,/runner/archives,read\n"
        write_executable(
            control, f"#!/bin/sh\n[ \"$1\" = vcpkg-env ] || exit 9\nprintf '%s' '{ready}'\n"
        )
        result, written = self.appended(job["steps"][asks])
        self.assertEqual((result.returncode, written), (0, ready), result.stderr)
        write_executable(control, "#!/bin/sh\necho VCPKG_ROOT=/half\nexit 1\n")
        result, written = self.appended(job["steps"][asks])
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(written, "")
        self.assertRegex(
            result.stdout,
            r"(?m)^::error::.*not provisioned for this checkout.*docs/release/runbook\.md",
        )

    def test_the_arm64_job_installs_what_is_missing_and_upgrades_nothing(self):
        job = self.job("release.yml", "build_packages")
        run = job["steps"][self.only_step(job, "Install release build dependencies")]["run"]
        installs = re.findall(r"apt-get\.sh install ([^\\\n]*)", run)
        self.assertEqual(len(installs), 1, run)
        self.assertIn("--no-upgrade", installs[0].split())

    def test_the_arm64_job_writes_no_vcpkg_value_of_its_own(self):
        job = self.job("release.yml", "build_packages")
        scopes = (yaml.safe_load(RELEASE_WORKFLOW.read_text()), job, *job["steps"])
        names = [name for scope in scopes for name in (scope.get("env") or {})]
        written = [name for name in names if name.startswith("VCPKG_")]
        scripts = "\n".join(step.get("run") or "" for step in job["steps"])
        changed = re.findall(r"\bVCPKG_\w+=|\bunset\b[^\n;|&]*\bVCPKG_\w+", scripts)
        self.assertEqual(written + changed, [])

    def test_the_amd64_job_prepares_vcpkg_after_clang_and_before_it_compiles(self):
        job = self.job("release.yml", "build_packages_amd64")
        use = self.only_step(job, VCPKG_ACTION_USE)
        self.assertLess(self.only_step(job, "/usr/bin/clang-15 100"), use)
        self.assertLess(use, self.only_step(job, "cargo build"))
        self.assertEqual(job["timeout-minutes"], "${{ %s && 60 || 180 }}" % self.PUBLISHES)

    def test_only_the_dependency_workflow_saves_a_cache_a_publishing_run_restores(self):
        callers = sorted(
            path.name for path in WORKFLOWS.glob("*.yml") if VCPKG_ACTION_USE in path.read_text()
        )
        self.assertEqual(callers, ["apt-lifecycle.yml", CACHE_WARMER.name, "release.yml",
                                   "supply-chain.yml"])
        self.assertEqual(self.mode(CACHE_WARMER.name, "warm"), "build-and-save")
        refs = {"default branch": ("branch", "develop"), "tag": ("tag", "v1.2.3"),
                "another branch": ("branch", "topic")}
        release = {
            (ref, publish): self.mode("release.yml", "build_packages_amd64", publish=publish,
                                      ref_type=ref_type, ref_name=ref_name)
            for ref, (ref_type, ref_name) in refs.items() for publish in ("true", "false")
        }
        # A branch other than the default one saves where no publishing run reads.
        expected = {(ref, "true"): "restore" for ref in refs}
        expected.update({(ref, "false"): "build" for ref in refs})
        expected["another branch", "false"] = "build-and-save"
        self.assertEqual(release, expected)
        # An event that names no default branch cannot show the ref is another one.
        unnamed = {"ref_name": "topic", "default_branch": ""}
        self.assertEqual(self.mode("release.yml", "build_packages_amd64", **unnamed), "build")
        events = ("pull_request", "workflow_dispatch", "push", "schedule")
        smoke = [self.mode("apt-lifecycle.yml", "cpu-only-smoke", event_name=on) for on in events]
        self.assertEqual(smoke, ["build-and-save", "build", "build", "build"])
        # The native closure leg reads what is warm and builds the rest; it never saves.
        native = [self.mode("supply-chain.yml", "native", event_name=on, ref_name=ref)
                  for on in events for ref in ("develop", "topic")]
        self.assertEqual(native, ["build"] * 8)

    def test_both_release_jobs_check_the_static_closure_of_what_they_built(self):
        built = {"build_packages_amd64": "packaging/scripts/build-deb.sh",
                 "build_packages": "build-release-artifacts.sh"}
        for name, builds in built.items():
            job = self.job("release.yml", name)
            self.assertLess(self.only_step(job, builds), self.only_step(job, CLOSURE_CHECK.name))

    def closure_steps(self) -> dict:
        """Each release job's closure step, in a tree of what it reads before the checker."""
        packages = {"build_packages_amd64": "dist/amd64/tensorplate-serving_1_amd64.deb",
                    "build_packages": "rel/tensorplate-serving_1_arm64.deb"}
        made = ("bin/file", "build/release/tensorplate-serving", "build/release/CMakeCache.txt",
                "tools/release/x", *packages.values(), "dist/amd64/tensorplate_1_amd64.deb")
        for path in made:
            (self.root / path).parent.mkdir(parents=True, exist_ok=True)
            (self.root / path).write_text("")
        write_executable(self.root / "bin/file", "#!/bin/sh\necho 'ELF 64-bit, x86-64'\n")
        ls = "echo '-rwxr-xr-x root/root 1 2026-01-01 00:00 ./usr"
        ships = f"*serving*) {ls}/lib/tensorplate/tensorplate-serving' ;; *) {ls}/share/doc/x' ;;"
        listing = f"#!/bin/sh\n[ \"$1\" = -c ] || exit 0\ncase \"$2\" in {ships} esac\n"
        write_executable(self.root / "bin/dpkg-deb", listing)
        needed = "echo ' 0x01 (NEEDED) Shared library: [libc.so.6]'"
        write_executable(self.root / "bin/readelf", f"#!/bin/sh\n{needed}\n")
        jobs = {name: self.job("release.yml", name) for name in packages}
        return {name: (job["steps"][self.only_step(job, CLOSURE_CHECK.name)], packages[name])
                for name, job in jobs.items()}

    def run_closure_step(self, step: dict):
        path = f"{self.root / 'bin'}{os.pathsep}{os.environ['PATH']}"
        return run_step(step, self.root, PATH=path, RELEASE_DIR="rel")

    def test_a_closure_the_checker_refuses_fails_the_job_that_built_it(self):
        called = self.root / "called"
        for name, (step, package) in self.closure_steps().items():
            for status in (0, 1):
                with self.subTest(job=name, checker_exits=status):
                    record = f"#!/bin/sh\nprintf '%s\\n' \"$@\" >'{called}'\nexit {status}\n"
                    write_executable(self.root / "tools/release" / CLOSURE_CHECK.name, record)
                    result = self.run_closure_step(step)
                    self.assertEqual(result.returncode != 0, status != 0, result.stderr)
                    passed = ["--deb", package, "--binary", "build/release/tensorplate-serving",
                              "--cmake-cache", "build/release/CMakeCache.txt"]
                    self.assertEqual(called.read_text().split(), passed)

    def test_a_worker_configured_without_streaming_fails_both_release_jobs(self):
        cache = self.root / "build/release/CMakeCache.txt"
        for name, (step, _) in self.closure_steps().items():
            shutil.copy2(CLOSURE_CHECK, self.root / "tools/release" / CLOSURE_CHECK.name)
            for value in ("ON", "OFF"):
                with self.subTest(job=name, streaming=value):
                    cache.write_text(f"X:STRING=x\nTP_ENABLE_STREAMING_GRPC:BOOL={value}\n")
                    result = self.run_closure_step(step)
                    self.assertEqual(result.returncode == 0, value == "ON", result.stderr)
                    if value == "OFF":
                        self.assertIn("TP_ENABLE_STREAMING_GRPC:BOOL=ON", result.stderr)

    def test_every_caller_installs_clang_before_the_action_keys_the_cache_on_it(self):
        callers = (("release.yml", "build_packages_amd64"), ("apt-lifecycle.yml", "cpu-only-smoke"),
                   (CACHE_WARMER.name, "warm"), (CACHE_WARMER.name, "restore"),
                   ("supply-chain.yml", "native"))
        for workflow, name in callers:
            job = self.job(workflow, name)
            installs = self.only_step(job, "/usr/bin/clang-15 100")
            self.assertLess(installs, self.only_step(job, VCPKG_ACTION_USE), name)

    def test_the_cache_warmer_then_restores_as_a_publishing_run_does(self):
        jobs = yaml.safe_load(CACHE_WARMER.read_text())["jobs"]
        self.assertEqual(list(jobs), ["warm", "restore"])
        restore = jobs["restore"]
        self.assertEqual((restore["needs"], restore["runs-on"]), ("warm", jobs["warm"]["runs-on"]))
        self.assertLessEqual(restore["timeout-minutes"], 30)
        self.assertEqual(self.mode(CACHE_WARMER.name, "restore"), "restore")
        configure = len(restore["steps"]) - 1
        self.assertLess(self.only_step(restore, VCPKG_ACTION_USE), configure)
        self.assertEqual(self.only_step(restore, f". {PROFILE}\n"), configure)

    def test_the_cache_warmer_runs_for_the_default_branch_and_is_never_cancelled(self):
        text = CACHE_WARMER.read_text()
        workflow = yaml.safe_load(text)
        triggers = workflow[True]  # PyYAML reads the key `on` as True.
        self.assertEqual(set(triggers), {"push", "schedule", "workflow_dispatch"})
        watched = {"vcpkg.json", f"{VCPKG_ACTION_USE[2:]}/**", PROFILE,
                   f".github/workflows/{CACHE_WARMER.name}"}
        push = triggers["push"]
        self.assertEqual((push["branches"], set(push["paths"])), (["develop"], watched))
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        self.assertIs(workflow["concurrency"]["cancel-in-progress"], False)
        self.assertNotIn("secrets.", text)
        # An Actions cache nothing reads for seven days is evicted.
        days = set()
        for entry in triggers["schedule"]:
            _, _, day_of_month, month, weekdays = entry["cron"].split()
            self.assertEqual((day_of_month, month), ("*", "*"), entry)
            days.update(range(7) if weekdays == "*" else map(int, weekdays.split(",")))
        self.assertGreaterEqual(len(days), 2, "the warmer must run at least twice a week")
        for use in re.findall(r"(?m)^\s*(?:- )?uses:\s*(\S+)", text):
            self.assertRegex(use, r"^\./|@[0-9a-f]{40}$")

    def test_the_smoke_and_the_cache_warmer_build_what_the_cache_lacks(self):
        smoke = self.job("apt-lifecycle.yml", "cpu-only-smoke")
        warmer = self.job(CACHE_WARMER.name, "warm")
        # sudo drops what the action exported unless the command names it.
        run = smoke["steps"][self.only_step(smoke, "verify_cpu_only_smoke.sh")]["run"]
        preserved = re.search(r"--preserve-env=(\S+)", run).group(1).split(",")
        exported = {"VCPKG_ROOT", "VCPKG_BINARY_SOURCES", "VCPKG_FORCE_DOWNLOADED_BINARIES"}
        self.assertLessEqual(exported, set(preserved))
        # The warmer configures the release profile and compiles nothing of this tree.
        scripts = "\n".join(step.get("run") or "" for step in warmer["steps"])
        self.assertIn(f". {PROFILE}\n", scripts)
        self.assertIn('"${TP_AMD64_CMAKE_ARGS[@]}"', scripts)
        self.assertNotIn("cmake --build", scripts)

    def test_the_action_names_no_commit_and_pins_what_it_uses(self):
        text = VCPKG_ACTION.read_text()
        uses = re.findall(r"(?m)^\s*(?:- )?uses:\s*(\S+)", text)
        self.assertTrue(uses, "the action restores no cache")
        for use in uses:
            self.assertRegex(use, r"@[0-9a-f]{40}$")
        rest = re.sub(r"(?m)^\s*(?:- )?uses:.*$", "", text)
        self.assertEqual(re.findall(r"[0-9a-f]{40}", rest), [])

    def test_each_mode_restores_and_saves_what_it_says_and_no_more(self):
        action = yaml.safe_load(VCPKG_ACTION.read_text())
        self.assertEqual(list(action["inputs"]), ["mode"])
        caches = {step["if"]: step for step in action["runs"]["steps"] if "uses" in step}
        key = "vcpkg-release-${{ runner.os }}-x64-linux-${{ steps.manifest.outputs.baseline }}-"
        exact = {"path": "${{ runner.temp }}/vcpkg-archives",
                 "key": key + "${{ steps.manifest.outputs.digest }}"}
        # No restore-keys anywhere: only the exact key is ever read or written.
        expected = {
            "restore": ("actions/cache/restore", {**exact, "fail-on-cache-miss": True}),
            "build": ("actions/cache/restore", exact),
            "build-and-save": ("actions/cache", exact),
        }
        self.assertEqual(set(caches), {f"inputs.mode == '{mode}'" for mode in expected})
        for mode, (uses, inputs) in expected.items():
            step = caches[f"inputs.mode == '{mode}'"]
            self.assertRegex(step["uses"], rf"^{uses}@[0-9a-f]{{40}}$")
            self.assertEqual(step["with"], inputs, mode)

    def test_the_action_keys_its_cache_on_the_baseline_and_the_dependencies(self):
        steps = self.action_steps()
        manifest = {"name": "fixture", "version-string": "0.3.1",
                    "builtin-baseline": "ab" * 20, "dependencies": ["grpc"]}
        output = self.root / "output"
        compiler = "Ubuntu clang version 15.0.7"

        # Where the action's own file is: not the workspace of the job that uses it.
        self.assertEqual(steps["manifest"]["env"], {"ACTION_PATH": "${{ github.action_path }}"})
        covered = {"profile": self.root / PROFILE, "action": self.root / "action/action.yml"}
        for path in covered.values():
            path.parent.mkdir(parents=True)
            path.write_text("fixture\n")

        def keyed(*compilers, **change):
            (self.root / "vcpkg.json").write_text(json.dumps({**manifest, **change}, indent=2))
            output.write_text("")
            values = {"GITHUB_OUTPUT": str(output), "PATH": self.tools(*compilers or (compiler,)),
                      "ACTION_PATH": str(self.root / "action")}
            return run_step(steps["manifest"], self.root, **values)

        def outputs(*compilers, **change) -> dict:
            result = keyed(*compilers, **change)
            self.assertEqual(result.returncode, 0, result.stderr)
            return dict(line.split("=", 1) for line in output.read_text().splitlines())

        first = outputs()
        said = {key: value for key, value in manifest.items() if key != "version-string"}
        canonical = json.dumps(said, sort_keys=True, separators=(",", ":"))
        binary = hashlib.sha256(b"fixture compiler").hexdigest()
        file = hashlib.sha256(b"fixture\n").hexdigest()
        keyed_on = f"{canonical}\n{compiler}\n{binary}\n{file}\n{file}"
        digest = hashlib.sha256(keyed_on.encode()).hexdigest()
        self.assertEqual(first, {"baseline": "ab" * 20, "digest": digest})
        # A change to the profile or to the action itself opens a new key.
        for name, path in covered.items():
            path.write_text("changed\n")
            self.assertNotEqual(outputs()["digest"], digest, name)
            path.write_text("fixture\n")
        # The release tooling rewrites the version; the cache must stay warm.
        for version in ("version", "version-string", "version-semver", "version-date"):
            self.assertEqual(outputs(**{version: "0.4.0"}), first, version)
        self.assertNotEqual(outputs(dependencies=["grpc", "protobuf"])["digest"], digest)
        # vcpkg keys packages on the compiler binary: another one is another key.
        self.assertNotEqual(outputs("Ubuntu clang version 15.0.8")["digest"], digest)
        self.assertNotEqual(outputs(compiler, "rebuilt under the same version")["digest"], digest)
        for missing in ((None,), (compiler, None)):
            unkeyed = keyed(*missing)
            self.assertNotEqual(unkeyed.returncode, 0, unkeyed.stdout)
            self.assertRegex(unkeyed.stdout, r"(?m)^::error::clang\S* is not on PATH")
            self.assertEqual(output.read_text(), "")
        for baseline in ("AB" * 20, "ab" * 19, None):
            bad = keyed(**{"builtin-baseline": baseline})
            self.assertNotEqual(bad.returncode, 0, baseline)
            self.assertEqual((output.read_text(), "builtin-baseline" in bad.stderr), ("", True))

    def test_the_action_lets_vcpkg_build_in_every_mode_but_restore(self):
        export = self.action_steps()["export"]
        root = "VCPKG_ROOT=/runner/temp/vcpkg\n"
        sources = "VCPKG_BINARY_SOURCES=clear;files,/runner/temp/vcpkg-archives,"
        # vcpkg's own cmake and ninja, whichever way the cache is used.
        own = "VCPKG_FORCE_DOWNLOADED_BINARIES=1\n"
        expected = {
            "restore": (0, root + sources + "read\n" + own + "TP_VCPKG_BINARY_ONLY=1\n"),
            "build": (0, root + sources + "readwrite\n" + own),
            "build-and-save": (0, root + sources + "readwrite\n" + own),
            # Any other value must not become a build that may run cold.
            "true": (1, ""),
            "": (1, ""),
        }
        for mode, outcome in expected.items():
            with self.subTest(mode=mode):
                result, written = self.appended(export, MODE=mode, RUNNER_TEMP="/runner/temp")
                self.assertEqual((result.returncode, written), outcome, result.stderr)


class NativeClosureScanTests(unittest.TestCase):
    """Run workflow steps with a stub scanner and real linked absence controls."""

    job = ReleaseWorkflowVcpkgTests.job
    only_step = ReleaseWorkflowVcpkgTests.only_step

    @classmethod
    def setUpClass(cls):
        from test_native_code_absence import require_native_toolchain
        require_native_toolchain()

    def setUp(self):
        ReleaseWorkflowVcpkgTests.setUp(self)
        from test_native_code_absence import build_fixture
        build_fixture(self.root / "temp")

    def scan_report(self, matches: int = 0) -> dict:
        dispositions = json.loads((REPO_ROOT / "tools/release/vulnerability-dispositions.json").read_text())
        ignored = [{"vulnerability": {"id": e["id"], "severity": "High"},
                    "artifact": {"name": e["package"], "version": "1.0", "type": "vcpkg"}}
                   for e in dispositions["dispositions"] if e["ecosystem"] == "vcpkg"]
        match = {"vulnerability": {"id": "CVE-2000-0001", "severity": "Unknown"},
                 "artifact": {"name": "openssl", "version": "3.0.0", "type": "vcpkg"}}
        return {"source": {"type": "sbom-file"}, "matches": [match] * matches,
                "ignoredMatches": ignored}

    def control_report(self) -> dict:
        from test_native_sbom import tool
        matches = []
        for artifact_id, name, identity in tool.control_entries():
            cpe = tool.cpe(identity.vendor, identity.product, identity.control_version)
            matches.append({
                "artifact": {"id": artifact_id, "name": name, "type": "vcpkg",
                             "purl": tool.purl(name, identity.control_version)},
                "vulnerability": {"id": identity.control_advisory},
                "matchDetails": [{"type": "cpe-match",
                                  "searchedBy": {"namespace": "nvd:cpe", "cpes": [cpe]},
                                  "found": {"cpes": [cpe], "vulnerabilityID": identity.control_advisory}}],
            })
        return {"matches": matches}

    def grype(self, exit_status: int, report: dict) -> str:
        """Record the scanner arguments and return the test's constructed report."""
        (self.root / "bin").mkdir(exist_ok=True)
        report_path = self.root / "stub-report.json"
        report_path.write_text(json.dumps(report))
        write_executable(
            self.root / "bin/grype",
            "#!/bin/sh\n"
            f"printf '%s\\n' \"$*\" >>'{self.root / 'grype-calls'}'\n"
            "for arg in \"$@\"; do case \"$arg\" in json=*) "
            f"cp '{report_path}' \"${{arg#json=}}\" ;; esac; done\n"
            f"exit {exit_status}\n",
        )
        return f"{self.root / 'bin'}{os.pathsep}{os.environ['PATH']}"

    def run_named(self, name: str, path: str,
                  today: datetime.date = datetime.date(2026, 10, 7)) -> subprocess.CompletedProcess:
        job = self.job("supply-chain.yml", "native")
        step = job["steps"][self.only_step(job, name)]
        write_executable(
            self.root / "bin/python3",
            "#!/bin/sh\n"
            "if [ \"$1\" = tools/release/check-vulnerability-dispositions.py ]; then\n"
            f"  exec '{sys.executable}' \"$@\" --today '{today.isoformat()}'\n"
            "fi\n"
            f"exec '{sys.executable}' \"$@\"\n",
        )
        return run_step(step, REPO_ROOT, PATH=path, RUNNER_TEMP=str(self.root / "temp"))

    def test_the_live_scan_enforces_the_supplied_expiry_date_before_scanning(self):
        entries = json.loads((REPO_ROOT / "tools/release/vulnerability-dispositions.json").read_text())["dispositions"]
        if not entries:
            self.skipTest("no live dispositions to expire")
        after_expiry = min(datetime.date.fromisoformat(e["review_by"]) for e in entries) + datetime.timedelta(days=1)
        result = self.run_named("grype, with the recorded dispositions",
                                self.grype(0, self.scan_report()), after_expiry)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("expired on ", result.stderr)
        self.assertFalse((self.root / "grype-calls").exists())

    def test_the_positive_control_needs_threshold_exit_and_every_pair(self):
        complete = self.control_report()
        incomplete = {"matches": complete["matches"][:-1]}
        cases = [(2, complete, True), (2, incomplete, False),
                 (2, {"matches": []}, False), (0, complete, False), (1, complete, False)]
        for status, report, passes in cases:
            with self.subTest(status=status, matches=len(report["matches"])):
                result = self.run_named("grype positive control", self.grype(status, report))
                self.assertEqual(result.returncode == 0, passes, result.stderr)
        control = json.loads((self.root / "temp/positive-control.spdx.json").read_text())
        self.assertEqual(len(control["packages"]), len(complete["matches"]))

    def test_scan_refuses_any_severity_incomplete_scan_and_unused_dispositions(self):
        valid = self.scan_report()
        unused = self.scan_report()
        unused["ignoredMatches"].pop()
        cases = [(0, valid, True), (0, unused, False), (0, self.scan_report(1), False),
                 (1, valid, False), (2, valid, False)]
        for status, report, passes in cases:
            with self.subTest(status=status, report=report):
                result = self.run_named("grype, with the recorded dispositions", self.grype(status, report))
                self.assertEqual(result.returncode == 0, passes, result.stdout + result.stderr)
        result = self.run_named("grype, with the recorded dispositions", self.grype(0, self.scan_report(1)))
        self.assertIn("CVE-2000-0001", result.stderr)
        self.assertIn("Unknown", result.stderr)
        self.assertNotIn("--fail-on", (self.root / "grype-calls").read_text().splitlines()[-1])
        config = yaml.safe_load((self.root / "temp/grype.yaml").read_text())
        self.assertNotIn("CVE-2026-0994", [rule["vulnerability"] for rule in config["ignore"]])

    def test_the_workflow_cannot_ignore_an_advisory_once_its_code_is_linked(self):
        from test_native_code_absence import build_fixture
        entries = json.loads((REPO_ROOT / "tools/release/vulnerability-dispositions.json").read_text())["dispositions"]
        ports = sorted({e["package"] for e in entries if e.get("requires_absent")})
        if not ports:
            self.skipTest("no live disposition rests on absent code")
        for port in ports:
            with self.subTest(port=port):
                build_fixture(self.root / "temp", included_port=port)
                result = self.run_named("grype, with the recorded dispositions",
                                        self.grype(0, self.scan_report()))
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(f"{port}: worker extracted archive members", result.stderr)
                self.assertFalse((self.root / "grype-calls").exists())

    def test_build_sbom_control_and_scan_run_in_order(self):
        job = self.job("supply-chain.yml", "native")
        order = [self.only_step(job, text) for text in (
            "Configure the release profile", "Build the worker and check absent native code",
            "native-sbom.py collect", "Upload the SBOM", "grype positive control",
            "grype, with the recorded dispositions")]
        self.assertEqual(order, sorted(order))
        self.assertIn("-DCMAKE_EXE_LINKER_FLAGS=-Wl,-Map,", job["steps"][order[0]]["run"])
        build = job["steps"][order[1]]["run"]
        self.assertIn("cmake --build", build)
        self.assertIn("--target tp_serving_worker", build)
        self.assertIn("--port grpc --port openssl --port zlib", build)
        collect = job["steps"][order[2]]["run"]
        for arg in ("native-sbom.py check", "--cmake-cache", "--worker", "--created"):
            self.assertIn(arg, collect)
        self.assertIn("git show -s --format=%ct HEAD", collect)
        self.assertIn("--grype-report", job["steps"][order[-1]]["run"])
        report = job["steps"][self.only_step(job, "Upload the scan report")]
        self.assertIn("${{ runner.temp }}/native/worker.map", report["with"]["path"].splitlines())


class StaticStreamingClosureTests(unittest.TestCase):
    """The closure checker against stand-ins for dpkg-deb and readelf."""

    ARGS = ("--deb", "serving.deb", "--binary", "worker", "--cmake-cache", "CMakeCache.txt")

    def check(self, depends="libc6 (>= 2.35), libstdc++6", needed=("libc.so.6",), dpkg_status=0,
              readelf_status=0, args=ARGS, pre_depends="tensorplate-common (= 1)", streaming="ON"):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "serving.deb").write_text("")
            (root / "worker").write_text("")
            cache = f"CMAKE_BUILD_TYPE:STRING=x\nTP_ENABLE_STREAMING_GRPC:BOOL={streaming}\n"
            (root / "CMakeCache.txt").write_text(cache)
            fields = f"-fDepends) echo '{depends}' ;; -fPre-Depends) echo '{pre_depends}' ;;"
            asked = f"#!/bin/sh\ncase \"$1$3\" in {fields} *) exit 9 ;; esac\n"
            write_executable(root / "dpkg-deb", f"{asked}exit {dpkg_status}\n")
            entries = [f" 0x01 (NEEDED)   Shared library: [{name}]" for name in needed]
            dynamic = "\n".join(["Dynamic section at offset 0x1 contains 2 entries:", *entries])
            listed = f"#!/bin/sh\n[ \"$1\" = -d ] || exit 9\necho '{dynamic}'\n"
            write_executable(root / "readelf", f"{listed}exit {readelf_status}\n")
            env = {**os.environ, "PATH": f"{root}{os.pathsep}{os.environ['PATH']}"}
            return subprocess.run(
                [str(CLOSURE_CHECK), *args], cwd=root, env=env,
                text=True, capture_output=True, timeout=30, check=False,
            )

    def test_a_streaming_worker_with_a_static_closure_passes_with_a_line_per_check(self):
        result = self.check(needed=("libstdc++.so.6", "libz.so.1", "libc.so.6"))
        self.assertEqual((result.returncode, len(result.stdout.splitlines())), (0, 3), result)

    def test_what_the_check_finds_or_cannot_make_fails_and_is_named(self):
        refused = {
            "libgrpc++1": {"depends": "libc6, libgrpc++1 (>= 1.30)"},
            "libprotobuf23": {"depends": "libc6 | libprotobuf23:any"},
            "libgrpc10": {"pre_depends": "tensorplate-common (= 1), libgrpc10"},
            "libabsl20220623": {"depends": "libabsl20220623 (>= 0~20220623.0-1)"},
            "libre2-9": {"depends": "libc6, libre2-9"},
            "libc-ares2": {"depends": "libc-ares2 (>= 1.11.0~rc1)"},
            "libupb0": {"pre_depends": "libupb0"},
            "libssl3": {"depends": "libssl3 (>= 3.0.0~~alpha1) | libssl1.1"},
            # What the worker itself asks the loader for, and nothing it inherits.
            **{name: {"needed": ("libc.so.6", name)} for name in (
                "libgrpc++.so.1", "libgpr.so.10", "libprotobuf.so.23", "libabsl_base.so.2206",
                "libre2.so.9", "libcares.so.2", "libupb.so.0", "libssl.so.3", "libcrypto.so.3",
                "/opt/lib/libgrpc.so.37")},
            # The feature must be compiled in: a worker without it has a clean closure too.
            "TP_ENABLE_STREAMING_GRPC:BOOL=ON": {"streaming": "OFF"},
            "does not record TP_ENABLE_STREAMING_GRPC:BOOL=ON": {"streaming": "ON_REQUEST"},
            "--cmake-cache": {"args": self.ARGS[:4]},
            "--cmake-cache needs a value": {"args": self.ARGS[:5]},
            "--deb needs a value": {"args": ("--binary", "worker", "--deb")},
            "--binary needs a value": {"args": ("--binary", "--deb", "serving.deb")},
            # A check that could not be made is not a pass.
            "dpkg-deb": {"dpkg_status": 2},
            "readelf": {"readelf_status": 1},
            "no NEEDED entry": {"needed": ()},
            "--binary": {"args": self.ARGS[:2] + self.ARGS[4:]},
            "--deb": {"args": self.ARGS[2:]},
        }
        for name, case in refused.items():
            with self.subTest(name=name):
                result = self.check(**case)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(name, result.stderr)


class SourceInstallStreamingTests(unittest.TestCase):
    """What the source install asks the builder for, with a stand-in for the builder."""

    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.root = Path(self.temp_dir.name)
        self.source = self.root / "source"
        (self.source / "tools/release").mkdir(parents=True)
        (self.source / "packaging").mkdir()
        write_executable(self.source / "packaging/version.sh", "#!/bin/sh\necho 0.2.1\n")
        (self.root / "vcpkg/scripts/buildsystems").mkdir(parents=True)
        (self.root / "vcpkg/scripts/buildsystems/vcpkg.cmake").write_text("")
        identity = ["-c", "user.name=Tests", "-c", "user.email=tests@tensorplate.invalid"]
        for step in (["init", "-q"], [*identity, "commit", "-q", "--allow-empty", "-m", "fixture"]):
            subprocess.run(["git", *step], cwd=self.source, check=True, capture_output=True)

    def install(self, arch: str, builder_takes_the_flag: bool = True, help_status: int = 0,
                **env: str):
        """(what the builder was called with, the wrapper's result); None when it never ran."""
        called = self.root / "called"
        called.unlink(missing_ok=True)
        usage = "--without-streaming" if builder_takes_the_flag else "--arch ARCH"
        write_executable(
            self.source / "tools/release/build-release-artifacts.sh",
            f"#!/bin/sh\n[ \"$1\" != --help ] || {{ echo '{usage}'; exit {help_status}; }}\n"
            f"printf '%s\\n' \"$@\" >'{called}'\nexit 7\n",
        )
        result = subprocess.run(
            ["bash", str(SOURCE_INSTALL), "--no-install", "--source-dir", str(self.source),
             "--artifacts-dir", str(self.root / "out"), "--arch", arch],
            cwd=self.root, env={"PATH": os.environ["PATH"], **env},
            text=True, capture_output=True, timeout=60,
        )
        return (called.read_text().split() if called.exists() else None), result

    def test_a_source_install_builds_with_streaming_only_when_given_a_vcpkg_checkout(self):
        checkout = {"VCPKG_ROOT": str(self.root / "vcpkg")}
        override = {"TP_CMAKE_TOOLCHAIN_FILE": "/elsewhere/toolchain.cmake"}
        cases = (  # arch, environment, builder takes the flag, its --help status, flag passed
            ("arm64", {}, True, 0, True), ("amd64", {}, True, 0, True),
            ("amd64", override, True, 0, True),
            ("arm64", checkout, True, 0, False), ("amd64", checkout, True, 0, False),
            ("arm64", override, True, 0, False),
            # A branch whose builder predates the flag is built as that builder builds.
            ("arm64", {}, False, 0, False), ("arm64", {}, True, 3, False),
        )
        # The decision reads the builder's --help, so the real one must name the flag.
        usage = subprocess.run([str(BUILD_SCRIPT), "--help"], text=True, capture_output=True)
        self.assertEqual((usage.returncode, "--without-streaming" in usage.stdout), (0, True))
        for arch, env, takes, help_status, passed in cases:
            with self.subTest(arch=arch, env=sorted(env), takes=takes, help_status=help_status):
                args, result = self.install(arch, takes, help_status, **env)
                self.assertEqual("--without-streaming" in args, passed, result.stderr)
                self.assertIn("--snapshot", args)
                said = [line for line in result.stdout.splitlines() if "without streaming" in line]
                self.assertEqual(len(said), 1 if passed else 0, result.stdout)
                self.assertEqual(result.returncode, 7, "the wrapper stops where its builder does")
                for line in said:
                    self.assertIn("VCPKG_ROOT", line)

    def built_package(self, arch: str, version_lines: list[str], exit_status: int = 0) -> Path:
        """A tensorplate-serving package whose worker prints the given --version lines."""
        tree = self.root / f"pkg-{arch}"
        shutil.rmtree(tree, ignore_errors=True)
        (tree / "DEBIAN").mkdir(parents=True)
        (tree / "usr/lib/tensorplate").mkdir(parents=True)
        (tree / "DEBIAN/control").write_text(
            f"Package: tensorplate-serving\nVersion: 0.2.1~dev.1.abc\nArchitecture: {arch}\n"
            "Maintainer: tests <tests@tensorplate.invalid>\nDescription: stub\n"
        )
        printed = "".join(f"echo '{line}'\n" for line in version_lines)
        write_executable(tree / "usr/lib/tensorplate/tensorplate-serving",
                         f"#!/bin/sh\n{printed}exit {exit_status}\n")
        package = self.root / f"tensorplate-serving_0.2.1.dev.1.abc_{arch}.deb"
        subprocess.run(["dpkg-deb", "-b", "--root-owner-group", str(tree), str(package)],
                       check=True, capture_output=True)
        return package

    def install_built(self, arch: str, version_lines: list[str], copies: int = 1,
                      exit_status: int = 0, path: str | None = None):
        """The wrapper with a builder that stages the stub package instead of building."""
        package = self.built_package(arch, version_lines, exit_status)
        shutil.rmtree(self.root / "out", ignore_errors=True)
        staged = " && ".join(
            f"cp '{package}' \"$out/tensorplate-serving_0.2.1.dev.{index}.abc_{arch}.deb\""
            for index in range(1, copies + 1)
        )
        write_executable(
            self.source / "tools/release/build-release-artifacts.sh",
            "#!/bin/sh\n[ \"$1\" != --help ] || { echo '--without-streaming'; exit 0; }\n"
            "while [ $# -gt 0 ]; do [ \"$1\" = --artifacts-dir ] && out=$2; shift; done\n"
            f"mkdir -p \"$out\" && {staged}\n",
        )
        return subprocess.run(
            ["bash", str(SOURCE_INSTALL), "--no-install", "--source-dir", str(self.source),
             "--artifacts-dir", str(self.root / "out"), "--arch", arch],
            cwd=self.root, env={"PATH": path or os.environ["PATH"]},
            text=True, capture_output=True, timeout=60,
        )

    def path_without(self, *names: str) -> str:
        """A PATH of links to every executable on the real one but the named tools."""
        links = self.root / ("bin-without-" + "-".join(names))
        links.mkdir(exist_ok=True)
        for directory in os.environ["PATH"].split(os.pathsep):
            for entry in sorted(Path(directory).glob("*")) if Path(directory).is_dir() else ():
                link = links / entry.name
                if entry.name not in names and not link.exists() and os.access(entry, os.X_OK):
                    link.symlink_to(entry)
        return str(links)

    def test_a_source_install_says_whether_the_built_worker_has_streaming_support(self):
        if shutil.which("dpkg-deb") is None or shutil.which("dpkg") is None:
            self.skipTest("the stub package needs dpkg-deb and dpkg")
        three = ["tensorplate-serving 0.2.1", "protocol 0.1", "bundle-format 0.1"]
        host = subprocess.run(["dpkg", "--print-architecture"], check=True,
                              capture_output=True, text=True).stdout.strip()
        other = "arm64" if host != "arm64" else "amd64"
        cases = (  # arch, the worker's --version lines, what the wrapper says
            (host, [*three, "streaming-grpc on"], "Streaming gRPC support in the built worker: on"),
            (host, [*three, "streaming-grpc off"],
             "Streaming gRPC support in the built worker: off"),
            (host, three, "Streaming gRPC support in the built worker: not reported"),
            (other, [*three, "streaming-grpc on"],
             f"Streaming gRPC support in the built worker: not run (the package is for {other}"),
        )
        prefix = "Streaming gRPC support in the built worker: "
        more = (  # arch, lines, what the wrapper says, copies, the worker's exit, PATH
            (host, three, f"{prefix}not run (expected one tensorplate-serving", 2, 0, None),
            (host, [*three, "streaming-grpc on"], f"{prefix}not run (the built worker exited 3",
             1, 3, None),
            (host, [*three, "streaming-grpc maybe"],
             f"{prefix}not recognised (the worker printed 'streaming-grpc maybe')", 1, 0, None),
            (host, [*three, "streaming-grpc on"], f"{prefix}not run (dpkg-deb is not available",
             1, 0, self.path_without("dpkg-deb")),
        )
        for arch, lines, said, copies, status, path in (*((*c, 1, 0, None) for c in cases), *more):
            with self.subTest(arch=arch, last=lines[-1], copies=copies, status=status,
                              path=path is not None):
                result = self.install_built(arch, lines, copies, status, path)
                self.assertEqual(result.returncode, 0, result.stderr)
                reported = [
                    line for line in result.stdout.splitlines()
                    if "Streaming gRPC support in the built worker" in line
                ]
                self.assertEqual(len(reported), 1, result.stdout)
                self.assertTrue(reported[0].startswith(said), reported[0])

    def test_a_vcpkg_root_that_names_no_checkout_is_refused_before_the_builder_runs(self):
        args, result = self.install("amd64", VCPKG_ROOT=str(self.root))
        self.assertEqual((args, result.returncode), (None, 1), result.stdout)
        self.assertIn("error: VCPKG_ROOT must name a vcpkg checkout", result.stderr)


if __name__ == "__main__":
    unittest.main()
