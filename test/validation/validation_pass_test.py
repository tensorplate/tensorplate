#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Index and compare the runs recorded on 2026-10-01, then break each guard once.

The inputs are the lifecycle report and the four candidate qualification
records filed under docs/validation/evidence; the expected numbers are the
ones those runs' READMEs state, or read from the record where no README has
them. Doctor's output is synthetic: no recorded run carries the
runner-profile findings yet.
"""

from __future__ import annotations

import copy
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/validation/validation-pass.py"
EVIDENCE = ROOT / "docs/validation/evidence"
ROW = "ubuntu2404-x86-l4-g2s8"
LIFECYCLE_REPORT = EVIDENCE / f"v0.3.1/{ROW}/upgrade-rollback-rc.1/lifecycle/lifecycle-report.json"
sys.path.insert(0, str(ROOT / "tools/validation"))
import jsonschema  # noqa: E402
import validation_index as index_mod  # noqa: E402

ENVELOPE = {
    "date": "2026-10-01",
    "session": "recorded runs of 2026-10-01",
    "row": ROW,
    "machine": "g2-standard-8, one NVIDIA L4",
    "build": "v0.3.1-rc.1",
    "kind": "evidence",
    "records": "docs/validation/evidence",
    "raw": "local-only",
    "notes": "",
    "settings": {"startup_timeout_ms": 120000},
}
REQUIRED = ("platform_row", "runner_profiles")


def record_path(candidate: str, run: int) -> Path:
    return EVIDENCE / f"speech-candidate-{candidate}-l4-2026-10-01/run-{run}/record.json"


def load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def envelope(**changes: object) -> dict:
    return {**copy.deepcopy(ENVELOPE), **changes}


def statuses(findings: list[dict]) -> dict[str, str]:
    return {finding["field"]: finding["status"] for finding in findings}


def refused(action, needle: str) -> None:
    try:
        action()
    except index_mod.IndexLineError as exc:
        assert needle in str(exc), (needle, str(exc))
        return
    raise AssertionError(f"not refused: expected an error naming {needle!r}")


def synthetic_doctor(**status_by_id: str) -> dict:
    findings = {"platform_row": "ok", "runner_profiles": "ok", "host_os": "ok"}
    findings.update(status_by_id)
    failing = sum(1 for status in findings.values() if status == "fail")
    return {
        "failing": failing,
        "findings": [
            {"id": finding_id, "status": status, "message": "synthetic"}
            for finding_id, status in findings.items()
        ],
    }


def run_tool(*arguments: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(TOOL), *arguments],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )


def write_index(path: Path, lines: list[dict]) -> Path:
    path.write_text("".join(index_mod.dump_line(line) for line in lines), encoding="utf-8")
    return path


def check_derivation() -> dict[str, dict]:
    schema = index_mod.load_schema()
    lifecycle = index_mod.lifecycle_line(load(LIFECYCLE_REPORT), envelope())
    assert lifecycle["result"] == "pass"
    assert lifecycle["metrics"] == {"stages_passed": 8, "stages_total": 8, "harness_wall_s": 157}
    assert lifecycle["checks"]["stage.rollback"] == "pass" and len(lifecycle["checks"]) == 8
    assert lifecycle["run_id"] == f"2026-10-01-{ROW}-lifecycle"
    assert "family_build" not in lifecycle

    lines = {"lifecycle": lifecycle}
    for candidate, subject in (("whisper", "stt-whisper"), ("kokoro", "tts-kokoro")):
        for run in (1, 2):
            line = index_mod.qualification_line(
                load(record_path(candidate, run)),
                envelope(run_suffix=f"run-{run}"),
                subject,
                family_build="ad792785",
            )
            jsonschema.validate(line, schema)
            assert line["family_build"] == "ad792785"
            lines[f"{subject}-{run}"] = line

    whisper = lines["stt-whisper-1"]
    assert whisper["result"] == "fail" and lines["stt-whisper-2"]["result"] == "incomplete"
    metrics = whisper["metrics"]
    assert metrics["runner_load_ms"] == 6409.7 and metrics["deploy_wall_ms"] == 16583
    assert metrics["memory.candidate_warm_idle.device_vram.python_sidecar_peak_mib"] == 2138.0
    assert metrics["memory.candidate_load.device_vram.python_sidecar_peak_mib"] == 2348.0
    assert metrics["memory.candidate_load.guest_ram.python_sidecar_peak_mib"] == 642.5
    assert round(metrics["fixture.en-clean-16k-01.rtf_median"], 3) == 0.034
    assert round(metrics["fixture.ar-clean-16k-01.rtf_median"], 3) == 0.029
    assert metrics["fixture.en-clean-16k-01.exchange_wall_median_ms"] == 197.1
    assert not [key for key in metrics if key.startswith("memory.after_teardown.")]
    checks = whisper["checks"]
    assert checks["negative.oom_at_load"] == "mismatched (timeout)"
    assert checks["negative.cancel_during_request"] == "recorded (unsupported)"
    assert checks["negative.corrupt_artifact_digest"] == "matched (load_failed)"
    assert checks["teardown"] == "failed (rollback)" and checks["fixtures"] == "all ok"
    assert checks["status_after_negatives"] == "failed: agent_state_as_expected"
    assert checks["memory.after_teardown"] == "not_run" and checks["deploy"] == "ok"
    assert lines["stt-whisper-2"]["checks"]["negative.oom_at_load"] == "not_run"

    kokoro = [lines["tts-kokoro-1"]["metrics"], lines["tts-kokoro-2"]["metrics"]]
    assert [m["runner_load_ms"] for m in kokoro] == [8838.3, 12650.4]
    assert [m["deploy_wall_ms"] for m in kokoro] == [11122, 14866]
    assert kokoro[0]["memory.candidate_warm_idle.device_vram.python_sidecar_peak_mib"] == 552.0
    assert kokoro[0]["memory.candidate_load.device_vram.python_sidecar_peak_mib"] == 1152.0
    assert lines["tts-kokoro-1"]["checks"]["negative.unsupported_voice"] == "mismatched (timeout)"
    return lines


def check_derivation_guards() -> None:
    report = load(LIFECYCLE_REPORT)
    refused(lambda: index_mod.lifecycle_line(report, envelope(row="other-row")), "is for row")
    refused(lambda: index_mod.lifecycle_line(report, envelope(kind="benchmark")), "/kind")

    record = load(record_path("whisper", 2))
    broken = copy.deepcopy(record)
    del broken["negatives"]
    refused(lambda: index_mod.qualification_line(broken, envelope(), "stt-whisper"), "malformed")

    broken = copy.deepcopy(record)
    broken["fixtures"][1]["samples"][0]["runner_timings_us"]["load"] += 1
    refused(lambda: index_mod.qualification_line(broken, envelope(), "stt-whisper"), "2 runner")

    broken = copy.deepcopy(record)
    domain = broken["memory"]["windows"]["candidate_load"]["domains"]["guest_ram"]
    domain["processes"][1]["role"] = domain["processes"][0]["role"]
    refused(lambda: index_mod.qualification_line(broken, envelope(), "stt-whisper"), "share")

    # A window that did not complete yields no number, and the comparison says so.
    whole = index_mod.qualification_line(record, envelope(), "stt-whisper")
    partial_record = copy.deepcopy(record)
    partial_record["memory"]["windows"]["candidate_load"]["status"] = "incomplete"
    partial = index_mod.qualification_line(
        partial_record, envelope(run_suffix="again"), "stt-whisper"
    )
    assert not [key for key in partial["metrics"] if key.startswith("memory.candidate_load.")]
    findings = index_mod.compare_lines(whole, partial)
    key = "metrics.memory.candidate_load.device_vram.python_sidecar_peak_mib"
    assert statuses(findings)[key] == "missing"
    assert index_mod.verdict(findings) == index_mod.EXIT_NOT_COMPARABLE

    failed_deploy = copy.deepcopy(record)
    failed_deploy["lifecycle"]["deploy"]["status"] = "failed"
    line = index_mod.qualification_line(failed_deploy, envelope(), "stt-whisper")
    assert "deploy_wall_ms" not in line["metrics"] and line["checks"]["deploy"] == "failed"


def check_doctor() -> None:
    line = index_mod.doctor_line(synthetic_doctor(), envelope(), False, REQUIRED)
    assert line["result"] == "pass" and line["subject"] == "doctor"
    assert line["metrics"] == {"findings_total": 3, "failing": 0}
    assert line["checks"]["interpreter_override"] == "absent"
    assert line["checks"]["finding.runner_profiles"] == "ok"

    # A warning passes `failing == 0`; the required list is what refuses it.
    warned = index_mod.doctor_line(
        synthetic_doctor(runner_profiles="warning"), envelope(), False, REQUIRED
    )
    assert warned["metrics"]["failing"] == 0 and warned["result"] == "fail"
    assert index_mod.doctor_line(synthetic_doctor(), envelope(), True, REQUIRED)["result"] == "fail"
    assert (
        index_mod.doctor_line(synthetic_doctor(host_os="fail"), envelope(), False)["result"]
        == "fail"
    )

    payload = synthetic_doctor()
    payload["findings"] = [f for f in payload["findings"] if f["id"] != "runner_profiles"]
    absent = index_mod.doctor_line(payload, envelope(), False, REQUIRED)
    assert absent["checks"]["finding.runner_profiles"] == "absent" and absent["result"] == "fail"

    payload = synthetic_doctor()
    payload["findings"].append(dict(payload["findings"][0]))
    refused(lambda: index_mod.doctor_line(payload, envelope(), False), "twice")


def check_comparison(lines: dict[str, dict]) -> None:
    findings = index_mod.compare_lines(lines["tts-kokoro-1"], lines["tts-kokoro-2"])
    by_field = statuses(findings)
    assert by_field["metrics.runner_load_ms"] == "regressed"
    assert by_field["metrics.deploy_wall_ms"] == "regressed"
    assert by_field["metrics.fixture.en-US-short-01.rtf_median"] == "unchanged"
    assert by_field["checks.teardown"] == "changed"
    assert by_field["checks.negative.oom_at_load"] == "changed"
    assert by_field["metrics.memory.after_teardown.guest_ram.consumed_max_mib"] == "missing"
    assert by_field["result"] == "unchanged"
    assert index_mod.verdict(findings) == index_mod.EXIT_NOT_COMPARABLE

    findings = index_mod.compare_lines(lines["stt-whisper-1"], lines["stt-whisper-2"])
    by_field = statuses(findings)
    assert by_field["result"] == "changed" and by_field["metrics.runner_load_ms"] == "unchanged"
    assert by_field["metrics.memory.after_teardown.guest_ram.consumed_max_mib"] == "new"
    assert "missing" not in by_field.values()
    assert index_mod.verdict(findings) == index_mod.EXIT_MOVED

    base = lines["stt-whisper-2"]

    def moved(change, tolerance: float = index_mod.DEFAULT_TOLERANCE_PCT) -> tuple[int, dict]:
        other = copy.deepcopy(base)
        change(other)
        found = index_mod.compare_lines(base, other, tolerance)
        return index_mod.verdict(found), statuses(found)

    code, by_field = moved(lambda line: None)
    assert code == index_mod.EXIT_UNCHANGED and set(by_field.values()) == {"unchanged"}

    code, by_field = moved(lambda line: line["metrics"].pop("runner_load_ms"))
    assert code == index_mod.EXIT_NOT_COMPARABLE and by_field["metrics.runner_load_ms"] == "missing"
    code, by_field = moved(lambda line: line["checks"].pop("teardown"))
    assert code == index_mod.EXIT_NOT_COMPARABLE and by_field["checks.teardown"] == "missing"

    def scale(factor: float):
        return lambda line: line["metrics"].__setitem__(
            "runner_load_ms", base["metrics"]["runner_load_ms"] * factor
        )

    assert moved(scale(1.09))[0] == index_mod.EXIT_UNCHANGED
    code, by_field = moved(scale(1.11))
    assert code == index_mod.EXIT_MOVED and by_field["metrics.runner_load_ms"] == "regressed"
    # A value that drops by half moved too: a model that left the GPU "improves" its VRAM.
    code, by_field = moved(scale(0.5))
    assert code == index_mod.EXIT_MOVED and by_field["metrics.runner_load_ms"] == "improved"
    assert moved(scale(1.09), tolerance=5.0)[0] == index_mod.EXIT_MOVED

    code, by_field = moved(lambda line: line["checks"].__setitem__("teardown", "ok (none)"))
    assert code == index_mod.EXIT_MOVED and by_field["checks.teardown"] == "changed"
    code, by_field = moved(lambda line: line["settings"].__setitem__("windows_s", "30"))
    assert code == index_mod.EXIT_UNCHANGED and by_field["settings.windows_s"] == "note"

    # A count is not a cost: one stage fewer is a change at any tolerance.
    fewer = copy.deepcopy(lines["lifecycle"])
    fewer["metrics"]["stages_passed"] = 7
    found = index_mod.compare_lines(lines["lifecycle"], fewer, 50.0)
    assert statuses(found)["metrics.stages_passed"] == "changed"
    assert index_mod.verdict(found) == index_mod.EXIT_MOVED

    refused(lambda: index_mod.compare_lines(lines["lifecycle"], base), "subject differs")
    elsewhere = copy.deepcopy(base)
    elsewhere["row"] = "another-row"
    refused(lambda: index_mod.compare_lines(base, elsewhere), "row differs")


def check_index_file_and_cli(lines: dict[str, dict]) -> None:
    schema = index_mod.load_schema()
    line = lines["stt-whisper-1"]
    # The in-tree validator and the reference library refuse the same malformed lines.
    for mutate in (
        lambda v: v["metrics"].__setitem__("runner_load_ms", [6.41, 6.42]),
        lambda v: v["metrics"].__setitem__("runner_load_ms", None),
        lambda v: v["metrics"].__setitem__("runner_load_ms", "6.41"),
        lambda v: v["checks"].__setitem__("teardown", ""),
        lambda v: v.__setitem__("kind", "benchmark"),
        lambda v: v.__setitem__("raw", "uploaded"),
        lambda v: v.__setitem__("raw", {"object": "archive.tgz", "sha256": "short"}),
        lambda v: v.__setitem__("extra", 1),
        lambda v: v.pop("settings"),
        lambda v: v.__setitem__("run_id", "whisper run 1"),
        lambda v: v.__setitem__("date", "1 Oct"),
        lambda v: v.__setitem__("row", "Bad Row"),
        lambda v: v.__setitem__("result", 3),
        lambda v: v.pop("notes"),
    ):
        broken = copy.deepcopy(line)
        mutate(broken)
        for validate in (
            lambda v: jsonschema.validate(v, schema),
            lambda v: index_mod.validate_line(v, schema),
        ):
            try:
                validate(broken)
            except (jsonschema.ValidationError, index_mod.IndexLineError):
                continue
            raise AssertionError(f"a malformed line passed validation: {broken!r:.200}")
    stored = copy.deepcopy(line)
    stored["raw"] = {"object": "archive.tgz", "sha256": "0" * 64, "bytes": 1}
    jsonschema.validate(stored, schema)
    index_mod.validate_line(stored, schema)

    with tempfile.TemporaryDirectory() as raw_tmp:
        tmp = Path(raw_tmp)
        first = write_index(
            tmp / "first.jsonl",
            [lines["lifecycle"], lines["stt-whisper-1"], lines["tts-kokoro-1"]],
        )
        second = write_index(tmp / "second.jsonl", [lines["stt-whisper-2"], lines["tts-kokoro-2"]])
        assert [entry["subject"] for entry in index_mod.read_index(first)] == [
            "lifecycle",
            "stt-whisper",
            "tts-kokoro",
        ]

        completed = run_tool("compare", "--previous", str(first), "--current", str(second))
        assert completed.returncode == 2, completed
        assert "regressed  metrics.runner_load_ms: 8838.3 -> 12650.4 (+43.1%)" in completed.stdout
        assert "== lifecycle: missing; the previous session" in completed.stdout
        assert "'failed (rollback)' -> 'ok (rollback)'" in completed.stdout

        whisper_only = write_index(tmp / "whisper-1.jsonl", [lines["stt-whisper-1"]])
        whisper_next = write_index(tmp / "whisper-2.jsonl", [lines["stt-whisper-2"]])
        completed = run_tool(
            "compare", "--previous", str(whisper_only), "--current", str(whisper_next)
        )
        assert completed.returncode == 1, completed

        again = copy.deepcopy(lines["stt-whisper-2"])
        again["run_id"] += "-again"
        same = write_index(tmp / "same.jsonl", [again])
        completed = run_tool("compare", "--previous", str(whisper_next), "--current", str(same))
        assert completed.returncode == 0 and "unchanged: 53 of 53" in completed.stdout, completed

        # The newest earlier line is the one compared with, and only the latest
        # session's subjects are owed.
        older = copy.deepcopy(lines["lifecycle"])
        older.update(
            run_id=f"2026-09-27-{ROW}-cuda-baseline",
            date="2026-09-27",
            session="an earlier session",
            subject="cuda-baseline",
        )
        elsewhere = copy.deepcopy(lines["lifecycle"])
        elsewhere.update(run_id="2026-10-01-another-row-lifecycle", row="another-row")
        history = write_index(
            tmp / "history.jsonl",
            [older, lines["stt-whisper-1"], lines["stt-whisper-2"], elsewhere],
        )
        completed = run_tool("compare", "--previous", str(history), "--current", str(same))
        assert completed.returncode == 0 and "stt-whisper-run-2 ->" in completed.stdout, completed
        # An index that already holds the pass's own line does not compare it with itself.
        completed = run_tool("compare", "--previous", str(history), "--current", str(whisper_next))
        assert completed.returncode == 1 and "stt-whisper-run-1 ->" in completed.stdout, completed

        # The worst subject decides, whichever order the lines are in.
        kokoro_again = copy.deepcopy(lines["tts-kokoro-1"])
        kokoro_again["run_id"] += "-again"
        both = write_index(tmp / "both.jsonl", [lines["stt-whisper-1"], lines["tts-kokoro-1"]])
        mixed = write_index(tmp / "mixed.jsonl", [lines["stt-whisper-2"], kokoro_again])
        completed = run_tool("compare", "--previous", str(both), "--current", str(mixed))
        assert completed.returncode == 1, completed

        # A subject nothing earlier ran is refused unless the operator says it is new.
        kokoro_only = write_index(tmp / "kokoro.jsonl", [lines["tts-kokoro-1"]])
        arguments = ("compare", "--previous", str(whisper_only), "--current", str(kokoro_only))
        completed = run_tool(*arguments)
        assert completed.returncode == 2 and "no earlier line" in completed.stdout, completed
        completed = run_tool(*arguments, "--new-subject", "tts-kokoro")
        assert completed.returncode == 2 and "stt-whisper: missing" in completed.stdout, completed
        empty = tmp / "empty.jsonl"
        empty.write_text("", encoding="utf-8")
        arguments = ("compare", "--previous", str(empty), "--current", str(kokoro_only))
        completed = run_tool(*arguments)
        assert completed.returncode == 2 and "no earlier line" in completed.stdout, completed
        assert run_tool(*arguments, "--new-subject", "tts-kokoro").returncode == 0
        assert run_tool(*arguments, "--new-subject", "stt-whisper").returncode == 2
        completed = run_tool("compare", "--previous", str(first), "--current", str(empty))
        assert completed.returncode == 2 and "holds no line" in completed.stderr, completed

        legacy = copy.deepcopy(lines["tts-kokoro-1"])
        legacy["metrics"] = {"runner_load_s": [8.84, 12.65]}
        bad = tmp / "bad.jsonl"
        bad.write_text(
            index_mod.dump_line(lines["lifecycle"]) + index_mod.dump_line(legacy), encoding="utf-8"
        )
        refused(lambda: index_mod.read_index(bad), "bad.jsonl:2")
        completed = run_tool("compare", "--previous", str(bad), "--current", str(second))
        assert completed.returncode == 2 and "bad.jsonl:2" in completed.stderr, completed
        write_index(bad, [lines["lifecycle"], lines["lifecycle"]])
        refused(lambda: index_mod.read_index(bad), "appears twice")

        out = tmp / "derived.jsonl"
        envelope_arguments = (
            "--date", "2026-10-01", "--session", "recorded runs of 2026-10-01", "--row", ROW,
            "--machine", "g2-standard-8, one NVIDIA L4", "--build", "v0.3.1-rc.1",
            "--kind", "evidence", "--records", "docs/validation/evidence",
            "--setting", "startup_timeout_ms=120000",
        )  # fmt: skip
        completed = run_tool(
            "index", *envelope_arguments, "--run-suffix", "run-1", "--family-build", "ad792785",
            "--lifecycle-report", str(LIFECYCLE_REPORT),
            "--qualification-record", f"stt-whisper={record_path('whisper', 1)}",
            "--out", str(out),
        )  # fmt: skip
        assert completed.returncode == 0, completed
        derived = index_mod.read_index(out)
        assert derived[1] == lines["stt-whisper-1"], "the command and the library disagree"
        assert derived[0]["metrics"] == lines["lifecycle"]["metrics"]

        doctor = tmp / "doctor.json"
        doctor.write_text(json.dumps({"payload": synthetic_doctor()}), encoding="utf-8")
        completed = run_tool("index", *envelope_arguments, "--doctor", str(doctor))
        assert completed.returncode == 2 and "--interpreter-override" in completed.stderr
        completed = run_tool(
            "index",
            *envelope_arguments,
            "--doctor",
            str(doctor),
            "--interpreter-override",
            "absent",
        )
        assert completed.returncode == 0, completed
        doctor_line = json.loads(completed.stdout)
        assert doctor_line["checks"]["finding.runner_launch_environment"] == "absent"
        assert doctor_line["result"] == "fail"

        for bad_arguments, needle in (
            (("--setting", "no-separator"), "KEY=VALUE"),
            (("--raw-object", "archive.tgz"), "go together"),
            (("--lifecycle-report", str(tmp / "absent.json")), "FileNotFoundError"),
            ((), "nothing to index"),
        ):
            completed = run_tool("index", *envelope_arguments, *bad_arguments)
            assert completed.returncode == 2 and needle in completed.stderr, completed


FIVE_OK = dict.fromkeys(
    (
        "platform_row",
        "python_pytorch_runtime",
        "runner_profiles",
        "runner_profile_dependencies",
        "runner_launch_environment",
    ),
    "ok",
)


def whisper_line(change, subject: str = "stt-whisper") -> dict:
    record = load(record_path("whisper", 2))
    change(record)
    return index_mod.qualification_line(record, envelope(), subject)


def check_more_guards(lines: dict[str, dict]) -> None:
    assert lines["stt-whisper-1"]["checks"]["bundle"] == "stt-whisper-candidate"
    refused(lambda: whisper_line(lambda r: r.__setitem__("provenance", "synthetic")), "nothing")

    # A null maximum is no measurement, not zero.
    def null_maxima(record: dict) -> None:
        domain = record["memory"]["windows"]["candidate_load"]["domains"]["device_vram"]
        domain["consumed"]["max_bytes"] = None
        domain["processes"][0]["peak"]["max_bytes"] = None

    metrics = whisper_line(null_maxima)["metrics"]
    assert "memory.candidate_load.device_vram.consumed_max_mib" not in metrics
    assert "memory.candidate_load.device_vram.python_sidecar_peak_mib" not in metrics
    assert "memory.candidate_load.guest_ram.consumed_max_mib" in metrics

    checks = whisper_line(lambda r: r["lifecycle"].__setitem__("status_after_deploy", None))[
        "checks"
    ]
    assert checks["status_after_deploy"] == "not taken"
    assert whisper_line(lambda r: r.__setitem__("fixtures", []))["checks"]["fixtures"] == "none ran"
    one_failed = whisper_line(lambda r: r["fixtures"][0]["summary"].__setitem__("failed_count", 1))
    assert one_failed["checks"]["fixtures"] == "1 of 5 had failed samples"

    refused(lambda: whisper_line(lambda r: r["negatives"].append(r["negatives"][0])), "twice")
    refused(lambda: whisper_line(lambda r: r["fixtures"].append(r["fixtures"][0])), "twice")

    def shared_role_with_null_peak(record: dict) -> None:
        domain = record["memory"]["windows"]["candidate_load"]["domains"]["guest_ram"]
        domain["processes"][0]["peak"]["max_bytes"] = None
        domain["processes"][1]["role"] = domain["processes"][0]["role"]

    refused(lambda: whisper_line(shared_role_with_null_peak), "share a role")

    # One candidate's record indexed as another's subject shows in the comparison.
    kokoro_as_whisper = index_mod.qualification_line(
        load(record_path("kokoro", 2)), envelope(run_suffix="mislabelled"), "stt-whisper"
    )
    found = statuses(index_mod.compare_lines(lines["stt-whisper-2"], kokoro_as_whisper))
    assert found["checks.bundle"] == "changed"

    report = load(LIFECYCLE_REPORT)
    failed = {**report, "outcome": "fail"}
    assert index_mod.lifecycle_line(failed, envelope())["result"] == "fail"
    skipped = copy.deepcopy(report)
    skipped["stages"][-1]["status"] = "skipped"
    line = index_mod.lifecycle_line(skipped, envelope())
    assert line["metrics"]["stages_passed"] == 7 and line["metrics"]["stages_total"] == 8
    assert line["checks"]["stage.rollback"] == "skipped"
    rebooted = {**report, "reboot": {"status": "pass"}}
    assert index_mod.lifecycle_line(rebooted, envelope())["checks"]["stage.reboot"] == "pass"
    twice = copy.deepcopy(report)
    twice["stages"].append(twice["stages"][0])
    refused(lambda: index_mod.lifecycle_line(twice, envelope()), "twice")
    fractional = {**report, "started_at": "2026-10-01T19:06:42.500+00:00"}
    assert index_mod.lifecycle_line(fractional, envelope())["metrics"]["harness_wall_s"] == 156

    # Doctor's total has to be the count of its own failing findings.
    payload = synthetic_doctor(host_os="fail")
    payload["failing"] = 0
    refused(lambda: index_mod.doctor_line(payload, envelope(), False), "its findings say 1")

    base = lines["stt-whisper-2"]
    for value in (float("nan"), float("inf")):
        broken = copy.deepcopy(base)
        broken["metrics"]["runner_load_ms"] = value
        refused(lambda broken=broken: index_mod.validate_line(broken), "finite")
        try:
            index_mod.dump_line(broken)
        except ValueError:
            pass
        else:
            raise AssertionError("a non-finite metric was written")
    for tolerance in (float("nan"), -5.0):
        refused(
            lambda tolerance=tolerance: index_mod.compare_lines(base, base, tolerance), "nothing"
        )

    def after(change, previous: dict = base, tolerance: float = 10.0) -> tuple[int, dict]:
        other = copy.deepcopy(previous)
        change(other)
        found = index_mod.compare_lines(previous, other, tolerance)
        return index_mod.verdict(found), statuses(found)

    vram = "memory.candidate_load.device_vram.python_sidecar_peak_mib"
    code, by_field = after(lambda line: line["metrics"].__setitem__(vram, 3000.0))
    assert code == index_mod.EXIT_MOVED and by_field[f"metrics.{vram}"] == "regressed"
    code, by_field = after(
        lambda line: line["metrics"].__setitem__("harness_wall_s", 170), lines["lifecycle"]
    )
    assert code == index_mod.EXIT_UNCHANGED and by_field["metrics.harness_wall_s"] == "unchanged"
    code, by_field = after(
        lambda line: line["metrics"].__setitem__("harness_wall_s", 300), lines["lifecycle"]
    )
    assert code == index_mod.EXIT_MOVED and by_field["metrics.harness_wall_s"] == "regressed"
    code, by_field = after(lambda line: line["checks"].__setitem__("negative.added", "matched"))
    assert code == index_mod.EXIT_UNCHANGED and by_field["checks.negative.added"] == "new"
    code, by_field = after(lambda line: line.__setitem__("build", "another build"))
    assert code == index_mod.EXIT_UNCHANGED and by_field["build"] == "note"

    older = copy.deepcopy(lines["stt-whisper-1"])
    older["run_id"] = "2026-09-01-older"
    own = {lines["stt-whisper-1"]["run_id"], lines["stt-whisper-2"]["run_id"]}
    index = [older, lines["stt-whisper-1"], lines["stt-whisper-2"]]
    for line in index[1:]:
        assert index_mod.select_previous(index, line, own) is older


def check_more_cli(lines: dict[str, dict]) -> None:
    with tempfile.TemporaryDirectory() as raw_tmp:
        tmp = Path(raw_tmp)
        # Both runs of a pass are compared with the session before, not with each other.
        older = copy.deepcopy(lines["stt-whisper-1"])
        older.update(run_id="2026-09-01-older", date="2026-09-01", session="an earlier session")
        older["metrics"]["runner_load_ms"] = 3000.0
        runs = [lines["stt-whisper-1"], lines["stt-whisper-2"]]
        history = write_index(tmp / "history.jsonl", [older, *runs])
        current = write_index(tmp / "current.jsonl", runs)
        completed = run_tool("compare", "--previous", str(history), "--current", str(current))
        assert completed.returncode == 1, completed
        assert completed.stdout.count("== stt-whisper: 2026-09-01-older -> ") == 2
        assert completed.stdout.count("regressed  metrics.runner_load_ms: 3000.0 -> ") == 2

        # The pass's own lines do not stand in for the session before it.
        baseline = copy.deepcopy(lines["lifecycle"])
        baseline.update(
            run_id="2026-09-01-baseline", session="an earlier session", subject="baseline"
        )
        history = write_index(tmp / "own.jsonl", [baseline, lines["stt-whisper-2"]])
        current = write_index(tmp / "own-current.jsonl", [lines["stt-whisper-2"]])
        completed = run_tool("compare", "--previous", str(history), "--current", str(current))
        assert completed.returncode == 2 and "== baseline: missing" in completed.stdout, completed

        first = write_index(tmp / "first.jsonl", [lines["tts-kokoro-1"]])
        second = write_index(tmp / "second.jsonl", [lines["tts-kokoro-2"]])
        arguments = ("compare", "--previous", str(first), "--current", str(second))
        regressed = "regressed  metrics.runner_load_ms"
        assert regressed in run_tool(*arguments).stdout
        assert regressed not in run_tool(*arguments, "--tolerance-pct", "50").stdout
        completed = run_tool(*arguments, "--tolerance-pct", "nan")
        assert completed.returncode == 2 and "compares nothing" in completed.stderr, completed

        # A fault is no verdict: it must not exit 1, which means "something moved".
        binary = tmp / "binary.jsonl"
        binary.write_bytes(b"\xff\xfe\x00")
        for bad in (binary, tmp / "absent.jsonl"):
            completed = run_tool("compare", "--previous", str(bad), "--current", str(second))
            assert completed.returncode == 2 and "validation-pass:" in completed.stderr, completed

        envelope_arguments = (
            "--date", "2026-10-01", "--session", "recorded runs of 2026-10-01", "--row", ROW,
            "--machine", "g2-standard-8, one NVIDIA L4", "--build", "v0.3.1-rc.1",
        )  # fmt: skip

        def doctor_result(payload: object, *extra: str) -> subprocess.CompletedProcess:
            path = tmp / "doctor.json"
            path.write_text(json.dumps(payload), encoding="utf-8")
            return run_tool("index", *envelope_arguments, "--doctor", str(path), *extra)

        absent = ("--interpreter-override", "absent")
        completed = doctor_result({"payload": synthetic_doctor(**FIVE_OK)}, *absent)
        assert completed.returncode == 0 and json.loads(completed.stdout)["result"] == "pass"
        for finding_id in FIVE_OK:
            payload = synthetic_doctor(**{**FIVE_OK, finding_id: "warning"})
            line = json.loads(doctor_result({"payload": payload}, *absent).stdout)
            assert line["result"] == "fail" and line["metrics"]["failing"] == 0, finding_id
        completed = doctor_result(
            {"payload": synthetic_doctor(**FIVE_OK)}, "--interpreter-override", "present"
        )
        line = json.loads(completed.stdout)
        assert line["result"] == "fail" and line["checks"]["interpreter_override"] == "present"
        for malformed in ({"payload": None}, {}, [], {"payload": {"failing": 0, "findings": None}}):
            completed = doctor_result(malformed, *absent)
            assert completed.returncode == 2 and "validation-pass:" in completed.stderr, completed
        (tmp / "doctor.json").write_text("{", encoding="utf-8")
        completed = run_tool(
            "index", *envelope_arguments, "--doctor", str(tmp / "doctor.json"), *absent
        )
        assert completed.returncode == 2 and "JSONDecodeError" in completed.stderr, completed

        record = f"stt-whisper={record_path('whisper', 2)}"
        completed = run_tool(
            "index", *envelope_arguments, "--qualification-record", record,
            "--notes", "a note", "--raw-object", "archive.tgz", "--raw-sha256", "0" * 64,
            "--setting", "startup_timeout_ms=120000", "--setting", "ld_library_path=family",
        )  # fmt: skip
        line = json.loads(completed.stdout)
        assert line["notes"] == "a note" and line["kind"] == "validation"
        assert line["raw"] == {"object": "archive.tgz", "sha256": "0" * 64}
        assert line["settings"] == {"startup_timeout_ms": 120000, "ld_library_path": "family"}
        for extra, needle in (
            (("--setting", "a=1", "--setting", "a=2"), "given twice"),
            (("--qualification-record", record), "share a run id"),
        ):
            completed = run_tool(
                "index", *envelope_arguments, "--qualification-record", record, *extra
            )
            assert completed.returncode == 2 and needle in completed.stderr, completed


def main() -> int:
    lines = check_derivation()
    check_derivation_guards()
    check_doctor()
    check_comparison(lines)
    check_index_file_and_cli(lines)
    check_more_guards(lines)
    check_more_cli(lines)
    print("validation pass: recorded runs index, compare and fail closed as expected")
    return 0


if __name__ == "__main__":
    sys.exit(main())
