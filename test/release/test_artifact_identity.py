#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""Black-box release artifact identity tests.

The release driver is deliberately exercised as a command-line program.  The
fixtures have valid checksums, so the rejection cases reach the version and
filename checks they are intended to protect.

Every fixture package enters its artifacts directory through the release
build's own staging code, so the file names under test are the ones a
release publishes rather than names a fixture chose.
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
BUILD_SCRIPT = REPO_ROOT / "tools/release/build-release-artifacts.sh"

# The characters GitHub rewrites in an uploaded asset's name, as observed:
# v0.2.1-rc.1 uploaded tensorplate-agent_0.2.1~rc.1-1_arm64.deb, and the
# release serves it only as tensorplate-agent_0.2.1.rc.1-1_arm64.deb, the
# tilde URL answering 404. Only what has been seen is listed; this is the
# test's own statement of the rule, not a copy of the build's.
GITHUB_ASSET_NAME_REWRITES = {"~": "."}


def github_served_name(name: str) -> str:
    for character, replacement in GITHUB_ASSET_NAME_REWRITES.items():
        name = name.replace(character, replacement)
    return name


# build-release-artifacts.sh compiles the runtime and cannot run here, so
# its staging functions are lifted out of it and run as written -- the
# approach test/release/run.sh takes for its changelog staging. A missing
# function fails loudly rather than leaving nothing to run.
_STAGE_SCRIPT = r"""
set -euo pipefail
build_script="$1" dest="$2"
DEB_VERSION="$3"
shift 3
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
functions="$(sed -n '/^release_asset_name() {$/,/^}$/p;/^stage_release_debs() {$/,/^}$/p' "$build_script")"
case "$functions" in
  *"release_asset_name() {"*"stage_release_debs() {"*) ;;
  *) die "build-release-artifacts.sh no longer defines release_asset_name and stage_release_debs" ;;
esac
eval "$functions"
: "$DEB_VERSION"  # read by stage_release_debs
stage_release_debs "$dest" "$@"
"""


def run_release_staging(
    dest: Path, deb_version: str, sources: list[Path]
) -> subprocess.CompletedProcess[str]:
    """Stage packages into dest with build-release-artifacts.sh's own code."""
    return subprocess.run(
        ["bash", "-c", _STAGE_SCRIPT, "bash", str(BUILD_SCRIPT), str(dest), deb_version,
         *(str(source) for source in sources)],
        env={**os.environ, "LC_ALL": "C"},
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )

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
    built: Path
    artifacts: Path
    manifest: Path
    checksums: Path


FINAL_VERSION = "0.2.1"
RC_DEB_VERSION = "0.2.1~rc.1"
RC_PYTHON_VERSION = "0.2.1rc1"
SNAPSHOT_VERSION = "0.2.1~dev.20260906.deadbeef1234"
# (version, deb_version, python_version, tag, snapshot) for each fixture kind.
FIXTURE_KINDS = {
    "final": (FINAL_VERSION, FINAL_VERSION, FINAL_VERSION, "v0.2.1", False),
    "rc": (FINAL_VERSION, RC_DEB_VERSION, RC_PYTHON_VERSION, "v0.2.1-rc.1", False),
    "snapshot": (
        SNAPSHOT_VERSION, SNAPSHOT_VERSION, SNAPSHOT_VERSION,
        "snapshot-develop-deadbeef1234", True,
    ),
}


def run_command(
    argv: tuple[str, ...] | list[str], *, cwd: Path
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        cwd=cwd,
        env={**os.environ, "LC_ALL": "C"},
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )


def init_fixture_repo(repo: Path) -> None:
    """A git repository holding an annotated tag for every fixture kind."""
    identity = ("-c", "user.name=TensorPlate Tests", "-c", "user.email=tests@tensorplate.invalid")
    steps = [
        ("git", "init", "-q", "-b", "release/0.2"),
        ("git", "add", "README"),
        ("git", *identity, "commit", "-qm", "fixture"),
    ]
    steps.extend(
        ("git", *identity, "tag", "-a", tag, "-m", tag)
        for *_, tag, _snapshot in FIXTURE_KINDS.values()
    )
    repo.mkdir(parents=True)
    (repo / "README").write_text("release fixture\n")
    for step in steps:
        result = run_command(step, cwd=repo)
        if result.returncode != 0:
            raise AssertionError(f"{' '.join(step)} failed: {result.stderr}")


def make_release_set(
    case_root: Path, kind: str, repo: Path, *, release_layout: bool = False
) -> ArtifactFixture:
    """Build, stage and manifest one release artifact set.

    The packages are written under the names dpkg-buildpackage gives them,
    staged by the release build's own code, and recorded by the release
    driver. With release_layout the manifest and SHA256SUMS land in the
    artifacts directory under the names a release publishes, which is the
    layout install.sh and the lifecycle harnesses read.
    """
    if kind not in FIXTURE_KINDS:
        raise AssertionError(f"unknown fixture kind: {kind}")
    version, deb_version, python_version, tag, snapshot = FIXTURE_KINDS[kind]
    built = case_root / "built"
    artifacts = case_root / "artifacts"
    built.mkdir(parents=True)
    artifacts.mkdir(parents=True)

    for package in REQUIRED_PACKAGES:
        architecture = "all" if package in ALL_PACKAGES else "arm64"
        (built / f"{package}_{deb_version}-1_{architecture}.deb").write_text(
            f"{kind} fixture for {package} {architecture}\n"
        )
    if not snapshot:
        for package in SECONDARY_PACKAGES:
            (built / f"{package}_{deb_version}-1_amd64.deb").write_text(
                f"{kind} fixture for {package} amd64\n"
            )
    staged = run_release_staging(artifacts, deb_version, sorted(built.iterdir()))
    if staged.returncode != 0:
        raise AssertionError(f"release staging failed:\n{staged.stdout}\n{staged.stderr}")
    if not snapshot:
        (artifacts / f"tensorplate_python-{python_version}-py3-none-any.whl").write_text(
            f"{kind} wheel fixture\n"
        )
        (artifacts / f"tensorplate_python-{python_version}.tar.gz").write_text(
            f"{kind} sdist fixture\n"
        )
    (artifacts / "install.sh").write_text("#!/usr/bin/env bash\n")

    if release_layout:
        manifest = artifacts / f"tensorplate-{tag}-artifacts.json"
        checksums = artifacts / "SHA256SUMS"
    else:
        manifest = case_root / "manifest.json"
        checksums = case_root / "SHA256SUMS"
    fixture = ArtifactFixture(
        version=version,
        deb_version=deb_version,
        python_version=python_version,
        tag=tag,
        snapshot=snapshot,
        built=built,
        artifacts=artifacts,
        manifest=manifest,
        checksums=checksums,
    )
    result = run_command(
        ("bash", str(RELEASE_DRIVER), "manifest", *identity_args(fixture)), cwd=repo
    )
    if result.returncode != 0:
        raise AssertionError(
            f"manifest generation failed\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return fixture


def identity_args(
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


class ReleaseArtifactIdentityTests(unittest.TestCase):
    FINAL_VERSION = FINAL_VERSION
    RC_DEB_VERSION = RC_DEB_VERSION
    RC_PYTHON_VERSION = RC_PYTHON_VERSION
    STALE_DEB_VERSION = "0.2.1~rc.2"
    STALE_PYTHON_VERSION = "0.2.1rc2"
    SNAPSHOT_VERSION = SNAPSHOT_VERSION
    STALE_SNAPSHOT_VERSION = "0.2.1~dev.20260905.cafebabe"

    def setUp(self) -> None:
        self._temporary = tempfile.TemporaryDirectory(prefix="tp-release-identity-")
        self.addCleanup(self._temporary.cleanup)
        self.root = Path(self._temporary.name)
        self.repo = self.root / "repo"
        self.cases = self.root / "cases"
        self.cases.mkdir()
        self._case_number = 0
        init_fixture_repo(self.repo)

    def _run(
        self,
        argv: tuple[str, ...] | list[str],
        *,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        return run_command(argv, cwd=cwd or self.repo)

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
        return identity_args(fixture, deb_version=deb_version, python_version=python_version)

    def _make_fixture(self, kind: str) -> ArtifactFixture:
        self._case_number += 1
        return make_release_set(self.cases / f"{self._case_number}-{kind}", kind, self.repo)

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

    def _replace_once(self, text: str, old: str, new: str) -> str:
        # A mutation that matched nothing would leave the fixture valid and
        # the rejection case passing for no reason.
        self.assertIn(old, text, "fixture mutation matched nothing")
        return text.replace(old, new, 1)

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
                        # The file name carries that version as GitHub spells it.
                        self.assertEqual(
                            artifact["file"],
                            f"{artifact['package']}_{github_served_name(artifact['version'])}"
                            f"_{artifact['architecture']}.deb",
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
                    artifact["file"] = self._replace_once(
                        artifact["file"],
                        github_served_name(self.RC_DEB_VERSION),
                        github_served_name(self.STALE_DEB_VERSION),
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
                    artifact["file"] = self._replace_once(
                        artifact["file"],
                        github_served_name(self.SNAPSHOT_VERSION),
                        github_served_name(self.STALE_SNAPSHOT_VERSION),
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
                artifact["file"] = self._replace_once(
                    artifact["file"], github_served_name(old), github_served_name(new)
                )
                artifact["version"] = self._replace_once(artifact["version"], old, new)
                old_path.rename(fixture.artifacts / artifact["file"])
                self._rewrite_integrity_files(fixture, manifest)
                self._assert_version_rejection(self._publish(fixture))

    def test_publish_rejects_unlisted_staged_package(self) -> None:
        fixture = self._make_fixture("rc")
        stray = f"tensorplate-agent_{github_served_name(self.STALE_DEB_VERSION)}-1_arm64.deb"
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

    # --- names GitHub publishes -------------------------------------------

    @staticmethod
    def _listed_names(fixture: ArtifactFixture) -> list[str]:
        return [
            line.split(maxsplit=1)[1]
            for line in fixture.checksums.read_text().splitlines()
            if line.strip()
        ]

    def test_every_listed_asset_is_served_by_github_under_its_listed_name(self) -> None:
        # The v0.2.1-rc.1 defect: SHA256SUMS and the manifest named
        # tilde files, GitHub served dot files, and a strict
        # `sha256sum -c SHA256SUMS` over the release reported thirteen
        # listed files unreadable.
        for kind in ("final", "rc", "snapshot"):
            with self.subTest(kind=kind):
                fixture = self._make_fixture(kind)
                manifest = json.loads(fixture.manifest.read_text())
                listed = self._listed_names(fixture)
                named = [artifact["file"] for artifact in manifest["artifacts"]]
                self.assertEqual(
                    [name for name in listed + named if github_served_name(name) != name],
                    [],
                    "asset names GitHub would serve under another name",
                )
                # The set as GitHub would serve it: every file uploaded
                # under the name GitHub gives it. A strict check of the
                # published SHA256SUMS then has to find every listed file.
                served = fixture.manifest.parent / "served"
                served.mkdir()
                for path in [*fixture.artifacts.iterdir(), fixture.manifest]:
                    shutil.copyfile(path, served / github_served_name(path.name))
                digests = {
                    name: digest
                    for digest, name in (
                        line.split(maxsplit=1)
                        for line in fixture.checksums.read_text().splitlines()
                        if line.strip()
                    )
                }
                for name, digest in digests.items():
                    self.assertTrue((served / name).is_file(), f"{name} is not served")
                    self.assertEqual(self._artifact_digest(served / name), digest, name)

    def test_a_final_release_keeps_the_names_dpkg_gave_its_packages(self) -> None:
        fixture = self._make_fixture("final")
        manifest = json.loads(fixture.manifest.read_text())
        self.assertEqual(
            sorted(a["file"] for a in manifest["artifacts"] if a.get("package")),
            sorted(path.name for path in fixture.built.iterdir()),
        )

    def test_a_candidate_records_its_debian_version_under_its_published_name(self) -> None:
        fixture = self._make_fixture("rc")
        manifest = json.loads(fixture.manifest.read_text())
        agent = self._package_artifact(manifest)
        self.assertEqual(agent["file"], "tensorplate-agent_0.2.1.rc.1-1_arm64.deb")
        # The version dpkg reports and apt orders on, never the published
        # spelling: 0.2.1.rc.1-1 sorts above 0.2.1-1.
        self.assertEqual(agent["version"], "0.2.1~rc.1-1")
        self.assertTrue((fixture.built / "tensorplate-agent_0.2.1~rc.1-1_arm64.deb").is_file())

    def test_manifest_generation_refuses_a_name_github_rewrites(self) -> None:
        fixture = self._make_fixture("rc")
        tilde = "tensorplate-agent_0.2.1~rc.1-1_arm64.deb"
        (fixture.artifacts / "tensorplate-agent_0.2.1.rc.1-1_arm64.deb").rename(
            fixture.artifacts / tilde
        )
        result = self._driver("manifest", *self._identity_args(fixture))
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            f"{tilde}: GitHub would publish this asset as "
            "tensorplate-agent_0.2.1.rc.1-1_arm64.deb",
            result.stderr,
        )

    def test_verify_refuses_a_listed_name_github_rewrites(self) -> None:
        # A manifest and SHA256SUMS regenerated together over a tilde
        # file are internally consistent, which is how v0.2.1-rc.1 passed
        # its own verification.
        fixture = self._make_fixture("rc")
        manifest = json.loads(fixture.manifest.read_text())
        artifact = self._package_artifact(manifest)
        tilde = "tensorplate-agent_0.2.1~rc.1-1_arm64.deb"
        (fixture.artifacts / artifact["file"]).rename(fixture.artifacts / tilde)
        artifact["file"] = tilde
        self._rewrite_integrity_files(fixture, manifest)
        result = self._verify(fixture)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            f"SHA256SUMS lists {tilde}, which GitHub publishes as "
            "tensorplate-agent_0.2.1.rc.1-1_arm64.deb",
            result.stderr,
        )

    def test_staging_refuses_a_tilde_outside_the_package_version(self) -> None:
        # The manifest recovers the package version from the published
        # name by restoring --deb-version's tilde, so a second tilde would
        # be recorded as a dot.
        source = self.root / "tensorplate-agent_0.2.1~rc.1-1~bpo1_arm64.deb"
        source.write_text("fixture\n")
        dest = self.root / "staged"
        dest.mkdir()
        result = run_release_staging(dest, self.RC_DEB_VERSION, [source])
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("carries a '~' outside its package version 0.2.1~rc.1", result.stderr)
        self.assertEqual(list(dest.iterdir()), [])

    def test_staging_refuses_two_packages_published_under_one_name(self) -> None:
        name = "tensorplate-agent_0.2.1~rc.1-1_arm64.deb"
        sources = []
        for directory in ("one", "two"):
            (self.root / directory).mkdir()
            sources.append(self.root / directory / name)
            sources[-1].write_text(f"{directory}\n")
        dest = self.root / "staged"
        dest.mkdir()
        result = run_release_staging(dest, self.RC_DEB_VERSION, sources)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "two packages would be published as tensorplate-agent_0.2.1.rc.1-1_arm64.deb",
            result.stderr,
        )
        self.assertEqual((dest / github_served_name(name)).read_text(), "one\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
