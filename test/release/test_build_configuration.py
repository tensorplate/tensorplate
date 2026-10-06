#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Black-box tests for the C++ configure step of the release builder.

The builder runs against a fixture checkout with stub dpkg, shellcheck,
cargo, cmake and clang++ on PATH. The stub cmake records the environment
compilers and every argument it is given and then fails, so each case stops
right after configure and nothing is compiled.

ReleaseStagingTests run the builder past configure to the end, with the
build steps stubbed, to see which names its packages are published under.
"""

from __future__ import annotations

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
RELEASE_WORKFLOW = REPO_ROOT / ".github/workflows/release.yml"
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
    "-DTP_ENABLE_STREAMING_GRPC=OFF",
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


class BuildConfigurationTests(BuilderFixture):
    # -- arm64 pins its release defaults ------------------------------------

    def test_arm64_snapshot_pins_configure_arguments(self) -> None:
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
        # The stub cmake fails configure; the builder stops there and says so.
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("error: C++ configure failed", result.stdout)
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

    def test_release_workflow_explicitly_disables_streaming(self) -> None:
        release = self.run_release_step()
        streaming_args = [
            arg for arg in release["args"] if arg.startswith("-DTP_ENABLE_STREAMING_GRPC=")
        ]
        self.assertEqual(streaming_args, ["-DTP_ENABLE_STREAMING_GRPC=OFF"])

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
    HELPERS = ("tools/ci/apt-get.sh",)

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


if __name__ == "__main__":
    unittest.main()
