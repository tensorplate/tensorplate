#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run a validation pass over a stubbed tree and machine, then break each step once.

The stubbed tree is a real git repository with fakes at the paths the driver
runs, and a directory of fake commands shadows `sudo` and everything run
through it. The fake harness and qualify tool replay the lifecycle report and
the candidate records filed on 2026-10-01. Doctor's output is synthetic: no
recorded run carries the runner-profile findings yet. The streaming client is
the measurement client's own `main` with its network half replaced: it reads
the arguments and gives the verdict, and what it "measured" is a record the
test builds, because no streaming run has been recorded on a machine, or the
record the client wrote against a closed port.
"""

from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import wave
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools/validation/validation-pass.py"
EVIDENCE = ROOT / "docs/validation/evidence"
ROW = "ubuntu2404-x86-l4-g2s8"
FILED_LIFECYCLE = EVIDENCE / f"v0.3.1/{ROW}/upgrade-rollback-rc.1/lifecycle"
FILED_RECORDS = {
    "stt-whisper-candidate": EVIDENCE / "speech-candidate-whisper-l4-2026-10-01/run-1/record.json",
    "tts-kokoro-candidate": EVIDENCE / "speech-candidate-kokoro-l4-2026-10-01/run-1/record.json",
}
sys.path.insert(0, str(ROOT / "tools/validation"))
import slice_measure as client_mod  # noqa: E402
import validation_index as index_mod  # noqa: E402
import validation_pass_run as run_mod  # noqa: E402

sys.path.insert(0, str(ROOT / "test/validation"))
import slice_record_builder as stream_mod  # noqa: E402

DEB_VERSION = "0.3.1~rc.1"
GPU_MIB = 20000
GPU_BYTES = GPU_MIB * 1024 * 1024
BUNDLES = ROOT / "test/models/bundles/v0_1"
RUNNER_FINDINGS = ("runner_profiles", "runner_profile_dependencies", "runner_launch_environment")
NO_NVIDIA_SMI = "no-nvidia-smi"
# What the measurement client wrote against a closed port: no session in either mode.
NO_SESSION_RECORD = ROOT / "test/validation/fixtures/slice_measurement_record_no_session.json"

# Every fake logs its call, then exits with the code FAKE_FAIL gives its name, if any.
PRELUDE = """#!/bin/sh
name=${FAKE_NAME:-$(basename "$0")}
printf '%s %s\\n' "$name" "$*" >>"$FAKE_CALLS"
code=$(sed -n "s/^$name //p" "$FAKE_FAIL" | head -1)
"""
EXIT = '[ -z "$code" ] || exit "$code"\n'
FAKES = {
    "sudo": 'if [ "$1" = -n ]; then\n  [ -z "$code" ] || exit "$code"\n  shift\nfi\nexec "$@"\n',
    "kill": EXIT + 'exec /bin/kill "$@"\n',
    "systemctl": EXIT,
    "sysctl": EXIT,
    "sync": EXIT,
    "apt-get": EXIT,
    # GNU install runs as given. BSD install has no long option, so there the
    # staging directory is made here.
    "install": 'if [ "$1" = --directory ] && ! /usr/bin/install --version >/dev/null 2>&1; then\n'
    '  for last; do :; done\n  mkdir -p "$last"\n  exit 0\nfi\nexec /usr/bin/install "$@"\n',
    "python3.12": EXIT,
    "dpkg-deb": EXIT + f"echo '{DEB_VERSION}-1'\n",
    "nvidia-smi": EXIT + '[ -z "$FAKE_GPU" ] || echo "$FAKE_GPU"\n',
    "tensorplate": """case " $* " in *" --output json "*) ;; *) exit 64 ;; esac
case "$1" in
  status) name=status ;; doctor) name=doctor ;; bundle) name=provision ;; deploy) name=deploy ;;
esac
code=$(sed -n "s/^$name //p" "$FAKE_FAIL" | head -1)
case "$name" in
  status)
    [ -z "$code" ] || { echo '{"status":"ok","payload":{"agent":{"available":false}}}'; exit 0; }
    cat "$FILED_LIFECYCLE/status-after-upgrade.json" ;;
  doctor) cat "$FAKE_DOCTOR"; exit "${code:-0}" ;;
  provision)
    [ -z "$code" ] || exit "$code"
    if [ -f "$FAKE_BUNDLES/$3/provision.json" ]; then
      cat "$FAKE_BUNDLES/$3/provision.json"
    else
      printf '{"status":"ok","payload":{"bundle":"%s","path":"%s"}}\\n' "$3" "$FAKE_BUNDLES/$3"
    fi ;;
  deploy)
    if [ -n "$code" ]; then
      echo '{"status":"ok","payload":{"phase":"failed","failure":{"error_code":"load_failed","message":"synthetic"}}}'
      exit "$code"
    fi
    cat "$(cat "$2/filed-deploy")" ;;
esac
""",
}
TREE = {
    "tools/validation/ubuntu-l4-cloud-lifecycle.sh": """if [ "$code" = hang ] || [ "$code" = hang-deaf ]; then
  trap 'echo cleaned >"$FAKE_PIDS.cleaned"; exit 143' TERM
  [ "$code" = hang ] || trap '' TERM
  sleep 60 &
  echo "$$ $!" >"$FAKE_PIDS"
  wait
fi
[ "$code" = no-report ] && exit 1
while [ "$1" != --evidence-dir ]; do shift; done
mkdir -p "$2" && cp -R "$FILED_LIFECYCLE/." "$2"
exit "${code:-0}"
""",
    "tools/validation/candidate-qualify.py": """[ -z "$code" ] || exit "$code"
bundle=$2
while [ "$1" != --evidence-dir ]; do shift; done
mkdir -p "$2" && cp "$(cat "$bundle/filed-record")" "$2/record.json"
exit 2
""",
    "tools/validation/slice_measure.py": """[ -z "$code" ] || [ "$code" = hang ] || exit "$code"
[ "$code" != hang ] || export FAKE_STREAM_HANG=1
exec "$FAKE_PYTHON" "$FAKE_STREAM_CLIENT" "$@"
""",
    "tools/release/stage-debian-changelog.sh": EXIT + 'echo "$1" >.staged-version\n',
    "packaging/scripts/build-deb.sh": EXIT
    + """echo "build-env $DEB_BUILD_PROFILES $TP_SPEECH_RUNTIME_WHEELHOUSE" >>"$FAKE_CALLS"
for package in tensorplate-speech-runtime tensorplate-speech-runtime-base; do
  : >"../${package}_$(cat .staged-version)-1_amd64.deb"
done
""",
    "install.sh": EXIT + 'eval "${FAKE_INSTALL_HOOK:-:}"\n',
}

# The client's `main` reads the arguments, writes the record and gives the exit status.
STREAM_CLIENT = f"""import json, os, signal, sys
from pathlib import Path

sys.path.insert(0, {str(ROOT / "tools/validation")!r})
import slice_measure


def measure(bindings, *, checkpoint, **settings):
    record = json.loads(Path(os.environ["FAKE_STREAM_RECORD"]).read_text(encoding="utf-8"))
    if os.environ.get("FAKE_STREAM_HANG"):
        # Saved as before a first session; then no end but a signal's.
        checkpoint(record)
        signal.alarm(60)
        signal.pause()
    return record


slice_measure.load_bindings = lambda out_dir: None
slice_measure.measure = measure
sys.exit(slice_measure.main(sys.argv[1:]))
"""


def write_fake(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(PRELUDE + body, encoding="utf-8")
    path.chmod(0o755)


def git(repo: Path, *arguments: str) -> str:
    identity = ("-c", "user.name=test", "-c", "user.email=test@example.invalid")
    done = subprocess.run(
        ["git", "-C", str(repo), *identity, *arguments], capture_output=True, text=True, check=True
    )
    return done.stdout.strip()


def synthetic_doctor(**status_by_id: str) -> dict:
    findings = dict.fromkeys(("platform_row", "python_pytorch_runtime", *RUNNER_FINDINGS), "ok")
    findings.update(status_by_id)
    failing = sum(1 for status in findings.values() if status == "fail")
    listed = [
        {"id": key, "status": value, "message": "synthetic"} for key, value in findings.items()
    ]
    return {"status": "ok", "payload": {"failing": failing, "findings": listed}}


def artifact_set(directory: Path, commit: str) -> Path:
    directory.mkdir()
    manifest = {"release": {"commit": commit}}
    (directory / "tensorplate-0.3.1-artifacts.json").write_text(json.dumps(manifest))
    write_fake(directory / "install.sh", TREE["install.sh"])
    # A release set carries every architecture's packages.
    for architecture in ("amd64", "arm64"):
        package = directory / f"tensorplate-serving_{DEB_VERSION}-1_{architecture}.deb"
        package.write_bytes(f"not a package ({architecture})\n".encode())
    sums = [
        f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
        for path in sorted(directory.iterdir())
    ]
    (directory / "SHA256SUMS").write_text("".join(sums), encoding="utf-8")
    return directory


class Machine:
    """A stubbed checkout, artifact sets, bundles and commands under one directory."""

    def __init__(self, tmp: Path) -> None:
        self.tmp = tmp
        self.repo = tmp / "checkout"
        for relative, body in TREE.items():
            if relative != "install.sh":
                write_fake(self.repo / relative, body)
        (self.repo / "packaging/VERSION").write_text("0.3.1\n", encoding="utf-8")
        git(self.repo, "init", "--quiet")
        git(self.repo, "add", ".")
        git(self.repo, "commit", "--quiet", "-m", "stub")
        self.commit = git(self.repo, "rev-parse", "HEAD")
        self.bin = tmp / "bin"
        for name, body in FAKES.items():
            write_fake(self.bin / name, body)
        self.assets = artifact_set(tmp / "candidate", self.commit)
        self.baseline = artifact_set(tmp / "baseline", "0" * 40)
        self.bundles = tmp / "bundles"
        for name, record in FILED_RECORDS.items():
            bundle, host = self.bundles / name, record.parents[1] / "host"
            bundle.mkdir(parents=True)
            (bundle / "filed-record").write_text(str(record))
            (bundle / "filed-deploy").write_text(str(host / "cold-deploy/deploy.json"))
            # The filed envelope, pointed at this machine's copy of the bundle.
            provisioned = json.loads((host / f"provision/provision-{name}.json").read_text())
            provisioned["payload"]["path"] = str(bundle)
            (bundle / "provision.json").write_text(json.dumps(provisioned))
        (tmp / "clips").mkdir()
        (tmp / "stream_client.py").write_text(STREAM_CLIENT, encoding="utf-8")
        with wave.open(str(tmp / "utterance.wav"), "wb") as utterance:
            utterance.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
            utterance.writeframes(bytes(2 * 3200))
        (tmp / "segment.txt").write_text("a segment of text\n", encoding="utf-8")
        # The CUDA smoke fixture, and the CPU one the lifecycle harness stages.
        self.cuda_predecessor = tmp / "x86_cuda_smoke"
        self.cpu_predecessor = tmp / "x86_fixture_smoke"
        shutil.copytree(BUNDLES / "x86_cuda_smoke", self.cuda_predecessor)
        shutil.copytree(BUNDLES / "x86_fixture_smoke", self.cpu_predecessor)
        self.environment_file = tmp / "tensorplate-agent"
        self.runs = 0

    def run(
        self,
        *arguments: str,
        fail: dict[str, object] | None = None,
        install_hook: str = "",
        doctor: dict | None = None,
        family: str = "host-built",
        candidates: bool = True,
        staging: bool = True,
        interrupt: int | None = None,
        gpu: str | None = None,
        stream: dict | None = None,
    ) -> Result:
        """`gpu` is what the fake nvidia-smi prints, None for a machine that has none.

        `stream` is the record the client "measures".
        """
        self.runs += 1
        out = self.tmp / f"pass-{self.runs}"
        scratch = {
            name: self.tmp / f"{name}-{self.runs}"
            for name in ("calls", "fail", "doctor", "pids", "stream")
        }
        record = stream or stream_mod.build_record(self.commit)
        scratch["stream"].write_text(json.dumps(record), encoding="utf-8")
        scratch["calls"].touch()
        scratch["fail"].write_text(
            "".join(f"{name} {code}\n" for name, code in (fail or {}).items()), encoding="utf-8"
        )
        scratch["doctor"].write_text(json.dumps(doctor or synthetic_doctor()), encoding="utf-8")
        environment = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}",
            "FAKE_CALLS": str(scratch["calls"]),
            "FAKE_FAIL": str(scratch["fail"]),
            "FAKE_DOCTOR": str(scratch["doctor"]),
            "FAKE_PIDS": str(scratch["pids"]),
            "FAKE_GPU": gpu or "",
            "FAKE_STREAM_RECORD": str(scratch["stream"]),
            "FAKE_STREAM_CLIENT": str(self.tmp / "stream_client.py"),
            "FAKE_PYTHON": sys.executable,
            "FAKE_BUNDLES": str(self.bundles),
            "FAKE_INSTALL_HOOK": install_hook,
            "FILED_LIFECYCLE": str(FILED_LIFECYCLE),
        }
        command = [
            sys.executable, str(TOOL), "run", "--repo-root", str(self.repo),
            "--assets-dir", str(self.assets), "--source-commit", self.commit,
            "--out-dir", str(out), "--speech-family", family, "--agent-wait-s", "0",
            "--agent-environment-file", str(self.environment_file),
            "--agent-hidden-root", str(self.tmp / "home"), "--stop-grace-s", "1",
            "--session", "stubbed", "--row", ROW, "--machine", "none", "--build", "stub",
        ]  # fmt: skip
        if candidates:
            command += [
                "--candidate", f"stt-whisper:stt-whisper-candidate:{self.tmp / 'clips'}",
                "--candidate", "tts-kokoro:tts-kokoro-candidate",
            ]  # fmt: skip
        if candidates and staging:
            command += ["--staging-dir", str(self.tmp / f"staging-{self.runs}")]
        if gpu is None:
            command += ["--nvidia-smi", str(self.tmp / NO_NVIDIA_SMI)]
        if "--confirm" not in arguments:
            command += ["--confirm", "RESET-TENSORPLATE"]
        mid = None
        if interrupt is None:
            done = subprocess.run(
                [*command, *arguments], capture_output=True, text=True, env=environment, check=False
            )
        else:
            # Signal the driver once the harness fake is waiting, and keep the report as it was then.
            process = subprocess.Popen(
                [*command, *arguments],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
            )
            deadline = time.monotonic() + 30
            while not scratch["pids"].exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            time.sleep(0.3)
            mid = json.loads((out / "pass-report.json").read_text(encoding="utf-8"))
            process.send_signal(interrupt)
            stdout, stderr = process.communicate(timeout=60)
            done = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
        self.mid = mid
        return Result(done, out, scratch["calls"], scratch["pids"])


class Result:
    def __init__(
        self, done: subprocess.CompletedProcess, out: Path, calls: Path, pids: Path
    ) -> None:
        self.done, self.out, self.pids = done, out, pids
        self.staging = out.with_name(out.name.replace("pass-", "staging-"))
        self.code = done.returncode
        self.calls = calls.read_text(encoding="utf-8").splitlines()
        report = out / "pass-report.json"
        self.report = json.loads(report.read_text(encoding="utf-8")) if report.exists() else None
        self.steps = {step["name"]: step for step in (self.report or {}).get("steps", [])}
        lines = out / "lines.jsonl"
        self.lines = (
            {line["subject"]: line for line in index_mod.read_index(lines)}
            if lines.exists()
            else {}
        )

    def statuses(self, *names: str) -> list[str]:
        return [self.steps[name]["status"] for name in names]

    def called(self, prefix: str) -> list[str]:
        return [call for call in self.calls if call.startswith(prefix)]

    def order(self, *prefixes: str) -> None:
        positions = [
            next(i for i, call in enumerate(self.calls) if call.startswith(p)) for p in prefixes
        ]
        assert positions == sorted(positions), (prefixes, positions)


CANDIDATE_STEPS = [
    f"{step}:{subject}"
    for subject in ("stt-whisper", "tts-kokoro")
    for step in ("provision", "cold-deploy", "qualify")
]


def check_full_pass(machine: Machine) -> None:
    result = machine.run(
        "--baseline-assets-dir", str(machine.baseline), "--allow-unsigned",
        "--predecessor-bundle", str(machine.bundles), "--sampler", str(machine.bin / "sync"),
        "--qualify-arg=--window=5s", "--qualify-arg=--predecessor-deployment-id=prior",
        "--oom-ballast", "stt-whisper=bytes:123", "--oom-ballast", "tts-kokoro=bytes:456",
        "--oom-ballast-python", "/usr/bin/python3",
    )  # fmt: skip
    # The replayed records' own results are `fail`, so the qualify tool exits 2 as it did.
    assert result.code == 1, result.done
    names = ["preflight", "lifecycle", "install", "speech-family", "doctor", *CANDIDATE_STEPS]
    assert list(result.steps) == [*names, "streaming-latency", "index"]
    qualify = [name for name in names if name.startswith("qualify")]
    assert result.statuses(*[name for name in names if name not in qualify]) == ["ok"] * 9
    assert [result.steps[name]["exit_code"] for name in qualify] == [2, 2]
    assert result.statuses("streaming-latency", "index") == ["not_run", "ok"]
    assert result.steps["streaming-latency"]["reason"] == (
        "no --streaming-arg names a stream endpoint"
    )
    # The record's own verdict stays beside the step's status.
    assert result.steps["lifecycle"]["result"] == "pass"
    assert [result.steps[name]["result"] for name in qualify] == ["fail", "fail"]
    assert result.steps["doctor"]["result"] is None
    assert result.report["finished"] is True and result.report["source_commit"] == machine.commit
    assert not (result.out / "pass-report.json.tmp").exists()
    assert set(result.report["artifact_sets"]) == {"candidate", "baseline"}
    for step in result.steps.values():
        assert "log" not in step or (result.out / step["log"]).is_file(), step

    result.order(
        "ubuntu-l4-cloud-lifecycle.sh", "install.sh", "systemctl", "stage-debian-changelog.sh",
        "python3.12", "build-deb.sh", "apt-get", "tensorplate doctor",
        "tensorplate bundle provision stt-whisper-candidate", "sysctl", "tensorplate deploy",
        "candidate-qualify.py", "tensorplate bundle provision tts-kokoro-candidate",
    )  # fmt: skip
    harness = result.called("ubuntu-l4-cloud-lifecycle.sh")[0]
    for expected in (
        f"--assets-dir {machine.assets} ", f"--baseline-assets-dir {machine.baseline}",
        "--tested-version 0.3.1 ", f"--row {ROW}", "--allow-unsigned", "--confirm RESET-TENSORPLATE",
    ):  # fmt: skip
        assert expected in harness, (expected, harness)
    install = result.called("install.sh")[0]
    assert (
        f"--local-artifacts {machine.assets} --yes --with-python-backend --allow-unsigned"
        in install
    )
    assert "--with-speech-runtime" not in install
    assert result.called("systemctl") == [
        "systemctl enable --now tensorplate-agent tensorplate-observability"
    ]
    assert result.called("stage-debian-changelog.sh") == [
        f"stage-debian-changelog.sh {DEB_VERSION}"
    ]
    work = Path(f"{result.out}-work")
    assert result.called("build-env") == [
        f"build-env pkg.tensorplate.speech-runtime {work / 'speech-wheelhouse'}"
    ]
    fetch = result.called("python3.12")[0]
    assert "build-environment.py fetch --lock-dir packaging/speech-runtime/lock" in fetch
    assert f"--wheelhouse {work / 'speech-wheelhouse'} --work-dir {work / 'speech-fetch'}" in fetch
    packages = [
        str(work / f"speech-family/{name}_{DEB_VERSION}-1_amd64.deb")
        for name in ("tensorplate-speech-runtime-base", "tensorplate-speech-runtime")
    ]
    assert result.called("apt-get") == [f"apt-get install --yes --reinstall {' '.join(packages)}"]
    assert result.called("sysctl") == ["sysctl --write vm.drop_caches=3"] * 2
    assert len(result.called("sync")) == 2
    whisper, kokoro = result.called("candidate-qualify.py")
    for expected in (
        f"--candidate-bundle {machine.bundles / 'stt-whisper-candidate'} ",
        f"--evidence-dir {result.out / 'stt-whisper/qualify'}", f"--source-commit {machine.commit}",
        f"--clips {machine.tmp / 'clips'}", f"--predecessor-bundle {machine.bundles}",
        f"--sampler {machine.bin / 'sync'}", "--agent-timeout-ms 120000", "--window=5s",
        "--deploy-wait-timeout-ms 300000", "--tensorplate tensorplate",
        "--oom-ballast-bytes 123 --oom-ballast-python /usr/bin/python3",
        f"--staging-dir {result.staging / 'stt-whisper'}", "--predecessor-deployment-id=prior",
    ):  # fmt: skip
        assert expected in whisper, (expected, whisper)
    assert "--clips" not in kokoro and f"--staging-dir {result.staging / 'tts-kokoro'}" in kokoro
    assert "--oom-ballast-bytes 456 --oom-ballast-python /usr/bin/python3" in kokoro
    # No GPU answered: the figure is absent with the reason, and a byte count is used as given.
    assert result.report["device_memory"] == {"bytes": None, "reason": UNRUNNABLE}
    made = result.called(
        f"sudo install --directory --mode=0755 --owner={os.getuid()} --group={os.getgid()} "
    )
    assert [call.split()[-1] for call in made] == [
        str(result.staging / subject) for subject in ("stt-whisper", "tts-kokoro")
    ]
    assert (result.staging / "stt-whisper").is_dir()
    # One wait for the agent after the install, after the family and before doctor.
    assert len(result.called("tensorplate status")) == 3
    result.order("systemctl enable", "tensorplate status", "dpkg-deb")
    result.order("apt-get", "tensorplate doctor")
    cold = [
        call for call in result.calls if "--deployment-id validation-pass-cold-stt-whisper" in call
    ]
    assert (
        len(cold) == 1 and "--wait-timeout-ms 300000 --timeout-ms 120000 --output json" in cold[0]
    )

    assert list(result.lines) == ["lifecycle", "doctor", "stt-whisper", "tts-kokoro"]
    assert result.lines["lifecycle"]["metrics"]["harness_wall_s"] == 157
    line = result.lines["stt-whisper"]
    direct = index_mod.qualification_line(
        json.loads(FILED_RECORDS["stt-whisper-candidate"].read_text(encoding="utf-8")),
        {**line, "run_suffix": None},
        "stt-whisper",
        machine.commit,
    )
    measured = line["metrics"].pop("deploy_wall_cold_cache_ms")
    assert isinstance(measured, int) and measured >= 0
    assert line["checks"].pop("cold_deploy") == "ok"
    assert line == direct, "the pass's line is the record's line plus the cold deploy"
    assert line["family_build"] == machine.commit and line["kind"] == "validation"
    assert line["settings"] == {"speech_family": "host-built"}
    assert line["metrics"]["first_request_ms"] == 677.8
    # The deploy-time negatives and the rollback after them reach the line.
    assert line["checks"]["negative.corrupt_artifact_digest"] == "matched (load_failed)"
    assert line["checks"]["negative.oom_at_load"] == "mismatched (timeout)"
    assert line["checks"]["teardown"] == "failed (rollback)"
    doctor = result.lines["doctor"]
    assert doctor["result"] == "pass" and doctor["checks"]["interpreter_override"] == "absent"
    cold_file = json.loads((result.out / "tts-kokoro/cold-deploy.json").read_text(encoding="utf-8"))
    assert cold_file["page_cache"] == "dropped" and cold_file["deploy"]["status"] == "ok"


def check_modes(machine: Machine) -> None:
    result = machine.run(family="in-assets")
    assert "speech-family" not in result.steps and not result.called("build-deb.sh")
    assert "--with-python-backend --with-speech-runtime" in result.called("install.sh")[0]
    assert "--allow-unsigned" not in result.called("install.sh")[0]
    assert "--baseline-assets-dir" not in result.called("ubuntu-l4-cloud-lifecycle.sh")[0]
    assert "family_build" not in result.lines["stt-whisper"]
    assert len(result.called("tensorplate status")) == 2
    assert "--oom-ballast" not in result.called("candidate-qualify.py")[0]
    assert result.lines["doctor"]["result"] == "pass"

    no_family = synthetic_doctor(**dict.fromkeys(RUNNER_FINDINGS, "skipped"))
    result = machine.run(family="none", candidates=False, doctor=no_family)
    assert result.code == 0, result.done
    assert list(result.steps) == [
        "preflight", "lifecycle", "install", "doctor", "streaming-latency", "index"
    ]  # fmt: skip
    assert result.lines["doctor"]["result"] == "pass"
    # Doctor reporting profiles on a machine said to have none is not a pass.
    result = machine.run(family="none", candidates=False)
    assert result.code == 0 and result.lines["doctor"]["result"] == "fail"
    result = machine.run(doctor=no_family)
    assert result.lines["doctor"]["result"] == "fail"
    result = machine.run(doctor=synthetic_doctor(python_pytorch_runtime="missing"))
    assert result.lines["doctor"]["result"] == "pass"

    for override in (
        "TP_PYTHON_PYTORCH_EXECUTABLE=/opt/venv/bin/python\n",
        'TP_TEST_PYTHON="/p"\n',
    ):
        machine.environment_file.write_text(f"# set by hand\nTMPDIR=/var/tmp\n{override}")
        assert run_mod.interpreter_override(machine.environment_file), override
    for unset in (
        "TP_PYTHON_PYTORCH_EXECUTABLE=/p\nTP_PYTHON_PYTORCH_EXECUTABLE=\n",
        "#TP_TEST_PYTHON=/p\n",
        'TP_TEST_PYTHON=""\n',
        "TP_TEST_PYTHON\n",
    ):
        machine.environment_file.write_text(unset)
        assert not run_mod.interpreter_override(machine.environment_file), unset
    machine.environment_file.unlink()
    assert not run_mod.interpreter_override(machine.environment_file)

    # Set after the family is installed, where the interpreter it names first exists.
    variables = (
        "TP_PYTHON_PYTORCH_EXECUTABLE=/opt/family/bin/python",
        "LD_LIBRARY_PATH=/opt/family",
    )
    stated = [flag for variable in variables for flag in ("--agent-environment", variable)]
    machine.environment_file.write_text("LD_LIBRARY_PATH=/earlier/pass", encoding="utf-8")
    result = machine.run(*stated)
    assert list(result.steps)[3:6] == ["speech-family", "agent-environment", "doctor"]
    assert len(result.called("tensorplate status")) == 4
    assert result.statuses("agent-environment", "provision:stt-whisper") == ["ok", "ok"]
    written = machine.environment_file.read_text(encoding="utf-8")
    lines = "".join(f"{variable}\n" for variable in variables)
    assert written == "LD_LIBRARY_PATH=/earlier/pass\n" + lines
    # What an earlier pass left is in the report, interpreter variable or not.
    assert result.report["agent_environment"] == {
        "at_preflight": ["LD_LIBRARY_PATH"],
        "at_doctor": ["LD_LIBRARY_PATH", "TP_PYTHON_PYTORCH_EXECUTABLE"],
    }
    result.order("apt-get", "systemctl restart tensorplate-agent", "tensorplate doctor")
    doctor = result.lines["doctor"]
    assert doctor["checks"]["interpreter_override"] == "present" and doctor["result"] == "fail"
    assert doctor["settings"] == {
        "speech_family": "host-built",
        "agent_environment": "LD_LIBRARY_PATH,TP_PYTHON_PYTORCH_EXECUTABLE",
    }
    machine.environment_file.unlink()
    # Under a private umask, so the mode is the step's doing and not the shell's default.
    umask = os.umask(0o077)
    result = machine.run(*stated[:2], family="in-assets")
    os.umask(umask)
    assert list(result.steps)[2:5] == ["install", "agent-environment", "doctor"]
    assert machine.environment_file.read_text(encoding="utf-8") == f"{variables[0]}\n"
    assert machine.environment_file.stat().st_mode & 0o777 == 0o644
    assert result.report["agent_environment"]["at_preflight"] == []
    # The pass leaves its line behind, and a second pass on the machine refuses it.
    assert machine.run().steps["preflight"]["status"] == "failed"
    machine.environment_file.unlink()
    assert "agent_environment" not in machine.run().lines["doctor"]["settings"]
    unwritable = ("--agent-environment-file", str(machine.tmp / "absent/tensorplate-agent"))
    result = machine.run(*stated, *unwritable)
    assert result.statuses("agent-environment", "provision:stt-whisper") == ["failed", "not_run"]
    assert result.steps["provision:stt-whisper"]["reason"] == "agent-environment is failed"
    assert not result.called("systemctl restart") and not result.called("tensorplate bundle")
    result = machine.run(*stated, fail={"build-deb.sh": 2})
    assert result.statuses("speech-family", "agent-environment") == ["failed", "not_run"]
    assert result.steps["agent-environment"]["reason"] == "speech-family is failed"


def check_preflight(machine: Machine) -> None:
    def refused(needle: str, *arguments: str) -> None:
        result = machine.run(*arguments)
        assert result.code == 1 and result.steps["preflight"]["status"] == "failed", result.done
        assert needle in result.steps["preflight"]["reason"], result.steps["preflight"]
        later = [step for name, step in result.steps.items() if name not in ("preflight", "index")]
        assert all(step["status"] == "not_run" for step in later), later
        assert result.steps["lifecycle"]["reason"] == "preflight is failed"
        # Nothing touched the machine: the harness purges and the installer installs.
        ran = [call for call in result.calls if not call.startswith("git")]
        assert ran == [] and not result.lines and result.statuses("index") == ["failed"]
        # What preflight did not get to is absent with that reason, not empty.
        unreached = {"bytes": None, "reason": "preflight ended before it"}
        assert result.report["device_memory"] == unreached and result.report["oom_ballast"] == {}
        assert result.report["predecessor_check"]["reason"] == "preflight ended before it"

    package = next(machine.assets.glob("*_amd64.deb"))
    sums = machine.assets / "SHA256SUMS"
    kept_package, kept_sums = package.read_bytes(), sums.read_text(encoding="utf-8")
    package.write_bytes(b"changed after the checksums were written\n")
    refused("does not match SHA256SUMS")
    package.unlink()
    refused("cannot be read")
    package.write_bytes(kept_package)
    outside = hashlib.sha256(kept_package).hexdigest()
    sums.write_text(f"{kept_sums}{outside}  ../{package.name}\n", encoding="utf-8")
    refused("does not name a file inside the set")
    sums.write_text("\n")
    refused("lists no file")
    sums.unlink()
    refused("SHA256SUMS")
    sums.write_text(kept_sums, encoding="utf-8")

    baseline_sums = machine.baseline / "SHA256SUMS"
    kept_baseline = baseline_sums.read_text(encoding="utf-8")
    flipped = "f" if kept_baseline[0] != "f" else "e"
    baseline_sums.write_text(flipped + kept_baseline[1:], encoding="utf-8")
    refused("does not match SHA256SUMS", "--baseline-assets-dir", str(machine.baseline))
    baseline_sums.write_text(kept_baseline, encoding="utf-8")

    refused("not from --source-commit", "--source-commit", "0" * 40)
    refused("not from --source-commit", "--source-commit", machine.commit[:12])
    second = machine.assets / "tensorplate-9.9.9-artifacts.json"
    second.write_text("{}")
    refused("holds 2 tensorplate-*-artifacts.json")
    second.unlink()
    (machine.repo / "later").write_text("a commit after the build\n")
    git(machine.repo, "add", "later")
    git(machine.repo, "commit", "--quiet", "-m", "later")
    refused("not at --source-commit")
    git(machine.repo, "reset", "--quiet", "--hard", machine.commit)
    (machine.repo / "untracked").touch()
    assert machine.run(candidates=False, family="none").statuses("preflight") == ["ok"]
    (machine.repo / "untracked").unlink()

    # A second pass on one machine: the first one's variable would misdirect the harness.
    machine.environment_file.write_text("TP_TEST_PYTHON=/opt/family/bin/python\n")
    refused("already names an interpreter")
    machine.environment_file.unlink()
    machine.environment_file.mkdir()
    refused("IsADirectoryError")
    machine.environment_file.rmdir()
    for hidden in ("home", "home/operator/staging"):
        refused("is hidden from the agent's unit", "--staging-dir", str(machine.tmp / hidden))
    version = machine.repo / "packaging/VERSION"
    version.write_text("0.3.2\n", encoding="utf-8")
    refused("uncommitted changes")
    version.write_text("0.3.1\n", encoding="utf-8")
    refused("is not a directory", "--candidate", f"extra:extra:{machine.tmp / 'absent'}")
    refused("--sampler", "--sampler", str(machine.tmp / "absent"))
    # A predecessor the agent's unit cannot read, whatever else the pass has: under a hidden
    # root, or reached through a link that is not.
    hidden = machine.tmp / "home/x86_cuda_smoke"
    shutil.copytree(machine.cuda_predecessor, hidden)
    link = machine.tmp / "linked_predecessor"
    link.symlink_to(hidden)
    for named in (hidden, link):
        needle = f"--predecessor-bundle {hidden} is hidden from the agent's unit"
        refused(needle, "--predecessor-bundle", str(named))
    link.unlink()
    shutil.rmtree(machine.tmp / "home")
    result = machine.run(fail={"sudo": 1})
    assert result.statuses("preflight", "lifecycle") == ["failed", "not_run"]
    assert "sudo would ask for a password" in result.steps["preflight"]["reason"]
    assert [call for call in result.calls if not call.startswith("git")] == ["sudo -n true"]
    assert machine.run(candidates=False, family="none").statuses("preflight") == ["ok"]


def check_each_step_failing(machine: Machine) -> None:
    after_install = ["doctor", *CANDIDATE_STEPS]

    result = machine.run(fail={"ubuntu-l4-cloud-lifecycle.sh": 1})
    assert result.statuses("lifecycle", "install", "doctor") == ["failed", "ok", "ok"]
    assert result.steps["lifecycle"]["exit_code"] == 1 and "lifecycle" in result.lines
    assert result.steps["lifecycle"]["result"] == "pass", "the report's own outcome is kept"
    result = machine.run(fail={"ubuntu-l4-cloud-lifecycle.sh": "no-report"})
    assert result.statuses("lifecycle", "install") == ["failed", "ok"]
    assert list(result.lines) == ["doctor", "stt-whisper", "tts-kokoro"]

    for broken in ({"install.sh": 3}, {"systemctl": 1}, {"status": "down"}):
        result = machine.run(fail=broken)
        assert (
            result.statuses("install", "speech-family", *after_install)
            == ["failed"] + ["not_run"] * 8
        )
        assert result.steps["doctor"]["reason"] == "install is failed"
        assert list(result.lines) == ["lifecycle"] and result.code == 1, broken
        assert not result.called("tensorplate doctor") and not result.called("apt-get")
    assert result.steps["install"]["reason"] == "the agent did not answer within 0 s"

    for broken, reason in (
        ({"dpkg-deb": 1}, "`dpkg-deb` exited 1"),
        ({"stage-debian-changelog.sh": 1}, "`stage-debian-changelog.sh` exited 1"),
        ({"python3.12": 1}, "`python3.12` exited 1"),
        ({"build-deb.sh": 2}, "`build-deb.sh` exited 2"),
        ({"build-deb.sh": 0}, f"no tensorplate-speech-runtime package at {DEB_VERSION}-1"),
        ({"apt-get": 100}, "`sudo` exited 100"),
    ):
        result = machine.run(fail=broken)
        assert result.statuses("speech-family", "doctor") == ["failed", "ok"], broken
        assert reason in result.steps["speech-family"]["reason"], result.steps["speech-family"]
        assert result.statuses(*CANDIDATE_STEPS) == ["not_run"] * 6
        assert result.steps["provision:stt-whisper"]["reason"] == "speech-family is failed"
        assert list(result.lines) == ["lifecycle", "doctor"]
        assert not result.called("tensorplate bundle")

    failing = synthetic_doctor(platform_row="fail")
    result = machine.run(fail={"doctor": 1}, doctor=failing)
    assert result.statuses("doctor", "provision:stt-whisper") == ["failed", "ok"]
    assert (
        result.lines["doctor"]["result"] == "fail"
        and result.lines["doctor"]["metrics"]["failing"] == 1
    )
    result = machine.run(fail={"doctor": 1}, doctor={"status": "error"})
    assert result.statuses("doctor", "index") == ["failed", "failed"]
    assert "doctor: KeyError" in result.steps["index"]["reason"] and "doctor" not in result.lines
    assert "stt-whisper" in result.lines, "one subject's fault does not lose the others"

    result = machine.run(fail={"provision": 1})
    assert result.statuses(*CANDIDATE_STEPS) == ["failed", "not_run", "not_run"] * 2
    assert list(result.lines) == ["lifecycle", "doctor"]
    # The fake names a directory per bundle; this bundle has none.
    result = machine.run("--candidate", "other:unprovisioned")
    other = [f"{step}:other" for step in ("provision", "cold-deploy", "qualify")]
    assert result.statuses(*other) == ["failed", "not_run", "not_run"]
    assert "named no bundle directory" in result.steps["provision:other"]["reason"]
    assert result.statuses("provision:stt-whisper", "cold-deploy:tts-kokoro") == ["ok", "ok"]

    second = machine.assets / "tensorplate-serving_9.9.9-1_amd64.deb"
    second.touch()
    result = machine.run()
    second.unlink()
    assert result.statuses("speech-family", "doctor") == ["failed", "ok"]
    assert "2 amd64 tensorplate-serving packages" in result.steps["speech-family"]["reason"]
    assert not result.called("dpkg-deb")

    for broken in ({"sync": 1}, {"sysctl": 1}):
        result = machine.run(fail=broken)
        assert result.statuses(*CANDIDATE_STEPS) == ["ok", "failed", "failed"] * 2
        assert not result.called("tensorplate deploy")
        assert not (result.out / "stt-whisper/cold-deploy.json").exists()
        checks, metrics = (result.lines["stt-whisper"][key] for key in ("checks", "metrics"))
        assert "cold_deploy" not in checks and "deploy_wall_cold_cache_ms" not in metrics

    result = machine.run(fail={"deploy": 1})
    assert result.statuses("cold-deploy:stt-whisper", "qualify:stt-whisper") == ["failed", "failed"]
    assert "load_failed" in result.steps["cold-deploy:stt-whisper"]["reason"]
    line = result.lines["stt-whisper"]
    assert line["checks"]["cold_deploy"] == "failed (load_failed)"
    assert (
        "deploy_wall_cold_cache_ms" not in line["metrics"] and "deploy_wall_ms" in line["metrics"]
    )

    result = machine.run(fail={"candidate-qualify.py": 1})
    assert result.statuses("qualify:stt-whisper", "index") == ["failed", "ok"]
    assert list(result.lines) == ["lifecycle", "doctor"]

    for deaf in (False, True):
        started = time.monotonic()
        hang = {"ubuntu-l4-cloud-lifecycle.sh": "hang-deaf" if deaf else "hang"}
        result = machine.run("--step-timeout", "lifecycle=1", fail=hang)
        assert time.monotonic() - started < 30
        assert result.statuses("lifecycle", "install") == ["timeout", "ok"]
        ended = "SIGKILL" if deaf else "SIGTERM"
        assert result.steps["lifecycle"]["reason"] == f"no end within 1 s; ended on {ended}"
        pids = result.pids.read_text().split()
        # TERM first, so the harness's own cleanup runs; KILL for whatever of the group is left.
        signals = [call.split()[2:] for call in result.called("sudo kill")]
        assert signals == [[name, "--", f"-{pids[0]}"] for name in ("-TERM", "-KILL")], signals
        assert Path(f"{result.pids}.cleaned").exists() is not deaf
        for pid in map(int, pids):
            for _ in range(50):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.1)
            else:
                raise AssertionError(f"process {pid} of the timed-out step is still running")


def check_an_environment_file_that_turns_unreadable(machine: Machine) -> None:
    # Fine at preflight, not text by the time doctor reads it: no line, rather than a guess.
    (machine.tmp / "not-text").write_bytes(b"\xff\xfe")
    hook = f"cp '{machine.tmp / 'not-text'}' '{machine.environment_file}'"
    result = machine.run(candidates=False, install_hook=hook)
    machine.environment_file.unlink()
    assert result.statuses("preflight", "install", "doctor") == ["ok", "ok", "failed"]
    assert "UnicodeDecodeError" in result.steps["doctor"]["reason"]
    assert (result.out / "doctor.json").exists() and list(result.lines) == ["lifecycle"]


def built_pass(machine: Machine, *arguments: str) -> run_mod.Pass:
    """The pass the tool builds from its command line, with nothing run."""
    spec = importlib.util.spec_from_file_location("validation_pass_cli", TOOL)
    cli = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cli)
    argv = [
        "run", "--assets-dir", str(machine.assets), "--source-commit", machine.commit,
        "--out-dir", str(machine.tmp / "unused"), "--confirm", "RESET-TENSORPLATE",
        "--speech-family", "none", "--repo-root", str(machine.repo), "--session", "s",
        "--row", ROW, "--machine", "m", "--build", "b", *arguments,
    ]  # fmt: skip
    args = cli.parse_args(argv)
    return run_mod.Pass(args, cli.envelope_from(args))


def check_root_is_refused_before_anything_runs(machine: Machine) -> None:
    a_pass = built_pass(machine)
    with mock.patch.object(run_mod.os, "geteuid", return_value=0):
        try:
            a_pass.preflight({})
        except run_mod.StepFailed as exc:
            assert "run as a normal user" in str(exc)
        else:
            raise AssertionError("preflight ran as root")


def check_an_interrupted_pass(machine: Machine) -> None:
    hang = {"ubuntu-l4-cloud-lifecycle.sh": "hang"}
    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        result = machine.run(fail=hang, interrupt=number)
        # While the harness ran, the report already showed its step.
        assert [step["status"] for step in machine.mid["steps"]] == ["ok", "running"]
        assert result.code == 1 and result.report["finished"] is False, result.done
        assert result.statuses("preflight", "lifecycle") == ["ok", "interrupted"]
        reason = f"the pass received {signal.Signals(number).name}"
        assert result.steps["lifecycle"]["reason"] == reason and len(result.steps) == 2
        assert not result.called("install.sh") and not result.lines
        pids = result.pids.read_text().split()
        signals = [call.split()[2:] for call in result.called("sudo kill")]
        assert signals == [[name, "--", f"-{pids[0]}"] for name in ("-TERM", "-KILL")], signals
        assert Path(f"{result.pids}.cleaned").exists(), "the harness got to clean up"
        for pid in map(int, pids):
            for _ in range(50):
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.1)
            else:
                raise AssertionError(f"process {pid} outlived the interrupted pass")


def check_a_command_that_cannot_be_stopped(machine: Machine) -> None:
    hang = {"ubuntu-l4-cloud-lifecycle.sh": "hang", "kill": 1}
    result = machine.run("--step-timeout", "lifecycle=1", fail=hang)
    pids = [int(pid) for pid in result.pids.read_text().split()]
    os.killpg(pids[0], 9)
    assert "had no end within 1 s and could not be stopped" in result.steps["lifecycle"]["reason"]
    assert len(result.called("sudo kill")) == 2
    # It may still be changing the machine, so nothing else is started; what is there is indexed.
    later = [name for name in result.steps if name not in ("preflight", "lifecycle", "index")]
    assert result.statuses("lifecycle", *later) == ["failed"] + ["not_run"] * len(later)
    assert result.steps["install"]["reason"] == "a command of lifecycle is still running"
    assert not result.called("install.sh") and result.statuses("index") == ["failed"]


def check_arguments(machine: Machine) -> None:
    def no_pass(needle: str, *arguments: str, **options: object) -> None:
        result = machine.run(*arguments, **options)
        assert result.code == 2 and needle in result.done.stderr, result.done
        assert result.report is None and result.calls == [], "nothing ran and nothing was written"

    no_pass("--confirm RESET-TENSORPLATE", "--confirm", "yes")
    no_pass("names one subject twice", "--candidate", "stt-whisper:other")
    no_pass("SUBJECT:BUNDLE[:CLIPS]", "--candidate", "stt-whisper")
    no_pass("SUBJECT:BUNDLE[:CLIPS]", "--candidate", ":bundle")
    # A subject names a directory made through sudo and an index subject.
    for subject in (
        "..", "a/b", "lifecycle", "doctor", "logs", "streaming-latency", "Upper", "-flag", "a b"
    ):  # fmt: skip
        no_pass("is reserved or does not match", f"--candidate={subject}:bundle")
    for bundle in ("-flag", "../x", "a/b"):
        no_pass("is not a name", "--candidate", f"other:{bundle}")
    no_pass("--speech-family none installs none", family="none")
    no_pass("a candidate needs --staging-dir", staging=False)
    for bad in (
        "stt-whisper", "stt-whisper=5", "stt-whisper=bytes", "stt-whisper=bytes:", "=bytes:5",
        "stt-whisper=bytes:0", "stt-whisper=bytes:-5", "stt-whisper=bytes:+5",
        "stt-whisper=bytes:05", "stt-whisper=bytes:1e9", "stt-whisper=bytes:5 ",
        "stt-whisper=bytes:\u0665", "stt-whisper=headroom:0", "stt-whisper=percent:5",
        "stt-whisper=bytes=5", "stt-whisper:bytes:5", "stt-whisper=none:5", "stt-whisper=none:",
        "stt-whisper=None", "stt-whisper=", "=none",
    ):  # fmt: skip
        no_pass("--oom-ballast takes SUBJECT=bytes:N, SUBJECT=headroom:N or SUBJECT=none", "--oom-ballast", bad)  # fmt: skip
    no_pass("names 'other', which no --candidate declares", "--oom-ballast", "other=bytes:5")
    no_pass("names 'other', which no --candidate declares", "--oom-ballast", "other=none")
    # A ballast is held by an interpreter the operator names; skipping the case needs none.
    for rule in ("bytes:5", "headroom:5"):
        no_pass("--oom-ballast needs --oom-ballast-python", "--oom-ballast", f"stt-whisper={rule}")
    no_pass("no --candidate declares", "--oom-ballast", "stt-whisper=bytes:5", candidates=False)
    no_pass(
        "names 'stt-whisper' twice",
        "--oom-ballast", "stt-whisper=bytes:5", "--oom-ballast", "stt-whisper=headroom:7",
    )  # fmt: skip
    # The pass-wide count is gone: one value could not fit two models.
    no_pass("unrecognized arguments: --oom-ballast-bytes", "--oom-ballast-bytes", "5")
    for named, given in (
        ("--oom-ballast-bytes", "--oom-ballast-bytes=5"),
        ("--oom-ballast-bytes", "--oom-ballast-b=5"),
        ("--oom-ballast-bytes", "--oom-ballast-bytes"),
        ("--oom-ballast-bytes", "--oom"),
        ("--predecessor-bundle", "--predecessor-bundle=/x"),
        ("--predecessor-bundle", "--predecessor-b=/x"),
        ("--predecessor-bundle", "--pred=/x"),
        # The sampler decides whether the predecessor is checked, so it is the pass's to give.
        ("--sampler", "--sampler=/x"),
        ("--sampler", "--sampler"),
        ("--sampler", "--sam=/x"),
    ):
        no_pass(f"--qualify-arg may not set {named}", f"--qualify-arg={given}")
    stream = ("--streaming-arg=--stt-endpoint=127.0.0.1:9", "--streaming-arg=--worker-build=b")
    for named, given in (
        ("--out", "--out=/x"),
        ("--out", "--ou=/x"),
        ("--source-commit", f"--source-commit={'0' * 40}"),
        ("--source-commit", "--source=x"),
        ("--provenance", "--provenance=synthetic"),
        ("--provenance", "--prov=recorded"),
    ):
        no_pass(f"--streaming-arg may not set {named}", *stream, f"--streaming-arg={given}")
    no_pass("names no stream endpoint", "--streaming-arg=--worker-version=x")
    no_pass("names no stream endpoint", "--streaming-arg=--iterations=3")
    no_pass("needs --worker-version or --worker-build", "--streaming-arg=--tts-endpoint=h:1")
    no_pass("needs --worker-version or --worker-build", "--streaming-arg=--stt-e=h:1")
    # A statement with no value states nothing: the client would write a synthetic record.
    for empty in (("--worker-version=",), ("--worker-build",), ("--worker-build=b", "--worker-b=")):
        stated = [f"--streaming-arg={item}" for item in ("--stt-endpoint=h:1", *empty)]
        no_pass("needs --worker-version or --worker-build with a value", *stated)
    # The pass reads both to set the step's time limit.
    for bad in ("--iterations=0", "--iterations=x", "--iter=1.5", "--event-timeout-ms=-1", "--event-timeout-ms"):  # fmt: skip
        no_pass("takes a whole number of at least 1", *stream, f"--streaming-arg={bad}")
    for bad in ("lifecycle", "lifecycle=0", "lifecycle=soon", "index=5"):
        no_pass("--step-timeout takes STEP=SECONDS", "--step-timeout", bad)
    no_pass("given twice", "--setting", "a=1", "--setting", "a=2")
    for bad in ("TMPDIR=/a b", "tmpdir=/a", "TMPDIR=", 'TMPDIR="/a"', "TMPDIR", "A=b\nB=c"):
        no_pass("--agent-environment takes NAME=VALUE", "--agent-environment", bad)
    used = machine.tmp / "used"
    used.mkdir()
    (used / "left-over").touch()
    no_pass("is not a new or empty directory", "--out-dir", str(used))
    no_pass("is not a new or empty directory", "--work-dir", str(used))
    result = machine.run(
        "--setting",
        "startup_timeout_ms=120000",
        "--date",
        "2026-10-07",
        candidates=False,
        family="none",
    )
    line = result.lines["lifecycle"]
    assert line["settings"] == {"startup_timeout_ms": 120000, "speech_family": "none"}
    assert line["run_id"] == f"2026-10-07-{ROW}-lifecycle" and line["raw"] == "local-only"


def check_deploy_outcome() -> None:
    def outcome(status: str = "ok", error: object = None, **payload: object) -> dict:
        return run_mod.deploy_outcome({"status": status, "payload": payload, "error": error})

    digest = "sha256:" + "a" * 64
    active = outcome(phase="active", bundle_digest=digest)
    assert active == {"status": "ok", "phase": "active", "bundle_digest": digest, "failure": None}
    assert outcome(phase="warming", bundle_digest=digest)["status"] == "failed"
    assert outcome("error", phase="active")["status"] == "failed"
    failed = outcome(phase="active", failure={"error_code": "load_failed", "message": "m"})
    assert failed["status"] == "failed"
    assert failed["failure"] == {"code": "load_failed", "message": "m"}
    refused = outcome("error", {"code": "busy", "message": "m"})
    assert refused["failure"]["code"] == "busy" and refused["phase"] is None
    assert outcome("error", "not an object")["failure"]["code"] == "untyped"
    assert outcome(phase=7, bundle_digest=7) == {
        "status": "failed", "phase": None, "bundle_digest": None, "failure": None
    }  # fmt: skip
    assert run_mod.deploy_outcome({"status": "ok", "payload": None})["status"] == "failed"


def check_small_guards() -> None:
    def refused(action, needle: str) -> None:
        try:
            action()
        except run_mod.StepFailed as exc:
            assert needle in str(exc), (needle, str(exc))
            return
        raise AssertionError(f"not refused: expected {needle!r}")

    with mock.patch.object(run_mod.os, "geteuid", return_value=0):
        refused(run_mod.require_operator, "run as a normal user")
    run_mod.require_operator()
    with tempfile.TemporaryDirectory() as raw_tmp:
        directory = Path(raw_tmp)
        refused(lambda: run_mod.manifest_commit(directory), "holds 0")
        manifest = directory / "tensorplate-1-artifacts.json"
        for content in ("{}", '{"release": []}', '{"release": {"commit": 7}}'):
            manifest.write_text(content)
            refused(lambda: run_mod.manifest_commit(directory), "names no release commit")
        manifest.write_text('{"release": {"commit": "abc"}}')
        assert run_mod.manifest_commit(directory) == "abc"
        assert run_mod.read_result(manifest, "release", "commit") == "abc"
        for keys in (("release", "absent"), ("release",), ("release", "commit", "deeper")):
            assert run_mod.read_result(manifest, *keys) is None, keys
        assert run_mod.read_result(directory / "absent.json", "release") is None


def check_names_match_their_sources() -> None:
    doctor = (ROOT / "cli/src/commands/doctor/runner_profiles.rs").read_text(encoding="utf-8")
    declared = doctor.split("const OWN_INTERPRETER_VARIABLES", 1)[1].split("];", 1)[0]
    assert tuple(re.findall(r'"(\w+)"', declared)) == run_mod.OWN_INTERPRETER_VARIABLES
    assert f'AGENT_ENVIRONMENT_FILE: &str = "{run_mod.AGENT_ENVIRONMENT_FILE}"' in doctor
    harness = (ROOT / "tools/validation/ubuntu-l4-cloud-lifecycle.sh").read_text(encoding="utf-8")
    assert f'readonly CONFIRM_TOKEN="{run_mod.CONFIRM_TOKEN}"' in harness
    for unit in run_mod.SERVICE_UNITS:
        assert f'_UNIT="{unit}"' in harness, unit
    rules = (ROOT / "packaging/debian/rules").read_text(encoding="utf-8")
    assert f"$(filter {run_mod.SPEECH_BUILD_PROFILE},$(DEB_BUILD_PROFILES))" in rules
    unit = (ROOT / "packaging/debian/tensorplate-agent.service").read_text(encoding="utf-8")
    assert "\nProtectHome=true\n" in unit and "\nPrivateTmp=true\n" in unit
    assert run_mod.AGENT_HIDDEN_ROOTS == ("/home", "/root", "/run/user", "/tmp", "/var/tmp")
    for relative in TREE:
        assert relative == "install.sh" or os.access(ROOT / relative, os.X_OK), relative
    runner = ROOT / "backends/python_pytorch/src/tensorplate_pytorch_backend/runner.py"
    registry = runner.read_text(encoding="utf-8").split("def default_backend_factories", 1)[1]
    assert set(run_mod.DEVICE_PROFILES) <= set(re.findall(r'"(\w+)":', registry.split("}", 1)[0]))
    assert run_mod.STREAMING_LATENCY == index_mod.STREAMING_SUBJECT
    client = (ROOT / "tools/validation/slice_measure.py").read_text(encoding="utf-8")
    for option in ("--out", "--source-commit", *run_mod.WORKER_STATEMENTS):
        assert f'"{option}"' in client, option
    assert {o[2:5] for o in run_mod.STREAM_ENDPOINTS} == set(index_mod.slice_record.MODES)
    for option in run_mod.STREAM_ENDPOINTS:
        assert f'f"--{{mode}}-{option.rsplit("-", 1)[1]}"' in client, option
    assert (ROOT / "packaging/scripts/install.sh").is_file()
    assert (ROOT / "packaging/speech-runtime/build-environment.py").is_file()
    assert (ROOT / "packaging/speech-runtime/lock").is_dir()


PROBE = "nvidia-smi --id=0 --query-gpu=memory.total --format=csv,noheader,nounits"
UNRUNNABLE = f"`{NO_NVIDIA_SMI}` cannot be run (No such file or directory)"
BALLAST_PYTHON = ("--oom-ballast-python", "/usr/bin/python3")
SKIP_BALLAST = ("--oom-ballast", "stt-whisper=none", "--oom-ballast", "tts-kokoro=none")


def sampled(machine: Machine, *arguments: str, **options: object) -> Result:
    return machine.run("--sampler", str(machine.bin / "sync"), *arguments, **options)


def ballast_of(result: Result, subject: str) -> str:
    commands = result.steps[f"qualify:{subject}"]["commands"]
    argv = next(command for command in commands if "--oom-ballast-bytes" in command)
    return argv[argv.index("--oom-ballast-bytes") + 1]


def check_ballast_per_candidate(machine: Machine) -> None:
    result = sampled(
        machine, "--predecessor-bundle", str(machine.cuda_predecessor),
        "--oom-ballast", "stt-whisper=bytes:5000000000",
        "--oom-ballast", "tts-kokoro=headroom:1000000000", *BALLAST_PYTHON, gpu=str(GPU_MIB),
    )  # fmt: skip
    assert result.statuses("preflight") == ["ok"], result.steps["preflight"]
    assert result.called("nvidia-smi") == [PROBE]
    whisper, kokoro = (ballast_of(result, subject) for subject in ("stt-whisper", "tts-kokoro"))
    assert (whisper, kokoro) == ("5000000000", str(GPU_BYTES - 1_000_000_000))
    assert result.report["device_memory"] == {"bytes": GPU_BYTES, "reason": None}
    assert result.report["oom_ballast"] == {
        "stt-whisper": {"rule": "bytes", "bytes": 5_000_000_000, "headroom_bytes": None, "reason": None},
        "tts-kokoro": {
            "rule": "headroom",
            "bytes": GPU_BYTES - 1_000_000_000,
            "headroom_bytes": 1_000_000_000,
            "reason": None,
        },
    }  # fmt: skip

    assert result.report["predecessor_check"] == {"applied": True, "reason": None}

    # The edges of what fits: one byte of ballast, and one byte of headroom less than the device.
    result = sampled(
        machine, "--predecessor-bundle", str(machine.cuda_predecessor),
        "--oom-ballast", f"stt-whisper=headroom:{GPU_BYTES - 1}",
        "--oom-ballast", f"tts-kokoro=bytes:{GPU_BYTES - 1}", *BALLAST_PYTHON, gpu=str(GPU_MIB),
    )  # fmt: skip
    assert result.statuses("preflight") == ["ok"]
    assert ballast_of(result, "stt-whisper") == "1"
    assert ballast_of(result, "tts-kokoro") == str(GPU_BYTES - 1)

    # Where a device answers, a candidate with no rule is refused: its line could not pass.
    predecessor = ("--predecessor-bundle", str(machine.cuda_predecessor))
    missing = "this machine has a GPU and no --oom-ballast names the candidate"
    refused_in_preflight(machine, f"stt-whisper: {missing}", gpu=str(GPU_MIB))
    refused_in_preflight(
        machine, f"tts-kokoro: {missing}", "--sampler", str(machine.bin / "sync"), *predecessor,
        "--oom-ballast", "stt-whisper=bytes:5000", *BALLAST_PYTHON, gpu=str(GPU_MIB),
    )  # fmt: skip
    # Skipping the case is said, and the report keeps the choice and what follows from it.
    result = sampled(
        machine, *predecessor, "--oom-ballast", "stt-whisper=bytes:5000", *BALLAST_PYTHON,
        "--oom-ballast", "tts-kokoro=none", gpu=str(GPU_MIB),
    )  # fmt: skip
    assert result.statuses("preflight") == ["ok"], result.steps["preflight"]
    kokoro = result.steps["qualify:tts-kokoro"]["commands"]
    assert not any("--oom-ballast-bytes" in command for command in kokoro)
    assert ballast_of(result, "stt-whisper") == "5000"
    assert result.report["oom_ballast"]["tts-kokoro"] == {
        "rule": "none",
        "bytes": None,
        "headroom_bytes": None,
        "reason": "--oom-ballast names none for this candidate: its out-of-memory case will not run",
    }
    # No interpreter is asked for when no candidate takes a ballast.
    result = machine.run(*SKIP_BALLAST, gpu=str(GPU_MIB))
    assert result.statuses("preflight") == ["ok"], result.steps["preflight"]
    assert {entry["rule"] for entry in result.report["oom_ballast"].values()} == {"none"}
    assert not any("--oom-ballast" in call for call in result.called("candidate-qualify.py"))
    assert result.report["predecessor_check"] == {
        "applied": False,
        "reason": "the pass takes no memory window",
    }

    # Where none answers, a candidate with no rule is not refused and the report says why.
    result = machine.run()
    assert result.statuses("preflight") == ["ok"]
    assert result.report["device_memory"] == {"bytes": None, "reason": UNRUNNABLE}
    unnamed = "no --oom-ballast names this candidate: its out-of-memory case will not run"
    for entry in result.report["oom_ballast"].values():
        assert entry == {"rule": None, "bytes": None, "headroom_bytes": None, "reason": unnamed}
    # A pass with no candidate asks the device nothing, and the report says that too.
    result = machine.run(candidates=False, family="none", gpu=str(GPU_MIB))
    assert result.called("nvidia-smi") == [] and result.report["oom_ballast"] == {}
    assert result.report["device_memory"] == {
        "bytes": None,
        "reason": "the pass has no candidate, so nothing needed it",
    }


def refused_in_preflight(machine: Machine, needle: str, *arguments: str, **options: object) -> None:
    result = machine.run(*arguments, **options)
    reason = result.steps["preflight"]["reason"]
    assert result.code == 1 and result.steps["preflight"]["status"] == "failed", result.done
    assert needle in reason, (needle, reason)
    later = [step for name, step in result.steps.items() if name not in ("preflight", "index")]
    assert all(step["status"] == "not_run" for step in later), later
    # Nothing was deployed: only the reading of the device ran before the refusal.
    assert [c for c in result.calls if not c.startswith(("git", "nvidia-smi"))] == [], result.calls
    assert not result.lines


def check_device_refusals(machine: Machine) -> None:
    sync = str(machine.bin / "sync")
    cpu = str(machine.cpu_predecessor)
    headroom = ("--oom-ballast", "stt-whisper=headroom:1000", *BALLAST_PYTHON)
    gpu = str(GPU_MIB)

    # A rule is evaluated on the machine; a machine that gives no figure is refused, not guessed.
    for printed in ("", "N/A", "0", "-5", "20000 MiB", "20000\n20000", "2.3e4", "\u0662\u0663"):
        refused_in_preflight(
            machine, "printed no single memory figure", *headroom, gpu=printed
        )  # fmt: skip
    refused_in_preflight(machine, "`nvidia-smi` exited 9", *headroom, fail={"nvidia-smi": 9}, gpu=gpu)  # fmt: skip
    refused_in_preflight(machine, f"needs the device's memory, and {UNRUNNABLE}", *headroom)
    for rule, needle in ((f"headroom:{GPU_BYTES}", "leaves no ballast"), (f"bytes:{GPU_BYTES}", "does not fit")):  # fmt: skip
        refused_in_preflight(
            machine, needle, "--oom-ballast", f"stt-whisper={rule}", "--oom-ballast",
            "tts-kokoro=none", *BALLAST_PYTHON, gpu=gpu,
        )  # fmt: skip

    # A pass that samples memory on a machine with a device needs a predecessor that loads onto it.
    refused_in_preflight(machine, "none is named", "--sampler", sync, *SKIP_BALLAST, gpu=gpu)
    refused_in_preflight(
        machine, "holds no device memory", "--sampler", sync, "--predecessor-bundle", cpu,
        *SKIP_BALLAST, gpu=gpu,
    )  # fmt: skip
    refused_in_preflight(
        machine, "cannot be read as a bundle (FileNotFoundError)", "--sampler", sync,
        "--predecessor-bundle", str(machine.tmp / "absent"), *SKIP_BALLAST, gpu=gpu,
    )  # fmt: skip
    # A directory of bundles is not a bundle.
    refused_in_preflight(
        machine, "predecessor_idle", "--sampler", sync, "--predecessor-bundle", str(machine.bundles),
        *SKIP_BALLAST, gpu=gpu,
    )  # fmt: skip

    # A machine with no nvidia-smi to run has no GPU the pass can tell of: the predecessor is
    # the operator's choice, and the report says why it was not checked.
    sampled_cpu = ("--sampler", sync, "--predecessor-bundle", cpu)
    result = machine.run(*sampled_cpu)
    assert result.statuses("preflight") == ["ok"], result.steps["preflight"]
    assert result.report["predecessor_check"] == {
        "applied": False,
        "reason": f"{UNRUNNABLE}, so the pass cannot tell that this machine has a GPU",
    }
    # One whose nvidia-smi runs has a GPU: a pass that samples memory is refused when it
    # fails, or gives no figure.
    sampling = "the pass samples memory and `nvidia-smi`"
    refused_in_preflight(machine, f"{sampling} exited 9", *sampled_cpu, fail={"nvidia-smi": 9}, gpu=gpu)  # fmt: skip
    for printed in ("", "N/A"):
        refused_in_preflight(
            machine, f"{sampling} printed no single memory figure", *sampled_cpu, gpu=printed
        )  # fmt: skip
    # Where no window is taken, no answer of nvidia-smi stops the pass.
    unsampled = "the pass takes no memory window"
    for options in (
        {"gpu": gpu, "fail": {"nvidia-smi": 9}},
        {"gpu": ""},
        {"gpu": gpu, "ballast": SKIP_BALLAST},
        {"gpu": gpu, "ballast": ("--sampler", sync), "candidates": False},
    ):
        arguments = ("--predecessor-bundle", cpu, *options.pop("ballast", ()))
        result = machine.run(*arguments, **options)
        assert result.statuses("preflight") == ["ok"], (options, result.steps["preflight"])
        assert result.report["predecessor_check"] == {"applied": False, "reason": unsampled}


def check_the_ballast_interpreter_is_checked_where_it_is_used(machine: Machine) -> None:
    """The speech runtime family's interpreter exists only once the pass has installed it."""
    python = machine.tmp / "family/bin/python"
    rules = (
        "--oom-ballast", "stt-whisper=bytes:5000", "--oom-ballast", "tts-kokoro=none",
        "--oom-ballast-python", str(python),
    )  # fmt: skip
    reason = f"--oom-ballast-python {python} is not a program that can be run"
    for hook in ("", f"mkdir -p '{python.parent}' && : >'{python}'"):
        result = machine.run(*rules, install_hook=hook)
        shutil.rmtree(machine.tmp / "family", ignore_errors=True)
        assert result.statuses("preflight", "cold-deploy:stt-whisper") == ["ok", "ok"], hook
        step = result.steps["qualify:stt-whisper"]
        assert (step["status"], step["reason"], step["exit_code"]) == ("failed", reason, None)
        # Refused before the recipe ran; the candidate that takes no ballast is qualified.
        recipe = result.called("candidate-qualify.py")
        assert len(recipe) == 1 and "tts-kokoro-candidate" in recipe[0], recipe
        assert not (result.staging / "stt-whisper").exists()
    # Installed by the time the step runs, it is what the recipe is given.
    installed = f"mkdir -p '{python.parent}' && ln -s /usr/bin/python3 '{python}'"
    result = machine.run(*rules, install_hook=installed)
    shutil.rmtree(machine.tmp / "family")
    whisper, kokoro = result.called("candidate-qualify.py")
    assert f"--oom-ballast-bytes 5000 --oom-ballast-python {python}" in whisper
    assert result.steps["qualify:stt-whisper"]["exit_code"] == 2


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def check_predecessor_problem(machine: Machine) -> None:
    def problem(edit=None, entry: dict | None = None, manifest: object = None) -> str | None:
        with tempfile.TemporaryDirectory() as raw_tmp:
            bundle = Path(raw_tmp) / "bundle"
            shutil.copytree(machine.cuda_predecessor, bundle)
            if entry is not None:
                write_json(bundle / "x86-cuda-smoke.json", entry)
            if manifest is not None:
                write_json(bundle / "manifest.json", manifest)
            if edit is not None:
                edit(bundle)
            return run_mod.predecessor_problem(bundle)

    def manifest_of(**changes: object) -> dict:
        document = json.loads((machine.cuda_predecessor / "manifest.json").read_text())
        document["artifacts"][0].update(changes)
        return document

    cuda = {"backend_profile": "cuda_fixture", "device": "cuda"}
    assert problem() is None
    assert run_mod.predecessor_problem(machine.cuda_predecessor) is None
    assert "holds no device memory" in run_mod.predecessor_problem(machine.cpu_predecessor)
    # The candidates' own bundles list weights and auxiliary files beside their one entry.
    for candidate in ("stt_whisper_candidate", "tts_kokoro_candidate"):
        assert run_mod.predecessor_problem(BUNDLES / candidate) is None, candidate
    for profile in run_mod.DEVICE_PROFILES:
        assert problem(entry={**cuda, "backend_profile": profile}) is None, profile
    for entry in (
        {**cuda, "device": "cpu"},
        {**cuda, "device": "CUDA"},
        {"backend_profile": "cuda_fixture"},
        {"device": "cuda"},
        {**cuda, "backend_profile": "fixture"},
        {**cuda, "backend_profile": "mps_fixture"},
    ):
        assert "holds no device memory" in (problem(entry=entry) or ""), entry

    assert "lists 0" in problem(manifest=manifest_of(kind="weights"))
    twice = manifest_of()
    twice["artifacts"].append(dict(twice["artifacts"][0]))
    assert "lists 2" in problem(manifest=twice)
    for path in ("../escape.json", "/etc/hosts"):
        assert "lies outside the bundle" in problem(manifest=manifest_of(path=path)), path

    def link_out(bundle: Path) -> None:
        elsewhere = bundle.parent / "elsewhere.json"
        write_json(elsewhere, cuda)
        (bundle / "x86-cuda-smoke.json").unlink()
        (bundle / "x86-cuda-smoke.json").symlink_to(elsewhere)

    assert "lies outside the bundle" in problem(edit=link_out)
    for edit, name in (
        (lambda bundle: (bundle / "manifest.json").write_text("{"), "JSONDecodeError"),
        (lambda bundle: (bundle / "manifest.json").unlink(), "FileNotFoundError"),
        (lambda bundle: (bundle / "x86-cuda-smoke.json").unlink(), "FileNotFoundError"),
        (lambda bundle: (bundle / "x86-cuda-smoke.json").write_text("[]"), "AttributeError"),
        (
            lambda bundle: (bundle / "x86-cuda-smoke.json").write_bytes(b"\xff"),
            "UnicodeDecodeError",
        ),
    ):
        assert problem(edit=edit) == f"it cannot be read as a bundle ({name})", name
    assert "(KeyError)" in problem(manifest={"name": "no artifacts"})
    assert "(TypeError)" in problem(manifest={"artifacts": 5})
    assert "(AttributeError)" in problem(manifest={"artifacts": [5]})
    assert "(KeyError)" in problem(manifest=manifest_of(path=None) | {"artifacts": [{"kind": "python_pytorch_entry"}]})  # fmt: skip


def check_device_memory_figure() -> None:
    mib = 1024 * 1024
    for text, expected in (
        ("20000\n", 20000 * mib), (" 20000 \n\n", 20000 * mib), ("1", mib),
        ("", None), ("\n", None), ("0", None), ("00", None), ("-1", None), ("+5", None),
        ("N/A", None), ("[N/A]", None), ("20000 MiB", None), ("20000\n20000\n", None),
        ("20000\nN/A", None), ("1e3", None), ("\u0663", None),
    ):  # fmt: skip
        assert run_mod.device_memory_bytes(text) == expected, text


def check_options_the_pass_sets_cannot_be_overridden() -> None:
    claims = run_mod.claims
    assert claims("--oom-ballast-bytes", ["--oom-ballast-bytes"])
    assert claims("--oom-ballast-bytes", ["--oom-ballast-bytes=5"])
    assert claims("--oom-ballast-bytes", ["--oom-ballast-b=5", "x"])
    assert claims("--oom-ballast-bytes", ["--oom"])
    assert not claims("--oom-ballast-bytes", ["--oom-ballast-python=x", "--window=5s", "5"])
    assert not claims("--predecessor-bundle", ["--predecessor-deployment-id=x"])
    assert not claims("--predecessor-bundle", ["--predecessor-bundles"])
    # A lone dash pair or a single dash is not an abbreviation of anything.
    assert not claims("--out", ["--", "-", "-o", "--=x", ""])
    assert run_mod.claims_any(("--a", "--b"), ["--b"])
    assert not run_mod.claims_any(("--a", "--b"), ["--c"])
    assert not claims("--sampler", ["--sample-interval=2s"])
    # A value is read as argparse reads it: joined or in the next argument, the last one given.
    value = run_mod.option_value
    assert value("--iterations", ["--iterations=5"]) == "5"
    assert value("--iterations", ["--iter", "5", "--voice=v"]) == "5"
    assert value("--iterations", ["--iterations=5", "--iterations=7"]) == "7"
    assert value("--iterations", ["--iterations=5", "--iterations"]) == ""
    assert value("--iterations", ["--iterations="]) == ""
    assert value("--iterations", ["--voice=v", "5"]) is None


def stream_arguments(machine: Machine, *more: str, without: str = "") -> list[str]:
    """Both modes against a port nothing listens on, as `--streaming-arg` values."""
    values = [
        "--stt-endpoint=127.0.0.1:9", "--stt-target=stub", "--stt-language=en",
        f"--audio={machine.tmp / 'utterance.wav'}", "--tts-endpoint=127.0.0.1:9",
        "--tts-target=stub", "--tts-language=en", "--voice=voice-a",
        f"--text-file={machine.tmp / 'segment.txt'}", "--worker-version=stub-build",
        "--iterations=3", *more,
    ]  # fmt: skip
    kept = [value for value in values if value.partition("=")[0] != without]
    return ["--streaming-python", "/bin/sh", *(f"--streaming-arg={value}" for value in kept)]


def check_streaming(machine: Machine) -> None:
    named = stream_arguments(machine)
    result = machine.run(*named, candidates=False, family="none")
    assert result.code == 0, result.done
    assert list(result.steps) == [
        "preflight", "lifecycle", "install", "doctor", "streaming-latency", "index"
    ]  # fmt: skip
    step = result.steps["streaming-latency"]
    assert (step["status"], step["result"], step["exit_code"]) == ("ok", "complete", 0)
    record = result.out / "streaming-latency/record.json"
    given = " ".join(value.partition("=")[2] for value in named[2:])
    assert result.called("slice_measure.py") == [
        f"slice_measure.py run --out {record} --source-commit {machine.commit} {given}"
    ]
    result.order("tensorplate doctor", "slice_measure.py")
    line = result.lines["streaming-latency"]
    assert line["result"] == "complete" and line["kind"] == "validation"
    assert line["metrics"]["stt.call_start_to_first_transcript.p50_ms"] == 383.0
    assert line["metrics"]["tts.segment_to_first_audio.count"] == 3
    assert line["settings"]["streaming_iterations"] == 3
    assert list(result.lines) == ["lifecycle", "doctor", "streaming-latency"]

    # With candidates the step comes after theirs.
    result = machine.run(*named)
    names = list(result.steps)
    assert names.index("streaming-latency") > names.index("qualify:tts-kokoro")
    assert names[-1] == "index" and "streaming-latency" in result.lines

    # The client's own verdict is the step's: an incomplete record fails the step and is indexed.
    partial = stream_mod.build_record(machine.commit, modes=("stt",))
    result = machine.run(*named, candidates=False, family="none", stream=partial)
    step = result.steps["streaming-latency"]
    assert (step["status"], step["result"], step["exit_code"]) == ("failed", "incomplete", 2)
    assert result.code == 1 and result.statuses("index") == ["ok"]
    assert result.lines["streaming-latency"]["result"] == "incomplete"
    assert result.lines["streaming-latency"]["checks"]["tts.sessions"] == "not run"
    assert not any(key.startswith("tts.") for key in result.lines["streaming-latency"]["metrics"])
    # Nothing answered, which is what the client wrote against a closed port: a `failed`
    # record with no session, indexed with the other subjects' lines.
    unanswered = json.loads(NO_SESSION_RECORD.read_text(encoding="utf-8"))
    result = machine.run(*named, candidates=False, family="none", stream=unanswered)
    step = result.steps["streaming-latency"]
    assert (step["status"], step["result"], step["exit_code"]) == ("failed", "failed", 1)
    assert result.statuses("index") == ["ok"], result.steps["index"]
    assert list(result.lines) == ["lifecycle", "doctor", "streaming-latency"]
    line = result.lines["streaming-latency"]
    assert line["result"] == "failed" and line["checks"] == dict.fromkeys(
        ("stt.sessions", "tts.sessions"), "none: the channel never became ready"
    )
    # A client that reaches the step's limit is stopped; the record it had saved before its
    # first session is the step's result and is indexed.
    unended = json.loads(NO_SESSION_RECORD.read_text(encoding="utf-8"))
    unended.update(stt=None, tts=None, run={"ended_by": None, "error": None})
    result = machine.run(
        *named, "--step-timeout", "streaming-latency=3", candidates=False, family="none",
        stream=index_mod.slice_record.finalize(unended), fail={"slice_measure.py": "hang"},
    )  # fmt: skip
    step = result.steps["streaming-latency"]
    assert (step["status"], step["result"], step["exit_code"]) == ("timeout", "failed", None)
    assert step["reason"] == "no end within 3 s; ended on SIGTERM"
    assert result.code == 1 and result.statuses("index") == ["ok"], result.steps["index"]
    line = result.lines["streaming-latency"]
    assert line["result"] == "failed"
    assert line["checks"] == {"stt.sessions": "not run", "tts.sessions": "not run"}
    assert line["metrics"] == {}

    # Either statement of the worker's release lets the client write a recorded record.
    build = stream_arguments(machine, "--worker-build=b", without="--worker-version")
    result = machine.run(*build, candidates=False, family="none")
    assert result.code == 0 and result.statuses("streaming-latency") == ["ok"], result.done
    # The client refuses arguments that leave a mode incomplete, here as on a machine.
    for dropped in ("--stt-target", "--voice"):
        short = stream_arguments(machine, without=dropped)
        result = machine.run(*short, candidates=False, family="none")
        step = result.steps["streaming-latency"]
        assert (step["status"], step["result"], step["exit_code"]) == ("failed", None, 2), dropped
        assert not (result.out / "streaming-latency/record.json").exists()
        assert "streaming-latency" not in result.lines
    # A client that wrote nothing leaves a step, a result of null, and no line.
    result = machine.run(*named, candidates=False, family="none", fail={"slice_measure.py": 3})
    step = result.steps["streaming-latency"]
    assert (step["status"], step["result"], step["exit_code"]) == ("failed", None, 3)
    assert "streaming-latency" not in result.lines and result.code == 1
    assert result.statuses("index") == ["ok"]

    # A record that cannot be trusted or measured nothing fails the step and is not indexed;
    # the others are kept.
    for broken, reason, needle in (
        (
            {"record_kind": "slice_measurement"},
            "`sh` exited 2",
            "the measurement record is invalid",
        ),
        (
            stream_mod.build_record(None, "synthetic"),
            "the client wrote a synthetic record, which measured nothing",
            "a synthetic record measured nothing",
        ),
    ):
        result = machine.run(*named, candidates=False, family="none", stream=broken)
        assert result.statuses("streaming-latency", "index") == ["failed", "failed"]
        assert result.steps["streaming-latency"]["reason"] == reason
        assert needle in result.steps["index"]["reason"], result.steps["index"]
        assert list(result.lines) == ["lifecycle", "doctor"]

    # The step needs the machine the other steps need.
    result = machine.run(*named, candidates=False, family="none", fail={"install.sh": 3})
    assert result.statuses("install", "streaming-latency") == ["failed", "not_run"]
    assert result.steps["streaming-latency"]["reason"] == "install is failed"
    assert not result.called("slice_measure.py") and result.code == 1


def check_the_client_reads_the_arguments_the_pass_gives(machine: Machine) -> None:
    """The measurement client's own argument handling, on the argument set this suite runs."""
    result = machine.run(*stream_arguments(machine), candidates=False, family="none")
    argv = result.steps["streaming-latency"]["commands"][0][2:]
    measured: list[dict] = []

    def measure(bindings: object, **settings: object) -> dict:
        measured.append(settings)
        return stream_mod.build_record(machine.commit)

    def client(arguments: list[str]) -> int:
        with (
            mock.patch.object(client_mod, "load_bindings", return_value=None),
            mock.patch.object(client_mod, "measure", measure),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            return client_mod.main(arguments)

    assert client(argv) == 0 and len(measured) == 1
    settings = measured.pop()
    assert settings["provenance"] == "recorded" and settings["source_commit"] == machine.commit
    assert settings["stt"] is not None and settings["tts"] is not None
    assert (settings["iterations"], settings["worker_version"]) == (3, "stub-build")
    # What the pass assumes of an option it is not given is what the client takes.
    assert client([item for item in argv if not item.startswith("--iterations")]) == 0
    settings = measured.pop()
    assert settings["iterations"] == run_mod.CLIENT_DEFAULTS["--iterations"]
    assert settings["event_timeout_ms"] == run_mod.CLIENT_DEFAULTS["--event-timeout-ms"]
    # Each argument a mode needs is one the client refuses to go without.
    for needed in ("--stt-target", "--stt-language", "--audio", "--tts-target", "--tts-language",
                   "--voice", "--text-file"):  # fmt: skip
        assert client([item for item in argv if item.partition("=")[0] != needed]) == 2, needed
    assert measured == []


def check_streaming_limit(machine: Machine) -> None:
    """Each wait at the event timeout, the audio in real time, and 300 s."""
    limit = run_mod.streaming_limit_s
    stt, both = ["--stt-endpoint=h:1"], ["--stt-endpoint=h:1", "--tts-endpoint=h:1"]
    short = f"--audio={machine.tmp / 'utterance.wav'}"
    twenty_seconds = machine.tmp / "twenty-seconds.wav"
    with wave.open(str(twenty_seconds), "wb") as clip:
        clip.setparams((1, 2, 100, 0, "NONE", "not compressed"))
        clip.writeframes(bytes(2 * 2000))
    # Two channels and three sessions of four waits each, at 2 s: 52 s. A 0.2 s clip three
    # times: 0.6 s.
    assert limit([*both, short, "--iterations=3", "--event-timeout-ms=2000"]) == 353
    # One channel and forty sessions at 1 s: 161 s. A 20 s clip forty times: 800 s.
    paced = [*stt, f"--audio={twenty_seconds}", "--iterations=40", "--event-timeout-ms=1000"]
    assert limit(paced) == 1261
    # The client's defaults, one session at 30 s: ten waits.
    assert limit(both) == 600
    # Synthesis alone sends no audio, whatever clip is named: 21 waits at 1.5 s.
    synthesis = ["--tts-endpoint=h:1", f"--audio={twenty_seconds}", "--iter=5"]
    assert limit([*synthesis, "--event-timeout-ms", "1500"]) == 332
    # A clip that cannot be read adds nothing: the client refuses it before any session.
    for unreadable in ("absent.wav", "segment.txt", "clips"):
        assert limit([*stt, f"--audio={machine.tmp / unreadable}"]) == 450, unreadable
    assert limit([*both, "--iterations=20", "--event-timeout-ms=1000", "--iterations=2"]) == 318
    # The step runs under it, and `--step-timeout` replaces it.
    named = stream_arguments(machine, "--event-timeout-ms=2000")
    assert built_pass(machine, *named).timeouts["streaming-latency"] == 353
    replaced = built_pass(machine, *named, "--step-timeout", "streaming-latency=7")
    assert replaced.timeouts["streaming-latency"] == 7
    assert built_pass(machine).timeouts["lifecycle"] == run_mod.STEP_TIMEOUTS_S["lifecycle"]


def main() -> int:
    if os.geteuid() == 0:
        raise SystemExit("run this test as a normal user: the driver refuses root")
    check_names_match_their_sources()
    check_small_guards()
    check_deploy_outcome()
    with tempfile.TemporaryDirectory() as raw_tmp:
        machine = Machine(Path(raw_tmp).resolve())
        check_full_pass(machine)
        check_modes(machine)
        check_preflight(machine)
        check_each_step_failing(machine)
        check_an_environment_file_that_turns_unreadable(machine)
        check_root_is_refused_before_anything_runs(machine)
        check_an_interrupted_pass(machine)
        check_a_command_that_cannot_be_stopped(machine)
        check_arguments(machine)
        check_ballast_per_candidate(machine)
        check_device_refusals(machine)
        check_the_ballast_interpreter_is_checked_where_it_is_used(machine)
        check_predecessor_problem(machine)
        check_streaming(machine)
        check_the_client_reads_the_arguments_the_pass_gives(machine)
        check_streaming_limit(machine)
    check_device_memory_figure()
    check_options_the_pass_sets_cannot_be_overridden()
    print("validation pass: the driver runs its steps in order and records each failure")
    return 0


if __name__ == "__main__":
    sys.exit(main())
