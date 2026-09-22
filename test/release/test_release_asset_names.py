#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""The release workflow's read-back of the asset names GitHub serves.

tools/release/check-release-asset-names.py compares what a GitHub Release
serves after upload with what its SHA256SUMS lists. These tests drive it
directly, and run the publish-release job's step that calls it, as written
in release.yml, against a release set the driver generated and a gh
stand-in that answers as GitHub would for that set.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
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

sys.path.insert(0, str(Path(__file__).resolve().parent))

from test_artifact_identity import (  # noqa: E402
    REPO_ROOT,
    github_served_name,
    init_fixture_repo,
    make_release_set,
)

CHECKER = REPO_ROOT / "tools/release/check-release-asset-names.py"
RELEASE_WORKFLOW = REPO_ROOT / ".github/workflows/release.yml"
CREATE_STEP = "Create GitHub Release with assets"
CHECK_STEP = "Check the served asset names against SHA256SUMS"

# `gh release view TAG --repo REPO --json assets` for a release whose
# uploaded files are FAKE_UPLOADED, each served under the name
# FAKE_SERVE_AS gives it: "as-uploaded", or "github" for the rewrite
# GitHub is known to make.
FAKE_GH = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

args = sys.argv[1:]
if args[:2] != ["release", "view"] or args[3:] != ["--repo", "tensorplate/tensorplate", "--json", "assets"]:
    sys.exit(f"gh stand-in: unexpected {args!r}")
if args[2] != os.environ["FAKE_TAG"]:
    sys.exit("release not found")
names = [Path(line).name for line in Path(os.environ["FAKE_UPLOADED"]).read_text().splitlines()]
if os.environ["FAKE_SERVE_AS"] == "github":
    names = [name.replace("~", ".") for name in names]
print(json.dumps({"assets": [{"name": name, "state": "uploaded"} for name in names]}))
'''


def release_json(path: Path, *names: str, state: str = "uploaded") -> Path:
    path.write_text(json.dumps({"assets": [{"name": n, "state": state} for n in names]}))
    return path


class CheckerTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="tp-asset-names-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.checksums = self.root / "SHA256SUMS"
        self.checksums.write_text(
            f"{'1' * 64}  tensorplate-v0.2.1-rc.2-artifacts.json\n"
            f"{'2' * 64}  tensorplate-agent_0.2.1.rc.2-1_arm64.deb\n"
            f"{'3' * 64}  install.sh\n"
        )
        self.listed = [
            "tensorplate-v0.2.1-rc.2-artifacts.json",
            "tensorplate-agent_0.2.1.rc.2-1_arm64.deb",
            "install.sh",
            "SHA256SUMS",
            "SHA256SUMS.cosign.bundle",
        ]

    def check(self, served: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(CHECKER), "--checksums", str(self.checksums),
             "--also", "SHA256SUMS", "--also", "SHA256SUMS.cosign.bundle",
             "--release-json", str(served)],
            capture_output=True, text=True, timeout=30, check=False,
        )

    def test_a_release_serving_exactly_the_listed_names_passes(self) -> None:
        result = self.check(release_json(self.root / "r.json", *reversed(self.listed)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("exactly the 5 assets", result.stdout)

    def test_a_name_served_under_another_is_refused_both_ways(self) -> None:
        # What v0.2.1-rc.1 was: SHA256SUMS listing the `~` name GitHub
        # served as `.`.
        self.checksums.write_text(self.checksums.read_text().replace(".rc.2-1", "~rc.2-1"))
        result = self.check(release_json(self.root / "r.json", *self.listed))
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("listed but not served: tensorplate-agent_0.2.1~rc.2-1_arm64.deb",
                      result.stderr)
        self.assertIn("served but not listed: tensorplate-agent_0.2.1.rc.2-1_arm64.deb",
                      result.stderr)

    def test_a_listed_asset_the_release_lacks_is_refused(self) -> None:
        result = self.check(release_json(self.root / "r.json", *self.listed[1:]))
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("listed but not served: tensorplate-v0.2.1-rc.2-artifacts.json",
                      result.stderr)

    def test_an_asset_nothing_lists_is_refused(self) -> None:
        result = self.check(release_json(self.root / "r.json", *self.listed, "notes.txt"))
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("served but not listed: notes.txt", result.stderr)

    def test_an_asset_not_fully_uploaded_is_refused(self) -> None:
        result = self.check(release_json(self.root / "r.json", *self.listed, state="starter"))
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("not fully uploaded: SHA256SUMS ('starter')", result.stderr)

    def test_a_name_served_twice_is_refused(self) -> None:
        result = self.check(release_json(self.root / "r.json", *self.listed, "install.sh"))
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("served more than once: install.sh", result.stderr)


class WorkflowStepTests(unittest.TestCase):
    """The publish-release step, run as written, against a generated set."""

    @classmethod
    def setUpClass(cls) -> None:
        workflow = yaml.safe_load(RELEASE_WORKFLOW.read_text())
        steps = workflow["jobs"]["publish-release"]["steps"]
        names = [step.get("name") for step in steps]
        cls.names = names
        cls.step = next((step for step in steps if step.get("name") == CHECK_STEP), None)

    def test_the_check_runs_right_after_the_release_is_created(self) -> None:
        self.assertIsNotNone(self.step, f"publish-release has no {CHECK_STEP!r} step")
        self.assertEqual(self.names.index(CHECK_STEP), self.names.index(CREATE_STEP) + 1)
        self.assertNotIn("if", self.step)
        self.assertNotIn("continue-on-error", self.step)

    def _run_step(self, kind: str, serve_as: str, *, rename_to_tilde: bool = False):
        root = Path(tempfile.mkdtemp(prefix="tp-asset-step-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        repo = root / "repo"
        init_fixture_repo(repo)
        fixture = make_release_set(root / "set", kind, repo, release_layout=True)
        bundle = fixture.artifacts / "SHA256SUMS.cosign.bundle"
        bundle.write_text("{}\n")
        if rename_to_tilde:
            # The set as v0.2.1-rc.1 was built: every name that carries
            # the package version spelled with dpkg's `~`.
            published = github_served_name(fixture.deb_version)
            for path in list(fixture.artifacts.iterdir()):
                if path.suffix == ".deb":
                    path.rename(path.with_name(path.name.replace(published, fixture.deb_version)))
            fixture.checksums.write_text(
                fixture.checksums.read_text().replace(published, fixture.deb_version)
            )
        # What the create step uploads: the same globs, in the same order.
        uploaded = sorted(fixture.artifacts.glob("*.deb")) + [
            fixture.artifacts / "install.sh",
            *sorted(fixture.artifacts.glob("tensorplate_python-*.whl")),
            *sorted(fixture.artifacts.glob("tensorplate_python-*.tar.gz")),
            fixture.manifest, fixture.checksums, bundle,
        ]
        (root / "uploaded").write_text("".join(f"{path}\n" for path in uploaded))
        bin_dir = root / "bin"
        bin_dir.mkdir()
        (bin_dir / "gh").write_text(FAKE_GH)
        (bin_dir / "gh").chmod(0o755)
        runner_temp = root / "runner-temp"
        runner_temp.mkdir()
        env = {
            **os.environ,
            "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            "RUNNER_TEMP": str(runner_temp),
            "GITHUB_REPOSITORY": "tensorplate/tensorplate",
            "FAKE_TAG": fixture.tag,
            "FAKE_UPLOADED": str(root / "uploaded"),
            "FAKE_SERVE_AS": serve_as,
        }
        expressions = {
            "TAG": fixture.tag,
            "CHECKSUMS": str(fixture.checksums),
            "BUNDLE": str(bundle),
        }
        for name, value in (self.step.get("env") or {}).items():
            if "${{" in str(value):
                if name == "GH_TOKEN":
                    env[name] = "fixture-token"
                    continue
                self.assertIn(name, expressions, f"no fixture value for step env {name}")
                env[name] = expressions[name]
            else:
                env[name] = str(value)
        script = root / "step.sh"
        script.write_text(self.step["run"])
        # `shell: bash` on GitHub Actions runs exactly this.
        return subprocess.run(
            ["bash", "--noprofile", "--norc", "-eo", "pipefail", str(script)],
            cwd=REPO_ROOT, env=env, capture_output=True, text=True, timeout=120, check=False,
        )

    def test_a_staged_release_passes_under_the_names_github_serves(self) -> None:
        self.assertIsNotNone(self.step)
        for kind in ("final", "rc"):
            with self.subTest(kind=kind):
                result = self._run_step(kind, "github")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_a_release_whose_names_github_rewrote_fails(self) -> None:
        # Control: the check fails for the reason v0.2.1-rc.1 would have,
        # and passes above because the names are right.
        self.assertIsNotNone(self.step)
        result = self._run_step("rc", "github", rename_to_tilde=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(
            "listed but not served: tensorplate-agent_0.2.1~rc.1-1_amd64.deb",
            result.stderr,
        )
        self.assertIn(
            "served but not listed: tensorplate-agent_0.2.1.rc.1-1_amd64.deb",
            result.stderr,
        )
        # Served as uploaded, the same set passes: the refusal above is
        # GitHub's rename, not the tilde names themselves.
        result = self._run_step("rc", "as-uploaded", rename_to_tilde=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
