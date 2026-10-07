#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive the streaming footprint checker and harness without a Nano.

The checker is shown to refuse, for its own reason, every way a record could
claim more than its samples support. The harness runs against fake packages
whose worker is a small compiled server that adds known amounts to its
stripped size and resident set, so each delta the record derives is checked
against what the fakes were told to add.
"""

from __future__ import annotations

import copy
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE = ROOT / "tools/validation/streaming_footprint_record.py"
HARNESS = ROOT / "tools/validation/streaming-footprint.sh"
THRESHOLDS = ROOT / "tools/validation/streaming-footprint-thresholds.json"
SCHEMA = ROOT / "config/schemas/streaming_footprint_record.json"
FAKES = ROOT / "test/validation/fixtures/streaming_footprint_fakes"
sys.path.insert(0, str(ROOT / "tools/validation"))
import streaming_footprint_record as record_mod  # noqa: E402

try:
    import jsonschema
except ImportError:  # the schema cross-check needs it; CI installs it
    jsonschema = None

MIB = 1024 * 1024
failures = 0


def check(what: str, ok: bool, detail: str = "") -> None:
    global failures
    if ok:
        print(f"  ok   {what}")
    else:
        print(f"  FAIL {what}" + (f"\n       {detail}" if detail else ""))
        failures += 1


def run_cli(*args: str, env: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(MODULE), *args], text=True, capture_output=True, env=env
    )


# --- the checker ------------------------------------------------------------------


def synthetic_run(index: int, stripped: int, idle_kib: int, steady_kib: int) -> dict:
    """One run; later runs are larger, so the pairing the delta uses is visible."""
    stripped += index * 4096
    idle_kib += index * 1000
    steady_kib += index * 1000
    return {
        "index": index,
        "elf_files": [
            {
                "path": "usr/lib/tensorplate/tensorplate-serving",
                "bytes": stripped + 4096,
                "stripped_bytes": stripped,
            }
        ],
        "idle_rss_samples_kib": [idle_kib, idle_kib + 8, idle_kib + 4],
        "steady": {
            "status": "measured",
            "reason": None,
            "streams": 16,
            "samples_kib": [steady_kib, steady_kib + 16, steady_kib + 8],
        },
    }


def synthetic_record(thresholds: dict) -> dict:
    runs = thresholds["runs"]
    sides = {
        "with_streaming": {
            "package": {
                "file": "tensorplate-serving_0.3.1_amd64.deb",
                "sha256": "a" * 64,
                "version": "0.3.1",
                "architecture": "amd64",
            },
            "version_output": ["tensorplate-serving 0.3.1", "protocol 0.1", "bundle-format 0.1"],
            "runs": [synthetic_run(i, 24 * MIB, 50_000, 90_000) for i in range(1, runs + 1)],
        },
        "without_streaming": {
            "package": {
                "file": "tensorplate-serving_0.3.1~dev_amd64.deb",
                "sha256": "b" * 64,
                "version": "0.3.1~dev",
                "architecture": "amd64",
            },
            "version_output": [],
            "runs": [synthetic_run(i, 10 * MIB, 30_000, 40_000) for i in range(1, runs + 1)],
        },
    }
    return {
        "schema_version": "0.1",
        "record_kind": "streaming_footprint",
        "provenance": "synthetic",
        "tool": {"name": "streaming-footprint", "source_commit": None},
        "recorded_at_utc": "2026-10-07T00:00:00Z",
        "host": {"machine": "x86_64", "kernel": "6.8.0"},
        "thresholds": copy.deepcopy(thresholds),
        "sides": sides,
        "result": record_mod.derive_result(thresholds, sides),
    }


def rederive(record: dict) -> dict:
    record["result"] = record_mod.derive_result(record["thresholds"], record["sides"])
    return record


def checker_cases(work: Path) -> dict:
    thresholds = record_mod.load_thresholds()
    check(
        "thresholds declare three budgets and three runs",
        thresholds["runs"] == 3
        and thresholds["budgets"]["installed_size"]["max_delta_bytes"] == 32 * MIB
        and thresholds["budgets"]["idle_rss"]["max_delta_bytes"] == 64 * MIB
        and thresholds["budgets"]["steady_rss"]["max_delta_bytes"] == 128 * MIB
        and thresholds["budgets"]["steady_rss"]["streams"] == 16,
        json.dumps(thresholds),
    )
    good = synthetic_record(thresholds)
    good_path = work / "good.json"
    good_path.write_text(record_mod.dump(good))
    result = run_cli("check", str(good_path))
    check(
        "a complete record within every budget passes",
        result.returncode == 0 and "pass:" in result.stdout,
        result.stdout + result.stderr,
    )
    check(
        "the maximum delta is the largest with-streaming value minus the smallest without",
        good["result"]["budgets"]["idle_rss"]["max_delta_bytes"] == (53_008 - 31_008) * 1024
        and good["result"]["budgets"]["installed_size"]["max_delta_bytes"] == 14 * MIB + 8192,
        json.dumps(good["result"]["budgets"]["idle_rss"]),
    )
    if jsonschema is not None:
        try:
            jsonschema.validate(good, json.loads(SCHEMA.read_text()))
            jsonschema.validate(
                thresholds,
                {
                    "$ref": "#/definitions/Thresholds",
                    "definitions": json.loads(SCHEMA.read_text())["definitions"],
                },
            )
            check("jsonschema accepts the record and the thresholds", True)
        except jsonschema.ValidationError as exc:
            check("jsonschema accepts the record and the thresholds", False, str(exc))

    def mutate(name: str, apply, expect_code: int, expect_text: str, honest: bool = False):
        record = copy.deepcopy(good)
        apply(record)
        if honest:
            rederive(record)
        path = work / "mutant.json"
        path.write_text(record_mod.dump(record))
        result = run_cli("check", str(path))
        output = result.stdout + result.stderr
        check(
            name,
            result.returncode == expect_code and expect_text in output,
            f"exit {result.returncode}; {output.strip()}",
        )
        if jsonschema is not None and "does not match its schema" in expect_text:
            try:
                jsonschema.validate(record, json.loads(SCHEMA.read_text()))
                check(f"jsonschema also rejects: {name}", False, "jsonschema accepted it")
            except jsonschema.ValidationError:
                check(f"jsonschema also rejects: {name}", True)

    def set_budget(record, budget, value):
        record["thresholds"]["budgets"][budget]["max_delta_bytes"] = value

    def drop_run(record, side):
        record["sides"][side]["runs"].pop()

    # An edited budget, even one the samples would still satisfy.
    mutate(
        "a threshold edited in the record is refused",
        lambda r: set_budget(r, "idle_rss", 64 * MIB + 1),
        2,
        "thresholds in the record differ from tools/validation/streaming-footprint-thresholds.json",
        honest=True,
    )
    mutate(
        "a threshold missing from the record is refused",
        lambda r: r["thresholds"]["budgets"].pop("steady_rss"),
        2,
        "record does not match its schema",
    )

    def fewer_runs_declared(record):
        record["thresholds"]["runs"] = 2
        for side in record_mod.SIDES:
            drop_run(record, side)

    mutate(
        "a record that lowered the declared run count is refused",
        fewer_runs_declared,
        2,
        "thresholds in the record differ",
        honest=True,
    )
    mutate(
        "a missing run leaves the record incomplete",
        lambda r: drop_run(r, "with_streaming"),
        2,
        "incomplete: installed_size incomplete (with_streaming: 2 of 3 runs recorded)",
        honest=True,
    )
    mutate(
        "a missing run claimed as a pass is refused",
        lambda r: drop_run(r, "without_streaming"),
        2,
        "result differs from the one derived from the samples",
    )
    mutate(
        "a run with no idle sample leaves the record incomplete",
        lambda r: r["sides"]["with_streaming"]["runs"][1].update(idle_rss_samples_kib=[]),
        2,
        "idle_rss incomplete (with_streaming run 2: no idle VmRSS sample)",
        honest=True,
    )
    mutate(
        "a run with no ELF file leaves the record incomplete",
        lambda r: r["sides"]["without_streaming"]["runs"][0].update(elf_files=[]),
        2,
        "installed_size incomplete (without_streaming run 1: no ELF file measured)",
        honest=True,
    )

    def steady_not_run(record):
        record["sides"]["with_streaming"]["runs"][2]["steady"] = {
            "status": "not_run",
            "reason": "no stream driver was given",
            "streams": None,
            "samples_kib": [],
        }

    mutate(
        "a steady state not run leaves the record incomplete, with the reason",
        steady_not_run,
        2,
        "steady_rss incomplete (with_streaming run 3: steady state not measured: no stream driver",
        honest=True,
    )

    def steady_not_run_no_reason(record):
        steady_not_run(record)
        record["sides"]["with_streaming"]["runs"][2]["steady"]["reason"] = ""

    mutate(
        "a steady state not run without a reason is refused",
        steady_not_run_no_reason,
        2,
        "steady state not run with no reason",
        honest=True,
    )
    mutate(
        "a steady state that held fewer streams than declared is refused",
        lambda r: r["sides"]["with_streaming"]["runs"][0]["steady"].update(streams=8),
        2,
        "steady state held 8 streams, 16 declared",
        honest=True,
    )
    mutate(
        "a steady state with no sample leaves the record incomplete",
        lambda r: r["sides"]["without_streaming"]["runs"][1]["steady"].update(samples_kib=[]),
        2,
        "steady_rss incomplete (without_streaming run 2: no steady-state VmRSS sample)",
        honest=True,
    )
    mutate(
        "more runs than declared are refused",
        lambda r: r["sides"]["with_streaming"]["runs"].append(synthetic_run(4, MIB, 1, 1)),
        2,
        "4 runs recorded, 3 declared",
        honest=True,
    )
    mutate(
        "a run out of order is refused",
        lambda r: r["sides"]["with_streaming"]["runs"].reverse(),
        2,
        "run 1 carries index 3",
        honest=True,
    )

    def exceed_idle(record):
        for run in record["sides"]["with_streaming"]["runs"]:
            run["idle_rss_samples_kib"] = [31_008 + 64 * 1024 + 1]

    mutate(
        "a delta over its budget fails",
        exceed_idle,
        1,
        "fail: installed_size within (14688256 of 33554432 bytes); idle_rss exceeded"
        " (67109888 of 67108864 bytes)",
        honest=True,
    )

    def exceed_idle_but_claim_pass(record):
        exceed_idle(record)
        rederive(record)
        record["result"]["status"] = "pass"
        record["result"]["budgets"]["idle_rss"]["status"] = "within"

    mutate(
        "an exceeded budget claimed as within is refused",
        exceed_idle_but_claim_pass,
        2,
        "result differs from the one derived from the samples: /budgets/idle_rss/status",
    )
    mutate(
        "a recorded delta smaller than the samples give is refused",
        lambda r: r["result"]["budgets"]["installed_size"].update(max_delta_bytes=1),
        2,
        "result differs from the one derived from the samples: /budgets/installed_size/max_delta",
    )
    mutate(
        "a recorded provenance outside the two allowed is refused",
        lambda r: r.update(provenance="measured"),
        2,
        "record does not match its schema",
    )

    broken = work / "thresholds-missing-budget.json"
    broken_thresholds = copy.deepcopy(thresholds)
    broken_thresholds["budgets"].pop("idle_rss")
    broken.write_text(json.dumps(broken_thresholds))
    status, detail = record_mod.check_record(copy.deepcopy(good), thresholds_path=broken)
    check(
        "a declared thresholds file missing a budget gives no verdict",
        status == "invalid" and "missing required field 'idle_rss'" in detail,
        f"{status}: {detail}",
    )
    status, detail = record_mod.check_record(copy.deepcopy(good), thresholds_path=work / "absent")
    check(
        "an absent thresholds file gives no verdict",
        status == "invalid" and detail.startswith("thresholds file"),
        f"{status}: {detail}",
    )

    summary_path = work / "summary.json"
    result = run_cli("summary", str(good_path), "--out", str(summary_path))
    summary = json.loads(summary_path.read_text()) if summary_path.exists() else {}
    check(
        "the summary carries the record's digest, identity and result and no sample",
        result.returncode == 0
        and summary.get("record_sha256") == hashlib.sha256(good_path.read_bytes()).hexdigest()
        and summary.get("result") == good["result"]
        and summary["sides"]["with_streaming"]["runs"] == 3
        and "samples" not in json.dumps(summary)
        and "elf_files" not in json.dumps(summary),
        result.stdout + result.stderr,
    )
    tampered = copy.deepcopy(good)
    tampered["result"]["status"] = "fail"
    (work / "tampered.json").write_text(record_mod.dump(tampered))
    result = run_cli("summary", str(work / "tampered.json"), "--out", str(work / "no.json"))
    check(
        "no summary is written for a record the check refuses",
        result.returncode == 2 and not (work / "no.json").exists(),
        result.stdout + result.stderr,
    )
    return good


# --- the harness, against fake packages -------------------------------------------

FAST = [
    "--settle-seconds",
    "0.3",
    "--samples",
    "3",
    "--sample-interval",
    "0.1",
    "--ready-timeout",
    "5",
]


def compile_worker(work: Path, name: str, *defines: str) -> Path:
    binary = work / name
    subprocess.run(
        [
            os.environ.get("CC", "cc"),
            "-O1",
            "-Wall",
            "-Wextra",
            "-pthread",
            *defines,
            "-o",
            str(binary),
            str(FAKES / "worker.c"),
        ],
        check=True,
    )
    return binary


def build_package(
    work: Path,
    name: str,
    worker: Path | None,
    version: str = "0.0.0~fake",
    architecture: str | None = None,
    script: str | None = None,
) -> Path:
    """A tensorplate-serving package shaped like the real one around the given worker."""
    tree = work / f"{name}.tree"
    shutil.rmtree(tree, ignore_errors=True)
    (tree / "DEBIAN").mkdir(parents=True)
    (tree / "usr/lib/tensorplate").mkdir(parents=True)
    (tree / "etc/tensorplate").mkdir(parents=True)
    if architecture is None:
        architecture = subprocess.run(
            ["dpkg", "--print-architecture"], check=True, capture_output=True, text=True
        ).stdout.strip()
    (tree / "DEBIAN/control").write_text(
        f"Package: tensorplate-serving\nVersion: {version}\nArchitecture: {architecture}\n"
        "Maintainer: test <test@example.com>\nDescription: fake serving worker\n"
    )
    target = tree / "usr/lib/tensorplate/tensorplate-serving"
    if worker is not None:
        shutil.copy2(worker, target)
    else:
        target.write_text(script or "#!/bin/sh\nexit 0\n")
    target.chmod(0o755)
    (tree / "etc/tensorplate/serving_worker.json").write_text("{}\n")
    package = work / f"{name}.deb"
    subprocess.run(
        ["dpkg-deb", "-b", "--root-owner-group", str(tree), str(package)],
        check=True,
        capture_output=True,
    )
    return package


def run_harness(
    out: Path, with_pkg: Path, without_pkg: Path, *extra: str
) -> subprocess.CompletedProcess:
    return subprocess.run(
        [
            str(HARNESS),
            "--with-streaming",
            str(with_pkg),
            "--without-streaming",
            str(without_pkg),
            "--out",
            str(out),
            "--provenance",
            "synthetic",
            *FAST,
            *extra,
        ],
        text=True,
        capture_output=True,
    )


def harness_cases(work: Path) -> None:
    work.mkdir()
    driver = FAKES / "driver.sh"
    on = compile_worker(
        work, "worker-on", "-DPAYLOAD_BYTES=2097152", "-DIDLE_MIB=8", "-DSTREAM_MIB=1"
    )
    off = compile_worker(work, "worker-off")
    heavy = compile_worker(work, "worker-heavy", "-DIDLE_MIB=70")
    exits = compile_worker(work, "worker-exits", "-DEXIT_AT_START")
    degraded = compile_worker(work, "worker-degraded", "-DNEVER_READY")
    on_pkg = build_package(work, "with", on)
    off_pkg = build_package(work, "without", off, version="0.0.0~fake.off")

    out = work / "pass"
    result = run_harness(out, on_pkg, off_pkg, "--stream-driver", str(driver))
    record_path = out / "record.json"
    record = json.loads(record_path.read_text()) if record_path.exists() else {}
    budgets = record.get("result", {}).get("budgets", {})
    check(
        "a driven run against the fakes passes and the check agrees",
        result.returncode == 0
        and record.get("result", {}).get("status") == "pass"
        and "pass:" in result.stdout,
        result.stdout + result.stderr,
    )
    size_delta = budgets.get("installed_size", {}).get("max_delta_bytes")
    check(
        "the stripped-size delta is the fake's 2 MiB payload",
        size_delta is not None and 2 * MIB - 4096 <= size_delta <= 2 * MIB + 8192,
        f"delta {size_delta}",
    )
    elf_files = [
        entry
        for side in record.get("sides", {}).values()
        for run in side.get("runs", [])
        for entry in run.get("elf_files", [])
    ]
    check(
        "every ELF file was stripped: the stripped size is below the installed size",
        bool(elf_files) and all(entry["stripped_bytes"] < entry["bytes"] for entry in elf_files),
        json.dumps(elf_files[:1]),
    )
    idle_delta = budgets.get("idle_rss", {}).get("max_delta_bytes")
    check(
        "the idle resident-set delta is the fake's 8 MiB, within 2 MiB",
        idle_delta is not None and 8 * MIB <= idle_delta <= 10 * MIB,
        f"delta {idle_delta}; {json.dumps(budgets.get('idle_rss'))}",
    )
    steady_delta = budgets.get("steady_rss", {}).get("max_delta_bytes")
    check(
        "the steady-state delta is the fake's 8 MiB plus 16 held MiB, within 3 MiB",
        steady_delta is not None and 24 * MIB <= steady_delta <= 27 * MIB,
        f"delta {steady_delta}; {json.dumps(budgets.get('steady_rss'))}",
    )
    runs = {
        side: len(record.get("sides", {}).get(side, {}).get("runs", []))
        for side in record_mod.SIDES
    }
    check(
        "three runs were captured per side",
        runs == {"with_streaming": 3, "without_streaming": 3},
        str(runs),
    )
    side = record.get("sides", {}).get("with_streaming", {})
    check(
        "the package identity and the worker's --version lines are recorded",
        side.get("package", {}).get("file") == "with.deb"
        and side.get("package", {}).get("sha256") == hashlib.sha256(on_pkg.read_bytes()).hexdigest()
        and side.get("package", {}).get("version") == "0.0.0~fake"
        and side.get("version_output")
        == ["tensorplate-serving 0.0.0-fake", "protocol 0.1", "bundle-format 0.1"]
        and record.get("provenance") == "synthetic",
        json.dumps(side.get("package")) + str(side.get("version_output")),
    )
    captures = out / "captures/with_streaming/run-3"
    check(
        "raw captures are kept: health, idle and steady samples, the stream count, the worker log",
        all(
            (captures / name).exists()
            for name in (
                "health.json",
                "idle-vmrss.tsv",
                "steady-vmrss.tsv",
                "steady-streams.txt",
                "worker.log",
                "elf-files.tsv",
                "size.txt",
                "worker-exit.txt",
                "driver.log",
            )
        )
        and (captures / "steady-streams.txt").read_text().strip() == "16"
        and json.loads((captures / "health.json").read_text())["state"] == "ready"
        and len((captures / "idle-vmrss.tsv").read_text().splitlines()) == 3,
        str(sorted(p.name for p in captures.iterdir())) if captures.exists() else "no captures",
    )
    summary_path = out / "summary.json"
    summary = json.loads(summary_path.read_text()) if summary_path.exists() else {}
    check(
        "the summary names the record's digest and result",
        record_path.exists()
        and summary.get("record_sha256") == hashlib.sha256(record_path.read_bytes()).hexdigest()
        and summary.get("result") == record.get("result"),
        json.dumps(summary)[:300],
    )
    result = run_harness(out, on_pkg, off_pkg)
    check(
        "a record directory is never captured into twice",
        result.returncode == 2 and "never over another" in result.stderr,
        result.stderr,
    )

    out = work / "undriven"
    result = run_harness(out, on_pkg, off_pkg)
    record = json.loads((out / "record.json").read_text()) if (out / "record.json").exists() else {}
    steady = record.get("result", {}).get("budgets", {}).get("steady_rss", {})
    check(
        "without a stream driver the record is incomplete, naming the steady state",
        result.returncode == 2
        and record.get("result", {}).get("status") == "incomplete"
        and steady.get("status") == "incomplete"
        and "no stream driver was given" in (steady.get("reason") or "")
        and record["result"]["budgets"]["idle_rss"]["status"] == "within"
        and (out / "summary.json").exists(),
        result.stdout + result.stderr,
    )

    out = work / "over"
    heavy_pkg = build_package(work, "heavy", heavy)
    result = run_harness(out, heavy_pkg, off_pkg, "--stream-driver", str(driver))
    record = json.loads((out / "record.json").read_text()) if (out / "record.json").exists() else {}
    budgets = record.get("result", {}).get("budgets", {})
    check(
        "an idle resident set 70 MiB above the baseline fails the 64 MiB budget",
        result.returncode == 1
        and record.get("result", {}).get("status") == "fail"
        and budgets.get("idle_rss", {}).get("status") == "exceeded"
        and budgets.get("installed_size", {}).get("status") == "within"
        and "idle_rss exceeded" in result.stderr
        and (out / "summary.json").exists(),
        result.stdout + result.stderr,
    )

    out = work / "exits"
    exits_pkg = build_package(work, "exits", exits)
    result = run_harness(out, on_pkg, exits_pkg)
    check(
        "a worker that exits at start stops the run with its log kept and no record",
        result.returncode == 2
        and "run 1: worker not ready (see" in result.stderr
        and not (out / "record.json").exists()
        and (out / "captures/without_streaming/run-1/worker.log").exists()
        and "exiting at start" in (out / "captures/without_streaming/run-1/worker.log").read_text(),
        result.stderr,
    )

    out = work / "degraded"
    degraded_pkg = build_package(work, "degraded", degraded)
    result = run_harness(out, degraded_pkg, off_pkg)
    health = out / "captures/with_streaming/run-1/health.json"
    check(
        "a worker that stays degraded is never sampled: no record, the last /health body kept",
        result.returncode == 2
        and "run 1: worker not ready (see" in result.stderr
        and not (out / "record.json").exists()
        and not (out / "captures/with_streaming/run-1/idle-vmrss.tsv").exists()
        and health.exists()
        and json.loads(health.read_text())["state"] == "degraded",
        result.stderr,
    )

    out = work / "script"
    script_pkg = build_package(work, "script", None)
    result = run_harness(out, on_pkg, script_pkg)
    check(
        "a package whose worker is not an ELF executable is refused",
        result.returncode == 2 and "is not an ELF executable" in result.stderr,
        result.stderr,
    )

    out = work / "foreign"
    foreign_pkg = build_package(work, "foreign", on, architecture="riscv64")
    result = run_harness(out, foreign_pkg, off_pkg)
    check(
        "a package for another architecture is refused",
        result.returncode == 2 and "is not this host's" in result.stderr,
        result.stderr,
    )

    out = work / "silent"
    silent = work / "silent-driver.sh"
    silent.write_text("#!/bin/sh\nexit 0\n")
    silent.chmod(0o755)
    result = run_harness(out, on_pkg, off_pkg, "--stream-driver", str(silent))
    check(
        "a stream driver that exits without holding the streams stops the run",
        result.returncode == 2 and "did not report 'held'" in result.stderr,
        result.stderr,
    )

    shellcheck = shutil.which("shellcheck")
    if shellcheck:
        result = subprocess.run(
            [shellcheck, str(HARNESS), str(driver)], capture_output=True, text=True
        )
        check(
            "shellcheck accepts the harness and the fake driver",
            result.returncode == 0,
            result.stdout,
        )
    else:
        print("  skip shellcheck (not installed)")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="tp-footprint-test-") as tmp:
        work = Path(tmp)
        (work / "checker").mkdir()
        print("checker")
        checker_cases(work / "checker")
        print("harness")
        harness_cases(work / "harness")
    if failures:
        print(f"streaming_footprint_test: {failures} check(s) failed")
        return 1
    print("streaming_footprint_test: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
