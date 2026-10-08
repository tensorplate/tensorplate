#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

"""The speech runtime package family's place in a release.

The family is built by its own release job and enters an artifact set only
on request. These cases run the release workflow's own step bodies, the
release driver, the installer's package selection and the apt publisher
against fixture packages.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
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
    RELEASE_DRIVER,
    SPEECH_RUNTIME_PACKAGES,
    fixture_control,
    github_served_name,
    identity_args,
    init_fixture_repo,
    make_release_set,
    run_command,
    run_release_staging,
    shell_array,
    write_fixture_package,
)

BUILD_SCRIPT = REPO_ROOT / "tools/release/build-release-artifacts.sh"
STAGE_CHANGELOG = REPO_ROOT / "tools/release/stage-debian-changelog.sh"
PUBLISH_APT = REPO_ROOT / "tools/release/publish-apt-repo.sh"
INSTALLER = REPO_ROOT / "packaging/scripts/install.sh"
CONTROL = REPO_ROOT / "packaging/debian/control"
WORKFLOW = yaml.safe_load((REPO_ROOT / ".github/workflows/release.yml").read_text())
FAMILY_NAME = "tensorplate-speech-runtime"
GATED = "${{ needs.meta.outputs.speech_runtime == 'stub' }}"
FAMILY = list(SPEECH_RUNTIME_PACKAGES)
# What a runner gives every step, beside the step's own `env`.
RUNNER_VARIABLES = ("GITHUB_OUTPUT", "RUNNER_TEMP")


def step(job: str, name: str) -> dict:
    steps = [s for s in WORKFLOW["jobs"][job]["steps"] if s.get("name") == name]
    if len(steps) != 1:
        raise AssertionError(f"release.yml: {job} has {len(steps)} steps named {name!r}")
    return steps[0]


def run_step(job: str, name: str, values: dict[str, str], *, cwd: Path,
             path: str | None = None, until: str | None = None):
    """Run a workflow step's body as `shell: bash` does. The body sees the
    `env` the step and its job declare and the runner's variables, and no
    variable of this process, so a name the step reads but does not declare
    is unset here as it is on a runner."""
    declared = {**(WORKFLOW["jobs"][job].get("env") or {}), **(step(job, name).get("env") or {})}
    missing = sorted(set(declared) - set(values))
    undeclared = sorted(set(values) - set(declared) - set(RUNNER_VARIABLES))
    if missing or undeclared:
        raise AssertionError(
            f"{job} / {name}: no fixture value for {missing}; "
            f"values for names the step does not declare: {undeclared}")
    body = step(job, name)["run"]
    if until is not None:
        body = body.split(until)[0]
    env = {"PATH": path or os.environ["PATH"], "LC_ALL": "C", **values}
    env.update({k: os.environ[k] for k in ("HOME", "TMPDIR") if k in os.environ})
    return subprocess.run(
        ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", body],
        cwd=cwd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        timeout=120, check=False,
    )


class Scratch(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="tp-speech-release-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def fake_bin(self, name: str, text: str) -> str:
        """PATH with one stand-in command in front of it."""
        directory = self.root / "bin"
        directory.mkdir(exist_ok=True)
        target = directory / name
        target.write_text(text)
        target.chmod(0o755)
        return f"{directory}{os.pathsep}{os.environ['PATH']}"


class FamilyDeclarationTests(unittest.TestCase):
    def test_the_family_is_declared_identically_wherever_it_is_listed(self) -> None:
        self.assertEqual(len(FAMILY), 8, FAMILY)
        self.assertEqual(FAMILY, shell_array(BUILD_SCRIPT, "SPEECH_RUNTIME_PACKAGES"))
        control = re.findall(rf"(?m)^Package: ({FAMILY_NAME}\S*)$", CONTROL.read_text())
        self.assertEqual(sorted(FAMILY), sorted(control))
        body = step("build_speech_runtime", "Build speech runtime packages")["run"]
        copied = re.search(r"(?s)for pkg in (.*?); do", body)
        self.assertIsNotNone(copied, body)
        self.assertEqual(FAMILY, copied.group(1).replace("\\\n", " ").split())


class SwitchTests(Scratch):
    """The meta job decides whether a run builds the family."""

    def resolve(self, event: str, *, publish: str = "", speech: str = "",
                tag: str = "v0.3.1-rc.2", on_tag: bool = True):
        output = self.root / "output"
        output.write_text("")
        result = run_step("meta", "Resolve release metadata", {
            "EVENT_NAME": event,
            "REF_NAME": tag if on_tag else "develop",
            "REF_TYPE": "tag" if on_tag else "branch",
            "INPUT_TAG": tag if event == "workflow_dispatch" else "",
            "INPUT_PUBLISH": publish,
            "INPUT_SOURCE_REF": "",
            "INPUT_SPEECH_RUNTIME": speech,
            "GITHUB_OUTPUT": str(output),
        }, cwd=self.root)
        outputs = dict(line.split("=", 1) for line in output.read_text().splitlines())
        return result, outputs

    def test_a_tag_push_never_builds_the_family(self) -> None:
        # A push carries no inputs; a value arriving anyway changes nothing.
        for speech in ("", "stub"):
            with self.subTest(speech=speech):
                result, outputs = self.resolve("push", speech=speech)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertEqual(outputs["speech_runtime"], "off")
                self.assertEqual(outputs["publish"], "true")

    def test_a_dispatch_builds_it_only_when_asked(self) -> None:
        for speech, expected in (("", "off"), ("off", "off"), ("stub", "stub")):
            with self.subTest(speech=speech):
                result, outputs = self.resolve(
                    "workflow_dispatch", publish="false", speech=speech, on_tag=False)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertEqual(outputs["speech_runtime"], expected)

    def test_stand_in_packages_are_refused_on_the_publish_path(self) -> None:
        result, outputs = self.resolve("workflow_dispatch", publish="true", speech="stub")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("never published", result.stdout)
        self.assertEqual(outputs, {})
        # The same dispatch without the family publishes.
        result, outputs = self.resolve("workflow_dispatch", publish="true", speech="off")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(outputs["publish"], "true")

    def test_an_unknown_mode_is_refused(self) -> None:
        result, outputs = self.resolve(
            "workflow_dispatch", publish="false", speech="locked", on_tag=False)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("must be off or stub", result.stdout)
        self.assertEqual(outputs, {})


class JobShapeTests(Scratch):
    JOB = "build_speech_runtime"

    def test_the_job_is_gated_per_step_and_never_skipped(self) -> None:
        job = WORKFLOW["jobs"][self.JOB]
        # A skipped job skips every job downstream of it, the publish chain
        # included, so the gate may not sit on the job.
        self.assertNotIn("if", job)
        self.assertIn(self.JOB, WORKFLOW["jobs"]["build_packages"]["needs"])
        # Everything but the notice runs in the one mode that builds, so a
        # value the meta job never set reads as off.
        notice, *work = job["steps"]
        self.assertEqual(notice["if"], "${{ needs.meta.outputs.speech_runtime != 'stub' }}")
        self.assertEqual([s["name"] for s in work if s.get("if") != GATED], [])
        self.assertEqual([s["uses"] for s in work if "setup-python" in s.get("uses", "")], [])
        self.assertEqual(WORKFLOW["jobs"]["meta"]["outputs"]["speech_runtime"],
                         "${{ steps.meta.outputs.speech_runtime }}")

    def test_the_job_installs_what_the_family_build_depends_on(self) -> None:
        depends = re.search(r"(?ms)^Build-Depends:\n(.*?)^\S", CONTROL.read_text()).group(1)
        needed = [line.split()[0].rstrip(",").replace("debhelper-compat", "debhelper")
                  for line in depends.splitlines()]
        self.assertIn("python3.12-venv", needed)
        body = step(self.JOB, "Install speech runtime build dependencies")["run"]
        installed = body.replace("\\\n", " ").split()
        self.assertEqual([package for package in needed if package not in installed], [])

    def test_the_collector_job_takes_the_family_the_speech_job_uploads(self) -> None:
        move_name = "Move speech runtime packages into release collection scope"
        upload = step(self.JOB, "Upload speech runtime packages")
        download = step("build_packages", "Stage speech runtime packages next to build outputs")
        self.assertEqual(download["with"]["name"], upload["with"]["name"])
        self.assertEqual([download.get("if"), step("build_packages", move_name).get("if")],
                         [GATED, GATED])
        # The move takes the packages from where the download put them.
        runner_temp = self.root / "runner"
        downloaded = Path(download["with"]["path"].replace("${{ runner.temp }}", str(runner_temp)))
        checkout = self.root / "work/checkout"
        for directory in (downloaded, checkout):
            directory.mkdir(parents=True)
        names = sorted(f"{package}_0.3.1~rc.2-1_amd64.deb" for package in FAMILY)
        for name in names:
            (downloaded / name).write_text("fixture\n")
        result = run_step("build_packages", move_name, {"RUNNER_TEMP": str(runner_temp)},
                          cwd=checkout)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(sorted(path.name for path in checkout.parent.glob("*.deb")), names)
        build = step("build_packages", "Build Debian packages and release manifest")["run"]
        self.assertRegex(
            build,
            r'if \[\[ "\$SPEECH_RUNTIME" == "stub" \]\]; then\n\s+args\+=\(--with-speech-runtime\)',
        )

    def test_the_wheelhouse_step_writes_stubs_then_fetches_the_real_pins(self) -> None:
        log = self.root / "python.log"
        path = self.fake_bin("python3.12", f'#!/bin/sh\necho "$@" >>"{log}"\n')
        result = run_step(self.JOB, "Stage the speech runtime wheelhouse",
                          {"RUNNER_TEMP": str(self.root)}, cwd=self.root, path=path)
        self.assertEqual(result.returncode, 0, result.stdout)
        calls = log.read_text().splitlines()
        self.assertEqual(len(calls), 2, calls)
        self.assertIn("speech_runtime_stub_wheelhouse.py", calls[0])
        self.assertIn("build-environment.py fetch", calls[1])
        self.assertIn(f"--lock-dir {self.root}/speech-runtime/lock", calls[1])


class ChangelogStagingTests(Scratch):
    HEAD = "tensorplate (0.2.1-1) unstable; urgency=medium"
    REST = "\n  * Release.\n\n -- Fixture <fixture@example.com>  Thu, 01 Jan 1970 00:00:00 +0000\n"

    def tree(self, name: str = "tree", head: str | None = None) -> Path:
        tree = self.root / name / "checkout"
        (tree / "tools/release").mkdir(parents=True)
        (tree / "packaging/debian").mkdir(parents=True)
        shutil.copy2(STAGE_CHANGELOG, tree / "tools/release")
        shutil.copy2(REPO_ROOT / "packaging/version.sh", tree / "packaging")
        (tree / "packaging/VERSION").write_text("0.2.1\n")
        (tree / "packaging/debian/changelog").write_text((head or self.HEAD) + "\n" + self.REST)
        return tree

    def stage(self, tree: Path, *args: str):
        return subprocess.run(
            [str(tree / "tools/release/stage-debian-changelog.sh"), *args],
            cwd=self.root, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            check=False,
        )

    def test_it_stages_candidate_final_and_snapshot_versions(self) -> None:
        cases = (
            (("0.2.1~rc.3",), "tensorplate (0.2.1~rc.3-1) unstable; urgency=medium"),
            (("0.2.1",), self.HEAD),
            (("0.2.1~dev.20260906.deadbeef1234", "UNRELEASED"),
             "tensorplate (0.2.1~dev.20260906.deadbeef1234-1) UNRELEASED; urgency=medium"),
        )
        for number, (args, head) in enumerate(cases):
            with self.subTest(args=args):
                tree = self.tree(f"case-{number}")
                result = self.stage(tree, *args)
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertEqual((tree / "packaging/debian/changelog").read_text(),
                                 head + "\n" + self.REST)

    def test_it_refuses_what_is_not_a_version_of_the_tree(self) -> None:
        cases = (
            (("0.2.2~rc.1",), "is not a version of this tree"),
            (("0.2.1-rc.1",), "DEB_VERSION must be"),
            (("0.2.1~rc.1-1",), "DEB_VERSION must be"),
            (("0.2.1~rc.0",), "DEB_VERSION must be"),
            (("0.2.1", "not a suite"), "DISTRIBUTION must be"),
        )
        for number, (args, message) in enumerate(cases):
            with self.subTest(args=args):
                tree = self.tree(f"case-{number}")
                result = self.stage(tree, *args)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(message, result.stdout)
                self.assertEqual((tree / "packaging/debian/changelog").read_text(),
                                 self.HEAD + "\n" + self.REST)

    def test_it_refuses_a_file_that_does_not_begin_with_a_changelog_entry(self) -> None:
        tree = self.tree(head="  * not a changelog head")
        before = (tree / "packaging/debian/changelog").read_text()
        result = self.stage(tree, "0.2.1~rc.1")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("does not begin with a tensorplate changelog entry", result.stdout)
        self.assertEqual((tree / "packaging/debian/changelog").read_text(), before)

    def build_step(self, deb_version: str = "0.2.1~rc.1"):
        """Run the speech job's build step over a stand-in for the package
        build that, like dpkg, takes the version from the changelog head."""
        tree = self.tree("speech")
        stage = self.root / "speech-runtime"
        for directory in ("lock", "wheelhouse"):
            (stage / directory).mkdir(parents=True, exist_ok=True)
        (tree / "packaging/scripts").mkdir()
        build_deb = tree / "packaging/scripts/build-deb.sh"
        build_deb.write_text(
            "#!/usr/bin/env bash\nset -euo pipefail\n"
            '[[ "$DEB_BUILD_PROFILES" == pkg.tensorplate.speech-runtime && "$1" == -B ]]\n'
            '[[ -d "$TP_SPEECH_RUNTIME_LOCK_DIR" && -d "$TP_SPEECH_RUNTIME_WHEELHOUSE" ]]\n'
            "version=\"$(sed -n '1s/^tensorplate (\\(.*\\)) .*/\\1/p' packaging/debian/changelog)\"\n"
            f"for pkg in {' '.join(FAMILY)}; do\n"
            "  printf 'Package: %s\\nVersion: %s\\nArchitecture: amd64\\n"
            "Depends: tensorplate-serving (= %s)\\nDescription: fixture\\n' \\\n"
            '    "$pkg" "$version" "$version" >"../${pkg}_${version}_amd64.deb"\n'
            "done\n"
        )
        build_deb.chmod(0o755)
        values = {"DEB_VERSION": deb_version, "RUNNER_TEMP": str(self.root)}
        result = run_step("build_speech_runtime", "Build speech runtime packages",
                          values, cwd=tree)
        return tree, values, result

    def closure(self, tree: Path, values: dict[str, str]):
        return run_step("build_speech_runtime", "Assert speech runtime package closure",
                        values, cwd=tree, path=self.fake_bin("dpkg-deb", DPKG_DEB_STUB))

    def test_both_package_jobs_stage_one_version(self) -> None:
        amd64 = self.tree("amd64")
        result = run_step("build_packages_amd64", "Build amd64 runtime packages",
                          {"DEB_VERSION": "0.2.1~rc.1", "NATIVE_CACHE_MODE": "build"}, cwd=amd64,
                          until="packaging/scripts/build-deb.sh")
        self.assertEqual(result.returncode, 0, result.stdout)
        speech, values, result = self.build_step()
        self.assertEqual(result.returncode, 0, result.stdout)
        heads = [(tree / "packaging/debian/changelog").read_text().splitlines()[0]
                 for tree in (amd64, speech)]
        self.assertEqual(heads, ["tensorplate (0.2.1~rc.1-1) unstable; urgency=medium"] * 2)
        # Built at that version, the family passes the job's own closure check.
        built = sorted(path.name for path in (speech / "dist/speech-runtime").iterdir())
        self.assertEqual(built, sorted(f"{pkg}_0.2.1~rc.1-1_amd64.deb" for pkg in FAMILY))
        result = self.closure(speech, values)
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_the_closure_check_refuses_a_family_that_cannot_bind(self) -> None:
        def control(package: str, **changed: str) -> str:
            fields = {"Version": "0.2.1~rc.1-1", "Architecture": "amd64",
                      "Depends": "python3.12, tensorplate-serving (= 0.2.1~rc.1-1)", **changed}
            return f"Package: {package}\n" + "".join(f"{k}: {v}\n" for k, v in fields.items())

        cases = (
            ({"Version": "0.2.1-1"}, "is version 0.2.1-1, not 0.2.1~rc.1-1"),
            ({"Architecture": "arm64"}, "is Architecture: arm64, not amd64"),
            ({"Depends": "tensorplate-serving (= 0.2.1-1)"},
             "does not depend on tensorplate-serving (= 0.2.1~rc.1-1)"),
        )
        speech, values, result = self.build_step()
        self.assertEqual(result.returncode, 0, result.stdout)
        target = speech / f"dist/speech-runtime/{FAMILY[-1]}_0.2.1~rc.1-1_amd64.deb"
        for changed, message in cases:
            with self.subTest(changed=changed):
                target.write_text(control(FAMILY[-1], **changed))
                result = self.closure(speech, values)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(message, result.stdout)
        target.unlink()
        result = self.closure(speech, values)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("expected the eight speech runtime packages; found 7", result.stdout)


class PackageCountTests(Scratch):
    def release_dir(self, debs: int) -> Path:
        directory = self.root / f"release-{debs}"
        directory.mkdir()
        for number in range(debs):
            (directory / f"package-{number}_0.3.1-1_all.deb").write_text(f"{number}\n")
        for name in ("install.sh", "tensorplate-v0.3.1-artifacts.json",
                     "tensorplate_python-0.3.1-py3-none-any.whl",
                     "tensorplate_python-0.3.1.tar.gz", "SHA256SUMS.cosign.bundle"):
            (directory / name).write_text(name + "\n")
        (directory / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
            for path in sorted(directory.iterdir()) if path.suffix != ".bundle"
        ))
        return directory

    def test_a_build_holds_exactly_the_set_its_mode_names(self) -> None:
        # An empty mode is what a run sees if the switch was never set: off.
        cases = (("off", 13, True), ("off", 12, False), ("off", 21, False),
                 ("stub", 21, True), ("stub", 13, False), ("stub", 22, False),
                 ("", 13, True), ("", 21, False))
        directories = {count: self.release_dir(count) for count in (12, 13, 21, 22)}
        for mode, count, accepted in cases:
            with self.subTest(mode=mode, count=count):
                result = run_step(
                    "build_packages", "Assert the release package count",
                    {"RELEASE_DIR": str(directories[count]), "SPEECH_RUNTIME": mode},
                    cwd=self.root)
                self.assertEqual(result.returncode == 0, accepted, result.stdout)
                if not accepted:
                    self.assertIn(f"found {count}", result.stdout)

    def test_the_publish_path_holds_exactly_the_core_set(self) -> None:
        if shutil.which("sha256sum") is None:
            message = "the publish path's package count NOT verified: no sha256sum"
            if os.environ.get("CI") == "true":
                raise AssertionError(message)
            self.skipTest(message)
        log = self.root / "gh.log"
        path = self.fake_bin("gh", (
            f'#!/bin/sh\necho "$1 $2" >>"{log}"\n'
            '[ "$1 $2" = "release view" ] && exit 1\nexit 0\n'))
        for count, accepted in ((13, True), (12, False), (14, False), (21, False)):
            with self.subTest(count=count):
                directory = self.release_dir(count)
                values = {"RELEASE_DIR": str(directory),
                          "MANIFEST": str(directory / "tensorplate-v0.3.1-artifacts.json"),
                          "CHECKSUMS": str(directory / "SHA256SUMS")}
                result = run_step("publish-release", "Verify downloaded release assets",
                                  values, cwd=self.root)
                self.assertEqual(result.returncode == 0, accepted, result.stdout)
                log.write_text("")
                result = run_step(
                    "publish-release", "Create GitHub Release with assets",
                    {**values, "GH_TOKEN": "", "TAG": "v0.3.1", "NOTES": "notes.md",
                     "BUNDLE": str(directory / "SHA256SUMS.cosign.bundle"),
                     "PRERELEASE": "false", "DRAFT": "true"},
                    cwd=self.root, path=path)
                self.assertEqual(result.returncode == 0, accepted, result.stdout)
                self.assertEqual("release create" in log.read_text(), accepted)


class ReleaseSetFixture(Scratch):
    """A candidate release set, with the family staged beside it on demand."""

    def setUp(self) -> None:
        super().setUp()
        self.repo = self.root / "repo"
        init_fixture_repo(self.repo)
        self.release = make_release_set(self.root / "set", "rc", self.repo, release_layout=True)

    def stage_family(self, packages=None, architecture: str = "amd64") -> None:
        version = f"{self.release.deb_version}-1"
        built = [write_fixture_package(self.release.built, package, version, architecture,
                                       "speech runtime fixture")
                 for package in (FAMILY if packages is None else packages)]
        staged = run_release_staging(self.release.artifacts, self.release.deb_version, built)
        self.assertEqual(staged.returncode, 0, staged.stdout + staged.stderr)

    def driver(self, command: str, *extra: str):
        return run_command(
            ("bash", str(RELEASE_DRIVER), command, *identity_args(self.release), *extra),
            cwd=self.repo)

    def manifest(self) -> dict:
        return json.loads(self.release.manifest.read_text())

    def listed_family(self) -> list[dict]:
        return [a for a in self.manifest()["artifacts"]
                if str(a.get("package", "")).startswith(FAMILY_NAME)]

    def assert_refused(self, result, message: str) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stdout + result.stderr)


class ManifestTests(ReleaseSetFixture):
    def test_a_set_asked_to_carry_the_family_lists_all_of_it(self) -> None:
        self.stage_family()
        result = self.driver("manifest", "--with-speech-runtime")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        listed = self.listed_family()
        self.assertEqual(sorted(a["package"] for a in listed), sorted(FAMILY))
        for artifact in listed:
            self.assertEqual(artifact["architecture"], "amd64")
            self.assertEqual(artifact["version"], "0.2.1~rc.1-1")
            self.assertEqual(artifact["target_os"], "Ubuntu 24.04 LTS (x86_64)")
            self.assertEqual(
                artifact["file"], f"{artifact['package']}_0.2.1.rc.1-1_amd64.deb")
            self.assertEqual(github_served_name(artifact["file"]), artifact["file"])
        self.assertEqual(len(list(self.release.artifacts.glob("*.deb"))), 21)
        result = self.driver("verify", "--with-speech-runtime")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_a_set_not_asked_to_carry_it_refuses_a_staged_package(self) -> None:
        self.stage_family(FAMILY[:1])
        self.assert_refused(self.driver("manifest"),
                            "speech runtime packages are staged")

    def test_an_incomplete_family_is_refused(self) -> None:
        self.stage_family()
        for package in FAMILY:
            with self.subTest(missing=package):
                path = self.release.artifacts / f"{package}_0.2.1.rc.1-1_amd64.deb"
                held = path.read_text()
                path.unlink()
                self.assert_refused(self.driver("manifest", "--with-speech-runtime"),
                                    f"missing package artifacts: {package}")
                path.write_text(held)

    def test_a_family_package_for_another_architecture_is_refused(self) -> None:
        self.stage_family(FAMILY[:-1])
        self.stage_family(FAMILY[-1:], architecture="arm64")
        self.assert_refused(self.driver("manifest", "--with-speech-runtime"),
                            "is published for architecture amd64 only")

    def test_a_package_the_family_does_not_declare_is_refused(self) -> None:
        self.stage_family([*FAMILY, f"{FAMILY_NAME}-extra"])
        self.assert_refused(self.driver("manifest", "--with-speech-runtime"),
                            f"not a package of the speech runtime family: {FAMILY_NAME}-extra")


class VerifyTests(ReleaseSetFixture):
    def test_a_listed_family_is_refused_where_it_was_not_expected(self) -> None:
        self.stage_family()
        self.assertEqual(self.driver("manifest", "--with-speech-runtime").returncode, 0)
        for command, extra in (("verify", ()), ("publish", ("--dry-run",))):
            with self.subTest(command=command):
                self.assert_refused(self.driver(command, *extra),
                                    "which this verification was not told to expect")

    def test_an_expected_family_must_be_there_whole(self) -> None:
        self.assert_refused(self.driver("verify", "--with-speech-runtime"),
                            "does not carry exactly the speech runtime family")
        # One package short, with the manifest and checksums still consistent.
        self.stage_family()
        self.assertEqual(self.driver("manifest", "--with-speech-runtime").returncode, 0)
        manifest = self.manifest()
        dropped = f"{FAMILY[3]}_0.2.1.rc.1-1_amd64.deb"
        manifest["artifacts"] = [a for a in manifest["artifacts"] if a["file"] != dropped]
        (self.release.artifacts / dropped).unlink()
        self.release.manifest.write_text(json.dumps(manifest, indent=2) + "\n")
        digest = hashlib.sha256(self.release.manifest.read_bytes()).hexdigest()
        self.release.checksums.write_text(
            f"{digest}  {self.release.manifest.name}\n"
            + "".join(f"{a['sha256']}  {a['file']}\n" for a in manifest["artifacts"]))
        self.assert_refused(self.driver("verify", "--with-speech-runtime"),
                            f"missing: {FAMILY[3]} amd64")


    def test_preflight_holds_the_family_to_the_same_rule(self) -> None:
        self.stage_family()
        self.assertEqual(self.driver("manifest", "--with-speech-runtime").returncode, 0)
        for relative in (
            "CMakeLists.txt", "Cargo.toml", "Cargo.lock", "vcpkg.json", "CHANGELOG.md",
            "packaging/VERSION", "packaging/debian/changelog",
            "protocol/rust/src/lib.rs", "include/tensorplate/version.hpp.in",
        ):
            destination = self.repo / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(REPO_ROOT / relative, destination)
        for extra, verdict in (
            ((), "artifact manifest verification failed"),
            (("--with-speech-runtime",), "artifact manifest and SHA256SUMS verify"),
        ):
            with self.subTest(extra=extra):
                report = self.root / f"preflight-{len(extra)}.md"
                self.driver("preflight", *extra, "--skip-ci", "--report", str(report))
                self.assertIn(verdict, report.read_text())


class InstallerSelectionTests(ReleaseSetFixture):
    """install.sh's own selection function, over manifests the driver wrote."""

    CORE = ["tensorplate-common", "tensorplate-agent", "tensorplate-serving",
            "tensorplate-observability", "tensorplate-cli"]
    BACKEND = "tensorplate-backend-python-pytorch"

    def select(self, architecture: str, *, speech: bool, manifest: Path | None = None):
        script = (
            'eval "$(sed -n \'/^readonly [A-Z_]*_PACKAGE=/p;/^readonly SPEECH_RUNTIME_/p;'
            '/^write_install_deb_list() {$/,/^}$/p\' "$1")"\n'
            'write_install_deb_list "$2" 0 runtime "$3" "$4"\n'
        )
        return subprocess.run(
            ["bash", "-euo", "pipefail", "-c", script, "bash", str(INSTALLER),
             str(manifest or self.release.manifest), architecture, "1" if speech else "0"],
            text=True, capture_output=True, check=False)

    def packages(self, result) -> list[str]:
        self.assertEqual(result.returncode, 0, result.stderr)
        return [name.split("_", 1)[0] for name in result.stdout.split()]

    def test_the_opt_in_installs_the_family_a_release_publishes(self) -> None:
        self.stage_family()
        self.assertEqual(self.driver("manifest", "--with-speech-runtime").returncode, 0)
        selected = self.select("amd64", speech=True)
        self.assertEqual(self.packages(selected), [*self.CORE, self.BACKEND, *sorted(FAMILY)])
        for name in selected.stdout.split():
            self.assertTrue((self.release.artifacts / name).is_file(), name)
        # Never without the opt-in, and never on another architecture.
        self.assertEqual(self.packages(self.select("amd64", speech=False)), self.CORE)
        refused = self.select("arm64", speech=True)
        self.assertNotEqual(refused.returncode, 0)
        self.assertEqual(refused.stdout, "")
        self.assertIn("does not publish the speech runtime packages for architecture arm64",
                      refused.stderr)

    def test_the_opt_in_is_refused_when_the_release_publishes_no_family(self) -> None:
        refused = self.select("amd64", speech=True)
        self.assertNotEqual(refused.returncode, 0)
        self.assertEqual(refused.stdout, "")
        self.assertIn("v0.2.1-rc.1 does not publish the speech runtime packages", refused.stderr)

    def test_components_without_the_metapackage_are_refused(self) -> None:
        self.stage_family()
        self.assertEqual(self.driver("manifest", "--with-speech-runtime").returncode, 0)
        manifest = self.manifest()
        manifest["artifacts"] = [a for a in manifest["artifacts"]
                                 if a.get("package") != FAMILY_NAME]
        partial = self.root / "partial.json"
        partial.write_text(json.dumps(manifest))
        refused = self.select("amd64", speech=True, manifest=partial)
        self.assertNotEqual(refused.returncode, 0)
        self.assertEqual(refused.stdout, "")
        self.assertIn("does not publish the speech runtime packages for architecture amd64",
                      refused.stderr)


APT_TOOLS = ("gpg", "gpgv", "dpkg-deb", "dpkg-scanpackages", "apt-ftparchive")


class AptPoolTests(Scratch):
    """The apt publisher, with real packages and a throwaway signing key."""

    @classmethod
    def setUpClass(cls) -> None:
        missing = [tool for tool in APT_TOOLS if shutil.which(tool) is None]
        if not missing:
            return
        message = f"apt pool handling of the family NOT verified: no {', '.join(missing)}"
        if os.environ.get("CI") == "true":
            raise AssertionError(message)
        raise unittest.SkipTest(message)

    def setUp(self) -> None:
        super().setUp()
        # A short home: gpg-agent's socket path has a length limit.
        home = tempfile.TemporaryDirectory(prefix="tpg-", dir="/tmp")
        self.addCleanup(home.cleanup)
        gpg = ["gpg", "--homedir", home.name, "--batch", "--quiet", "--pinentry-mode", "loopback",
               "--passphrase", ""]
        subprocess.run([*gpg, "--quick-generate-key", "fixture@example.invalid", "ed25519",
                        "sign", "never"], check=True, capture_output=True)
        self.addCleanup(subprocess.run, ["gpgconf", "--homedir", home.name, "--kill", "all"],
                        capture_output=True, check=False)
        for flag, name in (("--export-secret-keys", "secret.asc"), ("--export", "public.asc")):
            exported = subprocess.run([*gpg, "--armor", flag], check=True, capture_output=True)
            (self.root / name).write_bytes(exported.stdout)
        self.assets = self.root / "assets"
        self.assets.mkdir()
        for package, architecture in (("tensorplate-common", "all"), (FAMILY[0], "amd64"),
                                      (FAMILY[1], "amd64")):
            tree = self.root / "tree" / package / "DEBIAN"
            tree.mkdir(parents=True)
            (tree / "control").write_text(
                fixture_control(package, "0.3.1-1", architecture, "fixture")
                + "Maintainer: Fixture <fixture@example.invalid>\n")
            subprocess.run(
                ["dpkg-deb", "--build", "--root-owner-group", str(tree.parent),
                 str(self.assets / f"{package}_0.3.1-1_{architecture}.deb")],
                check=True, capture_output=True)
        (self.assets / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
            for path in sorted(self.assets.glob("*.deb"))))

    def publish(self, name: str, *extra: str):
        output = self.root / name
        result = subprocess.run(
            [str(PUBLISH_APT), "--assets-dir", str(self.assets), "--output", str(output),
             "--signing-key", str(self.root / "secret.asc"),
             "--verify-keyring", str(self.root / "public.asc"),
             "--allow-unverified-assets", *extra],
            cwd=REPO_ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            check=False)
        self.assertEqual(result.returncode, 0, result.stdout)
        pooled = sorted(path.name.split("_", 1)[0] for path in output.glob("pool/main/*.deb"))
        index = (output / "dists/jammy/main/binary-amd64/Packages").read_text()
        return result.stdout, pooled, re.findall(r"(?m)^Package: (.*)$", index)

    def test_the_family_stays_out_of_the_pool_unless_asked_for(self) -> None:
        log, pooled, indexed = self.publish("default")
        self.assertEqual(pooled, ["tensorplate-common"])
        self.assertEqual(indexed, ["tensorplate-common"])
        self.assertIn("left 2 speech runtime package(s) out of the pool", log)
        log, pooled, indexed = self.publish("asked", "--with-speech-runtime")
        self.assertEqual(pooled, sorted(["tensorplate-common", *FAMILY[:2]]))
        self.assertEqual(sorted(indexed), sorted(["tensorplate-common", *FAMILY[:2]]))
        self.assertNotIn("out of the pool", log)


if __name__ == "__main__":
    unittest.main()
