#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for tools/ci/install-pinned-toolchain.sh.

rustup is a stand-in that keeps the rule the hosted failure showed: an install
over a half-written toolchain directory stops at a file conflict, every time,
until the directory is gone. Each run gets its own pin so the script cannot
have the version written into it.
"""

from __future__ import annotations

import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "tools/ci/install-pinned-toolchain.sh"
WORKFLOWS = REPO_ROOT / ".github/workflows"
PIN = "7.31.4"

FAKE_RUSTUP = r"""#!/usr/bin/env bash
set -u
home="${RUSTUP_HOME}"
pin="$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)"
dir="${home}/toolchains/${pin}-x86_64-unknown-linux-gnu"
echo "rustup $*" >>"${FAKE_LOG}"
case "$1 $2" in
  "toolchain install")
    if [[ -e "${FAKE_FAIL_ALWAYS:-/nonexistent}" ]]; then
      echo "error: could not download component" >&2; exit 1
    fi
    if [[ -e "${FAKE_INCOMPLETE_ONCE:-/nonexistent}" ]]; then
      rm -f "${FAKE_INCOMPLETE_ONCE}"; mkdir -p "${dir}"; exit 0
    fi
    if [[ -e "${dir}/PARTIAL" ]]; then
      echo "error: failed to install component: 'clippy-preview', detected conflict: 'bin/cargo-clippy'" >&2
      exit 1
    fi
    mkdir -p "${dir}" && touch "${dir}/COMPLETE"
    ;;
  "toolchain uninstall")
    [[ "$3" == "${pin}" ]] || { echo "error: not the pinned toolchain: $3" >&2; exit 1; }
    rm -rf "${dir}"
    ;;
  *) exit 2 ;;
esac
"""

FAKE_TOOL = r"""#!/usr/bin/env bash
pin="$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)"
[[ -e "${RUSTUP_HOME}/toolchains/${pin}-x86_64-unknown-linux-gnu/COMPLETE" ]] || exit 1
echo "$(basename "$0") ${pin}"
"""


class InstallPinnedToolchain(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        tree = self.tmp / "repo"
        (tree / "tools/ci").mkdir(parents=True)
        shutil.copy(SCRIPT, tree / "tools/ci/install-pinned-toolchain.sh")
        (tree / "rust-toolchain.toml").write_text(
            f'[toolchain]\nchannel = "{PIN}"\nprofile = "minimal"\n'
        )
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        for name, body in (("rustup", FAKE_RUSTUP), ("rustc", FAKE_TOOL), ("cargo", FAKE_TOOL)):
            (bin_dir / name).write_text(body)
            (bin_dir / name).chmod(0o755)
        self.tree = tree
        self.home = self.tmp / "rustup-home"
        self.log = self.tmp / "calls.log"
        self.log.touch()
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "RUSTUP_HOME": str(self.home),
            "FAKE_LOG": str(self.log),
            "TOOLCHAIN_RETRY_DELAY": "0",
        }

    def toolchain_dir(self) -> pathlib.Path:
        return self.home / "toolchains" / f"{PIN}-x86_64-unknown-linux-gnu"

    def run_script(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.tree / "tools/ci/install-pinned-toolchain.sh")],
            env=self.env, capture_output=True, text=True, check=False, timeout=60,
        )

    def calls(self, verb: str) -> int:
        return sum(1 for l in self.log.read_text().splitlines() if l.startswith(f"rustup toolchain {verb}"))

    def test_clean_runner_installs_once(self) -> None:
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls("install"), 1)
        self.assertEqual(self.calls("uninstall"), 0)

    def test_partial_toolchain_is_removed_and_the_second_attempt_succeeds(self) -> None:
        self.toolchain_dir().mkdir(parents=True)
        (self.toolchain_dir() / "PARTIAL").touch()
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.calls("install"), 2)
        self.assertTrue((self.toolchain_dir() / "COMPLETE").exists())
        # The first failure's reason stays in the step log.
        self.assertIn("detected conflict: 'bin/cargo-clippy'", r.stderr)

    def test_sweep_leaves_other_toolchains_and_settings_alone(self) -> None:
        (self.toolchain_dir() / "bin").mkdir(parents=True)
        (self.toolchain_dir() / "PARTIAL").touch()
        keep = [
            self.home / "toolchains" / "1.90.0-x86_64-unknown-linux-gnu" / "COMPLETE",
            self.home / "toolchains" / "stable-x86_64-unknown-linux-gnu" / "COMPLETE",
            self.home / "toolchains" / f"{PIN}1-x86_64-unknown-linux-gnu" / "COMPLETE",
            self.home / "settings.toml",
        ]
        for f in keep:
            f.parent.mkdir(parents=True, exist_ok=True)
            f.touch()
        self.assertEqual(self.run_script().returncode, 0)
        for f in keep:
            self.assertTrue(f.exists(), f)

    def test_failed_sweep_warns_and_the_next_attempt_runs(self) -> None:
        (self.toolchain_dir() / "bin").mkdir(parents=True)
        (self.toolchain_dir() / "PARTIAL").touch()
        # A read-only directory the sweep cannot empty; uninstall of the stand-in is a no-op.
        fake = self.tmp / "bin/rustup"
        text = fake.read_text()
        self.assertIn('rm -rf "${dir}"', text)
        fake.write_text(text.replace('rm -rf "${dir}"', "true"))
        rm = self.tmp / "bin/rm"
        rm.write_text('#!/usr/bin/env bash\n'
                      'if [[ "$*" == *toolchains* ]]; then exit 1; fi\n'
                      'exec /bin/rm "$@"\n')
        rm.chmod(0o755)
        r = self.run_script()
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.calls("install"), 3)
        self.assertIn("could not remove the toolchain directory", r.stdout)
        self.assertIn("could not be installed in 3 attempts", r.stdout)

    def test_unset_home_is_a_clear_error(self) -> None:
        del self.env["RUSTUP_HOME"]
        self.env.pop("HOME", None)
        r = self.run_script()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("neither RUSTUP_HOME nor HOME is set", r.stdout)
        self.assertEqual(self.calls("install"), 0)

    def test_non_numeric_attempts_is_a_clear_error(self) -> None:
        self.env["TOOLCHAIN_INSTALL_ATTEMPTS"] = "three"
        r = self.run_script()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("TOOLCHAIN_INSTALL_ATTEMPTS must be a positive integer", r.stdout)
        self.assertEqual(self.calls("install"), 0)

    def test_unlisted_partial_directory_is_removed_too(self) -> None:
        # rustup no longer lists it, so only the directory sweep clears it.
        self.toolchain_dir().mkdir(parents=True)
        (self.toolchain_dir() / "PARTIAL").touch()
        fake = self.tmp / "bin/rustup"
        text = fake.read_text()
        self.assertIn('rm -rf "${dir}"', text)
        fake.write_text(text.replace('rm -rf "${dir}"', "true"))
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.calls("install"), 2)

    def test_attempts_are_bounded_and_the_step_fails(self) -> None:
        always = self.tmp / "fail-always"
        always.touch()
        self.env["FAKE_FAIL_ALWAYS"] = str(always)
        r = self.run_script()
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.calls("install"), 3)
        self.assertIn(f"toolchain {PIN} could not be installed", r.stdout)
        self.assertIn("could not download component", r.stderr)

    def test_install_that_exits_zero_without_a_usable_toolchain_is_retried(self) -> None:
        once = self.tmp / "incomplete-once"
        once.touch()
        self.env["FAKE_INCOMPLETE_ONCE"] = str(once)
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(self.calls("install"), 2)

    def test_moving_channel_is_refused_before_anything_is_swept(self) -> None:
        (self.tree / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "stable"\n')
        r = self.run_script()
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.calls("install"), 0)

    def test_missing_pin_fails_without_installing(self) -> None:
        (self.tree / "rust-toolchain.toml").write_text("[toolchain]\n")
        r = self.run_script()
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.calls("install"), 0)


class WorkflowCallSites(unittest.TestCase):
    def test_hosted_jobs_use_the_script_and_only_the_self_hosted_job_keeps_the_old_line(self) -> None:
        old = []
        for wf in sorted(WORKFLOWS.glob("*.yml")):
            for i, line in enumerate(wf.read_text().splitlines(), 1):
                if "rustup show active-toolchain" in line:
                    old.append((wf.name, i))
        self.assertEqual([w for w, _ in old], ["release.yml"], old)
        text = (WORKFLOWS / "release.yml").read_text().splitlines()
        job = [l for l in text[: old[0][1]] if re.match(r"^  [a-z_0-9-]+:$", l)][-1]
        self.assertEqual(job.strip(), "build_packages:")
        for name in ("supply-chain.yml", "rust.yml", "apt-lifecycle.yml", "release.yml"):
            self.assertIn("tools/ci/install-pinned-toolchain.sh", (WORKFLOWS / name).read_text())
        self.assertEqual((WORKFLOWS / "apt-lifecycle.yml").read_text().count("install-pinned-toolchain.sh"), 2)


if __name__ == "__main__":
    unittest.main()
