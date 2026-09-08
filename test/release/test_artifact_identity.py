#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Black-box release artifact identity tests.

The release driver is deliberately exercised as a command-line program.  The
fixtures have valid checksums, so the rejection cases reach the version and
filename checks they are intended to protect.
"""

from __future__ import annotations

import hashlib
import json
import os
from dataclasses import dataclass
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
RELEASE_DRIVER = REPO_ROOT / "tools/release/tensorplate-release.sh"

REQUIRED_PACKAGES = (
    "tensorplate-common",
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate-backend-python-pytorch",
    "tensorplate-apt-source",
    "tensorplate",
)
ALL_PACKAGES = {
    "tensorplate-common",
    "tensorplate-backend-python-pytorch",
    "tensorplate-apt-source",
}
SECONDARY_PACKAGES = (
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
    "tensorplate",
)


@dataclass(frozen=True)
class ArtifactFixture:
    version: str
    deb_version: str
    python_version: str
    tag: str
    snapshot: bool
    artifacts: Path
    manifest: Path
    checksums: Path


class ReleaseArtifactIdentityTests(unittest.TestCase):
    FINAL_VERSION = "0.2.1"
    RC_DEB_VERSION = "0.2.1~rc.1"
    RC_PYTHON_VERSION = "0.2.1rc1"
    STALE_DEB_VERSION = "0.2.1~rc.2"
    STALE_PYTHON_VERSION = "0.2.1rc2"
    SNAPSHOT_VERSION = "0.2.1~dev.20260906.deadbeef1234"
    STALE_SNAPSHOT_VERSION = "0.2.1~dev.20260905.cafebabe"

    def setUp(self) -> None:
        self._temporary = tempfile.TemporaryDirectory(prefix="tp-release-identity-")
        self.addCleanup(self._temporary.cleanup)
        self.root = Path(self._temporary.name)
        self.repo = self.root / "repo"
        self.cases = self.root / "cases"
        self.repo.mkdir()
        self.cases.mkdir()
        self._case_number = 0

        self._run(("git", "init", "-q", "-b", "release/0.2"), cwd=self.repo)
        (self.repo / "README").write_text("release fixture\n")
        self._run(("git", "add", "README"), cwd=self.repo)
        self._run(
            (
                "git",
                "-c",
                "user.name=TensorPlate Tests",
                "-c",
                "user.email=tests@tensorplate.invalid",
                "commit",
                "-qm",
                "fixture",
            ),
            cwd=self.repo,
        )
        for tag in ("v0.2.1", "v0.2.1-rc.1", "snapshot-develop-deadbeef1234"):
            self._run(
                (
                    "git",
                    "-c",
                    "user.name=TensorPlate Tests",
                    "-c",
                    "user.email=tests@tensorplate.invalid",
                    "tag",
                    "-a",
                    tag,
                    "-m",
                    tag,
                ),
                cwd=self.repo,
            )

    def _run(
        self,
        argv: tuple[str, ...] | list[str],
        *,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            argv,
            cwd=cwd or self.repo,
            env={**os.environ, "LC_ALL": "C"},
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )

    def _driver(self, command: str, *args: str) -> subprocess.CompletedProcess[str]:
        return self._run(("bash", str(RELEASE_DRIVER), command, *args))

    def _assert_ok(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(
            result.returncode,
            0,
            f"command failed\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def _assert_version_rejection(self, result: subprocess.CompletedProcess[str]) -> None:
        output = (result.stdout + result.stderr).lower()
        self.assertNotEqual(result.returncode, 0, output)
        self.assertRegex(output, r"version mismatch|contradicts|does not match")
        self.assertNotIn("checksum mismatch", output)

    def _identity_args(
        self,
        fixture: ArtifactFixture,
        *,
        deb_version: str | None = None,
        python_version: str | None = None,
    ) -> list[str]:
        args = [
            "--version",
            fixture.version,
            "--deb-version",
            deb_version or fixture.deb_version,
            "--python-version",
            python_version or fixture.python_version,
            "--tag",
            fixture.tag,
            "--artifacts-dir",
            str(fixture.artifacts),
            "--manifest",
            str(fixture.manifest),
            "--checksums",
            str(fixture.checksums),
            "--arch",
            "arm64",
        ]
        if fixture.snapshot:
            args.append("--allow-snapshot-version")
        return args

    def _make_fixture(self, kind: str) -> ArtifactFixture:
        self._case_number += 1
        case_root = self.cases / f"{self._case_number}-{kind}"
        artifacts = case_root / "artifacts"
        artifacts.mkdir(parents=True)

        if kind == "final":
            version = deb_version = python_version = self.FINAL_VERSION
            tag = "v0.2.1"
            snapshot = False
        elif kind == "rc":
            version = self.FINAL_VERSION
            deb_version = self.RC_DEB_VERSION
            python_version = self.RC_PYTHON_VERSION
            tag = "v0.2.1-rc.1"
            snapshot = False
        elif kind == "snapshot":
            version = deb_version = python_version = self.SNAPSHOT_VERSION
            tag = "snapshot-develop-deadbeef1234"
            snapshot = True
        else:
            self.fail(f"unknown fixture kind: {kind}")

        for package in REQUIRED_PACKAGES:
            architecture = "all" if package in ALL_PACKAGES else "arm64"
            (artifacts / f"{package}_{deb_version}-1_{architecture}.deb").write_text(
                f"{kind} fixture for {package} {architecture}\n"
            )
        if not snapshot:
            for package in SECONDARY_PACKAGES:
                (artifacts / f"{package}_{deb_version}-1_amd64.deb").write_text(
                    f"{kind} fixture for {package} amd64\n"
                )
            (artifacts / f"tensorplate_python-{python_version}-py3-none-any.whl").write_text(
                f"{kind} wheel fixture\n"
            )
            (artifacts / f"tensorplate_python-{python_version}.tar.gz").write_text(
                f"{kind} sdist fixture\n"
            )
        (artifacts / "install.sh").write_text("#!/usr/bin/env bash\n")

        fixture = ArtifactFixture(
            version=version,
            deb_version=deb_version,
            python_version=python_version,
            tag=tag,
            snapshot=snapshot,
            artifacts=artifacts,
            manifest=case_root / "manifest.json",
            checksums=case_root / "SHA256SUMS",
        )
        result = self._driver("manifest", *self._identity_args(fixture))
        self._assert_ok(result)
        return fixture

    def _verify(
        self,
        fixture: ArtifactFixture,
        *,
        deb_version: str | None = None,
        python_version: str | None = None,
        allow_snapshot: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        args = self._identity_args(
            fixture,
            deb_version=deb_version,
            python_version=python_version,
        )
        args.append("--skip-tag-verify")
        if allow_snapshot and "--allow-snapshot-version" not in args:
            args.append("--allow-snapshot-version")
        return self._driver("verify", *args)

    def _prepare_publish(self, fixture: ArtifactFixture) -> tuple[Path, Path]:
        release_notes = fixture.manifest.parent / "release-notes.md"
        release_notes.write_text("Fixture release notes.\n")
        signature = Path(f"{fixture.checksums}.cosign.bundle")
        signature.write_text("fixture signature bundle; dry-run only\n")
        return release_notes, signature

    def _publish(
        self,
        fixture: ArtifactFixture,
        *,
        deb_version: str | None = None,
        python_version: str | None = None,
        allow_snapshot: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        release_notes, _ = self._prepare_publish(fixture)
        args = self._identity_args(
            fixture,
            deb_version=deb_version,
            python_version=python_version,
        )
        args.extend(("--release-notes", str(release_notes), "--dry-run"))
        if allow_snapshot and "--allow-snapshot-version" not in args:
            args.append("--allow-snapshot-version")
        return self._driver("publish", *args)

    @staticmethod
    def _artifact_digest(path: Path) -> str:
        digest = hashlib.sha256()
        with path.open("rb") as artifact_file:
            for chunk in iter(lambda: artifact_file.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()

    def _rewrite_integrity_files(self, fixture: ArtifactFixture, manifest: dict) -> None:
        for artifact in manifest["artifacts"]:
            artifact["sha256"] = self._artifact_digest(fixture.artifacts / artifact["file"])
        fixture.manifest.write_text(json.dumps(manifest, indent=2) + "\n")
        lines = [
            f"{self._artifact_digest(fixture.manifest)}  {fixture.manifest.name}\n"
        ]
        lines.extend(
            f"{artifact['sha256']}  {artifact['file']}\n"
            for artifact in manifest["artifacts"]
        )
        fixture.checksums.write_text("".join(lines))
        self._assert_integrity_valid(fixture)

    def _assert_integrity_valid(self, fixture: ArtifactFixture) -> None:
        manifest = json.loads(fixture.manifest.read_text())
        checksums = {}
        for line in fixture.checksums.read_text().splitlines():
            digest, filename = line.split(maxsplit=1)
            checksums[filename] = digest
        self.assertEqual(
            checksums[fixture.manifest.name], self._artifact_digest(fixture.manifest)
        )
        for artifact in manifest["artifacts"]:
            digest = self._artifact_digest(fixture.artifacts / artifact["file"])
            self.assertEqual(artifact["sha256"], digest)
            self.assertEqual(checksums[artifact["file"]], digest)

    @staticmethod
    def _package_artifact(manifest: dict) -> dict:
        return next(
            artifact
            for artifact in manifest["artifacts"]
            if artifact.get("package") == "tensorplate-agent"
            and artifact.get("architecture") == "arm64"
        )

    @staticmethod
    def _sdk_artifact(manifest: dict, kind: str) -> dict:
        return next(
            artifact for artifact in manifest["artifacts"] if artifact.get("kind") == kind
        )

    def test_valid_final_rc_and_snapshot_manifest_and_verify(self) -> None:
        for kind in ("final", "rc", "snapshot"):
            with self.subTest(kind=kind):
                fixture = self._make_fixture(kind)
                self._assert_ok(self._verify(fixture))
                manifest = json.loads(fixture.manifest.read_text())
                self.assertEqual(manifest["release"]["version"], fixture.version)
                for artifact in manifest["artifacts"]:
                    if artifact.get("package"):
                        self.assertTrue(
                            artifact["version"] == fixture.deb_version
                            or artifact["version"].startswith(fixture.deb_version + "-"),
                            artifact,
                        )
                sdk = [a for a in manifest["artifacts"] if a.get("kind", "").startswith("python-")]
                if fixture.snapshot:
                    self.assertEqual(sdk, [])
                else:
                    self.assertEqual({a["version"] for a in sdk}, {fixture.python_version})

    def test_verify_rejects_stale_debian_filename_and_metadata(self) -> None:
        for mutation in ("filename", "metadata", "both"):
            with self.subTest(mutation=mutation):
                fixture = self._make_fixture("rc")
                manifest = json.loads(fixture.manifest.read_text())
                artifact = self._package_artifact(manifest)
                if mutation != "metadata":
                    old_path = fixture.artifacts / artifact["file"]
                    artifact["file"] = artifact["file"].replace(
                        self.RC_DEB_VERSION, self.STALE_DEB_VERSION, 1
                    )
                    old_path.rename(fixture.artifacts / artifact["file"])
                if mutation != "filename":
                    artifact["version"] = f"{self.STALE_DEB_VERSION}-1"
                self._rewrite_integrity_files(fixture, manifest)
                self._assert_version_rejection(self._verify(fixture))

    def test_verify_rejects_stale_sdk_filename_and_metadata(self) -> None:
        for sdk_kind in ("python-wheel", "python-sdist"):
            for mutation in ("filename", "metadata", "both"):
                with self.subTest(sdk_kind=sdk_kind, mutation=mutation):
                    fixture = self._make_fixture("rc")
                    manifest = json.loads(fixture.manifest.read_text())
                    artifact = self._sdk_artifact(manifest, sdk_kind)
                    if mutation != "metadata":
                        old_path = fixture.artifacts / artifact["file"]
                        artifact["file"] = artifact["file"].replace(
                            self.RC_PYTHON_VERSION, self.STALE_PYTHON_VERSION, 1
                        )
                        old_path.rename(fixture.artifacts / artifact["file"])
                    if mutation != "filename":
                        artifact["version"] = self.STALE_PYTHON_VERSION
                    self._rewrite_integrity_files(fixture, manifest)
                    self._assert_version_rejection(self._verify(fixture))

    def test_snapshot_verify_enforces_expected_artifact_version(self) -> None:
        for mutation in ("filename", "metadata", "both"):
            with self.subTest(mutation=mutation):
                fixture = self._make_fixture("snapshot")
                manifest = json.loads(fixture.manifest.read_text())
                artifact = self._package_artifact(manifest)
                if mutation != "metadata":
                    old_path = fixture.artifacts / artifact["file"]
                    artifact["file"] = artifact["file"].replace(
                        self.SNAPSHOT_VERSION, self.STALE_SNAPSHOT_VERSION, 1
                    )
                    old_path.rename(fixture.artifacts / artifact["file"])
                if mutation != "filename":
                    artifact["version"] = f"{self.STALE_SNAPSHOT_VERSION}-1"
                self._rewrite_integrity_files(fixture, manifest)
                self._assert_version_rejection(self._verify(fixture))

    def test_verify_rejects_each_tuple_argument_independently(self) -> None:
        fixture = self._make_fixture("rc")
        cases = (
            {"deb_version": self.STALE_DEB_VERSION},
            {"python_version": self.STALE_PYTHON_VERSION},
        )
        for overrides in cases:
            with self.subTest(overrides=overrides):
                self._assert_version_rejection(self._verify(fixture, **overrides))

    def test_repeated_candidate_marker_is_rejected(self) -> None:
        fixture = self._make_fixture("rc")
        for command in ("manifest", "verify", "publish"):
            with self.subTest(command=command):
                args = self._identity_args(fixture)
                args[args.index("--tag") + 1] = "v0.2.1-rc.bad-rc.1"
                if command == "publish":
                    args.append("--dry-run")
                result = self._driver(command, *args)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("malformed candidate number", result.stderr)

    def test_allow_snapshot_does_not_bypass_release_validation(self) -> None:
        fixture = self._make_fixture("rc")
        cases = (
            {"deb_version": self.STALE_DEB_VERSION},
            {"python_version": self.STALE_PYTHON_VERSION},
        )
        for command in ("verify", "publish"):
            for overrides in cases:
                with self.subTest(command=command, overrides=overrides):
                    runner = self._verify if command == "verify" else self._publish
                    self._assert_version_rejection(
                        runner(fixture, allow_snapshot=True, **overrides)
                    )

    def test_publish_dry_run_accepts_valid_tuple_without_publishing(self) -> None:
        fixture = self._make_fixture("rc")
        result = self._publish(fixture)
        self._assert_ok(result)
        self.assertIn("dry-run gh command:", result.stdout)
        self.assertIn("--prerelease", result.stdout)

    def test_publish_dry_run_rejects_each_tuple_argument_independently(self) -> None:
        fixture = self._make_fixture("rc")
        cases = (
            {"deb_version": self.STALE_DEB_VERSION},
            {"python_version": self.STALE_PYTHON_VERSION},
        )
        for overrides in cases:
            with self.subTest(overrides=overrides):
                self._assert_version_rejection(self._publish(fixture, **overrides))

    def test_publish_rejects_stale_artifact_versions(self) -> None:
        for artifact_kind in ("deb", "python-wheel", "python-sdist"):
            with self.subTest(artifact_kind=artifact_kind):
                fixture = self._make_fixture("rc")
                manifest = json.loads(fixture.manifest.read_text())
                if artifact_kind == "deb":
                    artifact = self._package_artifact(manifest)
                    old, new = self.RC_DEB_VERSION, self.STALE_DEB_VERSION
                else:
                    artifact = self._sdk_artifact(manifest, artifact_kind)
                    old, new = self.RC_PYTHON_VERSION, self.STALE_PYTHON_VERSION
                old_path = fixture.artifacts / artifact["file"]
                artifact["file"] = artifact["file"].replace(old, new)
                artifact["version"] = artifact["version"].replace(old, new)
                old_path.rename(fixture.artifacts / artifact["file"])
                self._rewrite_integrity_files(fixture, manifest)
                self._assert_version_rejection(self._publish(fixture))

    def test_publish_rejects_unlisted_staged_package(self) -> None:
        fixture = self._make_fixture("rc")
        stray = f"tensorplate-agent_{self.STALE_DEB_VERSION}-1_arm64.deb"
        (fixture.artifacts / stray).write_text("stale package outside the manifest\n")
        self._assert_integrity_valid(fixture)
        result = self._publish(fixture)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing from manifest", result.stderr)
        self.assertIn(stray, result.stderr)

    def test_debian_package_cannot_supply_an_sdk_kind(self) -> None:
        fixture = self._make_fixture("rc")
        manifest = json.loads(fixture.manifest.read_text())
        self._package_artifact(manifest)["kind"] = "python-wheel"
        self._rewrite_integrity_files(fixture, manifest)
        result = self._verify(fixture)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("kind mismatch", result.stderr)

    def test_preflight_reports_tuple_failure_without_exiting_early(self) -> None:
        fixture = self._make_fixture("rc")
        for relative in (
            "CMakeLists.txt", "Cargo.toml", "Cargo.lock", "vcpkg.json", "CHANGELOG.md",
            "packaging/VERSION", "packaging/debian/changelog",
            "protocol/rust/src/lib.rs", "include/tensorplate/version.hpp.in",
        ):
            destination = self.repo / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(REPO_ROOT / relative, destination)
        report = self.root / "preflight.md"
        result = self._driver(
            "preflight", *self._identity_args(fixture, deb_version=self.STALE_DEB_VERSION),
            "--skip-ci", "--report", str(report),
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(report.is_file(), result.stdout + result.stderr)
        self.assertIn("artifact manifest verification failed", report.read_text())


if __name__ == "__main__":
    unittest.main(verbosity=2)
