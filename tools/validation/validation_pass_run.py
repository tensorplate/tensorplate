# SPDX-License-Identifier: Apache-2.0
"""One rolling validation pass on the machine this runs on.

Steps, in order: preflight, lifecycle, install, speech-family,
agent-environment, doctor, then provision, cold-deploy and qualify per
candidate, streaming-latency, index.
Each step's commands, exit code and log are recorded in `pass-report.json`,
which is rewritten after every step. A step whose prerequisite did not end
`ok` is `not_run`. The pass creates, starts and deletes no machine and names
none. docs/validation/rolling-validation.md describes a pass.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import re
import subprocess
import time
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


def interpreter_override(environment_file: Path) -> bool:
    """Whether the agent's environment file names an interpreter, read as the unit reads it."""
    try:
        text = environment_file.read_text(encoding="utf-8")
    except FileNotFoundError:
        return False
    variables: dict[str, str] = {}
    for line in map(str.strip, text.splitlines()):
        name, separator, value = line.partition("=")
        if not separator:
            continue
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        variables[name.strip()] = value
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


class Candidate:
    def __init__(self, spec: str) -> None:
        parts = spec.split(":", 2)
        if len(parts) < 2 or not all(parts):
            raise index_mod.IndexLineError(
                f"--candidate takes SUBJECT:BUNDLE[:CLIPS], not {spec!r}"
            )
        self.subject, self.bundle = parts[0], parts[1]
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
        self.timeouts = dict(STEP_TIMEOUTS_S)
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
        self.sets: dict[str, str] = {}

    # -- running --

    def write_report(self, finished: bool = False) -> None:
        report = {
            "source_commit": self.args.source_commit,
            "artifact_sets": self.sets,
            "row": self.args.row,
            "speech_family": self.args.speech_family,
            "started_at": self.started_at.isoformat(timespec="seconds"),
            "finished": finished,
            "steps": self.steps,
        }
        (self.out / "pass-report.json").write_text(
            json.dumps(report, indent=2) + "\n", encoding="utf-8"
        )

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
            started = time.monotonic()
            try:
                body(step)
                step.update(status="ok", exit_code=0)
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
            try:
                stdout, stderr = process.communicate(timeout=limit)
            except subprocess.TimeoutExpired:
                ended = self.stop(process, log)
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
        return ended

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
        # The harness purges the family such a line points into, then deploys its own bundle.
        if interpreter_override(environment_file):
            raise StepFailed(
                f"{environment_file} already names an interpreter for the backend: remove the line"
            )
        if self.staging is not None:
            if any(root == self.staging or root in self.staging.parents for root in self.hidden):
                raise StepFailed(f"--staging-dir {self.staging} is hidden from the agent's unit")
        # Not the predecessor bundle: the harness may be what stages it.
        if self.args.sampler and not Path(self.args.sampler).exists():
            raise StepFailed(f"--sampler {self.args.sampler} does not exist")

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
        append = 'printf "%s\\n" "$@" >>"$0"'
        target = self.args.agent_environment_file
        self.run(step, ["sudo", "sh", "-c", append, target, *self.args.agent_environment])
        self.run(step, ["sudo", "systemctl", "restart", SERVICE_UNITS[0]])
        self.wait_for_agent(step)

    def doctor(self, step: dict[str, Any]) -> None:
        self.wait_for_agent(step)
        doctor = [self.args.tensorplate, "doctor", "--output", "json"]
        done = self.run(step, doctor, capture=True, check=False)
        (self.out / "doctor.json").write_text(done.stdout, encoding="utf-8")
        self.override = interpreter_override(Path(self.args.agent_environment_file))
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
            "--oom-ballast-bytes": args.oom_ballast_bytes,
            "--oom-ballast-python": args.oom_ballast_python,
        }
        for flag, value in optional.items():
            if value:
                argv += [flag, str(value)]
        staging = self.staging / candidate.subject
        owner = [f"--owner={os.getuid()}", f"--group={os.getgid()}"]
        self.run(step, ["sudo", "install", "--directory", "--mode=0755", *owner, str(staging)])
        try:
            self.run(step, [*argv, "--staging-dir", str(staging), *args.qualify_arg])
        finally:
            step["result"] = read_result(directory / "qualify/record.json", "result", "status")

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
        (self.out / "lines.jsonl").write_text(
            "".join(index_mod.dump_line(line) for line in lines), encoding="utf-8"
        )
        if faults:
            raise StepFailed("; ".join(faults))
        if not lines:
            raise StepFailed("no step left anything to index")

    def run_pass(self) -> int:
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
        self.not_run(STREAMING_LATENCY, "nothing measures streaming latency yet")
        self.step("index", self.index)
        self.write_report(finished=True)
        print(f"pass report: {self.out / 'pass-report.json'}")
        steps = [step for step in self.steps if step["name"] != STREAMING_LATENCY]
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
    parser.add_argument("--oom-ballast-bytes", type=int, default=0)
    parser.add_argument("--oom-ballast-python")
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
        help=f"one of {', '.join(STEP_TIMEOUTS_S)}; repeatable",
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
