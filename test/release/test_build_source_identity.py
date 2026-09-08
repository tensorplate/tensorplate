#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Focused black-box tests for release-builder source identity checks."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
BUILD_SCRIPT = REPO_ROOT / "tools/release/build-release-artifacts.sh"
VERSION_SCRIPT = REPO_ROOT / "packaging/version.sh"


class BuildSourceIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp_dir.name) / "fixture"
        (self.repo / "packaging/debian").mkdir(parents=True)
        shutil.copy2(VERSION_SCRIPT, self.repo / "packaging/version.sh")
        self.changelog = self.repo / "packaging/debian/changelog"
        self.original_changelog = (
            "tensorplate (0.2.1-1) unstable; urgency=medium\n\n"
            "  * Fixture.\n\n"
            " -- Fixture <fixture@example.com>  Thu, 01 Jan 1970 00:00:00 +0000\n"
        )
        self.changelog.write_text(self.original_changelog)

        self.fake_bin = Path(self.temp_dir.name) / "bin"
        self.fake_bin.mkdir()
        fake_dpkg = self.fake_bin / "dpkg"
        fake_dpkg.write_text(
            "#!/usr/bin/env sh\n"
            "if [ \"${1:-}\" = --print-architecture ]; then\n"
            "  printf '%s\\n' fixture-arch\n"
            "  exit 0\n"
            "fi\n"
            "exit 2\n"
        )
        fake_dpkg.chmod(0o755)
        subprocess.run(
            ["git", "init", "-q", "-b", "fixture"],
            cwd=self.repo,
            check=True,
        )

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def write_source_versions(
        self,
        *,
        packaging: str = "0.2.1",
        cmake: str = "0.2.1",
        cargo: str = "0.2.1",
    ) -> None:
        (self.repo / "packaging/VERSION").write_text(f"{packaging}\n")
        (self.repo / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 3.25)\n"
            "project(\n"
            "  TensorPlate\n"
            f"  VERSION {cmake}\n"
            "  LANGUAGES CXX\n"
            ")\n"
        )
        (self.repo / "Cargo.toml").write_text(
            "[workspace]\n"
            'members = []\n\n'
            "[workspace.package]\n"
            f'version = "{cargo}"\n'
            'edition = "2021"\n'
        )

    def run_builder(
        self,
        *,
        version: str = "0.2.1",
        deb_version: str = "0.2.1~rc.1",
        python_version: str = "0.2.1rc1",
        tag: str = "v0.2.1-rc.1",
        arch: str = "fixture-arch",
        snapshot: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        args = [
            str(BUILD_SCRIPT),
            "--version",
            version,
            "--deb-version",
            deb_version,
            "--python-version",
            python_version,
            "--tag",
            tag,
            "--artifacts-dir",
            str(self.repo / "artifacts"),
            "--manifest",
            str(self.repo / "manifest.json"),
            "--checksums",
            str(self.repo / "SHA256SUMS"),
            "--arch",
            arch,
            "--skip-tag-verify",
        ]
        if snapshot:
            args.append("--snapshot")
        env = os.environ.copy()
        env["PATH"] = f"{self.fake_bin}{os.pathsep}{env['PATH']}"
        return subprocess.run(
            args,
            cwd=self.repo,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=30,
            check=False,
        )

    def assert_identity_failure(
        self, result: subprocess.CompletedProcess[str], expected: str
    ) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("source version mismatch", result.stdout)
        self.assertIn(expected, result.stdout)
        self.assertEqual(self.changelog.read_text(), self.original_changelog)

    def test_rejects_requested_canonical_version_from_another_source(self) -> None:
        self.write_source_versions()
        result = self.run_builder(
            version="0.2.2",
            deb_version="0.2.2~rc.1",
            python_version="0.2.2rc1",
            tag="v0.2.2-rc.1",
        )
        self.assert_identity_failure(result, "packaging/VERSION base 0.2.1")

    def test_rejects_cmake_source_version_disagreement(self) -> None:
        self.write_source_versions(cmake="0.2.0")
        result = self.run_builder()
        self.assert_identity_failure(result, "CMakeLists.txt project VERSION 0.2.0")

    def test_rejects_cargo_source_version_disagreement(self) -> None:
        self.write_source_versions(cargo="0.2.0")
        result = self.run_builder()
        self.assert_identity_failure(
            result, "Cargo.toml workspace package version 0.2.0"
        )

    def test_snapshot_version_must_use_the_source_base(self) -> None:
        self.write_source_versions()
        result = self.run_builder(
            version="0.2.2~dev.20260906.deadbeef1234",
            deb_version="0.2.2~dev.20260906.deadbeef1234",
            python_version="0.2.2~dev.20260906.deadbeef1234",
            tag="snapshot-fixture-deadbeef1234",
            snapshot=True,
        )
        self.assert_identity_failure(result, "packaging/VERSION base 0.2.1")

    def test_rejects_repeated_release_candidate_marker(self) -> None:
        self.write_source_versions()
        result = self.run_builder(tag="v0.2.1-rc.bad-rc.1")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("positive numeric release-candidate number", result.stdout)
        self.assertEqual(self.changelog.read_text(), self.original_changelog)

    def test_consistent_sources_reach_the_next_preflight_check(self) -> None:
        self.write_source_versions()
        result = self.run_builder(arch="different-arch")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "runner architecture fixture-arch does not match release target different-arch",
            result.stdout,
        )
        self.assertNotIn("source version mismatch", result.stdout)
        self.assertEqual(self.changelog.read_text(), self.original_changelog)


if __name__ == "__main__":
    unittest.main()
