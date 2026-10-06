#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Rolling validation pass: run one, index what it recorded and compare two passes.

  run      run a pass on this machine: the lifecycle harness, the candidate
           build and the speech runtime family, doctor, each candidate through
           the qualification recipe, then index what ran
  index    derive index lines from a lifecycle report, qualification records and
           doctor's JSON output
  compare  report what moved between the newest earlier lines of an index and a
           pass's lines; exit 0 nothing moved, 1 something moved, 2 not comparable

docs/validation/rolling-validation.md describes a pass, the index and the comparison.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import math
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
import validation_index as index_mod  # noqa: E402
import validation_pass_run as run_mod  # noqa: E402

SPEECH_FAMILY_HELP = (
    "how the speech runtime family reached the machine; it decides what doctor must report"
)


def add_envelope_arguments(parser: argparse.ArgumentParser, a_pass: bool = False) -> None:
    """`a_pass` is always a validation run of today whose raw set is still on the machine."""
    if a_pass:
        today = f"{_dt.datetime.now(tz=_dt.timezone.utc):%Y-%m-%d}"
        parser.add_argument("--date", default=today, help="the session's day; today (UTC)")
        parser.set_defaults(kind="validation", raw_object=None, raw_sha256=None)
    else:
        parser.add_argument("--date", required=True, help="the session's day, YYYY-MM-DD")
        parser.add_argument("--kind", choices=("validation", "evidence"), default="validation")
        parser.add_argument("--raw-object", help="the raw archive's object name, once stored")
        parser.add_argument("--raw-sha256", help="the raw archive's sha256, with --raw-object")
    parser.add_argument("--session", required=True, help="the hardware session's name")
    parser.add_argument("--row", required=True, help="the platform row id")
    parser.add_argument("--machine", required=True, help="the machine type, in words")
    parser.add_argument("--build", required=True, help="the tag or commit installed")
    parser.add_argument("--family-build", help="the commit a host-built speech runtime came from")
    parser.add_argument("--records", default="not filed", help="where the record files are kept")
    parser.add_argument("--notes", default="")
    parser.add_argument("--run-suffix", help="appended to run ids when a subject runs twice")
    parser.add_argument(
        "--setting",
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help="a setting that makes the run differ from a default install; repeatable",
    )


def setting_value(text: str) -> int | float | str:
    """A number stays a number, so a setting compares equal to the one an index holds."""
    for parse in (int, float):
        try:
            value = parse(text)
        except ValueError:
            continue
        if math.isfinite(value):
            return value
    return text


def split_pair(text: str, flag: str) -> tuple[str, str]:
    key, separator, value = text.partition("=")
    if not separator or not key or not value:
        raise index_mod.IndexLineError(f"{flag} takes KEY=VALUE, not {text!r}")
    return key, value


def envelope_from(args: argparse.Namespace) -> dict[str, Any]:
    if bool(args.raw_object) != bool(args.raw_sha256):
        raise index_mod.IndexLineError("--raw-object and --raw-sha256 go together")
    raw: Any = "local-only"
    if args.raw_object:
        raw = {"object": args.raw_object, "sha256": args.raw_sha256}
    settings: dict[str, Any] = {}
    for item in args.setting:
        key, value = split_pair(item, "--setting")
        if key in settings:
            raise index_mod.IndexLineError(f"--setting {key} is given twice")
        settings[key] = setting_value(value)
    return {
        "date": args.date,
        "session": args.session,
        "row": args.row,
        "machine": args.machine,
        "build": args.build,
        "kind": args.kind,
        "records": args.records,
        "raw": raw,
        "notes": args.notes,
        "run_suffix": args.run_suffix,
        "settings": settings,
    }


def load_json(path: str) -> Any:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def derive_lines(
    envelope: dict[str, Any],
    family_build: str | None,
    lifecycle_report: str | None,
    qualification_records: list[tuple[str, str]],
    doctor: str | None,
    interpreter_override: bool,
    speech_family: str | None,
    cold_deploys: dict[str, str],
) -> list[dict[str, Any]]:
    lines = []
    if lifecycle_report:
        lines.append(index_mod.lifecycle_line(load_json(lifecycle_report), envelope))
    if doctor:
        payload = load_json(doctor)["payload"]
        required = index_mod.DOCTOR_REQUIREMENTS[index_mod.SPEECH_FAMILY_MODES[speech_family]]
        lines.append(index_mod.doctor_line(payload, envelope, interpreter_override, required))
    unused = set(cold_deploys) - {subject for subject, _ in qualification_records}
    if unused:
        raise index_mod.IndexLineError(f"--cold-deploy names no indexed record: {sorted(unused)}")
    for subject, path in qualification_records:
        cold = load_json(cold_deploys[subject]) if subject in cold_deploys else None
        lines.append(
            index_mod.qualification_line(load_json(path), envelope, subject, family_build, cold)
        )
    if not lines:
        raise index_mod.IndexLineError("nothing to index: no report, record or doctor output")
    run_ids = [line["run_id"] for line in lines]
    if len(set(run_ids)) != len(run_ids):
        raise index_mod.IndexLineError(
            "two lines share a run id: index one run of a subject at a time"
        )
    return lines


def command_index(args: argparse.Namespace) -> int:
    if args.doctor and (args.interpreter_override is None or args.speech_family is None):
        raise index_mod.IndexLineError(
            "--doctor needs --interpreter-override present|absent and --speech-family"
        )
    cold_deploys = [split_pair(item, "--cold-deploy") for item in args.cold_deploy]
    if len(dict(cold_deploys)) != len(cold_deploys):
        raise index_mod.IndexLineError("--cold-deploy names one subject twice")
    lines = derive_lines(
        envelope_from(args),
        args.family_build,
        args.lifecycle_report,
        [split_pair(item, "--qualification-record") for item in args.qualification_record],
        args.doctor,
        args.interpreter_override == "present",
        args.speech_family,
        dict(cold_deploys),
    )
    text = "".join(index_mod.dump_line(line) for line in lines)
    if args.out:
        Path(args.out).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


def describe(finding: dict[str, Any]) -> str:
    before, after = finding["previous"], finding["current"]
    text = f"  {finding['status']:<10} {finding['field']}: {before!r} -> {after!r}"
    numeric = all(isinstance(v, (int, float)) and not isinstance(v, bool) for v in (before, after))
    if numeric and before:
        text += f" ({(after - before) / abs(before) * 100:+.1f}%)"
    return text


def command_compare(args: argparse.Namespace) -> int:
    previous = index_mod.read_index(Path(args.previous))
    current = index_mod.read_index(Path(args.current))
    if not current:
        raise index_mod.IndexLineError(f"{args.current} holds no line")
    worst = index_mod.EXIT_UNCHANGED
    current_ids = {line["run_id"] for line in current}
    for line in current:
        earlier = index_mod.select_previous(previous, line, current_ids)
        if earlier is None:
            print(f"== {line['subject']} ({line['run_id']}): no earlier line on {line['row']}")
            if line["subject"] not in args.new_subject:
                worst = index_mod.EXIT_NOT_COMPARABLE
            continue
        findings = index_mod.compare_lines(earlier, line, args.tolerance_pct)
        print(f"== {line['subject']}: {earlier['run_id']} -> {line['run_id']}")
        for finding in findings:
            if finding["status"] != "unchanged":
                print(describe(finding))
        unchanged = sum(1 for finding in findings if finding["status"] == "unchanged")
        print(f"  unchanged: {unchanged} of {len(findings)} fields")
        worst = max(worst, index_mod.verdict(findings))
    for row, subject in index_mod.missing_subjects(previous, current):
        print(f"== {subject}: missing; the previous session on {row} ran it")
        worst = index_mod.EXIT_NOT_COMPARABLE
    return worst


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(dest="command", required=True)

    run = commands.add_parser("run", help="run a pass on this machine")
    run_mod.add_arguments(run)
    run.add_argument(
        "--speech-family",
        required=True,
        choices=sorted(index_mod.SPEECH_FAMILY_MODES),
        help=SPEECH_FAMILY_HELP,
    )
    add_envelope_arguments(run, a_pass=True)
    run.set_defaults(run=lambda args: run_mod.command_run(args, envelope_from(args)))

    index = commands.add_parser("index", help="derive index lines from recorded files")
    add_envelope_arguments(index)
    index.add_argument("--lifecycle-report", help="a lifecycle-report.json")
    index.add_argument(
        "--qualification-record",
        action="append",
        default=[],
        metavar="SUBJECT=PATH",
        help="a candidate qualification record.json and the subject it is indexed as",
    )
    index.add_argument("--doctor", help="the output of `tensorplate doctor --output json`")
    index.add_argument(
        "--interpreter-override",
        choices=("present", "absent"),
        help="whether the agent's environment file names an interpreter for the backend",
    )
    index.add_argument(
        "--speech-family", choices=sorted(index_mod.SPEECH_FAMILY_MODES), help=SPEECH_FAMILY_HELP
    )
    index.add_argument(
        "--cold-deploy",
        action="append",
        default=[],
        metavar="SUBJECT=PATH",
        help="the pass's cold-deploy.json for a subject indexed with --qualification-record",
    )
    index.add_argument("--out", help="write the lines here instead of standard output")
    index.set_defaults(run=command_index)

    compare = commands.add_parser("compare", help="report what moved since the previous pass")
    compare.add_argument("--previous", required=True, help="the index before this pass")
    compare.add_argument("--current", required=True, help="this pass's lines")
    compare.add_argument("--tolerance-pct", type=float, default=index_mod.DEFAULT_TOLERANCE_PCT)
    compare.add_argument(
        "--new-subject",
        action="append",
        default=[],
        metavar="SUBJECT",
        help="a subject this pass runs for the first time; any other with no earlier line is refused",
    )
    compare.set_defaults(run=command_compare)
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    # Any fault is "no verdict": a traceback would exit 1, which means "something moved".
    try:
        return args.run(args)
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"validation-pass: {type(exc).__name__}: {exc}\n")
        return index_mod.EXIT_NOT_COMPARABLE


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
