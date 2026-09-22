#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""The release as GitHub publishes it: installable, and ordered by its packages.

GitHub serves an uploaded asset whose name holds `~` under the name with `.`
in its place. These tests take a release set staged the way the release
build stages it, publish it through a stand-in for GitHub that applies that
rewrite, and check the two things the rewrite could break:

- install.sh fetches, verifies and installs every package it selects, from
  the release URL and from a local copy of the assets;
- the lifecycle harnesses still order a candidate below its release on the
  version inside each package, never on the name it is published under,
  whose spelling of a candidate sorts above the release it leads to, and
  the manifest signs each package at that version.

The ordering tests need dpkg-deb and dpkg. They run wherever those are
installed, which includes the CI runner (where their absence fails the
suite), and are skipped elsewhere with a message saying so.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from test_artifact_identity import (  # noqa: E402
    RELEASE_DRIVER,
    REPO_ROOT,
    github_served_name,
    identity_args,
    init_fixture_repo,
    make_release_set,
    run_command,
    run_release_staging,
)

INSTALLER = REPO_ROOT / "packaging/scripts/install.sh"
JETSON_HARNESS = REPO_ROOT / "tools/validation/jetson-lifecycle.sh"
CLOUD_HARNESS = REPO_ROOT / "tools/validation/ubuntu-l4-cloud-lifecycle.sh"
RELEASE_URL_PREFIX = "https://github.com/tensorplate/tensorplate/releases/download/"
RUNTIME_PACKAGES = (
    "tensorplate-common",
    "tensorplate-agent",
    "tensorplate-serving",
    "tensorplate-observability",
    "tensorplate-cli",
)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def _lines(path: Path) -> list[str]:
    """A stand-in's log, empty when the stand-in was never run."""
    return path.read_text().splitlines() if path.exists() else []


def publish_to_github(assets: Path, release: Path) -> None:
    """Upload every file in assets to a stand-in release, as GitHub stores it."""
    release.mkdir(parents=True)
    for path in assets.iterdir():
        shutil.copyfile(path, release / github_served_name(path.name))


# A stand-in for curl that answers only for the release download URL, from
# the directory publish_to_github filled, and fails an unknown asset the
# way `curl -f` fails a 404.
FAKE_CURL = r'''#!/usr/bin/env python3
import os, shutil, sys
from pathlib import Path

args, dest, url, i = sys.argv[1:], None, None, 0
while i < len(args):
    if args[i] in ("--output", "-o"):
        dest = args[i + 1]
        i += 2
        continue
    if args[i] in ("--retry", "--connect-timeout", "--speed-limit", "--speed-time"):
        i += 2
        continue
    if not args[i].startswith("-"):
        url = args[i]
    i += 1
with open(os.environ["FAKE_CURL_LOG"], "a") as log:
    log.write(f"{url}\n")
prefix = os.environ["FAKE_RELEASE_URL_PREFIX"]
if dest is None or url is None or not url.startswith(prefix):
    sys.exit(f"curl stand-in: refusing {url!r}")
tag, _, name = url[len(prefix):].partition("/")
asset = Path(os.environ["FAKE_GITHUB_ROOT"]) / tag / name
if "/" in name or not asset.is_file():
    print(f"curl: (22) The requested URL returned error: 404: {url}", file=sys.stderr)
    sys.exit(22)
shutil.copyfile(asset, dest)
'''

# apt-get as install.sh drives it: `update`, then `install --reinstall`
# over ./<file> paths in the verified work directory. It records the name
# and digest of every package it was handed.
FAKE_APT_GET = r'''#!/usr/bin/env python3
import hashlib, os, sys
from pathlib import Path

args = sys.argv[1:]
if "update" in args:
    sys.exit(0)
if "install" not in args:
    sys.exit(f"apt-get stand-in: unexpected {args!r}")
with open(os.environ["FAKE_APT_LOG"], "a") as log:
    for arg in args:
        if not arg.startswith("./"):
            continue
        path = Path(arg)
        if not path.is_file():
            sys.exit(f"E: Unsupported file {arg} given on commandline")
        log.write(f"{path.name} {hashlib.sha256(path.read_bytes()).hexdigest()}\n")
'''

FAKE_COSIGN = """#!/bin/sh
[ "$1" = verify-blob ] || { echo "cosign stand-in: unexpected $1" >&2; exit 2; }
echo "Verified OK" >&2
"""

FAKE_TENSORPLATE = """#!/bin/sh
case "$1" in
  doctor) printf '{"payload": {"findings": []}}\\n' ;;
  version) echo "tensorplate fixture" ;;
  *) exit 2 ;;
esac
"""

# install.sh minus the call to main, sourced with the arguments a user
# passes, then main. The one step replaced is require_root: install.sh
# refuses to run as anything but root, and bash's EUID cannot be set.
# Argument parsing, download, signature and checksum verification,
# package selection and the apt-get invocation are the script's own.
INSTALL_RUNNER = r"""
set -euo pipefail
installer="$1" lib="$2"
shift 2
last="$(tail -n 1 "$installer")"
if [[ "$last" != "main" ]]; then
  echo "install.sh no longer ends by calling main; it ends with: $last" >&2
  exit 97
fi
sed '$d' "$installer" >"$lib"
# shellcheck source=/dev/null
source "$lib" "$@"
require_root() { :; }
main
"""

# (TP_INSTALL_ARCH, Debian architecture) for each runtime platform.
PLATFORMS = (("aarch64", "arm64"), ("x86_64", "amd64"))


class PublishedReleaseInstallTests(unittest.TestCase):
    """install.sh against a candidate published the way GitHub publishes it."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="tp-published-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = self.root / "repo"
        init_fixture_repo(self.repo)
        self.fixture = make_release_set(self.root / "set", "rc", self.repo, release_layout=True)
        (self.fixture.artifacts / "SHA256SUMS.cosign.bundle").write_text("{}\n")
        self.github = self.root / "github"

        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, body in (
            ("curl", FAKE_CURL),
            ("apt-get", FAKE_APT_GET),
            ("cosign", FAKE_COSIGN),
            ("tensorplate", FAKE_TENSORPLATE),
            ("systemctl", "#!/bin/sh\nexit 0\n"),
        ):
            (self.bin / name).write_text(body)
            (self.bin / name).chmod(0o755)

        hosts = self.root / "host"
        hosts.mkdir()
        (hosts / "os-release-jammy").write_text('ID=ubuntu\nVERSION_ID="22.04"\n')
        (hosts / "os-release-noble").write_text('ID=ubuntu\nVERSION_ID="24.04"\n')
        (hosts / "nv-tegra-release").write_text("# R36 (release), REVISION: 4.0\n")
        (hosts / "device-model").write_text("NVIDIA Jetson Orin Nano Developer Kit\0")
        (hosts / "nvidia-version").write_text(
            "NVRM version: NVIDIA UNIX x86_64 Kernel Module  560.35.03\n"
        )
        self.hosts = hosts

        # install.sh waits for the agent's socket after enabling the units.
        sockets = Path(tempfile.mkdtemp(prefix="tp-sock-"))
        self.addCleanup(shutil.rmtree, sockets, ignore_errors=True)
        self.socket_path = sockets / "agent.sock"
        agent = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(agent.close)
        agent.bind(str(self.socket_path))

    def _install(
        self, deb_arch: str, *args: str, run: str
    ) -> tuple[subprocess.CompletedProcess[str], list[str], list[str]]:
        arch = dict((deb, uname) for uname, deb in PLATFORMS)[deb_arch]
        run_dir = self.root / run
        run_dir.mkdir()
        curl_log, apt_log = run_dir / "curl.log", run_dir / "apt.log"
        env = {
            **os.environ,
            "LC_ALL": "C",
            "PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}",
            "TMPDIR": str(run_dir),
            "TP_INSTALL_ARCH": arch,
            "TP_INSTALL_DEB_ARCH": deb_arch,
            "TP_INSTALL_NV_TEGRA_RELEASE": str(
                self.hosts / ("nv-tegra-release" if deb_arch == "arm64" else "absent")
            ),
            "TP_INSTALL_OS_RELEASE": str(
                self.hosts / ("os-release-jammy" if deb_arch == "arm64" else "os-release-noble")
            ),
            "TP_INSTALL_DEVICE_MODEL": str(self.hosts / "device-model"),
            "TP_INSTALL_NVIDIA_VERSION": str(self.hosts / "nvidia-version"),
            "TP_INSTALL_COSIGN": str(self.bin / "cosign"),
            # The self-check hashes the running file, which here is the
            # sourced copy rather than a downloaded install.sh.
            "TP_INSTALL_SKIP_SELF_CHECK": "1",
            "TP_INSTALL_AGENT_SOCKET_PATH": str(self.socket_path),
            "TP_INSTALL_SERVICE_READY_TIMEOUT_SECONDS": "5",
            "FAKE_GITHUB_ROOT": str(self.github),
            "FAKE_RELEASE_URL_PREFIX": RELEASE_URL_PREFIX,
            "FAKE_CURL_LOG": str(curl_log),
            "FAKE_APT_LOG": str(apt_log),
        }
        result = subprocess.run(
            ["bash", "-c", INSTALL_RUNNER, "bash", str(INSTALLER), str(run_dir / "install-lib.sh"),
             "--yes", *args],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
        return result, _lines(curl_log), _lines(apt_log)

    def _expected_install(self, deb_arch: str) -> list[str]:
        manifest = json.loads(self.fixture.manifest.read_text())
        return sorted(
            f"{artifact['file']} {artifact['sha256']}"
            for artifact in manifest["artifacts"]
            if artifact.get("package") in RUNTIME_PACKAGES
            and artifact.get("architecture") in (deb_arch, "all")
        )

    def _assert_installed(self, result, apt_log: list[str], deb_arch: str) -> None:
        self.assertEqual(result.returncode, 0, f"{result.stdout}\n{result.stderr}")
        self.assertIn("==> TensorPlate v0.2.1-rc.1 install complete", result.stdout)
        # The five runtime packages for this architecture, byte for byte
        # the ones the release manifest describes.
        self.assertEqual(sorted(apt_log), self._expected_install(deb_arch))
        self.assertEqual(len(apt_log), len(RUNTIME_PACKAGES))
        self.assertTrue(all(".rc.1-1_" in line for line in apt_log), apt_log)

    def _pre_fix_release(self) -> Path:
        """The set as v0.2.1-rc.1 recorded it: the tilde names dpkg gave.

        The manifest and SHA256SUMS name the files as they were before
        staging existed, and are otherwise the release driver's own output.
        """
        legacy = self.root / "pre-fix"
        shutil.copytree(self.fixture.artifacts, legacy)
        manifest_path = legacy / self.fixture.manifest.name
        manifest = json.loads(manifest_path.read_text())
        published = github_served_name(self.fixture.deb_version)
        for artifact in manifest["artifacts"]:
            if artifact.get("package"):
                tilde = artifact["file"].replace(published, self.fixture.deb_version, 1)
                self.assertNotEqual(tilde, artifact["file"])
                (legacy / artifact["file"]).rename(legacy / tilde)
                artifact["file"] = tilde
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
        (legacy / "SHA256SUMS").write_text(
            f"{sha256(manifest_path)}  {manifest_path.name}\n"
            + "".join(f"{a['sha256']}  {a['file']}\n" for a in manifest["artifacts"])
        )
        return legacy

    def test_network_install_of_a_candidate_github_published(self) -> None:
        publish_to_github(self.fixture.artifacts, self.github / self.fixture.tag)
        for _, deb_arch in PLATFORMS:
            with self.subTest(deb_arch=deb_arch):
                result, curl_log, apt_log = self._install(
                    deb_arch, "--version", "v0.2.1-rc.1", run=f"network-{deb_arch}"
                )
                self._assert_installed(result, apt_log, deb_arch)
                self.assertNotIn("404", result.stderr)
                fetched = [url.rsplit("/", 1)[1] for url in curl_log]
                self.assertIn(self.fixture.manifest.name, fetched)
                self.assertIn("SHA256SUMS", fetched)
                self.assertIn("SHA256SUMS.cosign.bundle", fetched)
                self.assertEqual(
                    sorted(name for name in fetched if name.endswith(".deb")),
                    sorted(line.split()[0] for line in apt_log),
                )

    def test_network_install_of_the_pre_fix_names_fails_as_v0_2_1_rc_1_did(self) -> None:
        # Control: the stand-in for GitHub reproduces the defect, so the
        # case above passes because the names are right and for no other
        # reason.
        publish_to_github(self._pre_fix_release(), self.github / self.fixture.tag)
        result, curl_log, apt_log = self._install(
            "arm64", "--version", "v0.2.1-rc.1", run="network-pre-fix"
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("The requested URL returned error: 404", result.stderr)
        self.assertTrue(curl_log[-1].endswith("_0.2.1~rc.1-1_all.deb"), curl_log)
        self.assertEqual(apt_log, [])

    def test_local_artifacts_install_from_the_staged_set_and_from_a_download(self) -> None:
        publish_to_github(self.fixture.artifacts, self.github / self.fixture.tag)
        for label, directory in (
            ("staged", self.fixture.artifacts),
            ("downloaded", self.github / self.fixture.tag),
        ):
            for _, deb_arch in PLATFORMS:
                with self.subTest(source=label, deb_arch=deb_arch):
                    result, curl_log, apt_log = self._install(
                        deb_arch, "--local-artifacts", str(directory),
                        run=f"local-{label}-{deb_arch}",
                    )
                    self._assert_installed(result, apt_log, deb_arch)
                    self.assertEqual(curl_log, [])

    def test_local_artifacts_install_of_a_downloaded_pre_fix_release_fails(self) -> None:
        # Control: what a user who downloaded v0.2.1-rc.1's assets held --
        # the files under GitHub's names, a manifest naming others.
        downloaded = self.github / self.fixture.tag
        publish_to_github(self._pre_fix_release(), downloaded)
        result, _, apt_log = self._install(
            "arm64", "--local-artifacts", str(downloaded), run="local-pre-fix"
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            f"local artifact package is missing: {downloaded}/tensorplate-common_0.2.1~rc.1-1_all.deb",
            result.stderr,
        )
        self.assertEqual(apt_log, [])



def _dpkg_available() -> bool:
    return bool(shutil.which("dpkg-deb") and shutil.which("dpkg"))


# The harness's own read_upgrade_path, lifted out and run as written over
# two directories, as the harness runs it in preflight.
READ_UPGRADE_PATH = r"""
set -euo pipefail
harness="$1"
BASELINE_DIR="$2" ASSETS_DIR="$3" DEB_ARCH="$4"
function="$(sed -n '/^read_upgrade_path() {$/,/^}$/p' "$harness")"
if [[ "$function" != *"read_upgrade_path() {"* ]]; then
  echo "$harness no longer defines read_upgrade_path" >&2
  exit 97
fi
eval "$function"
: "$BASELINE_DIR" "$ASSETS_DIR" "$DEB_ARCH"  # read by read_upgrade_path
read_upgrade_path
"""


class ControlVersionOrderingTests(unittest.TestCase):
    """A candidate's published name is not its version; its control file is."""

    RC = "0.2.1~rc.1"
    FINAL = "0.2.1"

    @classmethod
    def setUpClass(cls) -> None:
        if _dpkg_available():
            return
        message = "dpkg-deb and dpkg are not installed; control-version ordering NOT verified here"
        if os.environ.get("CI") == "true":
            raise AssertionError(message)
        raise unittest.SkipTest(message)

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="tp-ordering-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def _build_deb(self, built: Path, package: str, version: str, arch: str) -> Path:
        tree = built.parent / "trees" / f"{package}-{version}-{arch}"
        (tree / "DEBIAN").mkdir(parents=True)
        (tree / "DEBIAN" / "control").write_text(
            f"Package: {package}\nVersion: {version}\nArchitecture: {arch}\n"
            "Maintainer: TensorPlate Tests <tests@tensorplate.invalid>\n"
            f"Description: ordering fixture for {package}\n"
        )
        built.mkdir(parents=True, exist_ok=True)
        # dpkg-buildpackage's name for it: the version without its epoch.
        deb = built / f"{package}_{version}_{arch}.deb"
        result = subprocess.run(
            ["dpkg-deb", "--build", str(tree), str(deb)],
            capture_output=True, text=True, timeout=60, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return deb

    def _dpkg_deb_version(self, deb: Path) -> str:
        result = subprocess.run(
            ["dpkg-deb", "-f", str(deb), "Version"],
            capture_output=True, text=True, timeout=30, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    @staticmethod
    def _dpkg_orders(left: str, relation: str, right: str) -> bool:
        return subprocess.run(
            ["dpkg", "--compare-versions", left, relation, right], check=False
        ).returncode == 0

    def _staged_set(self, name: str, deb_version: str, tag: str, record_published: bool) -> Path:
        """A release set of real packages, staged by the release build's code.

        The manifest carries the fields both harnesses read, in the release
        driver's shape. With record_published it records each package's
        version in its published spelling, as a driver reading versions off
        the file names would; the harnesses must order on the packages
        regardless.
        """
        built = self.root / name / "built"
        staged = self.root / name / "set"
        staged.mkdir(parents=True)
        packages = [("tensorplate-common", "all"), ("tensorplate-backend-python-pytorch", "all")]
        packages += [
            (package, arch)
            for package in RUNTIME_PACKAGES[1:]
            for arch in ("arm64", "amd64")
        ]
        debs = [self._build_deb(built, p, f"{deb_version}-1", a) for p, a in packages]
        result = run_release_staging(staged, deb_version, debs)
        self.assertEqual(result.returncode, 0, result.stderr)
        artifacts = []
        for package, arch in packages:
            file = f"{package}_{github_served_name(deb_version)}-1_{arch}.deb"
            self.assertTrue((staged / file).is_file(), file)
            version = f"{deb_version}-1"
            artifacts.append({
                "file": file, "package": package, "architecture": arch,
                "version": github_served_name(version) if record_published else version,
            })
        release = {"project": "tensorplate", "version": self.FINAL, "tag": tag,
                   "unreleased": False, "provenance": "github-release"}
        (staged / f"tensorplate-{tag}-artifacts.json").write_text(
            json.dumps({"release": release, "artifacts": artifacts}, indent=2) + "\n"
        )
        return staged

    def _read_upgrade_path(self, harness: Path, baseline: Path, candidate: Path, deb_arch: str):
        return subprocess.run(
            ["bash", "-c", READ_UPGRADE_PATH, "bash", str(harness), str(baseline),
             str(candidate), deb_arch],
            capture_output=True, text=True, timeout=60, check=False,
        )

    def test_a_published_candidate_reports_its_control_version_and_sorts_below_its_release(
        self,
    ) -> None:
        deb = self._build_deb(self.root / "built", "tensorplate-common", f"{self.RC}-1", "all")
        staged = self.root / "staged"
        staged.mkdir()
        result = run_release_staging(staged, self.RC, [deb])
        self.assertEqual(result.returncode, 0, result.stderr)
        published = staged / "tensorplate-common_0.2.1.rc.1-1_all.deb"
        self.assertTrue(published.is_file(), sorted(p.name for p in staged.iterdir()))

        self.assertEqual(self._dpkg_deb_version(published), "0.2.1~rc.1-1")
        self.assertTrue(self._dpkg_orders(self._dpkg_deb_version(published), "lt", "0.2.1-1"))
        # Why nothing may order on the published name: its spelling of the
        # candidate sorts ABOVE the release, the inversion #191 fixed.
        self.assertTrue(self._dpkg_orders("0.2.1.rc.1-1", "gt", "0.2.1-1"))

    def test_the_manifest_signs_each_package_at_the_version_dpkg_deb_reads(self) -> None:
        repo = self.root / "repo"
        init_fixture_repo(repo)
        fixture = make_release_set(self.root / "set", "rc", repo, build_package=self._build_deb)
        manifest = json.loads(fixture.manifest.read_text())
        debs = [artifact for artifact in manifest["artifacts"] if artifact.get("package")]
        self.assertEqual(len(debs), 13)
        for artifact in debs:
            self.assertEqual(artifact["version"], "0.2.1~rc.1-1", artifact)
            self.assertEqual(
                self._dpkg_deb_version(fixture.artifacts / artifact["file"]), artifact["version"]
            )

        # A package built at the published spelling, staged by the release
        # build's code, lands under the name a 0.2.1~rc.1-1 build gets.
        staged = fixture.artifacts / "tensorplate-agent_0.2.1.rc.1-1_arm64.deb"
        staged.unlink()
        dotted = self._build_deb(self.root / "dotted", "tensorplate-agent", "0.2.1.rc.1-1", "arm64")
        result = run_release_staging(fixture.artifacts, self.RC, [dotted])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(staged.is_file())
        self.assertTrue(self._dpkg_orders(self._dpkg_deb_version(staged), "gt", "0.2.1-1"))
        result = run_command(
            ("bash", str(RELEASE_DRIVER), "manifest", *identity_args(fixture)),
            cwd=repo, stub_dpkg_deb=False,
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(
            "tensorplate-agent_0.2.1.rc.1-1_arm64.deb: the package's control Version is "
            "'0.2.1.rc.1-1', not 0.2.1~rc.1-1, the version its name is published for",
            result.stderr,
        )

    def test_both_harnesses_order_published_sets_on_their_control_versions(self) -> None:
        for record_published in (False, True):
            label = "published-spelling" if record_published else "debian-spelling"
            baseline = self._staged_set(f"rc-{label}", self.RC, "v0.2.1-rc.1", record_published)
            candidate = self._staged_set(f"final-{label}", self.FINAL, "v0.2.1", record_published)
            for harness, deb_arch, count in (
                (JETSON_HARNESS, "arm64", 5),
                (CLOUD_HARNESS, "amd64", 6),
            ):
                with self.subTest(harness=harness.name, manifest=label):
                    result = self._read_upgrade_path(harness, baseline, candidate, deb_arch)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    path = json.loads(result.stdout)
                    self.assertEqual(path["from"]["release_tag"], "v0.2.1-rc.1")
                    self.assertEqual(
                        set(path["from"]["packages"].values()), {"0.2.1~rc.1-1"}, path
                    )
                    self.assertEqual(set(path["to"]["packages"].values()), {"0.2.1-1"}, path)
                    self.assertEqual(len(path["from"]["packages"]), count)

                    swapped = self._read_upgrade_path(harness, candidate, baseline, deb_arch)
                    self.assertNotEqual(swapped.returncode, 0, swapped.stdout)
                    self.assertIn(
                        "the baseline's 0.2.1-1 is not older than the candidate's 0.2.1~rc.1-1",
                        swapped.stderr,
                    )


if __name__ == "__main__":
    unittest.main(verbosity=2)
