# SPDX-License-Identifier: Apache-2.0
"""One rolling validation pass on the machine this runs on.

Steps, in order: preflight, lifecycle, install, speech-family,
agent-environment, doctor, then provision, cold-deploy and qualify per
candidate, streaming-latency, index.
Each step's commands, exit code and log are recorded in `pass-report.json`,
which is rewritten when a step starts and when it ends. A step whose
prerequisite did not end `ok` is `not_run`. The pass creates, starts and deletes no machine and names
none. docs/validation/rolling-validation.md describes a pass.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import wave
from collections.abc import Callable
from pathlib import Path
from typing import Any

import candidate_record
import validation_index as index_mod

CONFIRM_TOKEN = "RESET-TENSORPLATE"
SERVICE_UNITS = ("tensorplate-agent", "tensorplate-observability")
AGENT_ENVIRONMENT_FILE = "/etc/default/tensorplate-agent"
# The agent starts a bundle that names no runner profile in the interpreter one of these names.
OWN_INTERPRETER_VARIABLES = ("TP_PYTHON_PYTORCH_EXECUTABLE", "TP_TEST_PYTHON_EXE", "TP_TEST_PYTHON")
SPEECH_BUILD_PROFILE = "pkg.tensorplate.speech-runtime"
# The family is built for this architecture only; a release set carries others too.
SPEECH_ARCHITECTURE = "amd64"
STEP_TIMEOUTS_S = {
    "preflight": 300,
    "lifecycle": 3600,
    "install": 1800,
    "speech-family": 7200,
    "agent-environment": 300,
    "doctor": 900,
    "provision": 3600,
    "cold-deploy": 900,
    "qualify": 3600,
}
AGENT_WAIT_S = 120
# Above the harness's own cleanup, which can wait three minutes for its units.
STOP_GRACE_S = 240
# What the agent's unit cannot read: it runs with ProtectHome and PrivateTmp.
AGENT_HIDDEN_ROOTS = ("/home", "/root", "/run/user", "/tmp", "/var/tmp")
STREAMING_LATENCY = "streaming-latency"
STREAM_ENDPOINTS = ("--stt-endpoint", "--tts-endpoint")
WORKER_STATEMENTS = ("--worker-version", "--worker-build")
# What the client takes when the option is absent; a test holds both to its parser.
CLIENT_DEFAULTS = {"--iterations": 1, "--event-timeout-ms": 30_000}
# The most waits one session makes for an event it needs: ready, the segment's
# completion, the finalize's answer and the call's end.
SESSION_WAITS = 4
# Beside the waits: the interpreter's start, generating the bindings and the GPU sampler.
STREAMING_MARGIN_S = 300
# A candidate's subject names a directory and an index subject beside these.
RESERVED_SUBJECTS = ("lifecycle", "doctor", "logs", STREAMING_LATENCY)
# Runner profiles whose load puts the entry's `device` to use.
DEVICE_PROFILES = ("cuda_fixture", "faster_whisper", "kokoro")
BALLAST_RULES = ("bytes", "headroom")
NO_BALLAST = "none"
BUNDLE_NAME = r"[A-Za-z0-9][A-Za-z0-9._-]*"
EXIT_OK, EXIT_STEP_FAILED = 0, 1


class StepFailed(Exception):
    def __init__(self, reason: str, exit_code: int | None = None) -> None:
        super().__init__(reason)
        self.exit_code = exit_code


def require_operator() -> None:
    if os.geteuid() == 0:
        raise StepFailed("run as a normal user: the pass calls sudo itself, as the harness does")


def manifest_commit(directory: Path) -> str:
    """The commit an artifact set's manifest says it was built from."""
    manifests = sorted(directory.glob("tensorplate-*-artifacts.json"))
    if len(manifests) != 1:
        raise StepFailed(f"{directory} holds {len(manifests)} tensorplate-*-artifacts.json")
    release = json.loads(manifests[0].read_text(encoding="utf-8")).get("release")
    commit = release.get("commit") if isinstance(release, dict) else None
    if not isinstance(commit, str):
        raise StepFailed(f"{manifests[0]} names no release commit")
    return commit


def read_result(path: Path, *keys: str) -> str | None:
    """The verdict a subject's own record carries, or None when it left none."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        for key in keys:
            value = value[key]
    except (OSError, ValueError, KeyError, TypeError):
        return None
    return value if isinstance(value, str) else None


class Unstoppable(StepFailed):
    """A command that outlived both signals: it may still be changing the machine."""


class StepTimeout(Exception):
    pass


class Interrupted(BaseException):
    """The pass itself was told to stop; not an Exception, so no step swallows it."""


def verify_checksums(directory: Path) -> int:
    """Every file `SHA256SUMS` lists is inside the directory with that digest."""
    sums = directory / "SHA256SUMS"
    try:
        lines = [line for line in sums.read_text(encoding="utf-8").splitlines() if line.strip()]
    except OSError as exc:
        raise StepFailed(f"{sums}: {exc.strerror}") from exc
    if not lines:
        raise StepFailed(f"{sums} lists no file")
    root = directory.resolve()
    for line in lines:
        digest, _, name = line.partition(" ")
        path = (root / name.strip().lstrip("*")).resolve()
        if root not in path.parents:
            raise StepFailed(f"{sums}: {line!r} does not name a file inside the set")
        try:
            actual = candidate_record.sha256_file(path)
        except OSError as exc:
            raise StepFailed(f"{path} is listed in SHA256SUMS and cannot be read") from exc
        if actual != f"sha256:{digest.lower()}":
            raise StepFailed(f"{path} does not match SHA256SUMS")
    return len(lines)


def environment_variables(environment_file: Path) -> dict[str, str]:
    """What the agent's environment file sets, read as the unit reads it."""
    try:
        text = environment_file.read_text(encoding="utf-8")
    except FileNotFoundError:
        return {}
    variables: dict[str, str] = {}
    for line in map(str.strip, text.splitlines()):
        name, separator, value = line.partition("=")
        if not separator:
            continue
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        variables[name.strip()] = value
    return variables


def interpreter_override(environment_file: Path) -> bool:
    variables = environment_variables(environment_file)
    return any(variables.get(name) for name in OWN_INTERPRETER_VARIABLES)


def cli_envelope(done: subprocess.CompletedProcess[str]) -> dict[str, Any] | None:
    """A failed deploy prints its envelope on stdout, a refused command on stderr."""
    for stream in (done.stdout, done.stderr):
        try:
            envelope = json.loads(stream)
        except ValueError:
            continue
        if isinstance(envelope, dict) and "status" in envelope:
            return envelope
    return None


def deploy_outcome(envelope: dict[str, Any]) -> dict[str, Any]:
    """What a `tensorplate deploy` envelope says, as the qualification recipe reads it."""
    payload = envelope.get("payload") if isinstance(envelope.get("payload"), dict) else {}
    failure = payload.get("failure") if isinstance(payload.get("failure"), dict) else None
    typed = None
    if failure is not None and isinstance(failure.get("error_code"), str):
        typed = {"code": failure["error_code"], "message": str(failure.get("message", ""))}
    elif envelope["status"] != "ok":
        error = envelope.get("error")
        code = error.get("code") if isinstance(error, dict) else None
        typed = {"code": code if isinstance(code, str) else "untyped", "message": ""}
    phase, digest = payload.get("phase"), payload.get("bundle_digest")
    # A status other than `ok` always leaves `typed` set.
    ok = phase == "active" and typed is None
    return {
        "status": "ok" if ok else "failed",
        "phase": phase if isinstance(phase, str) else None,
        "bundle_digest": digest if isinstance(digest, str) else None,
        "failure": typed,
    }


def claims(option: str, items: list[str]) -> bool:
    """Whether a pass-through argument names `option`, in full or as an argparse abbreviation."""
    return any(
        len(name) > 2 and option.startswith(name)
        for name in (item.partition("=")[0] for item in items)
    )


def claims_any(options: tuple[str, ...], items: list[str]) -> bool:
    return any(claims(option, items) for option in options)


def option_value(option: str, items: list[str]) -> str | None:
    """What pass-through arguments give `option`; as in argparse, the last one counts."""
    value = None
    for position, item in enumerate(items):
        name, separator, inline = item.partition("=")
        if claims(option, [name]):
            value = inline if separator else "".join(items[position + 1 : position + 2])
    return value


def client_count(option: str, items: list[str]) -> int:
    text = option_value(option, items)
    if text is None:
        return CLIENT_DEFAULTS[option]
    if re.fullmatch(r"[1-9][0-9]*", text) is None:
        raise index_mod.IndexLineError(
            f"--streaming-arg {option} takes a whole number of at least 1, not {text!r}"
        )
    return int(text)


def clip_seconds(items: list[str]) -> float:
    """How long the utterance plays, from its WAV header; 0 where the client sends none.

    A file that cannot be read counts 0 too: the client refuses it before any session.
    """
    path = option_value("--audio", items)
    if not path or not claims("--stt-endpoint", items):
        return 0.0
    try:
        with wave.open(path, "rb") as clip:
            return clip.getnframes() / clip.getframerate()
    except (OSError, EOFError, wave.Error, ZeroDivisionError):
        return 0.0


def streaming_limit_s(items: list[str]) -> int:
    """The streaming step's time limit: the client's waits and its audio, plus a margin.

    Per endpoint named, one wait for the channel and SESSION_WAITS per session, each
    at the event timeout; and the utterance once per session, sent in real time.
    """
    iterations = client_count("--iterations", items)
    endpoints = sum(1 for option in STREAM_ENDPOINTS if claims(option, items))
    waits = endpoints * (1 + iterations * SESSION_WAITS)
    waited_s = waits * client_count("--event-timeout-ms", items) / 1000
    return STREAMING_MARGIN_S + math.ceil(waited_s + iterations * clip_seconds(items))


def device_memory_bytes(text: str) -> int | None:
    """Device 0's memory from `nvidia-smi --format=csv,noheader,nounits`: one figure, in MiB."""
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    if len(lines) != 1 or re.fullmatch(r"[0-9]+", lines[0]) is None or int(lines[0]) == 0:
        return None
    return int(lines[0]) * 1024 * 1024


def predecessor_problem(bundle: Path) -> str | None:
    """Why a bundle cannot leave the sidecar holding device memory once deployed, or None."""
    try:
        manifest = json.loads((bundle / "manifest.json").read_text(encoding="utf-8"))
        entries = [a for a in manifest["artifacts"] if a.get("kind") == "python_pytorch_entry"]
        if len(entries) != 1:
            return f"its manifest lists {len(entries)} python_pytorch_entry artifacts, not one"
        path = (bundle / entries[0]["path"]).resolve()
        if bundle.resolve() not in path.parents:
            return "its entry lies outside the bundle"
        entry = json.loads(path.read_text(encoding="utf-8"))
        profile, device = entry.get("backend_profile"), entry.get("device")
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as exc:
        return f"it cannot be read as a bundle ({type(exc).__name__})"
    if device != "cuda" or profile not in DEVICE_PROFILES:
        return (
            f"its entry runs profile {profile!r} on device {device!r}, which holds no device memory"
        )
    return None


def parse_ballast(items: list[str], subjects: list[str]) -> dict[str, tuple[str, int]]:
    """Each named candidate's rule and count; `none` is the rule NO_BALLAST with no count."""
    rules: dict[str, tuple[str, int]] = {}
    for item in items:
        subject, _, rule = item.partition("=")
        kind, _, count = rule.partition(":")
        sized = kind in BALLAST_RULES and re.fullmatch(r"[1-9][0-9]*", count) is not None
        if not subject or not (sized or rule == NO_BALLAST):
            raise index_mod.IndexLineError(
                "--oom-ballast takes SUBJECT=bytes:N, SUBJECT=headroom:N or SUBJECT=none, "
                f"not {item!r}"
            )
        if subject not in subjects:
            raise index_mod.IndexLineError(
                f"--oom-ballast names {subject!r}, which no --candidate declares"
            )
        if subject in rules:
            raise index_mod.IndexLineError(f"--oom-ballast names {subject!r} twice")
        rules[subject] = (kind, int(count) if sized else 0)
    return rules


class Candidate:
    def __init__(self, spec: str) -> None:
        parts = spec.split(":", 2)
        if len(parts) < 2 or not all(parts):
            raise index_mod.IndexLineError(
                f"--candidate takes SUBJECT:BUNDLE[:CLIPS], not {spec!r}"
            )
        self.subject, self.bundle = parts[0], parts[1]
        rule = index_mod.load_schema()["properties"]["subject"]["pattern"]
        if self.subject in RESERVED_SUBJECTS or not re.fullmatch(rule.strip("^$"), self.subject):
            raise index_mod.IndexLineError(
                f"--candidate subject {self.subject!r} is reserved or does not match {rule}"
            )
        if not re.fullmatch(BUNDLE_NAME, self.bundle):
            raise index_mod.IndexLineError(f"--candidate bundle {self.bundle!r} is not a name")
        self.clips = Path(parts[2]).resolve() if len(parts) == 3 else None
        self.bundle_dir: Path | None = None


class Pass:
    def __init__(self, args: argparse.Namespace, envelope: dict[str, Any]) -> None:
        self.args = args
        self.envelope = envelope
        envelope["settings"].setdefault("speech_family", args.speech_family)
        self.repo = Path(args.repo_root).resolve()
        self.out = Path(args.out_dir).resolve()
        self.work = Path(args.work_dir).resolve() if args.work_dir else Path(f"{self.out}-work")
        self.assets = Path(args.assets_dir).resolve()
        self.baseline = (
            Path(args.baseline_assets_dir).resolve() if args.baseline_assets_dir else None
        )
        self.candidates = [Candidate(spec) for spec in args.candidate]
        self.staging = Path(args.staging_dir).resolve() if args.staging_dir else None
        bundle = args.predecessor_bundle
        self.predecessor = Path(bundle).resolve() if bundle else None
        if self.candidates and self.staging is None:
            raise index_mod.IndexLineError(
                "a candidate needs --staging-dir: the recipe writes bundles there for the agent"
            )
        self.hidden = [Path(root) for root in args.agent_hidden_root or AGENT_HIDDEN_ROOTS]
        subjects = [candidate.subject for candidate in self.candidates]
        if len(set(subjects)) != len(subjects):
            raise index_mod.IndexLineError("--candidate names one subject twice")
        if self.candidates and args.speech_family == "none":
            raise index_mod.IndexLineError(
                "a candidate needs a speech runtime family: --speech-family none installs none"
            )
        self.ballast_rules = parse_ballast(args.oom_ballast, subjects)
        sized = any(kind != NO_BALLAST for kind, _ in self.ballast_rules.values())
        if sized and not args.oom_ballast_python:
            raise index_mod.IndexLineError(
                "--oom-ballast needs --oom-ballast-python: the ballast is held by an "
                "interpreter that imports the runtime's torch, which the pass cannot choose"
            )
        for option in ("--oom-ballast-bytes", "--predecessor-bundle", "--sampler"):
            if claims(option, args.qualify_arg):
                raise index_mod.IndexLineError(
                    f"--qualify-arg may not set {option}: the pass does, from its own options"
                )
        self.streaming = claims_any(STREAM_ENDPOINTS, args.streaming_arg)
        # The pass's record is `recorded`: any other measured nothing and is not indexed.
        for option in ("--out", "--source-commit", "--provenance"):
            if claims(option, args.streaming_arg):
                raise index_mod.IndexLineError(f"--streaming-arg may not set {option}")
        if args.streaming_arg and not self.streaming:
            raise index_mod.IndexLineError(
                "--streaming-arg names no stream endpoint (--stt-endpoint or --tts-endpoint)"
            )
        stated = any(option_value(option, args.streaming_arg) for option in WORKER_STATEMENTS)
        if self.streaming and not stated:
            raise index_mod.IndexLineError(
                "--streaming-arg needs --worker-version or --worker-build with a value: "
                "a record that states neither is synthetic and cannot be indexed"
            )
        self.timeouts = {
            **STEP_TIMEOUTS_S,
            STREAMING_LATENCY: streaming_limit_s(args.streaming_arg),
        }
        for item in args.step_timeout:
            name, _, seconds = item.partition("=")
            if name not in self.timeouts or not seconds.isdigit() or int(seconds) == 0:
                raise index_mod.IndexLineError(f"--step-timeout takes STEP=SECONDS, not {item!r}")
            self.timeouts[name] = int(seconds)
        self.tested_version = args.tested_version or (
            (self.repo / "packaging/VERSION").read_text(encoding="utf-8").strip()
        )
        for item in args.agent_environment:
            # One plain line each: the file is read by systemd, not by a shell.
            if not re.fullmatch(r"[A-Z_][A-Z0-9_]*=[^\s\"'\\]+", item):
                raise index_mod.IndexLineError(
                    f"--agent-environment takes NAME=VALUE with no space or quote, not {item!r}"
                )
        if args.agent_environment:
            names = sorted(item.partition("=")[0] for item in args.agent_environment)
            envelope["settings"].setdefault("agent_environment", ",".join(names))
        self.steps: list[dict[str, Any]] = []
        self.started_at = _dt.datetime.now(tz=_dt.timezone.utc)
        self.override: bool | None = None
        self.stuck: str | None = None
        self.stopping = False
        self.pending: str | None = None
        self.environment: dict[str, list[str]] = {}
        self.sets: dict[str, str] = {}
        unreached = "preflight ended before it"
        self.device: dict[str, Any] = {"bytes": None, "reason": unreached}
        self.ballast: dict[str, dict[str, Any]] = {}
        self.predecessor_check: dict[str, Any] = {"applied": False, "reason": unreached}

    # -- running --

    def write_report(self, finished: bool = False) -> None:
        report = {
            "source_commit": self.args.source_commit,
            "artifact_sets": self.sets,
            "agent_environment": self.environment,
            "device_memory": self.device,
            "oom_ballast": self.ballast,
            "predecessor_check": self.predecessor_check,
            "row": self.args.row,
            "speech_family": self.args.speech_family,
            "started_at": self.started_at.isoformat(timespec="seconds"),
            "finished": finished,
            "steps": self.steps,
        }
        # Replaced whole: a reader never sees half a report.
        scratch = self.out / "pass-report.json.tmp"
        scratch.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        os.replace(scratch, self.out / "pass-report.json")

    def status_of(self, name: str) -> str | None:
        return next((step["status"] for step in self.steps if step["name"] == name), None)

    def step(
        self,
        name: str,
        body: Callable[[dict[str, Any]], None],
        requires: tuple[str, ...] = (),
    ) -> None:
        number = len(self.steps) + 1
        log = Path("logs") / f"{number:02d}-{name.replace(':', '-')}.log"
        step: dict[str, Any] = {
            "name": name,
            "status": "not_run",
            "exit_code": None,
            "result": None,
            "wall_s": 0.0,
            "reason": None,
            "log": str(log),
            "commands": [],
        }
        self.steps.append(step)
        (self.out / log).touch()
        unmet = [other for other in requires if self.status_of(other) != "ok"]
        if self.stuck and name != "index":
            step["reason"] = f"a command of {self.stuck} is still running"
        elif unmet:
            step["reason"] = f"{unmet[0]} is {self.status_of(unmet[0])}"
        else:
            step["status"] = "running"
            self.write_report()
            started = time.monotonic()
            try:
                body(step)
                step.update(status="ok", exit_code=0)
            except Interrupted as exc:
                step.update(status="interrupted", reason=f"the pass received {exc}")
                raise
            except StepFailed as exc:
                step.update(status="failed", reason=str(exc), exit_code=exc.exit_code)
                if isinstance(exc, Unstoppable):
                    self.stuck = name
            except StepTimeout as exc:
                step.update(status="timeout", reason=str(exc))
            # Any fault is that step's failure: the pass goes on and the report is written.
            except Exception as exc:  # noqa: BLE001
                step.update(status="failed", reason=f"{type(exc).__name__}: {exc}")
            step["wall_s"] = round(time.monotonic() - started, 1)
        print(f"== {name}: {step['status']}" + (f" ({step['reason']})" if step["reason"] else ""))
        self.write_report()

    def not_run(self, name: str, reason: str) -> None:
        self.steps.append(
            {"name": name, "status": "not_run", "exit_code": None, "result": None, "reason": reason}
            | {"wall_s": 0.0}
        )
        print(f"== {name}: not_run ({reason})")
        self.write_report()

    def run(
        self,
        step: dict[str, Any],
        argv: list[str],
        *,
        capture: bool = False,
        check: bool = True,
        env: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        """Run one command under its step's time limit.

        Output goes straight to the step's log, so a long command can be
        watched; with `capture` it is returned and logged when the command ends.
        """
        if argv not in step["commands"]:
            step["commands"].append(argv)
        limit = self.timeouts[step["name"].partition(":")[0]]
        with (self.out / step["log"]).open("a", encoding="utf-8") as log:
            log.write(f"$ {' '.join(argv)}\n")
            log.flush()
            process = None
            try:
                process = subprocess.Popen(
                    argv,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE if capture else log,
                    stderr=subprocess.PIPE if capture else log,
                    text=True,
                    cwd=cwd,
                    env={**os.environ, **(env or {})},
                    start_new_session=True,
                )
                stdout, stderr = process.communicate(timeout=limit)
            except Interrupted:
                # Its own session: nothing but this ends it when the pass is interrupted.
                if process is not None:
                    self.stop(process, log)
                raise
            except subprocess.TimeoutExpired:
                ended = self.stop(process, log)
                if self.pending:
                    raise Interrupted(self.pending) from None
                if ended is None:
                    raise Unstoppable(
                        f"`{Path(argv[0]).name}` had no end within {limit} s and could not be stopped"
                    ) from None
                raise StepTimeout(f"no end within {limit} s; ended on {ended}") from None
            log.write(f"{stdout or ''}{stderr or ''}exit {process.returncode}\n")
        if check and process.returncode != 0:
            raise StepFailed(
                f"`{Path(argv[0]).name}` exited {process.returncode}", process.returncode
            )
        return subprocess.CompletedProcess(argv, process.returncode, stdout or "", stderr or "")

    def stop(self, process: subprocess.Popen[str], log: Any) -> str | None:
        """End a command that reached its limit; the signal it ended on, or None if it runs on.

        TERM first, so it can clean up, then KILL for whatever of its group is left.
        Through sudo, because the group holds root-owned children this account cannot signal.
        """
        ended = None
        self.stopping = True
        for name, wait_s in (("SIGTERM", self.args.stop_grace_s), ("SIGKILL", 10)):
            kill = ["sudo", "kill", f"-{name[3:]}", "--", f"-{process.pid}"]
            log.write(f"$ {' '.join(kill)}\n")
            log.flush()
            try:
                subprocess.run(
                    kill, stdin=subprocess.DEVNULL, stdout=log, stderr=log, check=False, timeout=30
                )
                process.communicate(timeout=wait_s)
            except (OSError, subprocess.TimeoutExpired):
                continue
            ended = ended or name
        self.stopping = False
        return ended

    def on_signal(self, number: int, _frame: Any) -> None:
        name = signal.Signals(number).name
        if self.stopping:
            self.pending = self.pending or name
        else:
            raise Interrupted(name)

    def wait_for_agent(self, step: dict[str, Any]) -> None:
        deadline = time.monotonic() + self.args.agent_wait_s
        status = [self.args.tensorplate, "status", "--output", "json"]
        while True:
            answer = cli_envelope(self.run(step, status, capture=True, check=False)) or {}
            payload = answer.get("payload")
            agent = payload.get("agent") if isinstance(payload, dict) else None
            if isinstance(agent, dict) and agent.get("available") is True:
                return
            if time.monotonic() >= deadline:
                raise StepFailed(f"the agent did not answer within {self.args.agent_wait_s} s")
            time.sleep(2)

    # -- the steps --

    def preflight(self, step: dict[str, Any]) -> None:
        require_operator()
        for name, directory in (("candidate", self.assets), ("baseline", self.baseline)):
            if directory is not None:
                verify_checksums(directory)
                self.sets[name] = candidate_record.sha256_file(directory / "SHA256SUMS")
        built = manifest_commit(self.assets)
        if built != self.args.source_commit:
            raise StepFailed(f"the candidate set was built from {built}, not from --source-commit")
        git = ["git", "-C", str(self.repo)]
        head = self.run(step, [*git, "rev-parse", "HEAD"], capture=True).stdout.strip()
        if head != self.args.source_commit:
            raise StepFailed(f"the checkout is at {head}, not at --source-commit")
        changed = self.run(
            step, [*git, "status", "--porcelain", "--untracked-files=no"], capture=True
        )
        if changed.stdout.strip():
            raise StepFailed("the checkout has uncommitted changes: its tools are not the commit's")
        for candidate in self.candidates:
            if candidate.clips is not None and not candidate.clips.is_dir():
                raise StepFailed(f"{candidate.subject}: {candidate.clips} is not a directory")
        environment_file = Path(self.args.agent_environment_file)
        self.environment["at_preflight"] = sorted(environment_variables(environment_file))
        # The harness purges the family such a line points into, then deploys its own bundle.
        if interpreter_override(environment_file):
            raise StepFailed(
                f"{environment_file} already names an interpreter for the backend: remove the line"
            )
        if self.staging is not None and self.hidden_from_agent(self.staging):
            raise StepFailed(f"--staging-dir {self.staging} is hidden from the agent's unit")
        # By its path alone: the bundle is read in check_device, and only where a
        # device answers, because elsewhere the harness may be what stages it.
        if self.predecessor is not None and self.hidden_from_agent(self.predecessor):
            raise StepFailed(
                f"--predecessor-bundle {self.predecessor} is hidden from the agent's unit"
            )
        # Not the ballast interpreter: the family the pass installs later may hold it.
        if self.args.sampler and not Path(self.args.sampler).exists():
            raise StepFailed(f"--sampler {self.args.sampler} does not exist")
        self.check_device(step)
        # Last, so a refused pass has run nothing through sudo.
        if self.run(step, ["sudo", "-n", "true"], check=False).returncode != 0:
            raise StepFailed("sudo would ask for a password, which the pass cannot answer")

    def hidden_from_agent(self, path: Path) -> bool:
        return any(root == path or root in path.parents for root in self.hidden)

    def device_memory(self, step: dict[str, Any]) -> tuple[int | None, str | None, bool]:
        """Device 0's memory in bytes, or None and why not; and whether nvidia-smi ran."""
        argv = [self.args.nvidia_smi, "--id=0", "--query-gpu=memory.total"]
        argv.append("--format=csv,noheader,nounits")
        program = Path(argv[0]).name
        try:
            done = self.run(step, argv, capture=True, check=False)
        except OSError as exc:
            return None, f"`{program}` cannot be run ({exc.strerror})", False
        if done.returncode != 0:
            return None, f"`{program}` exited {done.returncode}", True
        total = device_memory_bytes(done.stdout)
        if total is None:
            return None, f"`{program}` printed no single memory figure for device 0", True
        return total, None, True

    def size_ballast(self, subject: str, total: int | None, reason: str | None) -> dict[str, Any]:
        sized: dict[str, Any] = {
            "rule": None,
            "bytes": None,
            "headroom_bytes": None,
            "reason": "no --oom-ballast names this candidate: its out-of-memory case will not run",
        }
        if subject not in self.ballast_rules:
            if total is not None:
                raise StepFailed(
                    f"{subject}: this machine has a GPU and no --oom-ballast names the "
                    "candidate, so its out-of-memory case would not run and its line could "
                    f"not pass; give it a rule, or --oom-ballast {subject}={NO_BALLAST} to "
                    "skip the case"
                )
            return sized
        kind, count = self.ballast_rules[subject]
        if kind == NO_BALLAST:
            sized.update(
                rule=kind,
                reason=f"--oom-ballast names {NO_BALLAST} for this candidate: "
                "its out-of-memory case will not run",
            )
        elif kind == "headroom":
            if total is None:
                raise StepFailed(
                    f"{subject}: --oom-ballast headroom:{count} needs the device's memory, "
                    f"and {reason}"
                )
            if count >= total:
                raise StepFailed(
                    f"{subject}: a headroom of {count} bytes leaves no ballast on a device "
                    f"of {total} bytes"
                )
            sized.update(rule=kind, bytes=total - count, headroom_bytes=count, reason=None)
        else:
            if total is not None and count >= total:
                raise StepFailed(
                    f"{subject}: a ballast of {count} bytes does not fit a device of {total} bytes"
                )
            sized.update(rule=kind, bytes=count, reason=None)
        return sized

    def check_device(self, step: dict[str, Any]) -> None:
        """Size each candidate's ballast and hold the predecessor to the device windows."""
        windows = bool(self.candidates and self.args.sampler)
        total, reason, ran = None, "the pass has no candidate, so nothing needed it", False
        if self.candidates:
            total, reason, ran = self.device_memory(step)
        self.device = {"bytes": total, "reason": reason}
        for candidate in self.candidates:
            self.ballast[candidate.subject] = self.size_ballast(candidate.subject, total, reason)
        if not windows:
            self.predecessor_check["reason"] = "the pass takes no memory window"
            return
        if total is None and ran:
            # A machine with no GPU has no nvidia-smi to run.
            raise StepFailed(
                f"the pass samples memory and {reason}: nvidia-smi runs, so this machine is "
                "taken to have a GPU, and one that does not report device 0 cannot give the "
                "device windows"
            )
        if total is None:
            self.predecessor_check["reason"] = (
                f"{reason}, so the pass cannot tell that this machine has a GPU"
            )
            return
        self.predecessor_check = {"applied": True, "reason": None}
        problem = (
            "none is named" if self.predecessor is None else predecessor_problem(self.predecessor)
        )
        if problem:
            raise StepFailed(
                "this machine has a GPU and the pass samples memory, so the predecessor_idle "
                f"and after_teardown device windows need a predecessor that loads onto it; "
                f"--predecessor-bundle: {problem}"
            )

    def lifecycle(self, step: dict[str, Any]) -> None:
        argv = [str(self.repo / "tools/validation/ubuntu-l4-cloud-lifecycle.sh")]
        argv += ["--assets-dir", str(self.assets), "--tested-version", self.tested_version]
        argv += ["--evidence-dir", str(self.out / "lifecycle"), "--row", self.args.row]
        if self.baseline is not None:
            argv += ["--baseline-assets-dir", str(self.baseline)]
        if self.args.allow_unsigned:
            argv.append("--allow-unsigned")
        try:
            self.run(step, [*argv, "--confirm", CONFIRM_TOKEN])
        finally:
            step["result"] = read_result(self.out / "lifecycle/lifecycle-report.json", "outcome")

    def install(self, step: dict[str, Any]) -> None:
        argv = ["sudo", "bash", str(self.assets / "install.sh")]
        argv += ["--local-artifacts", str(self.assets), "--yes", "--with-python-backend"]
        if self.args.allow_unsigned:
            argv.append("--allow-unsigned")
        if self.args.speech_family == "in-assets":
            argv.append("--with-speech-runtime")
        self.run(step, argv)
        self.run(step, ["sudo", "systemctl", "enable", "--now", *SERVICE_UNITS])
        self.wait_for_agent(step)

    def speech_family(self, step: dict[str, Any]) -> None:
        """Build the family from a scratch clone at the pass's commit and install it."""
        serving = sorted(self.assets.glob(f"tensorplate-serving_*_{SPEECH_ARCHITECTURE}.deb"))
        if len(serving) != 1:
            raise StepFailed(
                f"{len(serving)} {SPEECH_ARCHITECTURE} tensorplate-serving packages in the candidate set"
            )
        query = ["dpkg-deb", "--field", str(serving[0]), "Version"]
        version = self.run(step, query, capture=True).stdout.strip()
        deb_version, _, revision = version.rpartition("-")
        if not deb_version or not revision:
            raise StepFailed(f"the serving package's version {version!r} has no Debian revision")
        source, wheelhouse = self.work / "speech-family/source", self.work / "speech-wheelhouse"
        source.parent.mkdir(parents=True)
        self.run(step, ["git", "clone", "--quiet", "--no-hardlinks", str(self.repo), str(source)])
        self.run(step, ["tools/release/stage-debian-changelog.sh", deb_version], cwd=source)
        fetch = ["python3.12", "packaging/speech-runtime/build-environment.py", "fetch"]
        fetch += ["--lock-dir", "packaging/speech-runtime/lock", "--wheelhouse", str(wheelhouse)]
        self.run(step, [*fetch, "--work-dir", str(self.work / "speech-fetch")], cwd=source)
        build_env = {
            "DEB_BUILD_PROFILES": SPEECH_BUILD_PROFILE,
            "TP_SPEECH_RUNTIME_WHEELHOUSE": str(wheelhouse),
        }
        self.run(step, ["packaging/scripts/build-deb.sh", "-B"], cwd=source, env=build_env)
        packages = sorted(source.parent.glob(f"tensorplate-speech-runtime*_{version}_*.deb"))
        if not packages:
            raise StepFailed(f"the build left no tensorplate-speech-runtime package at {version}")
        # apt and not dpkg: the packages' own dependencies have to be installed too.
        apt = ["sudo", "env", "DEBIAN_FRONTEND=noninteractive", "apt-get", "install", "--yes"]
        self.run(step, [*apt, "--reinstall", *map(str, packages)])
        self.wait_for_agent(step)

    def agent_environment(self, step: dict[str, Any]) -> None:
        """Append the operator's variables to the agent's environment file and restart it."""
        # A new file is world-readable like the packaged one; a last line gets its newline.
        append = (
            '[ -e "$0" ] || install -m 0644 /dev/null "$0"; '
            '[ -z "$(tail -c 1 "$0")" ] || echo >>"$0"; '
            'printf "%s\\n" "$@" >>"$0"'
        )
        target = self.args.agent_environment_file
        self.run(step, ["sudo", "sh", "-c", append, target, *self.args.agent_environment])
        self.run(step, ["sudo", "systemctl", "restart", SERVICE_UNITS[0]])
        self.wait_for_agent(step)

    def doctor(self, step: dict[str, Any]) -> None:
        self.wait_for_agent(step)
        doctor = [self.args.tensorplate, "doctor", "--output", "json"]
        done = self.run(step, doctor, capture=True, check=False)
        (self.out / "doctor.json").write_text(done.stdout, encoding="utf-8")
        environment_file = Path(self.args.agent_environment_file)
        self.environment["at_doctor"] = sorted(environment_variables(environment_file))
        self.override = interpreter_override(environment_file)
        if done.returncode != 0:
            raise StepFailed(f"doctor exited {done.returncode}", done.returncode)

    def provision(self, candidate: Candidate, step: dict[str, Any]) -> None:
        argv = [self.args.tensorplate, "bundle", "provision", candidate.bundle, "--output", "json"]
        payload = (cli_envelope(self.run(step, argv, capture=True)) or {}).get("payload")
        path = payload.get("path") if isinstance(payload, dict) else None
        if not isinstance(path, str) or not Path(path).is_dir():
            raise StepFailed(f"provisioning named no bundle directory: {path!r}")
        candidate.bundle_dir = Path(path)

    def cold_deploy(self, candidate: Candidate, step: dict[str, Any]) -> None:
        """Time one deploy with nothing of the bundle in the page cache."""
        self.run(step, ["sync"])
        self.run(step, ["sudo", "sysctl", "--write", "vm.drop_caches=3"])
        deployment_id = f"validation-pass-cold-{candidate.subject}-{self.started_at:%Y%m%dT%H%M%SZ}"
        argv = [self.args.tensorplate, "deploy", str(candidate.bundle_dir)]
        argv += ["--deployment-id", deployment_id]
        argv += ["--wait-timeout-ms", str(self.args.deploy_wait_timeout_ms)]
        argv += ["--timeout-ms", str(self.args.agent_timeout_ms), "--output", "json"]
        started = time.monotonic_ns()
        done = self.run(step, argv, capture=True, check=False)
        wall_ms = (time.monotonic_ns() - started) // 1_000_000
        envelope = cli_envelope(done)
        if envelope is None:
            raise StepFailed(f"deploy printed no JSON envelope (exit {done.returncode})")
        outcome = deploy_outcome(envelope)
        deploy = {"deployment_id": deployment_id, "wall_ms": wall_ms, **outcome}
        measurement = {"subject": candidate.subject, "page_cache": "dropped", "deploy": deploy}
        directory = self.out / candidate.subject
        directory.mkdir(exist_ok=True)
        (directory / "cold-deploy.json").write_text(
            json.dumps(measurement, indent=2) + "\n", encoding="utf-8"
        )
        if outcome["status"] != "ok":
            failure = outcome["failure"]
            reached = failure["code"] if failure else f"phase {outcome['phase']}"
            raise StepFailed(f"the cold deploy did not become active ({reached})", done.returncode)

    def qualify(self, candidate: Candidate, step: dict[str, Any]) -> None:
        args = self.args
        directory = self.out / candidate.subject
        directory.mkdir(exist_ok=True)
        argv = [str(self.repo / "tools/validation/candidate-qualify.py")]
        argv += ["--candidate-bundle", str(candidate.bundle_dir)]
        argv += ["--evidence-dir", str(directory / "qualify"), "--tensorplate", args.tensorplate]
        argv += ["--source-commit", args.source_commit]
        argv += ["--deploy-wait-timeout-ms", str(args.deploy_wait_timeout_ms)]
        argv += ["--agent-timeout-ms", str(args.agent_timeout_ms)]
        optional = {
            "--clips": candidate.clips,
            "--predecessor-bundle": args.predecessor_bundle,
            "--sampler": args.sampler,
            "--oom-ballast-bytes": self.ballast[candidate.subject]["bytes"],
            "--oom-ballast-python": args.oom_ballast_python,
        }
        for flag, value in optional.items():
            if value:
                argv += [flag, str(value)]
        # Checked here and not in preflight: the family installed since then may hold it.
        python = args.oom_ballast_python
        if self.ballast[candidate.subject]["bytes"] and shutil.which(python) is None:
            raise StepFailed(f"--oom-ballast-python {python} is not a program that can be run")
        staging = self.staging / candidate.subject
        owner = [f"--owner={os.getuid()}", f"--group={os.getgid()}"]
        self.run(step, ["sudo", "install", "--directory", "--mode=0755", *owner, str(staging)])
        try:
            self.run(step, [*argv, "--staging-dir", str(staging), *args.qualify_arg])
        finally:
            step["result"] = read_result(directory / "qualify/record.json", "result", "status")

    def streaming_latency(self, step: dict[str, Any]) -> None:
        directory = self.out / STREAMING_LATENCY
        directory.mkdir(exist_ok=True)
        argv = [self.args.streaming_python, str(self.repo / "tools/validation/slice_measure.py")]
        argv += ["run", "--out", str(directory / "record.json")]
        argv += ["--source-commit", self.args.source_commit, *self.args.streaming_arg]
        try:
            self.run(step, argv)
        finally:
            step["result"] = read_result(directory / "record.json", "result", "status")
        provenance = read_result(directory / "record.json", "provenance")
        if provenance != "recorded":
            raise StepFailed(f"the client wrote a {provenance} record, which measured nothing")

    def index(self, step: dict[str, Any]) -> None:
        """Index whatever the steps above left; a subject that left nothing has no line."""
        family_build = self.args.family_build
        if family_build is None and self.args.speech_family == "host-built":
            family_build = self.args.source_commit
        lines: list[dict[str, Any]] = []
        faults: list[str] = []

        def load(path: Path) -> Any:
            return json.loads(path.read_text(encoding="utf-8"))

        def derive(subject: str, line: Callable[[], dict[str, Any]]) -> None:
            try:
                lines.append(line())
            except Exception as exc:  # noqa: BLE001
                faults.append(f"{subject}: {type(exc).__name__}: {exc}")

        report = self.out / "lifecycle/lifecycle-report.json"
        if report.exists():
            derive("lifecycle", lambda: index_mod.lifecycle_line(load(report), self.envelope))
        doctor = self.out / "doctor.json"
        # Without the environment file's answer the line would guess at the override.
        if doctor.exists() and self.override is not None:
            mode = index_mod.SPEECH_FAMILY_MODES[self.args.speech_family]
            required = index_mod.DOCTOR_REQUIREMENTS[mode]
            derive(
                "doctor",
                lambda: index_mod.doctor_line(
                    load(doctor)["payload"], self.envelope, bool(self.override), required
                ),
            )
        for candidate in self.candidates:
            directory = self.out / candidate.subject
            record, cold = directory / "qualify/record.json", directory / "cold-deploy.json"
            if record.exists():
                derive(
                    candidate.subject,
                    lambda record=record, cold=cold, subject=candidate.subject: (
                        index_mod.qualification_line(
                            load(record),
                            self.envelope,
                            subject,
                            family_build,
                            load(cold) if cold.exists() else None,
                        )
                    ),
                )
        streaming = self.out / STREAMING_LATENCY / "record.json"
        if streaming.exists():
            derive(
                STREAMING_LATENCY, lambda: index_mod.streaming_line(load(streaming), self.envelope)
            )
        (self.out / "lines.jsonl").write_text(
            "".join(index_mod.dump_line(line) for line in lines), encoding="utf-8"
        )
        if faults:
            raise StepFailed("; ".join(faults))
        if not lines:
            raise StepFailed("no step left anything to index")

    def run_pass(self) -> int:
        for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(number, self.on_signal)
        try:
            return self.run_steps()
        except Interrupted as exc:
            self.write_report()
            print(f"interrupted by {exc}; pass report: {self.out / 'pass-report.json'}")
            return EXIT_STEP_FAILED

    def run_steps(self) -> int:
        self.step("preflight", self.preflight)
        self.step("lifecycle", self.lifecycle, requires=("preflight",))
        self.step("install", self.install, requires=("preflight",))
        ready = ["install"]
        if self.args.speech_family == "host-built":
            self.step("speech-family", self.speech_family, requires=("install",))
            ready.append("speech-family")
        if self.args.agent_environment:
            self.step("agent-environment", self.agent_environment, tuple(ready))
            ready.append("agent-environment")
        self.step("doctor", self.doctor, requires=("install",))
        for candidate in self.candidates:
            provision = f"provision:{candidate.subject}"
            self.step(provision, lambda step, c=candidate: self.provision(c, step), tuple(ready))
            self.step(
                f"cold-deploy:{candidate.subject}",
                lambda step, c=candidate: self.cold_deploy(c, step),
                (provision,),
            )
            self.step(
                f"qualify:{candidate.subject}",
                lambda step, c=candidate: self.qualify(c, step),
                (provision,),
            )
        if self.streaming:
            self.step(STREAMING_LATENCY, self.streaming_latency, tuple(ready))
        else:
            self.not_run(STREAMING_LATENCY, "no --streaming-arg names a stream endpoint")
        self.step("index", self.index)
        self.write_report(finished=True)
        print(f"pass report: {self.out / 'pass-report.json'}")
        steps = [step for step in self.steps if self.streaming or step["name"] != STREAMING_LATENCY]
        return EXIT_OK if all(step["status"] == "ok" for step in steps) else EXIT_STEP_FAILED


def add_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--assets-dir", required=True, help="the candidate artifact set")
    parser.add_argument("--baseline-assets-dir", help="a published predecessor artifact set")
    parser.add_argument("--source-commit", required=True, help="the commit the set was built from")
    parser.add_argument("--out-dir", required=True, help="a new or empty directory for the pass")
    parser.add_argument("--work-dir", help="scratch for the family build; <out-dir>-work if absent")
    parser.add_argument("--confirm", required=True, help=f"must equal {CONFIRM_TOKEN}")
    parser.add_argument(
        "--candidate",
        action="append",
        default=[],
        metavar="SUBJECT:BUNDLE[:CLIPS]",
        help="a subject, its name in the provisioning manifest and its clip directory; repeatable",
    )
    parser.add_argument(
        "--staging-dir",
        help="where the recipe writes the bundles of its negative cases, one directory per "
        "subject, created through sudo; the agent must be able to read it",
    )
    parser.add_argument("--predecessor-bundle", help="deployed first, so teardown can roll back")
    parser.add_argument("--sampler", help="the memory_sample binary for the qualify tool")
    parser.add_argument(
        "--oom-ballast",
        action="append",
        default=[],
        metavar="SUBJECT=bytes:N|headroom:N|none",
        help="the device memory a candidate's out-of-memory case holds: N bytes, or the "
        "device's memory minus N bytes, read from nvidia-smi on this machine; repeatable. "
        "SUBJECT=none skips the case; where a device answers, a candidate with no rule "
        "is refused",
    )
    parser.add_argument(
        "--oom-ballast-python",
        help="the interpreter that holds the ballast: one that imports the runtime's torch",
    )
    parser.add_argument("--nvidia-smi", default="nvidia-smi", help="the nvidia-smi to run")
    parser.add_argument(
        "--streaming-arg",
        action="append",
        default=[],
        metavar="ARG",
        help="appended to the slice measurement client's run command; one that names "
        "--stt-endpoint or --tts-endpoint makes the pass run the streaming-latency step",
    )
    parser.add_argument(
        "--streaming-python",
        default=sys.executable,
        help="the interpreter that has the slice measurement client's packages",
    )
    parser.add_argument(
        "--qualify-arg", action="append", default=[], help="appended to every qualify command"
    )
    parser.add_argument("--deploy-wait-timeout-ms", type=int, default=300000)
    parser.add_argument("--agent-timeout-ms", type=int, default=120000)
    parser.add_argument("--tested-version", help="packaging/VERSION if absent")
    parser.add_argument("--allow-unsigned", action="store_true", help="a build-only candidate set")
    parser.add_argument("--repo-root", default=str(Path(__file__).resolve().parents[2]))
    parser.add_argument("--tensorplate", default="tensorplate", help="the tensorplate CLI to run")
    parser.add_argument("--agent-environment-file", default=AGENT_ENVIRONMENT_FILE)
    parser.add_argument(
        "--agent-environment",
        action="append",
        default=[],
        metavar="NAME=VALUE",
        help="appended to the agent's environment file once the speech runtime family is "
        "installed, and the agent restarted; repeatable",
    )
    parser.add_argument("--agent-wait-s", type=int, default=AGENT_WAIT_S)
    parser.add_argument(
        "--agent-hidden-root",
        action="append",
        metavar="DIR",
        help=f"a directory the agent's unit cannot read; {', '.join(AGENT_HIDDEN_ROOTS)} if absent",
    )
    parser.add_argument(
        "--stop-grace-s",
        type=int,
        default=STOP_GRACE_S,
        help="how long a command that reached its time limit has to end after SIGTERM",
    )
    parser.add_argument(
        "--step-timeout",
        action="append",
        default=[],
        metavar="STEP=SECONDS",
        help=f"one of {', '.join((*STEP_TIMEOUTS_S, STREAMING_LATENCY))}; repeatable. The "
        "streaming-latency limit is otherwise derived from the client's iterations and "
        "event timeout",
    )


def command_run(args: argparse.Namespace, envelope: dict[str, Any]) -> int:
    if args.confirm != CONFIRM_TOKEN:
        raise index_mod.IndexLineError(
            f"the pass purges TensorPlate from this machine: pass --confirm {CONFIRM_TOKEN}"
        )
    validation_pass = Pass(args, envelope)
    for directory in (validation_pass.out, validation_pass.work):
        if directory.exists() and (not directory.is_dir() or any(directory.iterdir())):
            raise index_mod.IndexLineError(f"{directory} is not a new or empty directory")
    (validation_pass.out / "logs").mkdir(parents=True, exist_ok=True)
    return validation_pass.run_pass()
