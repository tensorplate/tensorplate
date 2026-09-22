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
sys.path.insert(0, str(Path(__file__).resolve().parent))

from test_artifact_identity import (  # noqa: E402
    DPKG_DEB_STUB,
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

    def build(self, tag: str, deb_version: str, python_version: str):
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
             "--sdk-dist-dir", str(sdk)],
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
        return sorted(packages)

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
