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
# bare top-level statement, nothing but cleanup runs `set +e`, and no
# body ends an assertion with `[[ ]]` or `(( ))`, which macOS /bin/bash
# 3.2 exempts from errexit, or with `!`, which every bash exempts. Lint
# these rules against the harness, prove each lint discriminates on a
# control, and run the real run_stage to show a failing body records no
# pass.
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
SPACE = " \t\n\x01"

def without_heredocs(text):
    """The text with heredoc bodies and terminators blanked, lines kept."""
    lines = text.split("\n")
    heredoc_end = None
    for index, line in enumerate(lines):
        if heredoc_end is not None:
            if line.strip() == heredoc_end:
                heredoc_end = None
            lines[index] = ""
            continue
        heredoc = HEREDOC.search(line)
        if heredoc and not line.lstrip().startswith("#"):
            heredoc_end = heredoc.group(1)
    return "\n".join(lines)

def mask(text):
    """Blank everything that cannot separate or join commands: quoted
    strings, command substitutions, comments, and the insides of `[[ ]]`
    and `(( ))`. A newline inside them becomes \\x01, so line numbers
    survive but a list does not split there."""
    out = list(text)
    n = len(text)

    def blank(start, end):
        for k in range(start, min(end, n)):
            out[k] = "\x01" if text[k] == "\n" else "x"

    def single(i):
        j = text.find("'", i + 1)
        return n if j < 0 else j + 1

    def double(i):
        j = i + 1
        while j < n and text[j] != '"':
            if text[j] == "\\":
                j += 2
            elif text.startswith("$(", j):
                j = subst(j)
            else:
                j += 1
        return min(j + 1, n)

    def subst(i):
        depth, j = 0, i + 1
        while j < n:
            if text[j] == "\\":
                j += 2
                continue
            if text[j] == "'":
                j = single(j)
                continue
            if text[j] == '"':
                j = double(j)
                continue
            if text[j] == "(":
                depth += 1
            elif text[j] == ")":
                depth -= 1
                if depth == 0:
                    return j + 1
            j += 1
        return n

    def test(i, closer):
        j = i + 2
        while j < n:
            if text[j] == "\\":
                j += 2
            elif text[j] == "'":
                j = single(j)
            elif text[j] == '"':
                j = double(j)
            elif text.startswith("$(", j):
                j = subst(j)
            elif text.startswith(closer, j) and text[j + 2:j + 3] in ("", ";", "&", "|", ")") + tuple(SPACE):
                return j + 2
            else:
                j += 1
        return n

    i = 0
    while i < n:
        at_word = i == 0 or text[i - 1] in " \t\n;&|(!{"
        if text[i] == "\\":
            blank(i, i + 2)
            i += 2
        elif text[i] in "'\"" or text.startswith("$(", i):
            j = single(i) if text[i] == "'" else double(i) if text[i] == '"' else subst(i)
            blank(i, j)
            i = j
        elif text[i] == "#" and at_word:
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif at_word and text.startswith(("[[", "(("), i):
            j = test(i, "]]" if text[i] == "[" else "))")
            blank(i + 2, j - 2)
            i = j
        else:
            i += 1
    return "".join(out)

def and_or_lists(masked):
    """(offset, text) of each and-or list: split at `;`, `&` and newlines,
    but not at a newline that follows `&&`, `||` or `|`."""
    found, start, i, n = [], 0, 0, len(masked)
    while i < n:
        if masked.startswith(("&&", "||"), i) or masked[i] == "|":
            i += 2 if masked.startswith(("&&", "||"), i) else 1
            while i < n and masked[i] in SPACE:
                i += 1
            continue
        if masked[i] in ";\n" or (masked[i] == "&" and masked[i - 1:i] not in (">", "<")
                                  and masked[i + 1:i + 2] != ">"):
            found.append((start, masked[start:i]))
            start = i + 1
        i += 1
    found.append((start, masked[start:]))
    return found

def line_of(masked, offset):
    return masked.count("\n", 0, offset) + masked.count("\x01", 0, offset) + 1

def bare_assertions(text):
    """Line numbers of assertions errexit never acts on under bash 3.2: a
    list, outside an if/elif/while/until condition, whose last command is
    `[[ ]]`, `(( ))` or a `!` pipeline."""
    masked = mask(without_heredocs(text))
    found = []
    for offset, rest in and_or_lists(masked):
        while True:
            stripped = rest.lstrip(SPACE)
            offset += len(rest) - len(stripped)
            rest = stripped
            lead = re.match(r"(?:then|do|else|\{|[A-Za-z_]\w*\(\)\s*\{)(?=\s|$)", rest)
            if not lead:
                break
            offset += lead.end()
            rest = rest[lead.end():]
        if not rest or re.match(r"(?:if|elif|while|until)(?=\s|$)", rest):
            continue
        operators = list(re.finditer(r"&&|\|\|", rest))
        tail_start = operators[-1].end() if operators else 0
        tail = rest[tail_start:]
        offset += tail_start + len(tail) - len(tail.lstrip(SPACE))
        tail = tail.lstrip(SPACE)
        if tail.startswith(("[[", "((")) or re.match(r"!\s", tail):
            found.append(line_of(masked, offset))
    return found

def errexit_disabled(text):
    """Line numbers of `set +e` or `set +o errexit` outside cleanup()."""
    lines = text.split("\n")
    allowed = set()
    for index, line in enumerate(lines):
        if line.startswith("cleanup() {"):
            end = next(k for k in range(index, len(lines)) if lines[k] == "}")
            allowed.update(range(index + 1, end + 2))
    masked = mask(without_heredocs(text))
    found = []
    for match in re.finditer(r"(?:^|[\s;&|{(])set\s+(?:\+[A-Za-z]*e|\+o\s+errexit)(?=\s|$)",
                             masked):
        number = line_of(masked, match.end())
        if number not in allowed:
            found.append(number)
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
  [[ -n h ]] && [[ -n i ]]
  ! test -e j
  if [[ -n k ]]; then [[ -n l ]]; fi
  true; [[ -n m ]]
  true || (( 2 ))
  if ! test -e n; then :; fi
  while ! test -e o; do break; done
  printf 'a; [[ b ]]'
  x="$(true; [[ -n p ]])" || die p
  echo q # ; [[ -n q ]]
  [[ -n r ]] || ! test -e s || die rs
  [[ -n t ]] && echo t; [[ -n u ]] || die u
}
g() { [[ -n v ]]; }
cleanup() {
  set +e
}
h() {
  set +e
  set +o errexit
  set -e
}
run_stage ok f
  run_stage nested f
run_stage masked f || true
run_stage chained f && true
if run_stage conditional f; then :; fi
run_stage continued \\
  f
"""
assert bare_assertions(control) == [2, 5, 7, 18, 19, 20, 21, 22, 23, 32], \
    bare_assertions(control)
assert errexit_disabled(control) == [37, 38], errexit_disabled(control)
assert run_stage_call_violations(control) == (6, [42, 43, 44, 45, 46]), \
    run_stage_call_violations(control)

bare = bare_assertions(source)
assert not bare, (
    "harness statements end an assertion with [[ ]], (( )) or !, which bash "
    f"3.2 errexit does not act on; add || die at lines {bare}"
)
disabled = errexit_disabled(source)
assert not disabled, (
    "set +e outside cleanup() suspends errexit, which is what stops a failing "
    f"stage body; see lines {disabled}"
)
calls, bad = run_stage_call_violations(source)
assert calls >= 18, f"expected the harness's run_stage calls, found {calls}"
assert not bad, (
    "run_stage must be called as a bare top-level statement, or errexit is "
    f"suspended in its body; see lines {bad}"
)
# The assertion lint reads the harness through mask(); a quote or
# substitution it mis-scanned would blank the rest of the file and hide
# every statement after it. The run_stage calls are last, so they show it.
assert len(re.findall(r"^run_stage ", mask(without_heredocs(source)), re.M)) == calls, \
    "the assertion lint cannot see the harness's run_stage calls"

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

# status-logs: run the real stage body against a fake Homebrew prefix and
# a fake CLI whose logs output is built from the event file the way the
# real CLI builds it. Each log file holds an earlier run's output before
# the offset launchd-start would have recorded and this run's after it.
# Every case runs as a bare run_stage call and again with errexit
# suspended, because this body must fail through its own checks.
python3 - "$harness" "$repo_root" <<'PY'
import csv
import io
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
repo_root = pathlib.Path(sys.argv[2])

def function(name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^\}\n", source, re.M | re.S)
    assert match, f"missing harness function: {name}"
    return match.group(0)

def heredoc_function(name):
    # The naive pattern above stops at a column-0 `}` inside a Python
    # heredoc; this one ends at the heredoc terminator.
    match = re.search(r"^" + name + r"\(\) \{\n.*?^PY\n\}\n", source, re.M | re.S)
    assert match, f"missing harness function: {name}"
    return match.group(0)

fake_tensorplate = r'''
import json
import os
import pathlib
import sys

root = pathlib.Path(os.environ["TP_ROOT"])
mode = os.environ["TP_MODE"]
events = root / "prefix/var/log/tensorplate/events.ndjson"
args = sys.argv[1:]
if args == ["status", "--output", "json"]:
    print(json.dumps({
        "command": "doctor" if mode == "status-wrong-command" else "status",
        "status": "ok",
        "payload": {
            "severity": "degraded" if mode == "status-degraded" else "ready",
            "agent": {"active": {
                "deployment_id": "other" if mode == "status-other-deployment" else "smoke-1",
            }},
        },
    }))
    # A well-formed document does not excuse a failing exit status.
    sys.exit(1 if mode == "status-fails" else 0)
if args == ["logs", "--component", "observability", "--tail", "100", "--output", "json"]:
    entries = []
    for line in events.read_text().splitlines():
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if entry.get("component") == "observability":
            entries.append(entry)
    entries = entries[-100:]
    if mode == "logs-no-entries":
        entries = []
    source = "/elsewhere/events.ndjson" if mode == "logs-other-source" else str(events)
    print(json.dumps({
        "command": "status" if mode == "logs-wrong-command" else "logs",
        "status": "ok",
        "payload": {
            "source": source,
            "kind": "directory" if mode == "logs-kind-directory" else "file",
            "entries": entries,
        },
    }))
    if mode == "logs-exit-2":
        print("error: tensorplate logs: no log_source.path configured", file=sys.stderr)
        sys.exit(2)
    sys.exit(0)
print(f"unexpected tensorplate invocation: {args}", file=sys.stderr)
sys.exit(64)
'''

def event(name, timestamp):
    return json.dumps({
        "schema_version": "0.1", "component": "observability", "event": name,
        "level": "info", "monotonic_timestamp_ns": timestamp,
    }) + "\n"

checks_failed = "status-logs checks failed: "
cases = {
    "pass": None,
    "brew-prefix-fails": "error: brew --prefix failed",
    "status-fails": "error: tensorplate status did not answer",
    "record-fails": "error: could not record the status and logs output",
    "status-wrong-command": checks_failed + "status_command",
    "status-degraded": checks_failed + "status_severity",
    "status-other-deployment": checks_failed + "status_active_deployment",
    "agent-log-stale-only": checks_failed + "agent_log_current_run_output",
    "observability-log-stale-only": checks_failed + "observability_log_current_run_output",
    "agent-log-missing": "cannot read agent.error.log: No such file or directory",
    "observability-log-missing": "cannot read observability.error.log: No such file or directory",
    "logs-exit-2": "error: tensorplate logs failed on the Homebrew install",
    "logs-wrong-command": checks_failed + "logs_command",
    "logs-kind-directory": checks_failed + "logs_source_is_packaged_file",
    "logs-other-source": checks_failed + "logs_source_is_packaged_file",
    "logs-no-entries": checks_failed + "logs_include_current_run",
    "logs-stale-entries-only": checks_failed + "logs_include_current_run",
}
helpers = "\n".join(function(name) for name in ("die", "note", "pass", "run_stage"))
helpers += "\n" + heredoc_function("verify_status_logs")
for mode, expected in cases.items():
    for call in ("run_stage status-logs verify_status_logs",
                 "run_stage status-logs verify_status_logs || true"):
        with tempfile.TemporaryDirectory(prefix="tp-homebrew-status-logs-") as directory:
            root = pathlib.Path(directory)
            logs = root / "prefix/var/log/tensorplate"
            logs.mkdir(parents=True)
            (root / "evidence").mkdir()
            (root / "work").mkdir()
            earlier_agent = "platform admission: row=earlier\n"
            earlier_observability = "tensorplate-observability interval=1000ms (earlier)\n"
            # Several whole events, longer than either launchd log's earlier
            # part: an offset taken from the wrong file lands inside the
            # first one and lets the later ones through as current-run.
            earlier_events = (event("service.startup", 1111) + event("service.tick", 1112)
                              + event("service.tick", 1113))
            assert len(event("service.startup", 1111)) > max(
                len(earlier_agent), len(earlier_observability))
            (logs / "agent.error.log").write_text(earlier_agent + (
                "" if mode == "agent-log-stale-only"
                else "tensorplate-agent listening on agent.sock\n"))
            (logs / "observability.error.log").write_text(earlier_observability + (
                "" if mode == "observability-log-stale-only"
                else "tensorplate-observability interval=1000ms\n"))
            (logs / "events.ndjson").write_text(earlier_events + (
                "" if mode == "logs-stale-entries-only"
                else "not a json line\n" + event("service.startup", 2222)))
            if mode == "agent-log-missing":
                (logs / "agent.error.log").unlink()
            if mode == "observability-log-missing":
                (logs / "observability.error.log").unlink()
            (root / "fake-tensorplate.py").write_text(fake_tensorplate)
            script = helpers + f'''
set -Eeuo pipefail
agent_error_log_start={len(earlier_agent)}
observability_error_log_start={len(earlier_observability)}
events_log_start={len(earlier_events)}
''' + r'''
evidence_dir="$TP_ROOT/evidence"
work_dir="$TP_ROOT/work"
stage_results="$evidence_dir/stages.tsv"
smoke_deployment_id=smoke-1
brew() {
  [[ "$*" == "--prefix" && "$TP_MODE" != "brew-prefix-fails" ]] || return 9
  printf '%s\n' "$TP_ROOT/prefix"
}
tensorplate() { python3 "$TP_ROOT/fake-tensorplate.py" "$@"; }
if [[ "$TP_MODE" == "record-fails" ]]; then
  cat() { return 1; }
fi
''' + call + "\n"
            (root / "probe.sh").write_text(script)
            env = dict(os.environ, TP_ROOT=directory, TP_MODE=mode)
            result = subprocess.run(["bash", str(root / "probe.sh")], env=env,
                                    capture_output=True, text=True)
            rows_path = root / "evidence/stages.tsv"
            rows = rows_path.read_text() if rows_path.exists() else ""
            log_path = root / "evidence/status-logs.log"
            log = log_path.read_text() if log_path.exists() else ""
            context = (mode, call, result.returncode, log, result.stderr)
            if expected is None:
                assert result.returncode == 0, context
                assert "status-logs\tpass\t" in rows, context
                summary_text = (root / "evidence/status-logs.json").read_text()
                summary = json.loads(summary_text)
                assert "/" not in summary_text, summary_text
                assert summary["deployment_id"] == "smoke-1", summary
                assert summary["logs_entries_returned"] == 4, summary
                assert summary["current_run_structured_events"] == 1, summary
                assert summary["agent_launchd_log_current_run_bytes"] == len(
                    "tensorplate-agent listening on agent.sock"), summary
                assert summary["observability_launchd_log_current_run_bytes"] == len(
                    "tensorplate-observability interval=1000ms"), summary
                # Record-first: the raw CLI documents are in the local stage log.
                assert '"command": "logs"' in log and '"command": "status"' in log, log
            else:
                assert result.returncode != 0, context
                assert "status-logs\tpass\t" not in rows, context
                assert expected in log.splitlines(), (expected, context)
    print(f"macOS status-logs: {mode}: pass")

# Stage order: status-logs observes the deployment deploy-smoke made,
# before launchd-restart replaces the processes that wrote the logs.
def call_line(text):
    return next(i for i, line in enumerate(source.splitlines()) if line == text)
assert (call_line("run_stage deploy-smoke deploy_smoke")
        < call_line("run_stage status-logs verify_status_logs")
        < call_line("run_stage launchd-restart restart_services")), "status-logs is out of order"

# launchd-start must record each log's size before either service starts,
# or this run's startup output would sit before the offset and never be
# seen. Run the real stage against a fake prefix whose services append to
# their logs when started, with each log present at a distinct size or
# absent, and require every offset to be that file's size before start.
start_services = function("start_services")
start_script = "\n".join(function(name) for name in (
    "die", "note", "pass", "run_stage", "wait_for_service", "wait_for_agent_ready",
    "start_services",
)) + r'''
set -Eeuo pipefail
evidence_dir="$TP_ROOT/evidence"
stage_results="$evidence_dir/stages.tsv"
agent_error_log_start=unset
observability_error_log_start=unset
events_log_start=unset
logs="$TP_ROOT/prefix/var/log/tensorplate"
brew() {
  case "$*" in
    --prefix) printf '%s\n' "$TP_ROOT/prefix" ;;
    "services start tensorplate-agent")
      printf 'tensorplate-agent listening\n' >>"$logs/agent.error.log" ;;
    "services start tensorplate-observability")
      printf 'tensorplate-observability started\n' >>"$logs/observability.error.log"
      printf '{"component": "observability"}\n' >>"$logs/events.ndjson" ;;
    "services list")
      printf 'tensorplate-agent started\ntensorplate-observability started\n' ;;
    *) return 9 ;;
  esac
}
stat() {
  case "$1 $2" in
    "-f %z") python3 -c 'import os, sys; print(os.path.getsize(sys.argv[1]))' "$3" ;;
    "-f %Lp") printf '600\n' ;;
    *) return 9 ;;
  esac
}
launchctl() { printf 'state = running\n'; }
sleep() { :; }
tensorplate() { printf '{}\n'; }
run_stage launchd-start start_services
printf 'offsets %s %s %s\n' \
  "$agent_error_log_start" "$observability_error_log_start" "$events_log_start"
'''
earlier_sizes = {"agent.error.log": 11, "observability.error.log": 23, "events.ndjson": 37}
for absent in (None, *earlier_sizes):
    with tempfile.TemporaryDirectory(prefix="tp-homebrew-launchd-start-") as directory:
        root = pathlib.Path(directory)
        logs = root / "prefix/var/log/tensorplate"
        logs.mkdir(parents=True)
        (root / "evidence").mkdir()
        for name, size in earlier_sizes.items():
            if name != absent:
                (logs / name).write_text("x" * (size - 1) + "\n")
        (root / "probe.sh").write_text(start_script)
        result = subprocess.run(["bash", str(root / "probe.sh")], capture_output=True,
                                text=True, env=dict(os.environ, TP_ROOT=directory))
        expected = "offsets " + " ".join(
            str(0 if name == absent else size) for name, size in earlier_sizes.items())
        context = (absent, expected, result.returncode, result.stdout, result.stderr)
        assert result.returncode == 0, context
        assert expected in result.stdout.splitlines(), context

# The formulae decide where launchd writes each service's stderr and the
# harness reads those paths; neither side can move without the other.
formula_dir = repo_root / "packaging/homebrew/Formula"
assert 'error_log_path var/"log/tensorplate/agent.error.log"' in (
    formula_dir / "tensorplate-agent.rb").read_text()
assert 'error_log_path var/"log/tensorplate/observability.error.log"' in (
    formula_dir / "tensorplate-observability.rb").read_text()
assert '"file_path": "@HOMEBREW_PREFIX@/var/log/tensorplate/events.ndjson"' in (
    repo_root / "packaging/homebrew/conf/observability.json.in").read_text()
assert '"path": "@HOMEBREW_PREFIX@/var/log/tensorplate/events.ndjson"' in (
    repo_root / "packaging/homebrew/conf/cli.json.in").read_text()
for path in ("var/log/tensorplate/agent.error.log",
             "var/log/tensorplate/observability.error.log",
             "var/log/tensorplate/events.ndjson"):
    assert path in start_services, f"launchd-start does not size {path}"
for name in ("agent.error.log", "observability.error.log", "events.ndjson"):
    assert f'since("{name}"' in source, f"status-logs does not read {name}"

# The sanitized transcript carries only allowlisted stage results; without
# the status-logs entry the stage would fall back to {"completed": true}.
with tempfile.TemporaryDirectory(prefix="tp-homebrew-transcript-") as directory:
    root = pathlib.Path(directory)
    summary = {"deployment_id": "smoke-1", "logs_source": "packaged events.ndjson"}
    (root / "status-logs.json").write_text(json.dumps(summary))
    (root / "stages.tsv").write_text(
        "stage\tstatus\tstarted_at\tfinished_at\tlog\n"
        "status-logs\tpass\t2026-01-01T00:00:00Z\t2026-01-01T00:00:01Z\tstatus-logs.log\n")
    script = heredoc_function("write_sanitized_transcript") + r'''
set -Eeuo pipefail
stage_results="$TP_ROOT/stages.tsv"
evidence_dir="$TP_ROOT"
baseline_version=0.1.2
candidate_version=0.2.1
write_sanitized_transcript
'''
    (root / "probe.sh").write_text(script)
    result = subprocess.run(["bash", str(root / "probe.sh")], capture_output=True,
                            text=True, env=dict(os.environ, TP_ROOT=directory))
    assert result.returncode == 0, result.stderr
    transcript = json.loads((root / "sanitized-transcript.json").read_text())
    stage = transcript["stages"][0]
    assert stage["stage"] == "status-logs" and stage["summary"] == summary, transcript
print("macOS status-logs order, offsets, log paths and transcript: pass")

# The macOS runbook mapping is a coverage claim: it must name all eight
# canonical stages, a run where every harness stage passes must convert
# to a passing report, and a run that fails in any stage must not. The
# harness stops at its first failing stage, so each failing log below
# ends there.
schema = json.loads((repo_root / "config/schemas/lifecycle_report.json").read_text())
canonical = schema["properties"]["stages"]["items"]["properties"]["stage"]["enum"]
runbook = (repo_root / "docs/validation/physical-row-runbooks.md").read_text()
section = runbook.split("\n## MacBook Pro M1 Pro\n", 1)[1].split("\n## ", 1)[0]
blocks = [block for block in re.findall(r"```bash\n(.*?)```", section, re.S)
          if "lifecycle-report-from-stages.sh" in block]
assert len(blocks) == 1, blocks
mappings = re.findall(r"(?<!\S)([a-z0-9-]+=[a-z0-9-]+)(?!\S)", blocks[0])
targets = {mapping.split("=", 1)[1] for mapping in mappings}
assert targets == set(canonical), (sorted(targets), canonical)
# Real names are not enough: m1-exact-row=status-logs names a real stage
# and a canonical one, and claims status-logs for a stage that observes
# neither. Changing which harness stage backs a canonical stage has to
# change this list too.
assert sorted(mappings) == sorted([
    "clean-install=install", "upgrade=upgrade", "deploy-smoke=deploy-smoke",
    "status-logs=status-logs", "rollback=rollback", "launchd-restart=restart",
    "launchd-crash-loop=crash-loop", "offline-runtime=offline",
]), mappings
harness_stages = re.findall(r"^run_stage ([a-z0-9-]+) ", source, re.M)

def convert(failing):
    with tempfile.TemporaryDirectory(prefix="tp-homebrew-report-") as directory:
        root = pathlib.Path(directory)
        rows = io.StringIO()
        writer = csv.writer(rows, delimiter="\t", lineterminator="\n")
        writer.writerow(["stage", "status", "started_at", "finished_at", "log"])
        for name in harness_stages:
            status = "fail" if name == failing else "pass"
            writer.writerow([name, status, "2026-01-01T00:00:00Z",
                             "2026-01-01T00:00:01Z", f"{name}.log"])
            if status == "fail":
                break
        (root / "stages.tsv").write_text(rows.getvalue())
        result = subprocess.run(
            ["bash", str(repo_root / "tools/validation/lifecycle-report-from-stages.sh"),
             str(root / "stages.tsv"), "macos26-m1pro-16gb", "0.2.1",
             "macos-homebrew-lifecycle", str(root / "report.json"), *mappings],
            capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        return json.loads((root / "report.json").read_text())["outcome"]

assert convert(None) == "pass"
for failing in harness_stages:
    outcome = convert(failing)
    assert outcome != "pass", f"a run that failed in {failing} converts to {outcome}"
print("macOS runbook mapping covers all eight stages: pass")
PY

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "$harness"
else
  printf 'verify_macos_homebrew_lifecycle: shellcheck not found; skipping shellcheck\n'
fi

printf 'verify_macos_homebrew_lifecycle: ok\n'
