#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run a validation pass over a stubbed tree and machine, then break each step once.

The stubbed tree is a real git repository with fakes at the paths the driver
runs, and a directory of fake commands shadows `sudo` and everything run
through it. The fake harness and qualify tool replay the lifecycle report and
the candidate records filed on 2026-10-01. Doctor's output is synthetic: no
recorded run carries the runner-profile findings yet.
"""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import time
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
import validation_index as index_mod  # noqa: E402
import validation_pass_run as run_mod  # noqa: E402

DEB_VERSION = "0.3.1~dev.1"
RUNNER_FINDINGS = ("runner_profiles", "runner_profile_dependencies", "runner_launch_environment")

# Every fake logs its call, then exits with the code FAKE_FAIL gives its name, if any.
PRELUDE = """#!/bin/sh
name=${FAKE_NAME:-$(basename "$0")}
printf '%s %s\\n' "$name" "$*" >>"$FAKE_CALLS"
code=$(sed -n "s/^$name //p" "$FAKE_FAIL" | head -1)
"""
EXIT = '[ -z "$code" ] || exit "$code"\n'
FAKES = {
    "sudo": 'exec "$@"\n',
    "kill": EXIT + 'exec /bin/kill "$@"\n',
    "systemctl": EXIT,
    "sysctl": EXIT,
    "sync": EXIT,
    "apt-get": EXIT,
    "python3.12": EXIT,
    "dpkg-deb": EXIT + f"echo '{DEB_VERSION}-1'\n",
    "tensorplate": """case " $* " in *" --output json "*) ;; *) exit 64 ;; esac
case "$1" in
  status) name=status ;; doctor) name=doctor ;; bundle) name=provision ;; deploy) name=deploy ;;
esac
code=$(sed -n "s/^$name //p" "$FAKE_FAIL" | head -1)
case "$name" in
  status)
    [ -z "$code" ] || { echo '{"status":"ok","payload":{"agent":{"available":false}}}'; exit 0; }
    echo '{"status":"ok","payload":{"agent":{"available":true}}}' ;;
  doctor) cat "$FAKE_DOCTOR"; exit "${code:-0}" ;;
  provision)
    [ -z "$code" ] || exit "$code"
    printf '{"status":"ok","payload":{"name":"%s","path":"%s"}}\\n' "$3" "$FAKE_BUNDLES/$3" ;;
  deploy)
    if [ -n "$code" ]; then
      echo '{"status":"ok","payload":{"phase":"failed","failure":{"error_code":"load_failed","message":"synthetic"}}}'
      exit "$code"
    fi
    printf '{"status":"ok","payload":{"phase":"active","bundle_digest":"%s"}}\\n' "$(cat "$2/bundle-digest")" ;;
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
    "tools/release/stage-debian-changelog.sh": EXIT + 'echo "$1" >.staged-version\n',
    "packaging/scripts/build-deb.sh": EXIT
    + """echo "build-env $DEB_BUILD_PROFILES $TP_SPEECH_RUNTIME_WHEELHOUSE" >>"$FAKE_CALLS"
for package in tensorplate-speech-runtime tensorplate-speech-runtime-base; do
  : >"../${package}_$(cat .staged-version)-1_amd64.deb"
done
""",
    "install.sh": EXIT + 'eval "${FAKE_INSTALL_HOOK:-:}"\n',
}


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
            (self.bundles / name).mkdir(parents=True)
            digest = json.loads(record.read_text(encoding="utf-8"))["lifecycle"]["deploy"]
            (self.bundles / name / "bundle-digest").write_text(digest["bundle_digest"])
            (self.bundles / name / "filed-record").write_text(str(record))
        (tmp / "clips").mkdir()
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
    ) -> Result:
        self.runs += 1
        out = self.tmp / f"pass-{self.runs}"
        scratch = {
            name: self.tmp / f"{name}-{self.runs}" for name in ("calls", "fail", "doctor", "pids")
        }
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
        if "--confirm" not in arguments:
            command += ["--confirm", "RESET-TENSORPLATE"]
        done = subprocess.run(
            [*command, *arguments], capture_output=True, text=True, env=environment, check=False
        )
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
        "--qualify-arg=--window=5s", "--oom-ballast-bytes", "123",
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
    # The record's own verdict stays beside the step's status.
    assert result.steps["lifecycle"]["result"] == "pass"
    assert [result.steps[name]["result"] for name in qualify] == ["fail", "fail"]
    assert result.steps["doctor"]["result"] is None
    assert result.report["finished"] is True and result.report["source_commit"] == machine.commit
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
        f"--staging-dir {result.staging / 'stt-whisper'}",
    ):  # fmt: skip
        assert expected in whisper, (expected, whisper)
    assert "--clips" not in kokoro and f"--staging-dir {result.staging / 'tts-kokoro'}" in kokoro
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
    machine.environment_file.write_text("TMPDIR=/var/tmp\n", encoding="utf-8")
    result = machine.run(*stated)
    assert list(result.steps)[3:6] == ["speech-family", "agent-environment", "doctor"]
    assert len(result.called("tensorplate status")) == 4
    assert result.statuses("agent-environment", "provision:stt-whisper") == ["ok", "ok"]
    written = machine.environment_file.read_text(encoding="utf-8")
    assert written == "TMPDIR=/var/tmp\n" + "".join(f"{variable}\n" for variable in variables)
    result.order("apt-get", "systemctl restart tensorplate-agent", "tensorplate doctor")
    doctor = result.lines["doctor"]
    assert doctor["checks"]["interpreter_override"] == "present" and doctor["result"] == "fail"
    assert doctor["settings"] == {
        "speech_family": "host-built",
        "agent_environment": "LD_LIBRARY_PATH,TP_PYTHON_PYTORCH_EXECUTABLE",
    }
    machine.environment_file.unlink()
    result = machine.run(*stated[:2], family="in-assets")
    assert list(result.steps)[2:5] == ["install", "agent-environment", "doctor"]
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


def check_root_is_refused_before_anything_runs(machine: Machine) -> None:
    spec = importlib.util.spec_from_file_location("validation_pass_cli", TOOL)
    cli = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(cli)
    args = cli.parse_args(
        [
            "run",
            "--assets-dir",
            str(machine.assets),
            "--source-commit",
            machine.commit,
            "--out-dir",
            str(machine.tmp / "as-root"),
            "--confirm",
            "RESET-TENSORPLATE",
            "--speech-family",
            "none",
            "--repo-root",
            str(machine.repo),
            "--session",
            "s",
            "--row",
            ROW,
            "--machine",
            "m",
            "--build",
            "b",
        ]  # fmt: skip
    )
    a_pass = run_mod.Pass(args, cli.envelope_from(args))
    with mock.patch.object(run_mod.os, "geteuid", return_value=0):
        try:
            a_pass.preflight({})
        except run_mod.StepFailed as exc:
            assert "run as a normal user" in str(exc)
        else:
            raise AssertionError("preflight ran as root")


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
    no_pass("--speech-family none installs none", family="none")
    no_pass("a candidate needs --staging-dir", staging=False)
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
    assert (ROOT / "packaging/scripts/install.sh").is_file()
    assert (ROOT / "packaging/speech-runtime/build-environment.py").is_file()
    assert (ROOT / "packaging/speech-runtime/lock").is_dir()


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
        check_a_command_that_cannot_be_stopped(machine)
        check_arguments(machine)
    print("validation pass: the driver runs its steps in order and records each failure")
    return 0


if __name__ == "__main__":
    sys.exit(main())
