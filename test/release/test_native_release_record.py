#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise native-record orchestration; subprocess stand-ins record no build evidence."""

from __future__ import annotations

import contextlib
from datetime import datetime, timezone
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

try:
    import yaml
except ImportError:
    sys.exit("FAIL: this suite needs PyYAML to read the release workflow.\n"
             "Install it with: python3 -m pip install -r tools/release/requirements.txt")

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_artifact_identity as artifact_identity  # noqa: E402

spec = importlib.util.spec_from_file_location("native_release_record", ROOT / "tools/release/native-release-record.py")
tool = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(tool)


class Record(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.directory = self.root / "records"
        self.output = self.root / "github-output"
        self.calls = []
        self.packages = self.root / "packages"
        self.packages.mkdir()
        self.fields = {"Package": "tensorplate-serving", "Version": "0.3.1-1"}
        self.snapshot = '{"archives": []}'
        (self.root / "native-cache-before.json").write_text(self.snapshot)
        self.restore_started = "2026-10-08T14:00:00.123456789Z"
        (self.root / "native-cache-restore-started.txt").write_text(self.restore_started + "\n")
        (self.root / "native-cache-before-source.json").write_text('{"id":11}')
        for arch in ("amd64", "arm64"):
            (self.packages / f"tensorplate-serving_0.3.1-1_{arch}.deb").write_text("package stand-in")
        self.env = {"GITHUB_OUTPUT": str(self.output), "RUNNER_TEMP": str(self.root),
                    "GITHUB_REPOSITORY": "example/project", "GITHUB_REF": "refs/tags/v0.3.1",
                    "DEFAULT_BRANCH": "develop", "VCPKG_BINARY_SOURCES": f"clear;files,{self.root}/archives,read"}

    def invoke(self, mode="build", arch="amd64", fail=None, command="record", tag="v0.3.1", commit=None):
        def capture(name, *args):
            args = list(map(str, args))
            if args[0] == "snapshot":
                Path(args[args.index("--output") + 1]).write_text(self.snapshot)
                return
            self.calls.append((name, args))
            if "--worker" in args:
                worker = Path(args[args.index("--worker") + 1])
                self.assertEqual(worker.read_bytes(), b"packaged worker")
                self.assertTrue(str(worker).endswith("/payload/usr/lib/tensorplate/tensorplate-serving"))
            if "--output" in args:
                Path(args[args.index("--output") + 1]).write_text(name)
            if len(self.calls) == fail:
                raise subprocess.CalledProcessError(1, name)

        def output(args, **kwargs):
            if args[:2] == ["dpkg-deb", "-f"]:
                return self.fields.get(args[-1], arch) + "\n"
            if args[:2] == ["git", "rev-parse"]:
                return "a" * 40 + "\n"
            if args[:2] == ["git", "show"]:
                self.git_show_args = args
                if "--format=%cI" in args:
                    return "2026-10-08T14:30:00Z\n"
                self.assertEqual(args, ["git", "show", "-s", "--format=%ct", "HEAD"])
                return "1791469800\n"
            self.assertIn("key", args)
            return "exact-cache-key\n"

        def extract(args, **kwargs):
            self.assertEqual(args[:2], ["dpkg-deb", "-x"])
            self.assertTrue(Path(args[2]).is_file())
            worker = Path(args[3]) / "usr/lib/tensorplate/tensorplate-serving"
            worker.parent.mkdir(parents=True)
            worker.write_bytes(b"packaged worker")

        argv = ["native-release-record", command, "--directory", str(self.directory), "--tag", tag]
        if commit is not None:
            argv += ["--commit", commit]
        if command == "record":
            argv += ["--architecture", arch, "--mode", mode, "--build-dir", str(self.root / "build"),
                     "--packages", str(self.packages)]
        self.stderr = io.StringIO()
        with mock.patch.dict(os.environ, self.env, clear=True), mock.patch.object(sys, "argv", argv), \
                mock.patch.object(tool, "run", side_effect=capture), \
                mock.patch.object(tool.subprocess, "run", side_effect=extract), \
                mock.patch.object(tool.subprocess, "check_output", side_effect=output), \
                contextlib.redirect_stderr(self.stderr):
            return tool.main()

    def test_success_preserves_both_checked_records_and_actual_source_arguments(self):
        for arch, mode in ((arch, mode) for arch in ("amd64", "arm64") for mode in ("build", "build-and-save", "restore")):
            with self.subTest(arch=arch, mode=mode):
                self.calls.clear()
                self.output.unlink(missing_ok=True)
                self.assertEqual(self.invoke(mode=mode, arch=arch), 0)
                self.assertEqual([(self.directory / f"tensorplate-v0.3.1-{kind}-{arch}.{extension}").read_text()
                                  for kind, extension in (("native-closure", "spdx.json"), ("vcpkg-cache-provenance", "json"))],
                                 ["native-sbom.py", "native-cache-provenance.py"])
                self.assertEqual(dict(line.split("=", 1) for line in self.output.read_text().splitlines()),
                                 {"recorded": "true"})
                self.assertEqual(len(self.calls), 4)
                collect, provenance = self.calls[0][1], self.calls[1][1]
                self.assertNotIn(str(self.root / "build/tensorplate-serving"), collect)
                self.assertIn(str(self.root / "build/vcpkg_installed"), collect)
                self.assertIn({"amd64": "x64-linux", "arm64": "arm64-linux"}[arch], collect)
                self.assertIn("2026-10-08T14:30:00Z", collect)
                self.assertIn("a" * 40, provenance)
                self.assertIn("0.3.1", provenance)
                checked = self.calls[3][1]
                self.assertEqual(checked[checked.index("--commit") + 1], "a" * 40)
                if arch == "amd64":
                    for value in ("actions-cache", "exact-cache-key", "example/project", "refs/tags/v0.3.1", "develop"):
                        self.assertIn(value, provenance)
                    self.assertEqual(provenance[provenance.index("--restore-started") + 1], self.restore_started)
                    self.assertEqual(provenance[provenance.index("--restored-cache-id") + 1], "11")
                else:
                    self.assertIn("runner-filesystem", provenance)
                    self.assertIn(str(self.root / "provisioned.stamp"), provenance)

    def test_utc_committer_date_with_z_records_in_all_modes_on_python310(self):
        for mode in ("build", "build-and-save", "restore"):
            with self.subTest(mode=mode):
                self.calls.clear()
                self.assertEqual(self.invoke(mode=mode), 0)
                self.assertEqual(len(self.calls), 4)
                self.assertIn("--format=%ct", self.git_show_args)
                self.assertEqual(self.output.read_text().splitlines()[-1], "recorded=true")
                for _, args in self.calls[:2]:
                    self.assertEqual(args[args.index("--created") + 1], "2026-10-08T14:30:00Z")

    def test_failures_remove_partial_and_stale_records_and_report_false(self):
        for mode in ("build", "build-and-save", "restore"):
            for failure in range(1, 5):
                with self.subTest(mode=mode, failure=failure):
                    self.calls.clear()
                    self.output.unlink(missing_ok=True)
                    self.directory.mkdir(exist_ok=True)
                    for name in tool.names("v0.3.1", "amd64"):
                        (self.directory / name).write_text("stale")
                    self.assertEqual(self.invoke(mode=mode, fail=failure), int(mode == "restore"))
                    self.assertEqual(list(self.directory.iterdir()), [])
                    self.assertEqual(dict(line.split("=", 1) for line in self.output.read_text().splitlines()),
                                     {"recorded": "false"})

    def test_arm_requires_one_read_only_filesystem_source(self):
        for source in ("", "clear;files,/tmp/cache,readwrite", "files,/tmp/cache,read",
                       "clear;files,/tmp/cache,read;files,/tmp/other,read"):
            with self.subTest(source=source):
                self.env["VCPKG_BINARY_SOURCES"] = source
                self.assertEqual(self.invoke(mode="restore", arch="arm64"), 1)
                self.assertEqual(self.calls, [])
                self.assertEqual(list(self.directory.iterdir()), [])

    def test_optional_records_are_omitted_without_an_unchanged_before_build_snapshot(self):
        before = self.root / "native-cache-before.json"
        for mode in ("build", "build-and-save"):
            for content in (None, '{}'):
                with self.subTest(mode=mode, content=content):
                    before.unlink(missing_ok=True)
                    if content is not None:
                        before.write_text(content)
                    self.assertEqual(self.invoke(mode=mode), 0)
                    self.assertEqual(list(self.directory.iterdir()), [])
                    self.assertEqual(self.calls, [])
                    self.assertEqual(self.output.read_text().splitlines()[-1], "recorded=false")
                    self.assertIn("native-cache-before.json" if content is None else "build changed the restored cache archives",
                                  self.stderr.getvalue())

    def test_missing_restore_source_refuses_required_records_and_omits_optional_records(self):
        for filename in ("native-cache-restore-started.txt", "native-cache-before-source.json"):
            path = self.root / filename
            original = path.read_text()
            path.unlink()
            for mode in ("build", "build-and-save", "restore"):
                with self.subTest(filename=filename, mode=mode):
                    self.assertEqual(self.invoke(mode=mode), int(mode == "restore"))
                    self.assertEqual(list(self.directory.iterdir()), [])
                    self.assertEqual(self.calls, [])
                    self.assertEqual(self.output.read_text().splitlines()[-1], "recorded=false")
                    self.assertIn(filename, self.stderr.getvalue())
            path.write_text(original)

    def test_check_checks_both_architectures_with_exact_tag_names(self):
        for tag in ("v0.3.1", "v0.3.1-rc.2"):
            self.calls.clear()
            self.assertEqual(self.invoke(command="check", tag=tag), 0)
            self.assertEqual(len(self.calls), 4)
            for index, arch in enumerate(("amd64", "arm64")):
                for name in (f"tensorplate-{tag}-native-closure-{arch}.spdx.json",
                             f"tensorplate-{tag}-vcpkg-cache-provenance-{arch}.json"):
                    self.assertIn(str(self.directory / name), self.calls[index * 2 + 1][1])
                self.assertIn(tag[1:], self.calls[index * 2 + 1][1])
                self.assertIn({"amd64": "x64-linux", "arm64": "arm64-linux"}[arch], self.calls[index * 2 + 1][1])
        for failure in range(1, 5):
            self.calls.clear()
            self.assertEqual(self.invoke(command="check", fail=failure), 1)

    def test_check_passes_an_expected_commit_only_when_supplied(self):
        for commit in (None, "b" * 40):
            self.calls.clear()
            self.assertEqual(self.invoke(command="check", commit=commit), 0)
            for _, args in self.calls[1::2]:
                self.assertEqual("--commit" in args, commit is not None)
                if commit is not None:
                    self.assertEqual(args[args.index("--commit") + 1], commit)
        self.calls.clear()
        for commit in ("", "B" * 40, "abcd"):
            self.assertEqual(self.invoke(command="check", commit=commit), 1)
        self.assertEqual(self.calls, [])

    def test_invalid_tags_are_refused_before_collection(self):
        for tag in ("0.3.1", "v0.3", "v0.3.1-rc.0", "../v0.3.1", "v0.3.1\n"):
            self.assertEqual(self.invoke(tag=tag), 1)
        self.assertEqual(self.calls, [])

    def test_ambiguous_or_misidentified_serving_packages_are_refused(self):
        for field, value in (("Package", "other"), ("Version", "0.3.1~rc.1-1"), ("Architecture", "arm64")):
            with self.subTest(field=field), mock.patch.dict(self.fields, {field: value}):
                self.assertEqual(self.invoke(mode="restore"), 1)
                self.assertEqual(self.calls, [])
        package = self.packages / "tensorplate-serving_0.3.1-1_amd64.deb"
        package.unlink()
        self.assertEqual(self.invoke(mode="restore"), 1)
        package.touch()
        (self.packages / "tensorplate-serving_other_amd64.deb").touch()
        self.assertEqual(self.invoke(mode="restore"), 1)
        self.assertEqual(self.calls, [])


def evaluate(expression, context):
    if "${{" not in str(expression):
        return expression
    body = str(expression).strip().removeprefix("${{").removesuffix("}}")
    body = body.replace("&&", " and ").replace("||", " or ")
    body = re.sub(r"[A-Za-z_][A-Za-z0-9_-]*(?:\.[A-Za-z_][A-Za-z0-9_-]*)+",
                  lambda item: repr(context[item.group()]), body)
    return eval(body, {"__builtins__": {}})  # repository-owned workflow expressions


class Workflow(unittest.TestCase):
    def setUp(self):
        self.jobs = yaml.safe_load((ROOT / ".github/workflows/release.yml").read_text())["jobs"]

    def step(self, job, name):
        return next(step for step in self.jobs[job]["steps"] if step.get("name") == name)

    def test_restore_start_is_marked_before_the_cache_action_and_parses_as_utc(self):
        job = self.jobs["build_packages_amd64"]
        step = self.step("build_packages_amd64", "Mark the native cache restore start")
        action = self.step("build_packages_amd64", "Pinned vcpkg checkout and binary cache")
        self.assertEqual(job["steps"].index(step) + 1, job["steps"].index(action))
        for mode in ("build", "build-and-save", "restore"):
            self.assertEqual(evaluate(step["continue-on-error"], {"env.NATIVE_CACHE_MODE": mode}), mode != "restore")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "runner temp"
            root.mkdir()
            helper = root / "tools/release/native-cache-provenance.py"
            helper.parent.mkdir(parents=True)
            helper.write_text("import json, os, pathlib, sys\n"
                              "if sys.argv[1] == 'key': print('exact-cache-key')\n"
                              "else:\n"
                              " assert sys.argv[1] == 'cache'\n"
                              " pathlib.Path(sys.argv[sys.argv.index('--output') + 1]).write_text('{\"id\":11}')\n"
                              " pathlib.Path('capture.json').write_text(json.dumps([sys.argv[1:], "
                              "{k: os.environ[k] for k in ('GH_TOKEN', 'DEFAULT_BRANCH')}]))\n")
            context = {"github.token": "stand-in-token", "github.event.repository.default_branch": "develop"}
            env = {"PATH": os.environ["PATH"], "RUNNER_TEMP": str(root), "GITHUB_REPOSITORY": "example/project",
                   "GITHUB_REF": "refs/tags/v0.3.1", **{key: str(evaluate(value, context)) for key, value in step["env"].items()}}
            before = datetime.now(timezone.utc)
            result = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
                                    cwd=root, env=env,
                                    text=True, capture_output=True, check=False)
            after = datetime.now(timezone.utc)
            self.assertEqual(result.returncode, 0, result.stderr)
            marker = (root / "native-cache-restore-started.txt").read_text()
            self.assertRegex(marker, r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z\n$")
            timestamp = datetime.fromisoformat(marker[:26] + "+00:00")
            self.assertLessEqual(before, timestamp)
            self.assertLessEqual(timestamp, after)
            args, captured = json.loads((root / "capture.json").read_text())
            self.assertEqual(captured, {"GH_TOKEN": "stand-in-token", "DEFAULT_BRANCH": "develop"})
            for flag, value in (("--cache-key", "exact-cache-key"), ("--repository", "example/project"),
                                ("--ref", "refs/tags/v0.3.1"), ("--default-branch", "develop"),
                                ("--restore-started", marker.strip())):
                self.assertEqual(args[args.index(flag) + 1], value)
            self.assertEqual(json.loads((root / "native-cache-before-source.json").read_text()), {"id": 11})
            helper.write_text("import sys\nif sys.argv[1] == 'key': print('exact-cache-key')\n"
                              "else: raise SystemExit('expected exactly one cache entry')\n")
            result = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
                                    cwd=root, env=env, text=True, capture_output=True, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("expected exactly one cache entry", result.stderr)
            self.assertIn("docs/release/runbook.md#provision-the-vcpkg-checkout-and-binary-cache", result.stdout)
            self.assertIn("release-dependencies.yml before retrying", result.stdout)

    def test_record_step_gets_the_same_mode_as_the_action_from_declared_environment(self):
        job = self.jobs["build_packages_amd64"]
        action = self.step("build_packages_amd64", "Pinned vcpkg checkout and binary cache")
        step = self.step("build_packages_amd64", "Record the amd64 native closure")
        snapshot = self.step("build_packages_amd64", "Snapshot the restored native archives before a build")
        self.assertIs(snapshot["continue-on-error"], True)
        self.assertLess(job["steps"].index(action), job["steps"].index(snapshot))
        self.assertLess(job["steps"].index(snapshot), job["steps"].index(self.step("build_packages_amd64", "Build amd64 serving worker")))
        self.assertEqual(job["permissions"]["actions"], "read")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            helper = root / "tools/release/native-release-record.py"
            helper.parent.mkdir(parents=True)
            helper.write_text("import json, os, pathlib, sys\npathlib.Path('capture.json').write_text("
                              "json.dumps([sys.argv[1:], {k: os.environ[k] for k in ('DEFAULT_BRANCH', 'GH_TOKEN')}]))\n")
            for publish, branch, default, kind, want in (("true", "v0.3.1", "develop", "tag", "restore"),
                    ("false", "topic", "develop", "branch", "build-and-save"),
                    ("false", "develop", "develop", "branch", "build"),
                    ("false", "topic", "", "branch", "build")):
                context = {"needs.meta.outputs.publish": publish, "github.ref_type": kind,
                           "github.ref_name": branch, "github.event.repository.default_branch": default,
                           "github.token": "stand-in-token", "needs.meta.outputs.tag": "v0.3.1"}
                env = {"PATH": os.environ["PATH"]}
                for scope in (job, step):
                    env.update({key: str(evaluate(value, context)) for key, value in scope.get("env", {}).items()})
                    context.update({f"env.{key}": value for key, value in env.items()})
                self.assertEqual(evaluate(action["with"]["mode"], context), want)
                self.assertEqual(evaluate(snapshot["if"], context), want != "restore")
                result = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
                                        cwd=root, env=env, text=True, capture_output=True, check=False)
                self.assertEqual(result.returncode, 0, result.stderr)
                args, captured = json.loads((root / "capture.json").read_text())
                self.assertEqual(args[args.index("--mode") + 1], want)
                self.assertEqual(captured, {"DEFAULT_BRANCH": default, "GH_TOKEN": "stand-in-token"})

    def test_records_move_only_after_success_then_manifest_is_regenerated(self):
        upload = self.step("build_packages_amd64", "Upload the amd64 native records")
        download = self.step("build_packages", "Stage the amd64 native records")
        for recorded in ("", "false", "true"):
            self.assertEqual(evaluate(upload["if"], {"steps.native.outputs.recorded": recorded}), recorded == "true")
            self.assertEqual(evaluate(download["if"], {"needs.build_packages_amd64.outputs.native_recorded": recorded}), recorded == "true")
        self.assertEqual(self.jobs["build_packages_amd64"]["outputs"]["native_recorded"], "${{ steps.native.outputs.recorded }}")
        self.assertEqual(upload["with"]["name"], download["with"]["name"])
        names = [step.get("name") for step in self.jobs["build_packages"]["steps"]]
        manifest = names.index("Include native records in the signed artifact inventory")
        for name in ("Record the arm64 native closure", "Stage the amd64 native records"):
            self.assertLess(names.index(name), manifest)
        self.assertLess(manifest, names.index("Upload unsigned release build artifact bundle"))
        self.assertIn("tensorplate-release.sh manifest", self.jobs["build_packages"]["steps"][manifest]["run"])

    def test_manifest_step_executes_the_real_driver_and_covers_staged_records(self):
        step = self.step("build_packages", "Include native records in the signed artifact inventory")
        for speech, publish, tamper in (("off", "true", False), ("off", "false", False),
                                       ("stub", "false", False), ("off", "true", True)):
            with self.subTest(speech=speech, publish=publish, tamper=tamper), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                repo = root / "repo"
                artifact_identity.init_fixture_repo(repo)
                driver = repo / "tools/release/tensorplate-release.sh"
                driver.parent.mkdir(parents=True)
                driver.symlink_to(artifact_identity.RELEASE_DRIVER)
                fixture = artifact_identity.make_release_set(root / "case", "final", repo, release_layout=True)
                if publish == "false":
                    subprocess.run(["git", "tag", "-d", fixture.tag], cwd=repo, check=True, capture_output=True)
                if speech == "stub":
                    packages = [artifact_identity.write_fixture_package(fixture.built, package,
                                f"{fixture.deb_version}-1", "amd64", "speech package stand-in")
                                for package in artifact_identity.SPEECH_RUNTIME_PACKAGES]
                    staged = artifact_identity.run_release_staging(fixture.artifacts, fixture.deb_version, packages)
                    self.assertEqual(staged.returncode, 0, staged.stderr)
                records = {}
                for arch in ("amd64", "arm64"):
                    for suffix in (f"native-closure-{arch}.spdx.json", f"vcpkg-cache-provenance-{arch}.json"):
                        path = fixture.artifacts / f"tensorplate-{fixture.tag}-{suffix}"
                        path.write_text("unit-test record for artifact hashing\n")
                        records[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
                self.assertFalse(records.keys() & {item["file"] for item in json.loads(fixture.manifest.read_text())["artifacts"]})
                if tamper:
                    driver.unlink()
                    driver.write_text(f'#!/bin/bash\nset -e\n"{artifact_identity.RELEASE_DRIVER}" "$@"\n'
                                      'if [[ "$1" == manifest ]]; then printf changed >> "$TEST_CHANGED_RECORD"; fi\n')
                    driver.chmod(0o755)
                values = {"version": fixture.version, "deb_version": fixture.deb_version,
                          "python_version": fixture.python_version, "tag": fixture.tag,
                          "release_dir": str(fixture.artifacts), "manifest": str(fixture.manifest),
                          "checksums": str(fixture.checksums), "source_label": "reviewed-source", "speech_runtime": speech,
                          "publish": publish}
                context = {f"needs.meta.outputs.{key}": value for key, value in values.items()}
                env = {"PATH": f"{artifact_identity.dpkg_deb_stub_directory()}{os.pathsep}{os.environ['PATH']}",
                       **{key: str(evaluate(value, context)) for key, value in step["env"].items()}}
                if tamper:
                    env["TEST_CHANGED_RECORD"] = str(fixture.artifacts / next(iter(records)))
                result = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", step["run"]],
                                        cwd=repo, env=env, text=True, capture_output=True, check=False)
                if tamper:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("manifest checksum mismatch", result.stderr)
                    continue
                self.assertEqual(result.returncode, 0, result.stderr)
                document = json.loads(fixture.manifest.read_text())
                self.assertEqual(document["release"]["branch"], "reviewed-source")
                self.assertEqual(document["target"]["os"], "Ubuntu 22.04 / JetPack 6.x (L4T 36.x)")
                self.assertIn("skipping annotated tag check" if publish == "false" else "annotated tag", result.stdout)
                by_file = {item["file"]: item for item in document["artifacts"]}
                self.assertEqual({name: by_file[name]["sha256"] for name in records}, records)
                self.assertEqual(sum(item["file"].endswith(".deb") for item in document["artifacts"]),
                                 21 if speech == "stub" else 13)
                checked = subprocess.run(["sha256sum", "-c", str(fixture.checksums)], cwd=fixture.artifacts,
                                         env=env, text=True, capture_output=True, check=False)
                self.assertEqual(checked.returncode, 0, checked.stderr)

    def test_final_only_gate_runs_before_signing(self):
        gate = self.step("publish-release", "Require native records for a final release")
        for prerelease in ("true", "false", "", "unexpected"):
            self.assertEqual(evaluate(gate["if"], {"needs.build_packages.outputs.prerelease": prerelease}), prerelease != "true")
        names = [step.get("name") for step in self.jobs["publish-release"]["steps"]]
        self.assertLess(names.index(gate["name"]), names.index("Sign SHA256SUMS (keyless)"))
        self.assertIn('native-release-record.py check --directory "$RELEASE_DIR" --tag "$TAG"', gate["run"])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "repo"
            artifact_identity.init_fixture_repo(root)
            commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
            helper = root / "tools/release/native-release-record.py"
            helper.parent.mkdir(parents=True)
            helper.write_text(f"import sys\nassert sys.argv[1:] == ['check', '--directory', 'assets', '--tag', 'v0.3.1', '--commit', '{commit}']\nraise SystemExit(73)\n")
            context = {"needs.build_packages.outputs.tag": "v0.3.1", "needs.build_packages.outputs.release_dir": "assets"}
            env = {"PATH": os.environ["PATH"], **{key: str(evaluate(value, context)) for key, value in gate["env"].items()}}
            result = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", gate["run"]],
                                    cwd=root, env=env, text=True, capture_output=True, check=False)
            self.assertEqual(result.returncode, 73, result.stderr)


if __name__ == "__main__":
    unittest.main()
