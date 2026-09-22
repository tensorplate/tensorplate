#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# packaging: shared durable-state preservation guard verifier.
#
# The rollback stage of the Ubuntu x86_64 cloud harness and of the Jetson
# harness make the same claim in the same words -- that the documented
# rollback preserves the operator's durable state, file by file, against
# digests taken from the stopped agent's own directory before the move --
# and they make it with the same five functions: privileged_sha256,
# state_manifest, manifest_digest, capture_state_manifest and
# check_state_preserved.
#
# Two harnesses making one claim by copied code drift. Only one of them
# is driven against a stubbed appliance for any given regression, so a
# fix or a tightening applied to one leaves the other's evidence quietly
# weaker while still reading as if it were not -- and Jetson and the L4
# are two of the four Production rows whose evidence the release rests
# on. Nothing else reads both files: each harness verifier reads only its
# own harness.
#
# So this binds them. Each of the five functions must be defined exactly
# once in each harness, in any spelling bash accepts, and the two bodies
# must be byte-identical. Every harness variable those bodies touch must
# be bound on exactly one line in each harness, and that line must be the
# same in both. The variables are DERIVED from the bodies rather than
# listed here, so a body that starts reading something new cannot outgrow
# the binding silently: the new name is required to be bound too, in both
# files, or this fails.
#
# What that buys: verify_ubuntu_l4_cloud_lifecycle.sh drives this code
# against a stubbed appliance through every way the rollback can destroy
# state, and this says the Jetson harness holds that same code. The
# coverage is one harness's, and it reaches both.
#
# A binding with a blind spot is worse than none, because it reads as
# assurance. So after binding the real pair, this plants each kind of
# drift it claims to refuse into copies of the two harnesses and runs
# itself against them: a redefinition in every spelling, a variable read
# through every expansion form, a binding shadowed or respelled. Each must
# be refused for its own reason and no other, and the untouched copies
# must pass -- so deleting any one of the rules below fails this run.
#
# It runs in the core group rather than the harness group because it
# drives no appliance: it reads two files, and the planted cases reread
# copies of them. A copy-paste divergence should fail in the fast job,
# not fifteen minutes into the slow one.
#
# The comments ABOVE each function are deliberately not compared. They
# name facts that legitimately differ -- the L4's boot-bound machine-type
# record has no Jetson counterpart, and one says host where the other
# says device. The bodies are what make the claim.
#
# What a text scan cannot see: a definition or an assignment built at run
# time, by eval or by a file sourced under another name. Neither harness
# uses eval. Both source the same lifecycle-stages.sh, so anything it
# defines applies to both alike and is not drift between them.
#
# Usage: verify_lifecycle_state_guard.sh [CLOUD_HARNESS JETSON_HARNESS]
# With no arguments it binds the two harnesses in this checkout and then
# runs the planted cases; with two, it binds that pair and nothing else.
#
# Runs under bash 3.2: no readarray, no associative arrays. Statuses are
# captured explicitly rather than left to errexit, which bash suspends
# inside a function called from a tested context.

set -Eeuo pipefail

repo_root="$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)"
if (($# == 0)); then
  cloud="${repo_root}/tools/validation/ubuntu-l4-cloud-lifecycle.sh"
  jetson="${repo_root}/tools/validation/jetson-lifecycle.sh"
  planted_cases=1
elif (($# == 2)); then
  cloud="$1"
  jetson="$2"
  planted_cases=0
else
  printf 'usage: %s [CLOUD_HARNESS JETSON_HARNESS]\n' "$(basename "$0")" >&2
  exit 2
fi

guard_functions="privileged_sha256
state_manifest
manifest_digest
capture_state_manifest
check_state_preserved"

failures=0
checks=0
check() {
  local what="$1" expected="$2" actual="$3"
  checks=$((checks + 1))
  if [[ "$expected" == "$actual" ]]; then
    printf '  ok   %s\n' "$what"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$what" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

finish() {
  if ((failures == 0)); then
    printf '\nverify_lifecycle_state_guard: ok (%s checks)\n' "$checks"
  else
    printf '\n%s of %s check(s) failed\n' "$failures" "$checks"
  fi
  exit "$failures"
}

for harness in "$cloud" "$jetson"; do
  check "$(basename "$harness") is there" yes \
    "$([[ -f "$harness" ]] && echo yes || echo no)"
done
if ((failures > 0)); then
  finish
fi

td="$(mktemp -d)"
trap 'rm -rf "$td"' EXIT

# --- reading a harness.
#
# `functions` finds every definition of each of the five, cuts out the
# one this binding compares, and derives the variables each body
# touches. `bindings` then finds, for each of those variables, every line
# outside the five bodies that could give it a value.
#
# A body runs from its `name() {` line to the next line that is exactly
# `}`, which is this repo's shell style throughout: every function opens
# at column zero and closes at column zero, and nothing inside these five
# closes a brace there. That assumption is not taken on trust -- each
# extracted body is handed to `bash -n` below, so a cut that ended in the
# wrong place fails here rather than silently comparing fragments.
#
# The program is single-quoted on purpose: every $ in it belongs to the
# regular expressions, not to the shell.
cat >"${td}/analyse.py" <<'PY'
import re
import sys

IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
# Set per command inside the bodies (IFS= read, LC_ALL=C sort) or
# discarded (read -r name _), never read from the harness.
SHELL_OWN = {"IFS", "LC_ALL", "_"}


def harness_lines(path):
    return open(path, encoding="utf-8").read().split("\n")


def is_comment(line):
    return line.lstrip().startswith("#")


def definition_sites(lines, name):
    """Every place bash would define NAME as a function, in any spelling.

    `name() {`, `name () {`, `name()` with the brace on the next line,
    `function name {`, `function name() {`, a one-liner, a definition
    after another command on the same line, and one nested in another
    function's body that only takes effect when that function runs. Bash
    runs whichever definition it met last, so a second one in ANY of
    these forms silently replaces the copy this binding compares.
    Counting only the canonical `name() {` line would miss all of them.
    A mention inside a heredoc or a string counts too: that fails loudly,
    and a missed definition would not.
    """
    n = re.escape(name)
    pattern = re.compile(
        r"(?:^|[\s;&|(){}])(?:function\s+" + n + r"(?![\w-])|" + n + r"\s*\(\s*\))"
    )
    sites = []
    for number, line in enumerate(lines, 1):
        if is_comment(line):
            continue
        sites.extend([number] * len(pattern.findall(line)))
    return sites


def body_spans(lines, names):
    spans = {}
    for name in names:
        opens = [i for i, line in enumerate(lines) if line == f"{name}() {{"]
        if len(opens) != 1:
            continue
        start = opens[0]
        closes = [j for j in range(start + 1, len(lines)) if lines[j] == "}"]
        if closes:
            spans[name] = (start, closes[0])
    return spans


def strip_quoted(code):
    return re.sub(r"\"(?:[^\"\\]|\\.)*\"|'[^']*'", "''", code)


def declared_local(code):
    """Names a body declares with local, or declare/typeset without -g."""
    names = set()
    code = strip_quoted(code)
    for options, rest in re.findall(
        r"(?:^|[;&|])[ \t]*(?:local|declare|typeset)\b((?:[ \t]+-[A-Za-z]+)*)([^\n;&|]*)",
        code,
        re.M,
    ):
        if "g" in options:
            continue
        rest = re.sub(r"\$\([^()]*\)", "''", rest)
        for token in rest.split():
            match = re.fullmatch("(" + IDENT + ")(?:=.*)?", token)
            if match:
                names.add(match.group(1))
    return names


def touched(code):
    """Every variable a body reads or assigns, whatever the spelling.

    Deliberately generous: a name taken that is not really harness state
    is then required to be bound in both files, which fails loudly and is
    fixed by declaring it local. A name missed is a body that reads
    something the binding never looks at.
    """
    found = set()
    # Every parameter expansion, whatever follows the name: ${NAME},
    # ${NAME-}, ${NAME:-x}, ${NAME/a/b}, ${NAME[@]}, ${#NAME}, ${!NAME}.
    found |= set(re.findall(r"\$\{[#!]?(" + IDENT + ")", code))
    # And the bare form, $NAME.
    found |= set(re.findall(r"\$(" + IDENT + ")", code))
    # Names arithmetic reads without a $: (( )), $(( )), $[ ], let, the
    # operands of -eq/-ne/-lt/-le/-gt/-ge, and array subscripts.
    arithmetic = re.findall(r"\(\((.*?)\)\)", code, re.S)
    arithmetic += re.findall(r"\$\[(.*?)\]", code, re.S)
    arithmetic += re.findall(r"(?:^|[\s;&|(])let\s+([^\n;&|]*)", code)
    arithmetic += re.findall(r"\$\{" + IDENT + r"\[([^\]]*)\]", code)
    for expression in arithmetic:
        found |= set(re.findall(r"(?<![\w$#!{])(" + IDENT + ")", expression))
    found |= set(re.findall(r"(?<![\w$#!{])(" + IDENT + r")\s+-(?:eq|ne|lt|le|gt|ge)\b", code))
    found |= set(re.findall(r"\s-(?:eq|ne|lt|le|gt|ge)\s+(" + IDENT + r")\b", code))
    # A name handed to -v: test -v NAME, printf -v NAME.
    found |= set(re.findall(r"(?:^|\s)-v\s+['\"]?(" + IDENT + ")", code))
    # Every assignment: NAME=, NAME+=, NAME[i]=, and the prefix form.
    found |= set(re.findall(r"(?<![\w$#!{/.-])(" + IDENT + r")(?:\[[^\]]*\])?\+?=(?!=)", code))
    return found


def functions(path, out_dir, wanted):
    lines = harness_lines(path)
    names = wanted.split()
    spans = body_spans(lines, names)
    derived = set()
    with open(f"{out_dir}/definitions", "w", encoding="utf-8") as record:
        for name in names:
            record.write(f"{name} {len(definition_sites(lines, name))}\n")
    for name, (start, end) in spans.items():
        body = lines[start:end + 1]
        with open(f"{out_dir}/body.{name}", "w", encoding="utf-8") as handle:
            handle.write("\n".join(body) + "\n")
        code = "\n".join(line for line in body if not is_comment(line))
        derived |= touched(code) - declared_local(code) - SHELL_OWN
    with open(f"{out_dir}/variables", "w", encoding="utf-8") as handle:
        for name in sorted(derived):
            handle.write(name + "\n")


def bindings(path, out_dir, wanted, variables_path):
    """Every line outside the five bodies that names a variable unexpanded.

    That is every way a harness can give it a value: an assignment in any
    spelling (readonly, declare, export, +=), a local in a caller that
    shadows it for the bodies under bash's dynamic scope, a read or
    printf -v into it, an unset. An expansion -- $NAME, ${NAME...} -- only
    reads it and is left out.
    """
    lines = harness_lines(path)
    inside = set()
    for start, end in body_spans(lines, wanted.split()).values():
        inside |= set(range(start, end + 1))
    names = [n for n in open(variables_path, encoding="utf-8").read().split("\n") if n]
    with open(f"{out_dir}/bindings", "w", encoding="utf-8") as record:
        for name in names:
            pattern = re.compile(r"(?<![\w$#!{])" + re.escape(name) + r"(?!\w)")
            found = [
                line
                for number, line in enumerate(lines)
                if number not in inside and not is_comment(line) and pattern.search(line)
            ]
            record.write(f"{name} {len(found)}\n")
            with open(f"{out_dir}/binding.{name}", "w", encoding="utf-8") as handle:
                handle.write("\n".join(found))


if sys.argv[1] == "functions":
    functions(*sys.argv[2:])
elif sys.argv[1] == "bindings":
    bindings(*sys.argv[2:])
else:
    raise SystemExit(f"unknown mode {sys.argv[1]}")
PY

analyse() {
  local status=0
  python3 "${td}/analyse.py" "$@" || status=$?
  if ((status != 0)); then
    printf 'verify_lifecycle_state_guard: could not read %s (exit %s)\n' "$3" "$status" >&2
    exit 1
  fi
}

recorded() {
  # The number a `definitions` or `bindings` record holds for a name.
  awk -v name="$2" '$1 == name { print $2 }' "$1"
}

parses() {
  local status=0
  if [[ ! -f "$1" ]]; then
    printf 'missing'
    return 0
  fi
  bash -n "$1" 2>/dev/null || status=$?
  if ((status == 0)); then printf 'ok'; else printf 'unparsable'; fi
}

mkdir -p "${td}/cloud" "${td}/jetson"
analyse functions "$cloud" "${td}/cloud" "$guard_functions"
analyse functions "$jetson" "${td}/jetson" "$guard_functions"

# --- the five functions: one definition each, the same code in both.
#
# A function defined zero times, twice in any spelling, or only in a form
# the cut cannot read is a failure rather than an empty or a partial
# comparison: two harnesses that have both lost the guard must not read
# as two harnesses that agree, and a second definition is the one bash
# runs.
while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  check "${name} is defined exactly once in each harness" "1 1" \
    "$(printf '%s %s' "$(recorded "${td}/cloud/definitions" "$name")" \
       "$(recorded "${td}/jetson/definitions" "$name")")"
  # The cut is a whole function, not a fragment of one.
  check "${name}'s body parses on its own in both" "ok ok" \
    "$(printf '%s %s' "$(parses "${td}/cloud/body.${name}")" "$(parses "${td}/jetson/body.${name}")")"
  # And it is the same function in both harnesses, byte for byte.
  check "${name} is the same code in both harnesses" same \
    "$(if cmp -s "${td}/cloud/body.${name}" "${td}/jetson/body.${name}"; then
         printf 'same'
       else
         printf 'different: %s' \
           "$(diff "${td}/cloud/body.${name}" "${td}/jetson/body.${name}" 2>&1 | head -20 | tr '\n' ' ')"
       fi)"
done <<<"$guard_functions"

# --- every harness variable the shared bodies touch is bound the same way.
#
# Identical bodies still make different claims if the names in them mean
# different things: a STATE_ASIDE_DIR pointing somewhere else in one
# harness would compare the wrong directory while reading as the same
# check. The names are read out of the bodies, in both harnesses, rather
# than listed here, so this cannot fall behind them -- upper case or
# lower, whatever expansion form reads them.
cat "${td}/cloud/variables" "${td}/jetson/variables" | LC_ALL=C sort -u >"${td}/variables"
guard_variables="$(cat "${td}/variables")"

check "the shared bodies read harness state at all" yes \
  "$([[ -n "$guard_variables" ]] && echo yes || echo no)"

analyse bindings "$cloud" "${td}/cloud" "$guard_functions" "${td}/variables"
analyse bindings "$jetson" "${td}/jetson" "$guard_functions" "${td}/variables"

while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  check "${name} is bound on exactly one line in each harness" "1 1" \
    "$(printf '%s %s' "$(recorded "${td}/cloud/bindings" "$name")" \
       "$(recorded "${td}/jetson/bindings" "$name")")"
  check "${name} is bound to the same value in both" \
    "$(cat "${td}/cloud/binding.${name}")" "$(cat "${td}/jetson/binding.${name}")"
done <<<"$guard_variables"

if ((planted_cases == 0)); then
  finish
fi

# --- the planted cases.
#
# Each case copies the two harnesses, plants one kind of drift, runs this
# verifier against the copies, and requires it to fail with exactly the
# named checks and no others. Exactly, because a case that failed for a
# different reason -- a plant that broke the parse, say -- would keep
# passing after the rule it exists for was deleted. The first two cases
# are controls: untouched copies, and copies changed the same way in
# both, must pass, so the refusals below are the drift and not the
# copying.
#
# The planted text is bash, but it is only ever read by this verifier,
# never run.
cat >"${td}/plant.py" <<'PY'
import sys

# A redefinition of manifest_digest that restores the first-space split
# the shared body was changed away from.
OLD = r"""printf '%s\n' "$2" | awk -v name="$1" '$1 == name { print $2 }'"""

DEFINED = "manifest_digest is defined exactly once in each harness"
PARSES = "manifest_digest's body parses on its own in both"
SAME = "manifest_digest is the same code in both harnesses"


def bound(name):
    return [
        f"{name} is bound on exactly one line in each harness",
        f"{name} is bound to the same value in both",
    ]


ASIDE = 'readonly STATE_ASIDE_DIR="/var/lib/tensorplate/state.bak"'
RECOVERY = 'readonly STATE_RECOVERY_DIR="/var/lib/tensorplate/state.recovery"'
LIMIT = "readonly STATE_RECOVERY_LIMIT=3"


def reads(statement, definition, name):
    """Both bodies read a variable only the cloud harness defines."""
    return (
        [
            ("both", "into", "check_state_preserved", f"  {statement}"),
            ("cloud", "after_line", ASIDE, definition),
        ],
        bound(name),
    )


CASES = [
    ("unchanged", "the untouched copies bind", [], []),
    (
        "changed-alike",
        "a variable both bodies read, bound the same way in both, binds",
        [
            ("both", "into", "check_state_preserved", '  : "${STATE_RECOVERY_DIR-}"'),
            ("both", "after_line", ASIDE, RECOVERY),
        ],
        [],
    ),
    # One definition each, in any spelling.
    (
        "redefined-one-line",
        "a one-line redefinition after the checked copy",
        [("jetson", "after", "manifest_digest", f"manifest_digest() {{ {OLD}; }}")],
        [DEFINED],
    ),
    (
        "redefined-function-keyword",
        "a redefinition with the function keyword",
        [("jetson", "after", "manifest_digest", f"function manifest_digest {{ {OLD}; }}")],
        [DEFINED],
    ),
    (
        "redefined-keyword-and-parentheses",
        "a redefinition with the keyword and spaced parentheses",
        [("jetson", "after", "manifest_digest", f"function manifest_digest () {{ {OLD}; }}")],
        [DEFINED],
    ),
    (
        "redefined-brace-on-next-line",
        "a redefinition whose brace opens on the next line",
        [("jetson", "after", "manifest_digest", f"manifest_digest ()\n{{ {OLD}; }}")],
        [DEFINED],
    ),
    (
        "redefined-after-a-command",
        "a redefinition after another command on the same line",
        [("jetson", "after", "manifest_digest", f": && manifest_digest() {{ {OLD}; }}")],
        [DEFINED],
    ),
    (
        "redefined-at-run-time",
        "a redefinition nested in the stage that calls it",
        [("jetson", "into", "stage_rollback", f"  manifest_digest() {{ {OLD}; }}")],
        [DEFINED],
    ),
    (
        "removed",
        "the one definition renamed away",
        [("jetson", "replace_line", "manifest_digest() {", "manifest_digest_unused() {")],
        [DEFINED, PARSES, SAME],
    ),
    (
        "respelled",
        "the one definition respelled where the cut cannot read it",
        [("jetson", "replace_line", "manifest_digest() {", "function manifest_digest {")],
        [PARSES, SAME],
    ),
    # A variable the bodies read, whatever the expansion form.
    ("reads-default-expansion", "a body reading ${NAME-}", *reads(': "${STATE_RECOVERY_DIR-}"', RECOVERY, "STATE_RECOVERY_DIR")),
    ("reads-length", "a body reading ${#NAME}", *reads(': "${#STATE_RECOVERY_DIR}"', RECOVERY, "STATE_RECOVERY_DIR")),
    ("reads-indirection", "a body reading ${!NAME}", *reads(': "${!STATE_RECOVERY_DIR}"', RECOVERY, "STATE_RECOVERY_DIR")),
    ("reads-lower-case-braced", "a body reading a lower-case ${name%/}", *reads(': "${state_recovery_dir%/}"', 'state_recovery_dir="/var/lib/tensorplate/state.recovery"', "state_recovery_dir")),
    ("reads-lower-case-bare", "a body reading a lower-case $name", *reads(': "$state_recovery_dir"', 'state_recovery_dir="/var/lib/tensorplate/state.recovery"', "state_recovery_dir")),
    ("reads-arithmetic", "a body reading a name in (( ))", *reads("(( STATE_RECOVERY_LIMIT > 0 )) || :", LIMIT, "STATE_RECOVERY_LIMIT")),
    ("reads-legacy-arithmetic", "a body reading a name in $[ ]", *reads(": $[STATE_RECOVERY_LIMIT + 0]", LIMIT, "STATE_RECOVERY_LIMIT")),
    ("reads-let", "a body reading a name through let", *reads('let "STATE_RECOVERY_LIMIT > 0" || :', LIMIT, "STATE_RECOVERY_LIMIT")),
    ("reads-comparison", "a body reading a bare -gt operand", *reads("[[ STATE_RECOVERY_LIMIT -gt 0 ]] || :", LIMIT, "STATE_RECOVERY_LIMIT")),
    ("reads-comparison-right", "a body reading a bare -lt operand on the right", *reads("[[ 0 -lt STATE_RECOVERY_LIMIT ]] || :", LIMIT, "STATE_RECOVERY_LIMIT")),
    ("reads-subscript", "a body reading a name as a subscript", *reads('local rows=(); : "${rows[STATE_RECOVERY_LIMIT]-}"', LIMIT, "STATE_RECOVERY_LIMIT")),
    ("reads-test-v", "a body testing a name with -v", *reads("test -v STATE_RECOVERY_DIR || :", RECOVERY, "STATE_RECOVERY_DIR")),
    ("appends", "a body appending to a name", *reads("STATE_RECOVERY_LIMIT+=1", LIMIT, "STATE_RECOVERY_LIMIT")),
    # Only a real local declaration takes a name out: one taken by mistake
    # is a read nothing binds.
    ("named-in-a-local-value", "a body naming it inside a local's quoted value", *reads('local note="kept apart from STATE_RECOVERY_DIR on purpose"; : "${STATE_RECOVERY_DIR-}"', RECOVERY, "STATE_RECOVERY_DIR")),
    ("named-in-a-local-substitution", "a body naming it inside a local's command substitution", *reads('local note=$(printf %s STATE_RECOVERY_DIR ok); : "${STATE_RECOVERY_DIR-}"', RECOVERY, "STATE_RECOVERY_DIR")),
    ("declared-global", "a body declaring it with declare -g", *reads('declare -g STATE_RECOVERY_DIR; : "${STATE_RECOVERY_DIR-}"', RECOVERY, "STATE_RECOVERY_DIR")),
    # Bound once, in any spelling.
    (
        "shadowed-by-a-caller",
        "a local in the stage that calls the bodies",
        [("jetson", "into", "stage_rollback", '  local STATE_MANIFEST=""')],
        bound("STATE_MANIFEST"),
    ),
    (
        "rebound-in-another-spelling",
        "a second binding in another spelling",
        [("jetson", "after_line", 'STATE_MANIFEST=""', 'declare STATE_MANIFEST="stale"')],
        bound("STATE_MANIFEST"),
    ),
    (
        "bound-elsewhere-in-another-spelling",
        "the one binding respelled to another value",
        [("jetson", "replace_line", ASIDE, 'declare -r STATE_ASIDE_DIR="/var/lib/tensorplate/state.aside"')],
        ["STATE_ASIDE_DIR is bound to the same value in both"],
    ),
]


def apply(lines, operation, anchor, text):
    new = text.split("\n")
    if operation in ("into", "after"):
        opens = [i for i, line in enumerate(lines) if line == f"{anchor}() {{"]
        if len(opens) != 1:
            raise SystemExit(f"cannot plant: {anchor}() {{ appears {len(opens)} times")
        if operation == "into":
            at = opens[0] + 1
        else:
            at = next(j for j in range(opens[0] + 1, len(lines)) if lines[j] == "}") + 1
        return lines[:at] + new + lines[at:]
    matches = [i for i, line in enumerate(lines) if line == anchor]
    if len(matches) != 1:
        raise SystemExit(f"cannot plant: {anchor!r} appears {len(matches)} times")
    at = matches[0]
    if operation == "after_line":
        return lines[:at + 1] + new + lines[at + 1:]
    if operation == "replace_line":
        return lines[:at] + new + lines[at + 1:]
    raise SystemExit(f"cannot plant: unknown operation {operation}")


if sys.argv[1] == "list":
    for name, what, _, expected in CASES:
        print(f"{name}|{what}|{';'.join(sorted(expected))}|{len(expected)}")
    sys.exit(0)

_, cloud, jetson, out_dir, wanted = sys.argv[1:]
case = [c for c in CASES if c[0] == wanted]
if len(case) != 1:
    raise SystemExit(f"cannot plant: no case {wanted}")
copies = {
    "cloud": open(cloud, encoding="utf-8").read().split("\n"),
    "jetson": open(jetson, encoding="utf-8").read().split("\n"),
}
for side, operation, anchor, text in case[0][2]:
    for target in (("cloud", "jetson") if side == "both" else (side,)):
        copies[target] = apply(copies[target], operation, anchor, text)
for target, lines in copies.items():
    with open(f"{out_dir}/{target}.sh", "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines))
PY

cases="$(python3 "${td}/plant.py" list)"
while IFS='|' read -r case what expected count; do
  [[ -n "$case" ]] || continue
  dir="${td}/planted/${case}"
  mkdir -p "$dir"
  status=0
  python3 "${td}/plant.py" plant "$cloud" "$jetson" "$dir" "$case" >"${dir}/plant.log" 2>&1 || status=$?
  if ((status != 0)); then
    check "${what}: planted" planted "$(tr '\n' ' ' <"${dir}/plant.log")"
    continue
  fi
  status=0
  "$BASH" "$0" "${dir}/cloud.sh" "${dir}/jetson.sh" >"${dir}/verify.log" 2>&1 || status=$?
  refused="$(sed -n 's/^  FAIL //p' "${dir}/verify.log" | LC_ALL=C sort | paste -sd ';' - || true)"
  if [[ -z "$expected" ]]; then
    label="${what}"
  else
    label="${what} is refused"
  fi
  check "$label" "exit ${count}; refused: ${expected}" "exit ${status}; refused: ${refused}"
done <<<"$cases"

finish
