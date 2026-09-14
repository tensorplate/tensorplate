#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
harness="${repo_root}/tools/validation/macos-homebrew-lifecycle.sh"

[[ -x "$harness" ]] || {
  printf 'FAIL: macOS Homebrew lifecycle harness is not executable\n' >&2
  exit 1
}

bash -n "$harness"
"$harness" --help >/dev/null

if grep -Fq 'if "$@"' "$harness"; then
  printf 'FAIL: lifecycle stages must not mask intermediate command failures\n' >&2
  exit 1
fi

if grep -Fq 'state/lifecycle-marker"' "$harness"; then
  printf 'FAIL: lifecycle rollback marker must not use a persistent fixed path\n' >&2
  exit 1
fi
grep -Fq 'state/lifecycle-marker.XXXXXX' "$harness"
grep -Fq 'backend_profile") != "mps_fixture"' "$harness"
grep -Fq 'mps_tensor_operation_required_for_load' "$harness"
# shellcheck disable=SC2016 # Match the literal expression in the harness.
grep -Fq 'wave-2b-macos-deploy-smoke-$(date -u +%Y%m%dT%H%M%SZ)' "$harness"
grep -Fq '"status_severity": status.get("severity") == "ready"' "$harness"
grep -Fq 'serving_parts.hostname == "127.0.0.1"' "$harness"
grep -Fq '"serving_health_state": serving_health.get("state") == "ready"' "$harness"
grep -Fq 'serving_health.get("active_model_id") == expected_deployment' "$harness"
grep -Fq '"supervision_healthy_when_configured": supervision_healthy' "$harness"
grep -Fq 'sanitized-transcript.json' "$harness"
grep -Fq 'artifact-digest.txt' "$harness"
grep -Fq 'record_artifact_digest || die' "$harness"

# The digest must be recorded after the install, not beside the formula
# pin it reads. A preflight run returns before installing anything, so a
# digest recorded at pin time would attest an archive nobody fetched.
digest_line="$(grep -n 'record_artifact_digest || die' "$harness" | cut -d: -f1)"
install_line="$(grep -n 'run_stage clean-install install_candidate_clean' "$harness" | cut -d: -f1)"
if [[ "$digest_line" -le "$install_line" ]]; then
  printf 'FAIL: the artifact digest must be recorded after the candidate install\n' >&2
  exit 1
fi

# Execute the install and digest path with a fake Homebrew inventory. The
# real helpers are extracted from the harness; no hardware or installed
# Homebrew state is touched. Equal versions deliberately have different
# source pins, which is the case a version-only assertion cannot detect.
python3 - "$harness" <<'PY'
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")

def function(name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^\}\n", source, re.M | re.S)
    assert match, f"missing harness function: {name}"
    return match.group(0)

arrays = []
for name in ("FORMULAE", "COMPONENT_FORMULAE"):
    match = re.search(r"^readonly " + name + r"=\(\n.*?^\)\n", source, re.M | re.S)
    assert match, f"missing harness array: {name}"
    arrays.append(match.group(0))
formulae = re.findall(r"^  (tensorplate[\w-]*)$", arrays[0], re.M)
assert len(formulae) == 6
install_path = source.split("run_stage clean-install install_candidate_clean\n", 1)[1]
install_path = "run_stage clean-install install_candidate_clean\n" + install_path.split(
    "run_stage packaged-closure", 1
)[0]

fake_brew = r'''
import json
import os
import pathlib
import sys

root = pathlib.Path(os.environ["TP_FAKE_BREW_ROOT"])
state_path = root / "installed.json"
state = json.loads(state_path.read_text())
formulae = json.loads((root / "formulae.json").read_text())
mode = os.environ["TP_FAKE_BREW_MODE"]
args = sys.argv[1:]
with (root / "brew-calls.jsonl").open("a") as out:
    out.write(json.dumps(args) + "\n")
name = args[-1].split("/")[-1]
if args[0] == "services":
    assert args[1] == "stop"
elif args[0] == "list":
    sys.exit(0 if name in state else 1)
elif args[0] == "info":
    print(json.dumps({"formulae": [{
        "name": name,
        "versions": {"stable": "0.2.1-rc.1"},
        "linked_keg": state.get(name, {}).get("version"),
    }]}))
elif args[0] == "deps":
    print("tensorplate")
elif args[0] == "uninstall":
    # Model the dependency order used by the real formula graph.
    if name != "tensorplate" and "tensorplate" in state:
        sys.exit(11)
    if name == "tensorplate-serving" and "tensorplate-agent" in state:
        sys.exit(12)
    if mode == "uninstall-failure" and name == "tensorplate-agent":
        sys.exit(13)
    if not (mode == "retained-keg" and name == "tensorplate-cli"):
        state.pop(name, None)
elif args[0] == "install":
    assert name == "tensorplate"
    if mode == "install-failure":
        sys.exit(14)
    for component in formulae:
        if mode == "missing-component" and component == "tensorplate-cli":
            continue
        # Homebrew reuses dependencies whose version is already current.
        if component not in state:
            state[component] = {"version": "0.2.1-rc.1", "source": "b" * 64}
    if mode == "wrong-component-version":
        state["tensorplate-serving"]["version"] = "0.2.0"
else:
    raise AssertionError(f"unexpected fake brew invocation: {args}")
state_path.write_text(json.dumps(state))
'''

helpers = "\n".join(function(name) for name in (
    "die", "note", "pass", "run_stage", "stop_candidate_services",
    "formula_is_installed", "linked_formula_version", "remove_candidate_graph",
    "stage_candidate_tap", "record_formula_graph", "install_candidate_clean",
    "record_artifact_digest",
))
script = "\n".join(arrays) + r'''
set -Eeuo pipefail
evidence_dir="$TP_FAKE_BREW_ROOT/evidence"
formula_dir="$TP_FAKE_BREW_ROOT/formulae"
tap_repo="$TP_FAKE_BREW_ROOT/tap"
tap_backup="$TP_FAKE_BREW_ROOT/backup"
stage_results="$evidence_dir/stages.tsv"
tap_name=tensorplate/tap
candidate_active=0
brew() { python3 "$TP_FAKE_BREW_ROOT/fake-brew.py" "$@"; }
''' + helpers + "\n" + install_path

cases = (
    "clean-baseline", "same-version-old-source", "uninstall-failure",
    "retained-keg", "install-failure", "missing-component", "wrong-component-version",
)
for mode in cases:
    with tempfile.TemporaryDirectory(prefix="tp-homebrew-binding-") as directory:
        root = pathlib.Path(directory)
        for name in ("evidence", "formulae", "tap/Formula", "backup"):
            (root / name).mkdir(parents=True)
        for name in formulae:
            (root / "formulae" / f"{name}.rb").write_text("candidate formula\n")
        state = {"tensorplate": {"version": "0.1.2", "source": "baseline"}}
        if mode != "clean-baseline":
            state.update({name: {"version": "0.2.1-rc.1", "source": "a" * 64}
                          for name in formulae if name != "tensorplate"})
        (root / "installed.json").write_text(json.dumps(state))
        (root / "formulae.json").write_text(json.dumps(formulae))
        (root / "fake-brew.py").write_text(fake_brew)
        pin = {"source_sha256": "b" * 64, "source_url": "https://example.invalid/candidate-B.tar.gz"}
        (root / "evidence/formula-pin.json").write_text(json.dumps(pin))
        (root / "probe.sh").write_text(script)
        env = dict(os.environ, TP_FAKE_BREW_ROOT=directory, TP_FAKE_BREW_MODE=mode)
        result = subprocess.run(["bash", str(root / "probe.sh")], env=env,
                                capture_output=True, text=True)
        artifact = root / "evidence/artifact-digest.txt"
        if mode in ("clean-baseline", "same-version-old-source"):
            assert result.returncode == 0, (mode, result.stdout, result.stderr)
            installed = json.loads((root / "installed.json").read_text())
            assert set(installed) == set(formulae), (mode, installed)
            assert all(item["source"] == pin["source_sha256"]
                       for item in installed.values()), (mode, installed)
            assert artifact.read_text() == f"{pin['source_sha256']}  {pin['source_url']}\n"
        else:
            assert result.returncode != 0, f"{mode}: unexpectedly succeeded"
            assert not artifact.exists(), f"{mode}: attested a failed candidate install"
        print(f"macOS artifact binding: {mode}: pass")
PY

grep -Fq 'run_stage m1-exact-row verify_m1_exact_row' "$harness"
grep -Fq '"family_row_not_selected": selected_row != family_row' "$harness"
grep -Fq '"family_row_16_gib_ceiling"' "$harness"
grep -Fq 'current-run agent log contains no platform admission decision' "$harness"
# shellcheck disable=SC2016 # Match the literal expression in the harness.
grep -Fq 'agent_error_log_start="$(stat -f' "$harness"
grep -Fq 'var/run/tensorplate")" == "700"' "$harness"
grep -Fq 'var/run/tensorplate/agent.sock")" == "600"' "$harness"

macos_install_doc="${repo_root}/docs/install/macos-cli.md"
uninstall_block="$(
  awk '/^brew uninstall tensorplate/ {capture=1} capture {print} /^brew untap/ {exit}' \
    "$macos_install_doc"
)"
for formula_name in \
  tensorplate \
  tensorplate-agent \
  tensorplate-backend-python-pytorch \
  tensorplate-cli \
  tensorplate-observability \
  tensorplate-serving; do
  grep -Fq "$formula_name" <<<"$uninstall_block"
done
grep -Fq 'M-series compatibility is Preview' "$macos_install_doc"
grep -Fq 'current hardware-validation target is an Apple M1 Pro' "$macos_install_doc"

printf '%s\n' '{"formulae":[{"name":"tensorplate","versions":{"stable":"0.2.1-rc.1"}}]}' |
  python3 -c 'import json,sys; f=json.load(sys.stdin)["formulae"][0]; print(f["name"], f["versions"]["stable"])' |
  grep -Fxq 'tensorplate 0.2.1-rc.1'

if "$harness" \
  --candidate-formula-dir /missing \
  --baseline-formula /missing \
  --bundle-dir /missing \
  --evidence-dir /missing 2>/dev/null; then
  printf 'FAIL: lifecycle harness ran without its mutation opt-in\n' >&2
  exit 1
fi

# run_stage writes a pass row as soon as its body returns, so a failing
# body is stopped only by errexit. That holds only while every call is a
# bare top-level statement and no body asserts with a bare `[[ ]]` or
# `(( ))`, which macOS /bin/bash 3.2 exempts from errexit. Lint both
# rules against the harness, prove each lint discriminates on a control,
# and run the real run_stage to show a failing body records no pass.
python3 - "$harness" <<'PY'
import os
import pathlib
import re
import subprocess
import sys
import tempfile

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")

def function(name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^\}\n", source, re.M | re.S)
    assert match, f"missing harness function: {name}"
    return match.group(0)

HEREDOC = re.compile(r"(?<!<)<<-?'?([A-Z_][A-Z0-9_]*)'?(?:\s|$)")

def bare_assertions(text):
    """Line numbers of `[[`/`((` statements not followed by `||` or `&&`."""
    lines = text.splitlines()
    found = []
    heredoc_end = None
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if heredoc_end is not None:
            if stripped == heredoc_end:
                heredoc_end = None
            index += 1
            continue
        heredoc = HEREDOC.search(lines[index])
        if heredoc and not stripped.startswith("#"):
            heredoc_end = heredoc.group(1)
        opener = stripped[:2]
        if opener not in ("[[", "(("):
            index += 1
            continue
        closer = re.compile(re.escape("]]" if opener == "[[" else "))") + r"(?=\s|$)")
        start = index
        statement = stripped
        while not closer.search(statement) and index + 1 < len(lines):
            index += 1
            statement += " " + lines[index].strip()
        close = closer.search(statement)
        after = statement[close.end():].strip() if close else ""
        if not after.startswith(("||", "&&")):
            found.append(start + 1)
        index += 1
    return found

def run_stage_call_violations(text):
    """Line numbers of run_stage calls that are not bare top-level statements."""
    calls = 0
    bad = []
    for number, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("#") or line.startswith("run_stage() {"):
            continue
        if not re.search(r"\brun_stage\b", line):
            continue
        calls += 1
        if (not re.fullmatch(r"run_stage [a-z0-9][a-z0-9-]* [^|&;]+", line)
                or line.rstrip().endswith("\\")):
            bad.append(number)
    return calls, bad

control = """\
f() {
  [[ -e x ]]
  [[ -e y ]] || die y
  if [[ -e z ]]; then :; fi
  [[ -n a &&
    -n b ]]
  (( 0 ))
  for ((i = 0; i < 1; i += 1)); do :; done
  [[ -n c &&
    -n d ]] ||
    die cd
  (( 1 )) || die one
  [[ -n e ]] && echo e
  python3 - <<'DOC'
[[1, 2]]
((3))
DOC
  [[ -n g ]]
}
run_stage ok f
  run_stage nested f
run_stage masked f || true
run_stage chained f && true
if run_stage conditional f; then :; fi
run_stage continued \\
  f
"""
assert bare_assertions(control) == [2, 5, 7, 18], bare_assertions(control)
assert run_stage_call_violations(control) == (6, [21, 22, 23, 24, 25]), \
    run_stage_call_violations(control)

bare = bare_assertions(source)
assert not bare, (
    "harness statements assert with a bare [[ ]] or (( )), which bash 3.2 "
    f"does not apply errexit to; add || die at lines {bare}"
)
calls, bad = run_stage_call_violations(source)
assert calls >= 18, f"expected the harness's run_stage calls, found {calls}"
assert not bad, (
    "run_stage must be called as a bare top-level statement, or errexit is "
    f"suspended in its body; see lines {bad}"
)

with tempfile.TemporaryDirectory(prefix="tp-homebrew-run-stage-") as directory:
    root = pathlib.Path(directory)
    script = "\n".join(function(name) for name in ("die", "note", "pass", "run_stage"))
    script += r'''
set -Eeuo pipefail
evidence_dir="$TP_ROOT"
stage_results="$TP_ROOT/stages.tsv"
succeeds() { touch "$TP_ROOT/succeeded"; }
fails_midway() { false; touch "$TP_ROOT/after-failure"; }
run_stage control succeeds
run_stage probe fails_midway
'''
    (root / "probe.sh").write_text(script)
    result = subprocess.run(["bash", str(root / "probe.sh")], capture_output=True,
                            text=True, env=dict(os.environ, TP_ROOT=directory))
    rows = (root / "stages.tsv").read_text() if (root / "stages.tsv").exists() else ""
    assert (root / "succeeded").exists() and "control\tpass\t" in rows, (rows, result.stderr)
    assert result.returncode != 0, "run_stage returned success for a failing body"
    assert "probe\tpass\t" not in rows, f"run_stage recorded pass for a failing body: {rows}"
    assert not (root / "after-failure").exists(), "run_stage kept running a failed body"
print("macOS errexit rules: pass")
PY

# The m1-exact-row stage reads the agent's admission line from its
# launchd log. Render that line from the format string the agent prints
# it with, and require the harness's own pattern to recover row, reason
# and memory ceiling from it, so a field added to the line cannot leave
# the stage matching nothing on hardware.
python3 - "$harness" "${repo_root}/agent/src/main.rs" <<'PY'
import pathlib
import re
import sys

harness = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
agent = pathlib.Path(sys.argv[2]).read_text(encoding="utf-8")

literal = re.search(r'"(platform admission:(?:[^"\\]|\\.)*)"', agent, re.S)
assert literal, "agent/src/main.rs no longer prints a `platform admission:` line"
template = re.sub(r"\\\n\s*", "", literal.group(1))
assert "\n" not in template and "\\" not in template, template

findall = re.search(r"re\.findall\(\s*((?:r\"[^\"]*\"\s*)+),\s*agent_log,", harness)
assert findall, "harness no longer parses the admission line with re.findall"
pattern = "".join(re.findall(r'r"([^"]*)"', findall.group(1)))

row, reason, memory = "macos26-m1pro-16gb", "none", "17179869184"
for posture, posture_from, evidence in (
    ("validated_row_required", "row", "validated"),
    ("technical_prerequisites", "operator",
     "unvalidated (admitted on technical prerequisites)"),
    ("none", "none", "none"),
):
    positional = iter((row, reason, memory))
    named = {"posture": posture, "posture_from": posture_from, "evidence": evidence}

    def fill(match):
        name = match.group(1)
        return next(positional) if not name else named[name]

    line = re.sub(r"\{(\w*)\}", fill, template)
    assert next(positional, None) is None, f"unexpected placeholders in {template!r}"
    log = (
        "platform registry: rows=12 supported=4 roadmap_targets=2 dir=registry\n"
        + line
        + "\ntensorplate-agent listening on agent.sock\n"
    )
    matches = re.findall(pattern, log)
    assert matches == [(row, reason, memory)], (pattern, line, matches)
print("macOS admission line contract: pass")
PY

# launchd-crash-loop must see a config error written after it broke the
# config, not one left in the append-only agent log by an earlier run.
# Run the real stage body against a fake Homebrew prefix and launchd.
# Every case also runs from a context where errexit is suspended, so each
# failure is shown to come from the stage's own explicit check.
python3 - "$harness" <<'PY'
import os
import pathlib
import re
import subprocess
import sys
import tempfile

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")

def function(name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^\}\n", source, re.M | re.S)
    assert match, f"missing harness function: {name}"
    return match.group(0)

fake_restart = r'''
import json
import os
import pathlib

root = pathlib.Path(os.environ["TP_ROOT"])
config = root / "prefix/etc/tensorplate/agent.json"
log = root / "prefix/var/log/tensorplate/agent.error.log"
try:
    json.loads(config.read_text())
    line = "tensorplate-agent listening on agent.sock\n"
except ValueError:
    line = "" if os.environ["TP_MODE"] == "agent-silent" else "config error: agent.json is not valid JSON\n"
with log.open("a") as handle:
    handle.write(line)
'''

helpers = "\n".join(function(name) for name in (
    "die", "note", "pass", "run_stage", "wait_for_service", "wait_for_agent_ready",
    "exercise_crash_loop",
))
stubs = r'''
set -Eeuo pipefail
evidence_dir="$TP_ROOT/evidence"
work_dir="$TP_ROOT/work"
stage_results="$evidence_dir/stages.tsv"
brew() {
  case "$*" in
    --prefix) printf '%s\n' "$TP_ROOT/prefix" ;;
    "services list") printf 'tensorplate-agent started\n' ;;
    "services restart tensorplate-agent") python3 "$TP_ROOT/fake-restart.py" ;;
    *) return 9 ;;
  esac
}
stat() {
  [[ "$1 $2" == "-f %z" ]] || return 9
  python3 -c 'import os, sys; print(os.path.getsize(sys.argv[1]))' "$3"
}
launchctl() { printf 'state = running\n'; }
sleep() { :; }
tensorplate() { printf '{}\n'; }
'''

prior_config_error = "config error: agent.json is not valid JSON\n"
cases = {
    # mode: (earlier-run log content, expected failure message or None)
    "config-error-this-run": ("tensorplate-agent listening on agent.sock\n", None),
    "config-error-earlier-run-only": (
        prior_config_error, "agent logged no config error after its config was broken"),
    "agent-silent": ("", "agent logged no config error after its config was broken"),
    "agent-log-missing": (
        None, "cannot size the agent launchd error log before breaking the config"),
}
for mode, (earlier, message) in cases.items():
    fake_mode = "agent-silent" if mode in ("config-error-earlier-run-only", "agent-silent") else mode
    for call in ("run_stage launchd-crash-loop exercise_crash_loop",
                 "run_stage launchd-crash-loop exercise_crash_loop || true"):
        with tempfile.TemporaryDirectory(prefix="tp-homebrew-crash-loop-") as directory:
            root = pathlib.Path(directory)
            for name in ("evidence", "work", "prefix/etc/tensorplate",
                         "prefix/var/log/tensorplate"):
                (root / name).mkdir(parents=True)
            original_config = '{"listen": "agent.sock"}\n'
            (root / "prefix/etc/tensorplate/agent.json").write_text(original_config)
            if earlier is not None:
                (root / "prefix/var/log/tensorplate/agent.error.log").write_text(earlier)
            (root / "fake-restart.py").write_text(fake_restart)
            (root / "probe.sh").write_text(helpers + stubs + call + "\n")
            env = dict(os.environ, TP_ROOT=directory, TP_MODE=fake_mode)
            result = subprocess.run(["bash", str(root / "probe.sh")], env=env,
                                    capture_output=True, text=True)
            rows_path = root / "evidence/stages.tsv"
            rows = rows_path.read_text() if rows_path.exists() else ""
            log_path = root / "evidence/launchd-crash-loop.log"
            log = log_path.read_text() if log_path.exists() else ""
            context = (mode, call, result.returncode, log, result.stderr)
            if message is None:
                assert result.returncode == 0, context
                assert "launchd-crash-loop\tpass\t" in rows, context
                restored = (root / "prefix/etc/tensorplate/agent.json").read_text()
                assert restored == original_config, context
            else:
                assert result.returncode != 0, context
                assert "launchd-crash-loop\tpass\t" not in rows, context
                assert f"error: {message}" in log, context
    print(f"macOS launchd crash-loop: {mode}: pass")
PY

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "$harness"
else
  printf 'verify_macos_homebrew_lifecycle: shellcheck not found; skipping shellcheck\n'
fi

printf 'verify_macos_homebrew_lifecycle: ok\n'
