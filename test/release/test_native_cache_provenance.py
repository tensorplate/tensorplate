#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Runtime-constructed unit inputs around the recorded vcpkg install-tree fixture."""
from __future__ import annotations

import argparse
import copy
from contextlib import redirect_stderr
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

try:
    import yaml
except ImportError:
    sys.exit("FAIL: this suite needs PyYAML to read the release workflow.\n"
             "      Install the release tooling dependencies with:\n"
             "        python3 -m pip install -r tools/release/requirements.txt")

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/release/native-cache-provenance.py"
spec = importlib.util.spec_from_file_location("cache_provenance", TOOL)
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)


class Provenance(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tree = self.root / "install"
        shutil.copytree(ROOT / "test/release/fixtures/native-sbom/install-tree", self.tree)
        self.args = argparse.Namespace(manifest=ROOT / "vcpkg.json", triplet="x64-linux", version="0.3.1-rc.2",
            sbom=self.root / "native.spdx.json", archives=self.root / "archives", repository="example/project",
            ref="refs/tags/v0.3.1-rc.2", default_branch="develop", stamp=self.root / "provisioned.stamp",
            restore_started="2026-10-07T19:00:00Z", restored_cache_id=11)
        self.baseline, self.canonical = tool.manifest_identity(self.args.manifest)
        self.args.cache_key = f"vcpkg-release-Linux-x64-linux-{self.baseline}-" + "a" * 64
        ports = tool.native.installed_ports(tool.native.parse_status(self.tree / "vcpkg/status"), "x64-linux")
        for name, port in ports.items():
            abi = port["abi"]
            path = self.args.archives / abi[:2] / f"{abi}.zip"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(f"synthetic unit archive for {name}".encode())
        worker = self.root / "worker"
        worker.write_bytes(b"synthetic unit worker")
        collect = argparse.Namespace(**vars(self.args), install_root=self.tree, feature="streaming-grpc",
            output=self.args.sbom, worker=worker, cmake_cache=None, created="2026-10-07T00:00:00Z")
        self.document = tool.native.collect(collect)
        self.args.sbom.write_text(json.dumps(self.document))
        self.archives = tool.archive_list(self.args.archives)
        self.entry = {"id": 11, "key": self.args.cache_key, "ref": "refs/heads/develop", "version": "b" * 64,
                      "created_at": "2026-10-07T18:08:57.483526Z", "private_field": "never published"}
        self.run = {"id": 22, "path": ".github/workflows/release-dependencies.yml", "conclusion": "success",
                    "head_branch": "develop", "head_sha": "c" * 40}
        self.job = {"id": 33, "run_attempt": 1, "conclusion": "success",
                    "started_at": "2026-10-07T18:00:00Z", "completed_at": "2026-10-07T18:08:57Z"}
        self.log_time = "2026-10-07T18:08:57.6023133Z"
        self.logs = self.log_time + " Cache saved with key: " + self.args.cache_key + "\n"
        self.calls = []

    def github(self, argv):
        self.calls.append(argv)
        endpoint = next(a for a in argv if a.startswith("repos/"))
        if endpoint.endswith("/logs"):
            return "unrelated private log text\n" + self.logs
        self.assertIn("--paginate", argv)
        self.assertIn("--slurp", argv)
        if endpoint.endswith("/caches"):
            field = "actions_caches"
            rows = [self.entry] if f"ref={self.entry['ref']}" in argv else []
        elif endpoint.endswith("/runs"):
            self.assertIn("branch=" + self.run["head_branch"], argv)
            self.assertIn("created=2026-10-06T18:08:57Z..2026-10-07T18:09:57Z", argv)
            field, rows = "workflow_runs", [self.run]
        else:
            self.assertIn("filter=all", argv)
            field, rows = "jobs", [self.job]
        return json.dumps([{field: [], "total_count": len(rows)}, {field: rows, "total_count": len(rows)}])

    def record(self, command=None):
        with patch.object(tool, "command", side_effect=command or self.github):
            source = tool.actions_source(self.args)
        return {"schema_version": 1, "source": "actions-cache", "architecture": "amd64", "triplet": "x64-linux",
                "feature": "streaming-grpc", "baseline": self.baseline, "version": self.args.version,
                "commit": "d" * 40, "created": "2026-10-07T00:00:00Z", "archives": self.archives,
                "sbom_sha256": tool.native.digest_of(self.args.sbom)["sha256"], **source}

    def record_arguments(self, output):
        return ["record", "--source", "actions-cache", "--manifest", str(self.args.manifest),
                "--triplet", self.args.triplet, "--version", self.args.version, "--sbom", str(self.args.sbom),
                "--archives", str(self.args.archives), "--commit", "d" * 40,
                "--created", "2026-10-07T00:00:00Z", "--output", str(output),
                "--cache-key", self.args.cache_key, "--ref", self.args.ref,
                "--default-branch", self.args.default_branch, "--repository", self.args.repository,
                "--restore-started", self.args.restore_started, "--restored-cache-id", str(self.args.restored_cache_id)]

    def assert_unavailable(self, record, reason):
        self.assertEqual(record["log_evidence"], {"status": "unavailable", "reason": reason})
        self.assertNotIn("saved_by", record)
        with patch.object(tool, "command", side_effect=AssertionError("offline check attempted a command")):
            tool.check_record(record, self.args)

    def test_exact_save_line_and_paginated_queries_identify_writer(self):
        record = self.record()
        tool.check_record(record, self.args)
        self.assertEqual(record["log_evidence"], {"status": "available"})
        self.assertEqual(record["saved_by"]["job_id"], 33)
        self.assertEqual(record["saved_by"]["saved_at"], self.log_time)
        self.assertNotIn("private", json.dumps(record))
        self.assertNotIn(str(self.root), json.dumps(record))
        self.assertEqual(len([c for c in self.calls if any(a.endswith("/caches") for a in c)]), 2)

    def test_current_ref_wins_and_release_workflow_is_also_a_writer(self):
        self.entry["ref"] = self.args.ref
        self.run.update(path=".github/workflows/release.yml", head_branch="v0.3.1-rc.2")
        self.assertEqual(self.record()["saved_by"]["workflow"], self.run["path"])
        self.assertEqual(len([c for c in self.calls if any(a.endswith("/caches") for a in c)]), 1)

    def test_new_preferred_cache_refuses_before_fallback_or_writer_lookup(self):
        self.entry["ref"] = self.args.ref
        self.args.restore_started = "2026-10-07T18:08:57.483525999Z"
        with self.assertRaisesRegex(tool.Refused, "created after restore started"):
            self.record()
        self.assertEqual(len(self.calls), 1, "must neither fall back nor look for a writer")
        self.assertTrue(any(a.endswith("/caches") for a in self.calls[0]))
        self.args.restore_started = self.entry["created_at"]
        self.run["head_branch"] = "v0.3.1-rc.2"
        record = self.record()
        self.assertEqual(record["restore_started"], self.args.restore_started)
        tool.check_record(record, self.args)

    def test_restore_start_is_required_valid_and_bound_in_the_record(self):
        record = self.record()
        for value in (None, "", "not-time", "2026-10-07T19:00:00", "2026-13-07T19:00:00Z"):
            self.args.restore_started = value
            self.calls.clear()
            with self.subTest(value=value), self.assertRaisesRegex(tool.Refused, "invalid UTC timestamp"):
                self.record()
            self.assertEqual(self.calls, [])
            changed = {**record, "restore_started": value}
            with self.assertRaisesRegex(tool.Refused, "invalid UTC timestamp"):
                tool.check_record(changed, self.args)
        changed = {**record, "restore_started": "2026-10-07T18:08:57.483525999Z"}
        with self.assertRaisesRegex(tool.Refused, "created after restore started"):
            tool.check_record(changed, self.args)
        del record["restore_started"]
        with self.assertRaisesRegex(tool.Refused, "unexpected provenance record fields"):
            tool.check_record(record, self.args)
        result = subprocess.run([sys.executable, str(TOOL), "record", "--source", "actions-cache",
            "--manifest", str(self.args.manifest), "--triplet", self.args.triplet, "--version", self.args.version,
            "--sbom", str(self.args.sbom), "--archives", str(self.args.archives), "--commit", "d" * 40,
            "--created", "2026-10-07T00:00:00Z", "--output", str(self.root / "record.json"),
            "--cache-key", self.args.cache_key, "--ref", self.args.ref,
            "--default-branch", self.args.default_branch, "--repository", self.args.repository,
            "--restored-cache-id", str(self.args.restored_cache_id)],
            capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("required source arguments are absent: --restore-started", result.stderr)
        self.assertFalse((self.root / "record.json").exists())

    def test_evicted_preferred_cache_cannot_be_replaced_by_default_cache(self):
        default = copy.deepcopy(self.entry)
        self.entry.update(ref=self.args.ref, id=44)
        with patch.object(tool, "command", side_effect=self.github):
            before = tool.select_cache(self.args)
        self.args.restored_cache_id = before["id"]
        self.entry = default
        self.calls.clear()
        with self.assertRaisesRegex(tool.Refused, "differs from the cache selected before restore"):
            self.record()
        self.assertEqual(len(self.calls), 2)
        self.assertTrue(all(any(a.endswith("/caches") for a in c) for c in self.calls))
        self.args.restored_cache_id = self.entry["id"]
        tool.check_record(self.record(), self.args)

    def test_pre_restore_cache_command_writes_only_identity_and_refuses_ambiguity(self):
        output = self.root / "before.json"
        argv = ["cache", "--cache-key", self.args.cache_key, "--ref", self.args.ref,
                "--default-branch", self.args.default_branch, "--repository", self.args.repository,
                "--restore-started", self.args.restore_started, "--output", str(output)]
        with patch.object(tool, "command", side_effect=self.github):
            self.assertEqual(tool.main(argv), 0)
        expected = {k: self.entry[k] for k in ("id", "key", "ref", "version", "created_at")}
        self.assertEqual(json.loads(output.read_text()), expected)
        self.assertNotIn("private", output.read_text())
        for field, value in (("id", True), ("version", "not-a-digest")):
            old = self.entry[field]
            self.entry[field] = value
            with patch.object(tool, "command", side_effect=self.github), self.assertRaisesRegex(tool.Refused, "invalid cache identity"):
                tool.select_cache(self.args)
            self.entry[field] = old
        with patch.object(tool, "api", return_value=[self.entry, {**self.entry, "version": "e" * 64}]):
            with self.assertRaisesRegex(tool.Refused, "exactly one cache entry"):
                tool.select_cache(self.args)

    def test_pre_restore_id_is_required_and_positive(self):
        for value in (None, 0, -1, True, "11"):
            self.args.restored_cache_id = value
            self.calls.clear()
            with self.subTest(value=value), self.assertRaisesRegex(tool.Refused, "positive integer"):
                self.record()
            self.assertEqual(self.calls, [])
        del self.args.restored_cache_id
        with self.assertRaisesRegex(tool.Refused, "positive integer"):
            self.record()

    def test_fractional_timestamps_are_portable_without_losing_ordering_precision(self):
        base = "2026-10-07T18:08:57"
        seconds = tool.time_ns(base + "Z")
        for fraction in ("1", "12", "123", "1234", "12345", "123456", "1234567", "12345678", "123456789"):
            value = base + "." + fraction + "Z"
            self.assertEqual(tool.timestamp(value).microsecond, int((fraction + "000000")[:6]))
            self.assertEqual(tool.time_ns(value) - seconds, int((fraction + "000000000")[:9]))

    def test_missing_wrong_key_early_and_late_logs_are_unmatched_and_duplicates_ambiguous(self):
        good = self.logs
        cases = ["", good.replace(self.args.cache_key, "different-key"),
                 good.replace(self.log_time, "2026-10-07T18:08:57.483525Z"),
                 good.replace(self.log_time, "2026-10-07T18:09:58Z"), good + good]
        for self.logs in cases:
            reason = "ambiguous" if self.logs == good + good else "unmatched"
            with self.subTest(log=self.logs), redirect_stderr(io.StringIO()):
                self.assert_unavailable(self.record(), reason)
        self.logs = good
        self.assertEqual(self.record()["log_evidence"], {"status": "available"})
        self.entry["created_at"] = "2026-10-07T18:08:57.6023134Z"
        with redirect_stderr(io.StringIO()):
            self.assert_unavailable(self.record(), "unmatched")

    def test_wrong_workflow_and_unsuccessful_savers_are_unmatched(self):
        for subject, key, value in [(self.run, "path", "other.yml"), (self.run, "conclusion", "failure"),
                                    (self.job, "conclusion", "failure")]:
            old = subject[key]
            subject[key] = value
            with self.subTest(key=key), redirect_stderr(io.StringIO()):
                self.assert_unavailable(self.record(), "unmatched")
            subject[key] = old

    def test_expired_denied_and_rate_limited_logs_write_records_with_diagnostics(self):
        output = self.root / "record.json"
        errors = ("gh: Not Found (HTTP 404): logs expired", "gh: Resource not accessible (HTTP 403)",
                  "gh: API rate limit exceeded (HTTP 403)")
        for error in errors:
            def run(argv, **kwargs):
                if any(a.endswith("/logs") for a in argv):
                    return subprocess.CompletedProcess(argv, 1, "", error)
                return subprocess.CompletedProcess(argv, 0, self.github(argv), "")
            stderr = io.StringIO()
            with self.subTest(error=error), patch.object(tool.subprocess, "run", side_effect=run), redirect_stderr(stderr):
                self.assertEqual(tool.main(self.record_arguments(output)), 0, stderr.getvalue())
            record = json.loads(output.read_text())
            self.assert_unavailable(record, "lookup-failed")
            self.assertEqual(record["archives"], self.archives)
            self.assertEqual(record["cache"]["id"], self.args.restored_cache_id)
            self.assertIn(error, stderr.getvalue())
            self.assertNotIn(error, output.read_text())
            self.assertNotIn("private", output.read_text())

    def test_one_writer_match_plus_an_unavailable_candidate_cannot_claim_uniqueness(self):
        def github(argv):
            if any(a.endswith("/jobs") for a in argv):
                return json.dumps([{"jobs": [self.job, {**self.job, "id": 34}]}])
            if any(a.endswith("/jobs/34/logs") for a in argv):
                raise tool.Refused(["second candidate's logs expired"])
            return self.github(argv)
        stderr = io.StringIO()
        with redirect_stderr(stderr):
            self.assert_unavailable(self.record(github), "lookup-failed")
        self.assertTrue(any(any(a.endswith("/jobs/33/logs") for a in call) for call in self.calls))
        self.assertIn("second candidate's logs expired", stderr.getvalue())

    def test_malformed_remote_writer_data_and_unbounded_queries_are_unavailable(self):
        for subject, field, value in ((self.job, "started_at", "not-time"),
                                      (self.job, "run_attempt", None), (self.run, "head_sha", "not-a-sha")):
            original = subject[field]
            subject[field] = value
            with self.subTest(field=field), redirect_stderr(io.StringIO()):
                self.assert_unavailable(self.record(), "lookup-failed")
            subject[field] = original
        self.logs = self.logs.replace(self.log_time, "not-time")
        with redirect_stderr(io.StringIO()):
            self.assert_unavailable(self.record(), "lookup-failed")
        def github(argv):
            if any(a.endswith("/runs") for a in argv):
                return '[{"workflow_runs": [], "total_count": 1001}]'
            return self.github(argv)
        stderr = io.StringIO()
        with redirect_stderr(stderr):
            self.assert_unavailable(self.record(github), "lookup-failed")
        self.assertIn("result bound", stderr.getvalue())

    def test_log_evidence_schema_requires_exact_writer_presence(self):
        good = self.record()
        malformed = [None, [], {}, {"status": "unknown"}, {"status": "available", "reason": "unmatched"},
                     {"status": "unavailable"}, {"status": "unavailable", "reason": "raw error text"}]
        for evidence in malformed:
            with self.subTest(evidence=evidence), self.assertRaises(tool.Refused):
                tool.check_record({**good, "log_evidence": evidence}, self.args)
        self.logs = ""
        with redirect_stderr(io.StringIO()):
            unavailable = self.record()
        for changed in ({k: v for k, v in good.items() if k != "log_evidence"},
                        {k: v for k, v in good.items() if k != "saved_by"},
                        {**unavailable, "saved_by": good["saved_by"]}):
            with self.assertRaises(tool.Refused):
                tool.check_record(changed, self.args)

    def test_unavailable_logs_do_not_waive_cache_or_archive_and_sbom_bindings(self):
        self.logs = ""
        with redirect_stderr(io.StringIO()):
            good = self.record()
        changes = (lambda r: r["cache"].update(id=None), lambda r: r["cache"].update(version="bad"),
                   lambda r: r.update(restore_started="2026-10-07T18:08:56Z"),
                   lambda r: r["archives"][0].update(sha256="0" * 64),
                   lambda r: r.update(sbom_sha256="0" * 64))
        for change in changes:
            changed = copy.deepcopy(good)
            change(changed)
            with self.assertRaises(tool.Refused):
                tool.check_record(changed, self.args)
        (self.args.archives / self.archives[0]["path"]).write_bytes(b"tampered local archive")
        output, stderr = self.root / "record.json", io.StringIO()
        with patch.object(tool, "command", side_effect=self.github), redirect_stderr(stderr):
            self.assertEqual(tool.main(self.record_arguments(output)), 1)
        self.assertIn("SBOM archive reference does not match provenance", stderr.getvalue())
        self.assertFalse(output.exists())

    def test_check_commit_is_optional_but_validated_and_bound_when_supplied(self):
        record = self.record()
        path = self.root / "record.json"
        path.write_text(json.dumps(record))
        argv = ["check", "--record", str(path), "--manifest", str(self.args.manifest),
                "--triplet", self.args.triplet, "--version", self.args.version, "--sbom", str(self.args.sbom)]
        self.assertEqual(tool.main(argv), 0)
        self.assertEqual(tool.main([*argv, "--commit", record["commit"]]), 0)
        for value, message in (("e" * 40, "source commit mismatch"), ("", "invalid expected source commit"),
                               ("D" * 40, "invalid expected source commit"), ("bad", "invalid expected source commit")):
            stderr = io.StringIO()
            with self.subTest(commit=value), redirect_stderr(stderr):
                self.assertEqual(tool.main([*argv, "--commit", value]), 1)
            self.assertIn(message, stderr.getvalue())

    def test_record_identity_and_archive_binding_mutations_refuse(self):
        good = self.record()
        for key, value in [("baseline", "0" * 40), ("version", "0.3.1"), ("triplet", "arm64-linux"),
                           ("source", "unknown"), ("sbom_sha256", "0" * 64), ("schema_version", True)]:
            changed = {**good, key: value}
            with self.subTest(key=key), self.assertRaises(tool.Refused):
                tool.check_record(changed, self.args)
        for change in (lambda r: r["archives"][0].update(sha256="0" * 64),
                       lambda r: r["archives"].append(r["archives"][0]),
                       lambda r: r["saved_by"].update(run_id=None),
                       lambda r: r["saved_by"].update(saved_at="2026-10-07T18:08:56Z"),
                       lambda r: r.update(unexpected="raw log")):
            changed = copy.deepcopy(good)
            change(changed)
            with self.assertRaises(tool.Refused):
                tool.check_record(changed, self.args)
        tool.check_record(good, self.args)

    def test_worker_digest_and_version_are_required_even_with_rebound_sbom_hash(self):
        record = self.record()
        for field, value in (("checksums", []), ("versionInfo", "0.3.1")):
            doc = copy.deepcopy(self.document)
            doc["packages"][0][field] = value
            self.args.sbom.write_text(json.dumps(doc))
            record["sbom_sha256"] = tool.native.digest_of(self.args.sbom)["sha256"]
            with self.subTest(field=field), self.assertRaisesRegex(tool.Refused, "SBOM worker"):
                tool.check_record(record, self.args)

    def stamp(self):
        fields = {"baseline": self.baseline, "dependencies_sha256": hashlib.sha256(self.canonical.encode()).hexdigest(),
                  "triplet": "arm64-linux", "feature": "streaming-grpc", "compiler": "synthetic unit compiler",
                  "vcpkg_version": "synthetic unit vcpkg", "provisioned_at": "2026-10-07T00:00:00Z",
                  "archives_sha256": tool.names_digest(self.archives)}
        return "".join(f"{key}={value}\n" for key, value in fields.items())

    def test_runner_stamp_binds_names_and_projects_unbounded_tool_text_as_hashes(self):
        record = self.record()
        self.args.triplet = "arm64-linux"
        self.args.stamp.write_text(self.stamp())
        source = tool.runner_source(self.args, self.archives, self.baseline, self.canonical)
        for key in ("repository", "restore_started", "cache", "saved_by", "log_evidence"):
            del record[key]
        record.update(source="runner-filesystem", architecture="arm64", triplet="arm64-linux", **source)
        tool.check_record(record, self.args)
        self.assertNotIn("synthetic unit compiler", json.dumps(record))
        self.assertIn("not contents", record["limitation"])
        self.assertEqual(source["stamp"]["archives_sha256"], tool.names_digest(self.archives))

    def test_runner_missing_duplicate_malformed_and_mismatched_stamp_refuse(self):
        self.args.triplet = "arm64-linux"
        good = self.stamp()
        cases = [good.replace("feature=streaming-grpc\n", ""), good + "feature=streaming-grpc\n",
                 good + "malformed\n", good.replace("triplet=arm64-linux", "triplet=x64-linux"),
                 good.replace("archives_sha256=", "archives_sha256=0")]
        for text in cases:
            self.args.stamp.write_text(text)
            with self.subTest(text=text), self.assertRaises(tool.Refused):
                tool.runner_source(self.args, self.archives, self.baseline, self.canonical)

    def test_archive_tree_rejects_symlinks_and_unexpected_files(self):
        stray = self.args.archives / "private-stamp"
        stray.write_text("synthetic unit data")
        with self.assertRaisesRegex(tool.Refused, "unexpected archive path"):
            tool.archive_list(self.args.archives)
        stray.unlink()
        stray.symlink_to(self.args.sbom)
        with self.assertRaisesRegex(tool.Refused, "symlink"):
            tool.archive_list(self.args.archives)

    def test_snapshot_handles_absent_empty_and_populated_archives(self):
        output = self.root / "snapshot.json"
        absent = self.root / "absent"
        for directory, expected in ((absent, []), (self.args.archives, self.archives)):
            result = subprocess.run([sys.executable, str(TOOL), "snapshot", "--archives", str(directory),
                                     "--output", str(output)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(output.read_text()), expected)
        absent.mkdir()
        self.assertEqual(tool.archive_list(absent, allow_empty=True), [])
        linked = self.root / "linked"
        linked.symlink_to(self.args.archives, target_is_directory=True)
        with self.assertRaisesRegex(tool.Refused, "symlink"):
            tool.archive_list(linked, allow_empty=True)

    def test_new_gh_escape_guard_is_handled_only_in_captured_output(self):
        denied = subprocess.CompletedProcess([], 1, "", "pass --allow-escape-sequences to output it anyway")
        allowed = subprocess.CompletedProcess([], 0, "captured\x1b[31mlog", "")
        with patch.object(tool.subprocess, "run", side_effect=[denied, allowed]) as run:
            self.assertEqual(tool.command(["gh", "api", "endpoint"]), allowed.stdout)
            self.assertIn("--allow-escape-sequences", run.call_args.args[0])
            self.assertTrue(run.call_args.kwargs["capture_output"])
        with patch.object(tool.subprocess, "run", return_value=allowed) as run:
            tool.command(["gh", "api", "endpoint"])
            self.assertNotIn("--allow-escape-sequences", run.call_args.args[0])

    def test_key_matches_execution_of_the_unchanged_action(self):
        action = ROOT / ".github/actions/release-vcpkg/action.yml"
        profile = ROOT / "tools/release/amd64-build-profile.sh"
        (self.root / "tools/release").mkdir(parents=True)
        shutil.copy2(profile, self.root / "tools/release" / profile.name)
        shutil.copy2(self.args.manifest, self.root / "vcpkg.json")
        bindir = self.root / "bin"
        bindir.mkdir()
        for name in ("clang", "clang++"):
            path = bindir / name
            path.write_text("#!/bin/sh\nprintf 'fixture clang version 15\\nignored second line\\n'\n")
            path.chmod(0o755)
        output = self.root / "output"
        env = {**os.environ, "PATH": str(bindir) + os.pathsep + os.environ["PATH"],
               "GITHUB_OUTPUT": str(output), "ACTION_PATH": str(action.parent)}
        step = yaml.safe_load(action.read_text())["runs"]["steps"][0]["run"]
        result = subprocess.run(["bash", "-c", step], cwd=self.root, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        fields = dict(line.split("=", 1) for line in output.read_text().splitlines())
        args = argparse.Namespace(manifest=self.args.manifest, profile=profile, action=action)
        with patch.dict(os.environ, env):
            self.assertEqual(tool.cache_key(args), f"vcpkg-release-Linux-x64-linux-{fields['baseline']}-{fields['digest']}")


if __name__ == "__main__":
    unittest.main()
