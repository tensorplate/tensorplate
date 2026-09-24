#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The public hygiene scanner.
#
# Both directions matter. Every alternative of every disclosure shape must
# fail with its own class and without its value in the output, because CI
# logs outlive a history rewrite; each fixture's context must pass with a
# synthetic word in the value's place, so the finding is the value's; the
# passing fixture and the five positive source files must pass, and the
# positive files must be ones the evidence scanner rejects, or they would
# not show that the source policy is its own. The repository tree must
# pass as committed, with every allowlist entry's count exact.
#
# Every disclosure-shaped value is generated at runtime and lives only in
# throwaway repositories, so this file and its fixtures carry nothing that
# looks like a live identifier.

set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scanner="${repo_root}/tools/validation/check-public-hygiene.sh"
evidence_scanner="${repo_root}/tools/validation/check-evidence-publication.sh"
fixtures="${repo_root}/test/validation/fixtures/public_hygiene"
allowlist=tools/validation/public-hygiene-allowlist.txt
failures=0
case_count=0
r=""

die() {
  printf 'public_hygiene_test: %s\n' "$1" >&2
  exit 2
}

check() {
  local what="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    printf '  ok   %s\n' "$what"
  else
    printf '  FAIL %s\n       expected: %s\n       actual:   %s\n' "$what" "$expected" "$actual"
    failures=$((failures + 1))
  fi
}

work="$(mktemp -d)" || die "mktemp failed"
trap 'rm -rf "$work"' EXIT

# The scanner reads the user name from the environment first, so a
# synthetic one keeps --local cases independent of the machine.
synthetic_user="tpu$(python3 -c 'import secrets, string
print("".join(secrets.choice(string.ascii_lowercase) for _ in range(9)))')" \
  || die "could not generate a user name"
export LOGNAME="$synthetic_user" USER="$synthetic_user"

random_word() {
  python3 -c 'import secrets, string, sys
print("".join(secrets.choice(string.ascii_lowercase) for _ in range(int(sys.argv[1]))))' "$1"
}
# random_from <alphabet> <n>
random_from() {
  python3 -c 'import secrets, sys
print("".join(secrets.choice(sys.argv[1]) for _ in range(int(sys.argv[2]))))' "$1" "$2"
}
random_digits() {
  printf '%s%s' "$((1 + RANDOM % 9))" "$(random_from 0123456789 "$(($1 - 1))")"
}
random_hex() {
  python3 -c 'import secrets, sys; print(secrets.token_hex(int(sys.argv[1])))' "$1"
}
random_uuid() {
  python3 -c 'import uuid; print(uuid.uuid4())'
}
random_octet() {
  printf '%s' "$((1 + RANDOM % 254))"
}
alnum="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
token_chars="${alnum}_-"
base64_chars="${alnum}+/"

# g <git args...>: git in the current case's repository, as a synthetic
# identity, with no hooks or signing from the environment.
g() {
  git -C "$r" -c user.name="TP Synthetic" -c user.email="tp-synthetic@example.com" \
    -c commit.gpgsign=false -c core.hooksPath=/dev/null "$@"
}

# new_case: a fresh repository in $r whose one commit is branch base; later
# commits go on main, so --base base scans them.
new_case() {
  case_count=$((case_count + 1))
  r="${work}/case-${case_count}"
  git init -q -b main "$r" || die "git init failed"
  printf 'base\n' >"${r}/README.md" || die "could not write README.md"
  g add README.md || die "git add failed"
  g commit -q -m "Base" || die "the base commit failed"
  g branch base || die "git branch failed"
}

# put <path> <text>: writes a file in the case repository and stages it.
put() {
  mkdir -p "$(dirname "${r}/$1")" || die "mkdir failed"
  printf '%s\n' "$2" >"${r}/$1" || die "could not write $1"
  g add -- "$1" || die "git add failed"
}

commit() {
  g commit -q -m "${1:-Change}" || die "commit failed"
}

# expand <fixture> <value> <out>: the fixture with @VALUE@ replaced. An
# @NL@ in the value is a line break, since a generator prints one value
# per line.
expand() {
  python3 - "$1" "$2" "$3" <<'PY' || die "could not expand $1"
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    text = handle.read()
if "@VALUE@" not in text:
    sys.exit("no @VALUE@ placeholder")
with open(sys.argv[3], "w", encoding="utf-8", newline="") as handle:
    handle.write(text.replace("@VALUE@", sys.argv[2].replace("@NL@", "\n")))
PY
}

# scan <out> <scanner args...>: runs the scanner in $r and prints its exit
# status.
scan() {
  local out="$1" status=0
  shift
  (cd "$r" && "$scanner" "$@") >"$out" 2>&1 || status=$?
  printf '%s' "$status"
}

has() {
  if grep -qF -- "$1" "$2"; then echo yes; else echo no; fi
}

# printed <value> <out>: whether the value appears in the output outside
# the class labels, which name the kind of a local literal.
printed() {
  if sed -E 's/: (local-identity|operator-literal) .*$//' "$2" | grep -qF -- "$1"; then
    echo yes
  else
    echo no
  fi
}

# expect_finding <what> <class> <value> <scanner args...>: scans $r.
expect_finding() {
  local what="$1" class="$2" value="$3" out="${r}.out"
  shift 3
  check "$what is a finding" "1" "$(scan "$out" "$@")"
  check "  classed ${class}" "yes" "$(has ": ${class}" "$out")"
  if [[ -n "$value" ]]; then
    check "  without printing the value" "no" "$(printed "$value" "$out")"
  fi
  check "  without a traceback" "no" "$(has "Traceback" "$out")"
}

printf 'public hygiene scanner\n'

# --- The repository as committed: clean, every allowlist count exact.
out="${work}/tree.out"
tree_status=0
(cd "$repo_root" && "$scanner" --tree) >"$out" 2>&1 || tree_status=$?
check "the repository tree passes --tree" "0" "$tree_status"
if [[ "$tree_status" != 0 ]]; then
  sed 's/^/       /' "$out"
fi

# --- Every row's evidence location is under an evidence prefix, or under
# an ignored build path that is never committed.
check "every platform row's evidence location is an evidence path or never committed" "ok" \
  "$(python3 - "$repo_root" "$scanner" <<'PY'
import ast, glob, json, os, re, subprocess, sys
root, scanner = sys.argv[1], sys.argv[2]
with open(scanner, encoding="utf-8") as handle:
    match = re.search(r"^EVIDENCE_PREFIXES = (\(.*\))$", handle.read(), re.M)
prefixes = ast.literal_eval(match.group(1))
bad = []
for path in sorted(glob.glob(os.path.join(root, "config/platform/rows/*.json"))):
    with open(path, encoding="utf-8") as handle:
        location = json.load(handle).get("evidence", {}).get("location")
    if location is None or location.startswith(prefixes):
        continue
    ignored = subprocess.run(["git", "-C", root, "check-ignore", "-q", "--no-index", location])
    tracked = subprocess.run(["git", "-C", root, "ls-files", "--", location],
                             capture_output=True).stdout
    if ignored.returncode != 0 or tracked:
        bad.append(os.path.basename(path))
print("ok" if not bad else "not scanned: " + " ".join(bad))
PY
)"

# --- Positive fixtures: source the evidence scanner rejects and the
# source policy must pass unchanged.
positive=(
  platform/src/probe.rs
  platform/tests/host_identity.rs
  protocol/rust/tests/round_trip.rs
  test/release/test_build_configuration.py
  tools/validation/check-evidence-publication.sh
)
new_case
for file in "${positive[@]}"; do
  mkdir -p "$(dirname "${r}/${file}")" || die "mkdir failed"
  cp "${repo_root}/${file}" "${r}/${file}" || die "could not copy ${file}"
  g add -- "$file" || die "git add failed"
  evidence_status=0
  "$evidence_scanner" --patterns-only "${repo_root}/${file}" >/dev/null 2>&1 || evidence_status=$?
  check "the evidence scanner rejects ${file}" "1" "$evidence_status"
done
commit "Add the positive fixtures"
check "the positive fixtures pass as a change" "0" "$(scan "${r}.out" --base base)"

new_case
cp "${fixtures}/passing.txt" "${r}/passing.txt" || die "could not copy passing.txt"
g add passing.txt || die "git add failed"
commit "Add the passing forms"
check "the passing fixture passes as a change" "0" "$(scan "${r}.out" --base base)"
check "  and as a tree" "0" "$(scan "${r}.tree.out" --tree)"
check "  and says what it scanned" "yes" "$(has "publishable: " "${r}.out")"

# --- Negative fixtures: every alternative of every shape, deterministically.
# fixture_class <name> names a fixture's class. fixture_values <name>
# prints one line per alternative, `<value>` or `<value><TAB><secret>`
# where only part of the value must stay unprinted, with a fresh random
# payload each run. Case labels are exact names, so the test can list
# them and require a fixture for each.
fixture_class() {
  case "$1" in
    credential-*) printf 'credential' ;;
    cloud-project-number*) printf 'cloud-project-number' ;;
    home-path-*) printf 'home-path' ;;
    ipv4*) printf 'ipv4' ;;
    ipv6*) printf 'ipv6' ;;
    private-label-*) printf 'private-label' ;;
    device-uuid | service-account | private-repository) printf '%s' "$1" ;;
    *) printf 'unknown' ;;
  esac
}

tab=$'\t'
private_ipv4() {
  printf '10.%s.%s.%s\n' "$(random_octet)" "$(random_octet)" "$(random_octet)"
  printf '172.%s.%s.%s\n' "$((16 + RANDOM % 16))" "$(random_octet)" "$(random_octet)"
  printf '192.168.%s.%s\n' "$(random_octet)" "$(random_octet)"
  printf '34.%s.%s.%s\n' "$(random_octet)" "$(random_octet)" "$(random_octet)"
}
global_ipv6() {
  printf '2600:1900:%s:%s::%s\n' "$(random_hex 2)" "$(random_hex 2)" "$(random_hex 2)"
}

fixture_values() {
  local n prefix kind key ip name digits token
  case "$1" in
    credential-private-key)
      for kind in "RSA " "OPENSSH " "EC " "DSA " "ENCRYPTED " ""; do
        printf -- '-----BEGIN %sPRIVATE KEY-----\tPRIVATE KEY\n' "$kind"
      done
      printf -- '-----BEGIN %s-----\tPRIVATE KEY\n' "PGP PRIVATE KEY BLOCK"
      ;;
    credential-kubeconfig-client-key)
      # On the key's line, and after it: a YAML block scalar, or a plain
      # scalar continued on the next line, past a blank one too.
      for key in 'client-key-data: ' '"client-key-data": "' 'client-key-data: "' '\"client-key-data\": \"' \
        'client-key-data: |@NL@      ' 'client-key-data: >-@NL@      ' 'client-key-data: |2+@NL@      ' \
        'client-key-data:@NL@      ' 'client-key-data:@NL@@NL@      ' $'client-key-data:\e[0m@NL@      ' \
        'client-key-data: | # base64 PEM@NL@      ' 'client-key-data: &key |@NL@      ' \
        'client-key-data: !!binary |@NL@      ' 'client-key-data:@NL@      # rotated@NL@      ' \
        'client-key-data: &key '; do
        token="$(random_from "$base64_chars" 96)"
        printf '%s%s\t%s\n' "$key" "$token" "$token"
      done
      ;;
    credential-kubeconfig-client-key-json)
      # Pretty-printed, the value on a later line than its key; escaped
      # inside a string, the line break an escape; and a carriage return
      # between key and value.
      for key in 'client-key-data: ' '"client-key-data": "' 'client-key-data: "' '\"client-key-data\": \"' \
        '"client-key-data":@NL@          "' '"client-key-data"@NL@          :@NL@          "' \
        $'"client-key-data":\r"'; do
        token="$(random_from "$base64_chars" 96)"
        printf '%s%s\t%s\n' "$key" "$token" "$token"
      done
      # The value starts with an escaped slash, so only the variant that
      # decodes both escapes joins key and value.
      for key in '\"client-key-data\":\n          \"' '\"client-key-data\":\u000a          \"'; do
        token="$(random_from "$base64_chars" 96)"
        printf '%sLS0t\\/%s\t%s\n' "$key" "$token" "$token"
      done
      ;;
    credential-github-token)
      for prefix in ghp gho ghu ghs ghr; do
        printf '%s_%s\n' "$prefix" "$(random_from "$alnum" 36)"
      done
      ;;
    credential-github-fine-grained-token)
      printf 'github_pat_%s_%s\n' "$(random_from "$alnum" 22)" "$(random_from "$alnum" 59)" ;;
    credential-google-api-key) printf 'AIza%s\n' "$(random_from "$token_chars" 35)" ;;
    credential-google-oauth-token) printf 'ya29.%s\n' "$(random_from "$token_chars" 120)" ;;
    credential-google-oauth-client-secret) printf 'GOCSPX-%s\n' "$(random_from "$token_chars" 28)" ;;
    credential-aws-access-key)
      for prefix in AKIA ASIA; do
        printf '%s%s\n' "$prefix" "$(random_from ABCDEFGHIJKLMNOPQRSTUVWXYZ234567 16)"
      done
      ;;
    credential-pypi-token) printf 'pypi-AgEIcHlwaS5vcmc%s\n' "$(random_from "$token_chars" 120)" ;;
    credential-anthropic-key) printf 'sk-ant-api03-%s\n' "$(random_from "$token_chars" 93)" ;;
    credential-hugging-face-token) printf 'hf_%s\n' "$(random_from "$alnum" 34)" ;;
    credential-ngc-api-key) printf 'nvapi-%s\n' "$(random_from "$token_chars" 64)" ;;
    credential-authorization-header)
      for key in 'Authorization: Bearer ' 'Authorization: Basic ' 'Proxy-Authorization: Bearer ' \
        'authorization: bearer ' 'Authorization=Bearer ' '"Authorization": "Bearer '; do
        token="$(random_from "$alnum" 40)"
        printf '%s%s\t%s\n' "$key" "$token" "$token"
      done
      ;;
    credential-authorization-header-multiline)
      for key in 'Authorization: >-@NL@      Bearer ' 'Authorization: |@NL@      Basic ' \
        'Authorization:@NL@      Bearer ' 'Authorization: Bearer@NL@      ' \
        '"Proxy-Authorization":@NL@      "Bearer ' 'authorization:@NL@  bearer ' \
        'Authorization: >- # from the vault@NL@      Bearer '; do
        token="$(random_from "$alnum" 40)"
        printf '%s%s\t%s\n' "$key" "$token" "$token"
      done
      ;;
    service-account)
      printf '%s@%s-%s.iam.gserviceaccount.com\n' "$(random_word 8)" "$(random_word 6)" "$(random_digits 6)"
      printf '%s-compute@%s\n' "$(random_digits 12)" "developer.gserviceaccount.com"
      ;;
    cloud-project-number)
      digits="$(random_digits 12)"
      printf 'projects/%s\t%s\n' "$digits" "$digits"
      ;;
    cloud-project-number-keyed)
      for key in "projectNumber: '" 'project_number: ' 'PROJECT_NUMBER=' '"projectNumber": "'; do
        digits="$(random_digits 12)"
        printf '%s%s\t%s\n' "$key" "$digits" "$digits"
      done
      ;;
    home-path-linux)
      name="$(random_word 9)"
      printf '/home/%s\t%s\n' "$name" "$name"
      ;;
    home-path-macos)
      name="$(random_word 9)"
      printf '/Users/%s\t%s\n' "$name" "$name"
      ;;
    home-path-windows)
      bs="\\"
      for kind in "C:${bs}Users${bs}" "C:${bs}${bs}Users${bs}${bs}" "c:${bs}users${bs}"; do
        name="$(random_word 9)"
        printf '%s%s\t%s\n' "$kind" "$name" "$name"
      done
      ;;
    home-path-encoded)
      # A literal tilde and backtick: these are text, not shell.
      # shellcheck disable=SC2088,SC2016
      for kind in '/private/tmp/agent/-Users-%s-workspace' '/tmp/agent/-home-%s-src' \
        '~/.cache/tool/projects/-Users-%s' '"-Users-%s-workspace-app"' '`-home-%s-src`' \
        'DIR=-Users-%s-work' ' -Users-%s-workspace'; do
        name="$(random_word 9)"
        # shellcheck disable=SC2059 # The format is the alternative.
        printf "${kind}\t%s\n" "$name" "$name"
      done
      ;;
    home-path-json-escaped)
      for kind in '\/' '\u002f' '\x2f'; do
        name="$(random_word 9)"
        printf '%sUsers%s%s%ssrc\t%s\n' "$kind" "$kind" "$name" "$kind" "$name"
      done
      ;;
    device-uuid)
      for prefix in GPU MIG; do
        name="$(random_uuid)"
        printf '%s-%s\t%s\n' "$prefix" "$name" "$name"
      done
      ;;
    ipv4 | ipv4-after-newline-escape | ipv4-after-unicode-escape | ipv4-after-control-sequence \
      | ipv4-after-equals)
      private_ipv4
      ;;
    ipv4-after-control-escape)
      for kind in '\033[1m' '\e[1m' '\x1b[1m' '\x1B[1m' '\u001b[1m' '\u001B[1m'; do
        ip="10.$(random_octet).$(random_octet).$(random_octet)"
        printf '%s%s\t%s\n' "$kind" "$ip" "$ip"
      done
      ;;
    ipv6 | ipv6-in-prose | ipv6-before-error | ipv6-in-url | ipv6-in-emphasis) global_ipv6 ;;
    ipv6-with-port)
      # Unbracketed, so the port reads as one more group: after a full
      # address, after a compressed one (five digits are not a group), and
      # before an error.
      ip="2600:1900:$(random_hex 2):$(random_hex 2):$(random_hex 2):$(random_hex 2):$(random_hex 2):$(random_hex 2)"
      printf '%s:8080\t%s\n' "$ip" "$ip"
      printf '%s:8080: connection refused\t%s\n' "$ip" "$ip"
      ip="2600:1900:$(random_hex 2):1::$(random_hex 2)"
      printf '%s:50051\t%s\n' "$ip" "$ip"
      printf '%s:52144: connection refused\t%s\n' "$ip" "$ip"
      ;;
    private-repository) printf '%s-%s\n' "$(random_word 10)" "internals" ;;
    private-label-ledger-row)
      printf 'PR-%s\n' "$((1 + RANDOM % 9))" "$(random_digits 2)" "$(random_digits 3)"
      printf 'PR-%s%s\n' "$(random_digits 2)" "$(random_from abc 1)"
      token="PR-$(random_digits 3)"
      printf 'notes_%s_draft\t%s\n' "$token" "$token"
      ;;
    private-label-hardware-gate)
      for n in 1 2 3; do
        printf 'HW-%s%s\n' "$(random_from ABCDEFGHIJKLMNOPQRSTUVWXYZ 1)" "$((n + RANDOM % 9))"
      done
      printf 'HW-%s%s\n' "$(random_from ABCDEFGHIJKLMNOPQRSTUVWXYZ 1)" "$(random_digits 2)"
      ;;
    private-label-contract-section) printf 'C%s\n' "$(random_from 0123456789 2)" "$(random_from 0123456789 2)" ;;
    private-label-decision) printf 'D%s\n' "$(random_from 0123456789 2)" "$(random_from 0123456789 2)" ;;
    private-label-cross-model) printf 'XM%s\n' "$(random_from 0123456789 2)" "$(random_from 0123456789 2)" ;;
    *) return 1 ;;
  esac
}

names=()
while IFS= read -r file; do
  names+=("$(basename "$file" .txt)")
done < <(find "$fixtures" -maxdepth 1 -type f -name '*.txt' ! -name passing.txt | LC_ALL=C sort)
check "at least one negative fixture exists" "yes" "$([[ ${#names[@]} -gt 0 ]] && echo yes || echo no)"

# Every generator's case label names an existing fixture, and every
# fixture has a generator: the labels are read from the function itself.
generators="$(declare -f fixture_values | sed -nE 's/^ +([a-z0-9 |-]+)\)$/\1/p' | tr '|' '\n' \
  | tr -d ' ' | grep -v '^$' | LC_ALL=C sort)"
check "every generator has a fixture and every fixture a generator" \
  "$(printf '%s\n' "${names[@]}" | LC_ALL=C sort | tr '\n' ' ')" "$(printf '%s\n' "$generators" | tr '\n' ' ')"

new_case
mkdir -p "${r}/matrix" || die "mkdir failed"
matrix_rows="${work}/matrix.tsv"
: >"$matrix_rows"
n=0
for name in "${names[@]}"; do
  class="$(fixture_class "$name")"
  check "fixture ${name} has a class" "yes" "$([[ "$class" != unknown ]] && echo yes || echo no)"
  values="$(fixture_values "$name")" || {
    check "fixture ${name} has a generator" "yes" "no"
    continue
  }
  n=$((n + 1))
  # The encoded home form ends a name at a dash, so its control is an
  # allowed name without one.
  control="tp-synthetic-operator"
  [[ "$name" == home-path-encoded ]] && control="Shared"
  expand "${fixtures}/${name}.txt" "$control" "${r}/matrix/control-${n}.txt"
  while IFS= read -r line; do
    value="${line%%"${tab}"*}"
    secret="${line#*"${tab}"}"
    n=$((n + 1))
    expand "${fixtures}/${name}.txt" "$value" "${r}/matrix/${n}.txt"
    printf '%s\t%s\t%s\t%s\n' "$n" "$name" "$class" "$secret" >>"$matrix_rows"
  done <<<"$values"
done
g add matrix || die "git add failed"
commit "Add the matrix"
out="${r}.out"
matrix_status="$(scan "$out" --base base)"
check "the negative matrix is a finding" "1" "$matrix_status"
check "  without a traceback" "no" "$(has "Traceback" "$out")"
check "  and every fixture's context passes with a synthetic value" "no" \
  "$(grep -q '^matrix/control-' "$out" && echo yes || echo no)"
while IFS=$'\t' read -r n name class secret; do
  check "${name} #${n} is ${class}" "yes" \
    "$(grep -qE "^matrix/${n}\.txt:[0-9]+: ${class}( |$)" "$out" && echo yes || echo no)"
  check "  without printing the value" "no" "$(printed "$secret" "$out")"
done <"$matrix_rows"

# A keyed credential is one finding, on its key's line, whether its value
# is on that line or a later one.
key_value="$(random_from "$base64_chars" 96)"
new_case
put notes/kubeconfig.json "{"$'\n'"  \"users\": [{"$'\n'"    \"client-key-data\":"$'\n'"      \"${key_value}\""$'\n'"  }]"$'\n'"}"
commit
expect_finding "a kubeconfig key whose value is on the next line" credential "$key_value" --base base
check "  reported once, on the key's line" "notes/kubeconfig.json:3: credential" \
  "$(grep -oE '^notes/kubeconfig.json:[0-9]+: credential' "${r}.out" | tr '\n' ' ' | sed 's/ $//')"
new_case
put notes/kubeconfig.yaml "    client-key-data: ${key_value}"
commit
expect_finding "a kubeconfig key and value on one line" credential "$key_value" --base base
check "  reported once" "1" "$(grep -c ': credential' "${r}.out" || :)"

# --- Where a value can hide besides a file's lines.
ip="10.$(random_octet).$(random_octet).$(random_octet)"
token="ghp_$(random_from "$alnum" 36)"

new_case
put notes/a.txt "clean"
g commit -q -m "Reach the build host at ${ip}" || die "commit failed"
expect_finding "an address in a commit message" ipv4 "$ip" --base base
check "  reported against the commit" "yes" "$(has "commit $(g rev-parse --short=12 HEAD):1: ipv4" "${r}.out")"

new_case
put notes/a.txt "clean"
commit
printf 'Title\n\nContext: %s\n' "$ip" >"${r}.body" || die "could not write a body"
expect_finding "an address in a --message file" ipv4 "$ip" --base base --message "${r}.body"
check "  reported against the message" "yes" "$(has "message#1:3: ipv4" "${r}.out")"

new_case
label="PR-$(random_digits 3)"
g checkout -q -b "fix-${label}-timeout" || die "git checkout failed"
put notes/a.txt "clean"
commit
expect_finding "a label in the branch checked out" private-label "$label" --base base
check "  reported against the branch" "yes" "$(has "branch:1: private-label" "${r}.out")"

new_case
put notes/a.txt "token ${token}"
commit "Add a note"
put notes/a.txt "token removed"
commit "Remove the token"
expect_finding "a token added by one commit and removed by the next" credential "$token" --base base
check "  reported against the commit that carried it" "yes" \
  "$(has "notes/a.txt@$(g rev-parse --short=12 HEAD~1):1: credential" "${r}.out")"

new_case
put notes/a.txt "token ${token}"
commit "Add a note"
g rm -q notes/a.txt || die "git rm failed"
commit "Remove the note"
expect_finding "a token in a file a later commit deletes" credential "$token" --base base

# A value that only a merge's own resolution wrote, removed again later.
new_case
put notes/a.txt "one"
commit "Main"
g checkout -q -b side base || die "git checkout failed"
put notes/b.txt "two"
commit "Side"
g checkout -q main || die "git checkout failed"
g merge -q --no-commit side >/dev/null 2>&1 || die "git merge failed"
put notes/a.txt "one ${token}"
g commit -q -m "Merge side" || die "merge commit failed"
merge_commit="$(g rev-parse --short=12 HEAD)"
put notes/a.txt "one"
commit "Clean up"
expect_finding "a token only a merge resolution wrote" credential "$token" --base base
check "  reported against the merge" "yes" "$(has "notes/a.txt@${merge_commit}:1: credential" "${r}.out")"

new_case
put "notes/${ip}.txt" "clean"
commit
expect_finding "an address in a file name" ipv4 "$ip" --base base
check "  reported against a masked path" "yes" "$(has "changed-path#1:0: ipv4" "${r}.out")"

new_case
python3 -c 'import sys; sys.stdout.buffer.write(b"caf\xe9 au lait\n")' >"${r}/notes.txt" \
  || die "could not write notes.txt"
g add notes.txt || die "git add failed"
commit
expect_finding "a file that is not UTF-8" binary "" --base base

new_case
printf 'text\0more\n' >"${r}/notes.txt" || die "could not write notes.txt"
g add notes.txt || die "git add failed"
commit
expect_finding "a UTF-8 file containing NUL" binary "" --base base

# An allowlisted binary accepts only the finding that it is binary: a
# credential or a home path in its printable runs is still reported, and
# the short shapes compressed data produces by chance are not looked for.
account="$(random_word 9)"
new_case
put tools/validation/public-hygiene-allowlist.txt "logo.png binary 1 A synthetic image."
python3 - "${r}/logo.png" "$account" "$token" <<'PY' || die "could not write logo.png"
import sys
with open(sys.argv[1], "wb") as handle:
    # A chance label and address, as compressed data holds them.
    noise = (" D%d!1g %d.%d.%d.%d " % (13, 10, 1, 2, 3)).encode()
    handle.write(b"\x89PNG\r\n\x1a\n\x00\x00" + noise + b"\x00<stRef:filePath>/Users/"
                 + sys.argv[2].encode() + b"/Desktop/logo.psd</stRef:filePath>\x00"
                 + sys.argv[3].encode() + b"\x00")
PY
g add logo.png || die "git add failed"
commit
expect_finding "a credential in an allowlisted binary" credential "$token" --base base
check "  and its home path" "yes" "$(has "logo.png:0: home-path" "${r}.out")"
check "  without printing the account" "no" "$(printed "$account" "${r}.out")"
check "  and not the short shapes in its compressed data" "no" \
  "$(grep -qE ': (private-label|ipv4)' "${r}.out" && echo yes || echo no)"

new_case
put tools/validation/public-hygiene-allowlist.txt "logo.png binary 1 A synthetic image."
python3 -c 'import sys
noise = (" D%d!1g %d.%d.%d.%d " % (13, 10, 1, 2, 3)).encode()
sys.stdout.buffer.write(b"\x89PNG\r\n\x1a\n\x00\x00" + noise + b"\x00")' \
  >"${r}/logo.png" || die "could not write logo.png"
g add logo.png || die "git add failed"
commit
check "an allowlisted binary with no long shapes passes" "0" "$(scan "${r}.out" --base base)"

new_case
g update-index --add --cacheinfo "160000,$(g rev-parse HEAD),vendor/lib" || die "git update-index failed"
put .gitmodules $'[submodule "vendor/lib"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n\tignore = all'
commit
expect_finding "a submodule, even one its config says to ignore" submodule "" --base base

new_case
account="$(random_word 9)"
ln -s "/home/${account}/evidence" "${r}/latest" || die "ln failed"
g add latest || die "git add failed"
commit
expect_finding "a symlink to a home directory" home-path "$account" --base base

new_case
mkdir -p "${r}/docs/validation/evidence" || die "mkdir failed"
ln -s ../../../README.md "${r}/docs/validation/evidence/linked.log" || die "ln failed"
g add docs/validation/evidence/linked.log || die "git add failed"
commit
expect_finding "a symlink under an evidence path" symlink "" --base base

# --- Evidence paths go through the evidence scanner as well.
host="vm-$(random_word 10)"
journal_line="Sep 14 01:42:03 ${host} tensorplate-agent[42]: started"

new_case
put docs/validation/evidence/v9.9.9/synthetic-row/stage.log "$journal_line"
commit
expect_finding "a journal host in committed evidence" journal-host "$host" --base base

new_case
put docs/validation/evidence/v9.9.9/synthetic-row/stage.log "$journal_line"
commit "Add evidence"
put docs/validation/evidence/v9.9.9/synthetic-row/stage.log "clean"
commit "Replace evidence"
expect_finding "a journal host in an earlier version of evidence" journal-host "$host" --base base
check "  reported against that version" "yes" \
  "$(has "stage.log@$(g rev-parse --short=12 HEAD~1):1: journal-host" "${r}.out")"

new_case
put test/platform/rec/capture.txt "$journal_line"
commit "Add a recording"
g rm -q test/platform/rec/capture.txt || die "git rm failed"
commit "Remove the recording"
expect_finding "a journal host in a recording a later commit deletes" journal-host "$host" --base base

new_case
put notes/stage.log "$journal_line"
commit
check "the same line outside the evidence paths passes the source policy" "0" \
  "$(scan "${r}.out" --base base)"

# A tree entry named `..`, `.` or nothing (git plumbing writes each; git
# only warns) is not a path the scan can stage or review: no verdict, and
# nothing written. The `..` case climbs five levels, out of the staging
# directory the scanner makes under TMPDIR.
crafted() {
  local name="$1" levels="$2" blob tree readme ok level
  blob="$(printf 'Static hostname: %s\n' "$host" | g hash-object -w --stdin)" || return 1
  ok="$(g rev-parse HEAD:docs/validation/evidence/ok.log)" || return 1
  readme="$(g rev-parse HEAD:README.md)" || return 1
  tree="$(printf '100644 blob %s\tescaped.log\n' "$blob" | g mktree)" || return 1
  for ((level = 1; level < levels; level++)); do
    tree="$(printf '040000 tree %s\t%s\n' "$tree" "$name" | g mktree)" || return 1
  done
  tree="$(printf '040000 tree %s\t%s\n100644 blob %s\tok.log\n' "$tree" "$name" "$ok" | g mktree)" || return 1
  tree="$(printf '040000 tree %s\tevidence\n' "$tree" | g mktree)" || return 1
  tree="$(printf '040000 tree %s\tvalidation\n' "$tree" | g mktree)" || return 1
  tree="$(printf '100644 blob %s\tREADME.md\n040000 tree %s\tdocs\n' "$readme" "$tree" | g mktree)" \
    || return 1
  g commit-tree "$tree" -p HEAD -m "Crafted"
}
# git itself refuses to read an entry with an empty name, so that scan
# stops at git: no verdict too.
for spec in "..|5|not a plain relative path" ".|1|not a plain relative path" "|1|git diff failed"; do
  IFS='|' read -r entry levels reason <<<"$spec"
  new_case
  put docs/validation/evidence/ok.log "clean"
  commit "Add evidence"
  if ! crafted_commit="$(crafted "$entry" "$levels" 2>/dev/null)"; then
    printf '  skip a tree entry named "%s" (this git will not write one)\n' "$entry"
    continue
  fi
  g update-ref refs/heads/main "$crafted_commit" || die "git update-ref failed"
  staging="${work}/staging-${case_count}"
  mkdir -p "$staging" || die "mkdir failed"
  check "a path with a \"${entry}\" component is no verdict" "2" \
    "$(TMPDIR="$staging" scan "${r}.out" --base base)"
  check "  and says why" "yes" "$(has "$reason" "${r}.out")"
  check "  and writes nothing" "" "$(find "$staging" "$work" -name escaped.log -print)"
done

# Names that differ only in case are separate files in git; on a
# case-insensitive disk they must not overwrite each other in the scan.
# Staged through the index, since such a disk cannot hold both.
new_case
for pair in "Host.txt:${journal_line}" "host.txt:clean"; do
  blob="$(printf '%s\n' "${pair#*:}" | g hash-object -w --stdin)" || die "git hash-object failed"
  g update-index --add --cacheinfo "100644,${blob},test/platform/rec/${pair%%:*}" \
    || die "git update-index failed"
done
commit
expect_finding "a finding beside a name that differs only in case" journal-host "$host" --base base

# Both tiers measure an address as written, so a zero-padded one is one
# finding, not one per form.
new_case
put test/platform/rec/net.txt "peer 2600:1900:0000:$(random_hex 2):0:0:0:5 up"
commit
expect_finding "a zero-padded address in a platform fixture" ipv6 "" --base base
check "  reported once, though both tiers find it" "1" "$(grep -c ': ipv6' "${r}.out" || :)"

# Assembled, so this file holds no address.
mapped_prefix="::$(printf ffff)"
new_case
put test/platform/rec/net.txt "peer ${mapped_prefix}:10.$(random_octet).$(random_octet).$(random_octet) up"
commit
expect_finding "an IPv4-mapped address in a platform fixture" ipv4 "" --base base
check "  reported once, though both tiers find it" "1" "$(grep -c ': ipv4' "${r}.out" || :)"

new_case
put test/platform/accelerator/new-card.txt "NVIDIA L4, 23034, 580.173.02, GPU-$(random_uuid), [N/A]"
commit
expect_finding "a device UUID in a platform fixture" device-uuid "" --base base
check "  reported once, though both tiers find it" "1" "$(grep -c ': device-uuid' "${r}.out" || :)"

new_case
put test/platform/accelerator/new-card.txt "NVIDIA L4, 23034, 580.173.02, GPU-00000000-0000-0000-0000-000000000009, [N/A]"
commit
check "a synthetic device UUID in a platform fixture passes" "0" "$(scan "${r}.out" --base base)"

# The evidence scanner reports one finding per line, class and length, so
# a second value only it decodes (here a UUID spelled as a byte array)
# would count as the first. No allowlist entry may name an evidence file:
# the entry is no verdict, and the file's findings stand without it.
plain_uuid="$(random_uuid)"
encoded_uuid="$(random_uuid)"
byte_array="$(python3 -c 'import sys; print(list(("GPU-" + sys.argv[1]).encode()))' "$encoded_uuid")"
for evidence_file in test/platform/accelerator/card.json docs/validation/evidence/v9.9.9/synthetic-row/card.json; do
  new_case
  put "$evidence_file" "{\"uuid\": \"GPU-${plain_uuid}\", \"raw\": ${byte_array}}"
  put "$allowlist" "${evidence_file} device-uuid 1 One UUID."
  commit
  check "an allowlist entry for an evidence file is no verdict (${evidence_file%%/*})" "2" \
    "$(scan "${r}.out" --base base)"
  check "  and says why" "yes" "$(has "names a file under an evidence path" "${r}.out")"
  g rm -q "$allowlist" || die "git rm failed"
  commit "Drop the entry"
  expect_finding "  without it, a plain and a decoded UUID on one line" device-uuid "$encoded_uuid" --base base
  check "  without printing the other" "no" "$(printed "$plain_uuid" "${r}.out")"
done

# An evidence path differs from its prefix only in case on a
# case-insensitive disk, so it is evidence too.
new_case
put test/Platform/accelerator/card.json "{\"uuid\": \"GPU-${plain_uuid}\", \"raw\": ${byte_array}}"
put "$allowlist" "test/Platform/accelerator/card.json device-uuid 1 One UUID."
commit
check "an allowlist entry for an evidence file in another case is no verdict" "2" "$(scan "${r}.out" --base base)"

# A symlink standing in for an evidence directory, or one above it, would
# move evidence out of the evidence tier.
for link in test/platform test; do
  new_case
  put fixtures/platform/accelerator/card.txt "NVIDIA L4, 23034, 580.173.02, GPU-00000000-0000-0000-0000-000000000009, [N/A]"
  mkdir -p "$(dirname "${r}/${link}")" || die "mkdir failed"
  ln -s "$(python3 -c 'import os, sys; print(os.path.relpath(sys.argv[1], os.path.dirname(sys.argv[2])))' \
    "${r}/fixtures/platform" "${r}/${link}")" "${r}/${link}" || die "ln failed"
  g add "$link" || die "git add failed"
  commit
  expect_finding "a symlink at ${link}" symlink "" --base base
done

# An address is judged by its value, not its spelling: mixed notation
# (a dotted quad for the last 32 bits), with and without compression,
# and all hex. Only an IPv4-mapped address takes the IPv4 rules, as
# ipv4, whichever way it is written; any other address in mixed notation
# is one IPv6 finding, and its dotted quad is not an IPv4 address too.
# spellings <kind>: one address of that kind, randomized, in each form.
# Built from integers, so this file holds no address.
spellings() {
  python3 - "$1" <<'PY'
import ipaddress, secrets, sys
r16, r24 = secrets.randbits(16), secrets.randbits(24)
mapped = 0xffff << 32
value = {
    "unique-local, loopback tail": (0xfd42 << 112) | (r16 << 96) | (127 << 24) | 1,
    "unique-local, private tail": (0xfd42 << 112) | (r16 << 96) | (10 << 24) | r24,
    "global, public tail": (0x26001900 << 96) | (r16 << 80) | (34 << 24) | r24,
    "NAT64, public tail": (0x64ff9b << 96) | (34 << 24) | r24,
    "IPv4-compatible loopback": (127 << 24) | 1,
    "mapped private": mapped | (10 << 24) | r24,
    "mapped public": mapped | (34 << 24) | r24,
    "mapped loopback": mapped | (127 << 24) | 1,
    "mapped metadata": mapped | (169 << 24) | (254 << 16) | (169 << 8) | 254,
    "mapped documentation": mapped | (192 << 24) | (2 << 8) | 9,
    "documentation, documentation tail": (0x20010db8 << 96) | (192 << 24) | (2 << 8) | 1,
    "documentation, private tail": (0x20010db8 << 96) | (10 << 24) | r24,
}[sys.argv[1]]
groups = ["%x" % ((value >> (112 - 16 * i)) & 0xffff) for i in range(8)]


def compress(groups, tail=""):
    best, i = (0, 0), 0
    while i < len(groups):
        j = i
        while j < len(groups) and groups[j] == "0":
            j += 1
        if j - i >= 2 and j - i > best[1] - best[0]:
            best = (i, j)
        i = max(j, i + 1)
    parts = groups + ([tail] if tail else [])
    if best == (0, 0):
        return ":".join(parts)
    head, rest = ":".join(groups[:best[0]]), ":".join(parts[best[1]:])
    return head + "::" + rest


dotted = str(ipaddress.IPv4Address(value & 0xffffffff))
for spelling in (compress(groups[:6], dotted), ":".join(groups[:6] + [dotted]),
                 ":".join(groups), compress(groups)):
    if ipaddress.IPv6Address(spelling) != ipaddress.IPv6Address(value):
        sys.exit("a spelling does not denote the address")
    print(spelling)
PY
}
while IFS='|' read -r kind expected; do
  spelled="$(spellings "$kind")" || die "could not spell ${kind}"
  while IFS= read -r spelling; do
    new_case
    put notes/a.txt "peer ${spelling} up"
    commit
    status="$(scan "${r}.out" --base base)"
    found="$({ grep -oE ': ipv[46] ' "${r}.out" || :; } | tr -d ': ' | LC_ALL=C sort | tr '\n' ' ' | sed 's/ $//')"
    check "${kind} written ${spelling}" "$expected" "$(printf '%s %s' "$status" "$found" | sed 's/ $//')"
    if [[ "$status" != 0 ]]; then
      check "  without printing it" "no" "$(printed "$spelling" "${r}.out")"
    fi
  done <<<"$spelled"
done <<'EOF'
unique-local, loopback tail|1 ipv6
unique-local, private tail|1 ipv6
global, public tail|1 ipv6
NAT64, public tail|1 ipv6
IPv4-compatible loopback|1 ipv6
mapped private|1 ipv4
mapped public|1 ipv4
mapped loopback|0
mapped metadata|0
mapped documentation|0
documentation, documentation tail|0
documentation, private tail|0
EOF

# Where the IPV6 pattern cannot start a run (after an escape, a control
# sequence or a colon), the quad still ends the address: one finding, of
# the address's class. And after a mapped address, `-1` is no Debian
# revision: its dotted and hex spellings agree.
tab_escape='\t'
for kind_expected in "unique-local, private tail|1 ipv6" "documentation, private tail|0"; do
  kind="${kind_expected%%|*}"
  address="$(spellings "$kind" | head -n 1)" || die "could not spell ${kind}"
  for prefix in "$tab_escape" '\e[1m' $'\e[1m' 'peer:'; do
    new_case
    put notes/a.txt "peer ${prefix}${address} up"
    commit
    status="$(scan "${r}.out" --base base)"
    found="$({ grep -oE ': ipv[46] ' "${r}.out" || :; } | tr -d ': ' | LC_ALL=C sort | tr '\n' ' ' | sed 's/ $//')"
    check "${kind} after $(printf '%q' "$prefix")" "${kind_expected#*|}" \
      "$(printf '%s %s' "$status" "$found" | sed 's/ $//')"
  done
done
while IFS= read -r spelling; do
  new_case
  put notes/a.txt "peer ${spelling}-1 up"
  commit
  status="$(scan "${r}.out" --base base)"
  found="$({ grep -oE ': ipv[46] ' "${r}.out" || :; } | tr -d ': ' | tr '\n' ' ' | sed 's/ $//')"
  check "mapped private written ${spelling} before -1" "1 ipv4" "$(printf '%s %s' "$status" "$found")"
done < <(spellings "mapped private")

# The dotted quad is bounded as an IPv4 address is: a fifth group or a
# word after it makes it no address, after a colon-hex run as on its own.
ula="fd42:$(random_hex 2)"
quad="10.$(random_octet).$(random_octet).$(random_octet)"
for spec in "a fifth group|${quad}.$(random_octet)" "a word|${quad}$(random_word 3)"; do
  new_case
  put notes/a.txt "build ${spec#*|} and ${ula}::${spec#*|}"
  commit
  check "a dotted quad followed by ${spec%%|*} is no address, alone or in mixed notation" "0" \
    "$(scan "${r}.out" --base base)"
done

run_id="$(random_uuid)"
new_case
put "docs/validation/evidence/v9.9.9/run-${run_id}.log" "peer ${ip}"
commit
expect_finding "an evidence file whose name the evidence scanner masks" ipv4 "$run_id" --base base
check "  every finding on it names the masked path" "no" "$(has "run-" "${r}.out")"

new_case
mkdir -p "${r}/docs/validation/evidence/run-${run_id}" || die "mkdir failed"
python3 -c 'import sys; sys.stdout.buffer.write(b"\x89PNG\r\n\x1a\n\x00\x00")' \
  >"${r}/docs/validation/evidence/run-${run_id}/shot.png" || die "could not write shot.png"
ln -s shot.png "${r}/docs/validation/evidence/run-${run_id}/latest" || die "ln failed"
g add docs/validation/evidence || die "git add failed"
commit
expect_finding "a binary and a symlink in an evidence directory whose name is a finding" binary \
  "$run_id" --base base
check "  names neither path" "no" "$(has "run-" "${r}.out")"

new_case
mkdir -p "${r}/docs/validation/evidence" || die "mkdir failed"
python3 -c 'print("[" * 100000 + "]" * 100000)' >"${r}/docs/validation/evidence/deep.json" \
  || die "could not write deep.json"
g add docs/validation/evidence/deep.json || die "git add failed"
commit
check "an evidence scanner fault is no verdict" "2" "$(scan "${r}.out" --base base)"
check "  without a traceback" "no" "$(has "Traceback" "${r}.out")"

# --- Literals: the operator's private file and this machine's identity.
literal="lit$(random_word 9)"
literals_dir="${work}/private"
mkdir -p "$literals_dir" || die "mkdir failed"
printf '# operator literals\n%s\n' "$literal" >"${literals_dir}/literals.txt" \
  || die "could not write the literal file"

new_case
put notes/a.txt "deployed from ${literal}"
commit
expect_finding "an operator literal in a file" "operator-literal literal #2" "$literal" \
  --base base --literals "${literals_dir}/literals.txt"
check "  and nothing without the literal file" "0" "$(scan "${r}.plain.out" --base base)"

new_case
put docs/validation/evidence/v9.9.9/synthetic-row/stage.log "deployed from ${literal}"
commit
expect_finding "an operator literal in evidence" "operator-literal literal #2" "$literal" \
  --base base --literals "${literals_dir}/literals.txt"
check "  reported once, though both tiers find it" "1" "$(grep -c ': operator-literal' "${r}.out" || :)"

new_case
put notes/a.txt "clean"
g -c user.email="${literal}@example.com" commit -q -m "Change" || die "commit failed"
expect_finding "an operator literal in a commit's author" "operator-literal literal #2" "$literal" \
  --base base --literals "${literals_dir}/literals.txt"
check "  reported against the author line" "yes" "$(has ":author: operator-literal" "${r}.out")"

new_case
put notes/a.txt "clean"
GIT_COMMITTER_EMAIL="${literal}@example.com" g commit -q -m "Change" || die "commit failed"
expect_finding "an operator literal in a commit's committer" "operator-literal literal #2" "$literal" \
  --base base --literals "${literals_dir}/literals.txt"
check "  reported against the committer line only" "committer" \
  "$(grep -oE ':(author|committer): operator-literal' "${r}.out" | cut -d: -f2 | tr '\n' ' ' | sed 's/ $//')"

# In a binary file's printable runs, a literal of eight or more
# characters is looked for, and a shorter one is not (compressed data holds
# short words by chance).
printf '%s\nqz7\n' "$literal" >"${literals_dir}/binary.txt" || die "could not write a literal file"
new_case
put tools/validation/public-hygiene-allowlist.txt "logo.png binary 1 A synthetic image."
python3 - "${r}/logo.png" "$literal" <<'PY' || die "could not write logo.png"
import sys
with open(sys.argv[1], "wb") as handle:
    handle.write(b"\x89PNG\r\n\x1a\n\x00\x00 made on " + sys.argv[2].encode()
                 + b" \x00 zz qz7 zz \x00")
PY
g add logo.png || die "git add failed"
commit
expect_finding "a long literal in an allowlisted binary" "operator-literal literal #1" "$literal" \
  --base base --literals "${literals_dir}/binary.txt"
check "  and not the short one" "no" "$(has "literal #2" "${r}.out")"

# A literal shorter than eight characters matches only as a whole word,
# or it would flag ordinary words that contain it.
printf 'qz7\n' >"${literals_dir}/short-word.txt" || die "could not write a literal file"
new_case
put notes/a.txt "aqz7b qz7x xqz7"
commit
check "a short literal inside a longer word passes" "0" \
  "$(scan "${r}.out" --base base --literals "${literals_dir}/short-word.txt")"
new_case
put notes/a.txt "built on qz7."
commit
expect_finding "a short literal as a whole word" "operator-literal literal #1" "qz7" \
  --base base --literals "${literals_dir}/short-word.txt"

new_case
put notes/a.txt "clean"
commit
printf '%s\n' "$literal" >"${r}/literals.txt" || die "could not write a literal file"
check "a literal file inside the repository is no verdict" "2" \
  "$(scan "${r}.out" --base base --literals "${r}/literals.txt")"
printf 'ab\n' >"${literals_dir}/short.txt" || die "could not write a literal file"
check "a literal shorter than three characters is no verdict" "2" \
  "$(scan "${r}.out" --base base --literals "${literals_dir}/short.txt")"

# A literal file in a checkout that encloses the scanned one is refused
# too: it is one `git add` from a commit there.
outer="${work}/outer"
git init -q "$outer" || die "git init failed"
printf '%s\n' "$literal" >"${outer}/literals.txt" || die "could not write a literal file"
case_count=$((case_count + 1))
r="${outer}/inner"
git init -q -b main "$r" || die "git init failed"
put notes/a.txt "clean"
commit
check "a literal file in an enclosing checkout is no verdict" "2" \
  "$(scan "${r}.out" --tree --literals "${outer}/literals.txt")"
check "  and says why" "yes" "$(has "inside the repository" "${r}.out")"

# --local: the user name comes from the environment (a synthetic one here),
# the host name from the machine, the project from gcloud on PATH, which
# is a stub for every --local scan here: CI runners ship a real one.
stubs="${work}/stubs"
mkdir -p "$stubs" || die "mkdir failed"
gcloud_answers() {
  printf '#!/bin/sh\n%s\n' "$1" >"${stubs}/gcloud" || die "could not write a stub"
  chmod +x "${stubs}/gcloud" || die "chmod failed"
}
gcloud_answers 'printf "(unset)\n"'
local_scan() {
  PATH="${stubs}:${PATH}" scan "$@" --local
}
project="tp-synthetic-project-$(random_word 6)"

new_case
put notes/a.txt "built by ${synthetic_user}"
commit
check "this user's name with --local is a finding" "1" "$(local_scan "${r}.out" --base base)"
check "  classed local-identity (user)" "yes" "$(has ": local-identity (user)" "${r}.out")"
check "  without printing the value" "no" "$(printed "$synthetic_user" "${r}.out")"

new_case
put notes/a.txt "clean"
g -c user.email="${synthetic_user}@users.noreply.github.com" commit -q -m "Change" \
  || die "commit failed"
check "the user's name in their own commit identity passes --local" "0" \
  "$(local_scan "${r}.out" --base base)"

new_case
put notes/a.txt "deployed to ${project}"
commit
gcloud_answers "printf '%s\\n' '${project}'"
check "the gcloud project with --local is a finding" "1" "$(local_scan "${r}.out" --base base)"
check "  classed local-identity (project)" "yes" "$(has ": local-identity (project)" "${r}.out")"
check "  without printing the value" "no" "$(printed "$project" "${r}.out")"

new_case
put notes/a.txt "clean"
commit
gcloud_answers 'exit 1'
check "a gcloud that fails under --local is no verdict" "2" "$(local_scan "${r}.out" --base base)"
check "  and says so" "yes" "$(has "gcloud failed" "${r}.out")"
new_case
put notes/a.txt "gcloud project: (unset)"
commit
gcloud_answers 'printf "(unset)\n"'
check "an unset gcloud project under --local is no literal" "0" "$(local_scan "${r}.out" --base base)"

new_case
put notes/a.txt "the runner and the jetson"
commit
check "a generic user name is not a literal" "0" \
  "$(LOGNAME=runner USER=runner local_scan "${r}.out" --base base)"

host_name="$(hostname)"
host_name="${host_name%%.*}"
lowered="$(printf '%s' "$host_name" | tr '[:upper:]' '[:lower:]')"
# Text the scanner always prints here: a host name inside it would read as
# printed although no value was.
fixed_text="notes/a.txt local-identity (host) commit author finding(s): not publishable. rewrite the \
branch so that no commit carries them before it is pushed: a later commit that removes a value does \
not clear it. see rule 10 of docs/validation/fixture-and-evidence-rules.md."
[[ "$fixed_text" == *"$lowered"* ]] && lowered=localhost
case "$lowered" in
  admin | codespace | jetson | localhost | nvidia | root | runner | ubuntu | user | vscode)
    printf '  skip --local host (a generic name here)\n' ;;
  *)
    if [[ ${#host_name} -lt 3 ]]; then
      printf '  skip --local host (shorter than a literal)\n'
    else
      new_case
      put notes/a.txt "built on ${host_name}"
      commit
      check "this machine's host name with --local is a finding" "1" "$(local_scan "${r}.out" --base base)"
      check "  classed local-identity (host)" "yes" "$(has ": local-identity (host)" "${r}.out")"
      check "  without printing the value" "no" "$(printed "$host_name" "${r}.out")"
      new_case
      put notes/a.txt "clean"
      g -c user.email="builder@${host_name}.local" commit -q -m "Change" || die "commit failed"
      check "this machine's host name in a commit identity is a finding" "1" \
        "$(local_scan "${r}.out" --base base)"
      check "  reported against the author line" "yes" "$(has ":author: local-identity (host)" "${r}.out")"
    fi
    ;;
esac

# --- The allowlist.

new_case
put notes/a.txt "peer ${ip}"
put "$allowlist" "notes/a.txt ipv4 1 A synthetic peer address."
commit
check "an allowlisted finding passes" "0" "$(scan "${r}.out" --base base)"
check "  and the entry's count is exact in a tree scan" "0" "$(scan "${r}.tree.out" --tree)"

other_ip="10.$(random_octet).$(random_octet).$(random_octet)"
new_case
put notes/a.txt "peer ${ip}"$'\n'"peer ${other_ip}"
put "$allowlist" "notes/a.txt ipv4 1 A synthetic peer address."
commit
expect_finding "a second address in an allowlisted file" ipv4 "$other_ip" --base base
check "  reports both" "2" "$(grep -c '^notes/a.txt:[0-9]*: ipv4' "${r}.out" || :)"

# Every occurrence counts: a second address of the same length on the
# same line is a second finding.
same_length_ip="10.$(random_octet).$(random_octet).$(random_octet)"
while [[ ${#same_length_ip} -ne ${#ip} || "$same_length_ip" == "$ip" ]]; do
  same_length_ip="10.$(random_octet).$(random_octet).$(random_octet)"
done
new_case
put notes/a.txt "peer ${ip} and ${same_length_ip}"
put "$allowlist" "notes/a.txt ipv4 1 A synthetic peer address."
commit
expect_finding "a second address of the same length on an allowlisted line" ipv4 "$same_length_ip" \
  --base base
check "  reports both" "2" "$(grep -c '^notes/a.txt:1: ipv4' "${r}.out" || :)"

# Two values of one length, one seen only as written and the other only
# with its escapes decoded, are two findings, not one per variant.
while :; do
  hidden_octets="$(random_octet).$(random_octet).$(random_octet)"
  [[ ${#hidden_octets} -eq $((${#ip} - 3)) && "10.${hidden_octets}" != "$ip" ]] && break
done
new_case
put notes/a.py "URLS = [\"http://${ip}\\x41\", \"http://10\\x2e${hidden_octets}\"]"
put "$allowlist" "notes/a.py ipv4 1 A synthetic peer address."
commit
expect_finding "a second address only its decoded escapes reveal" ipv4 "10.${hidden_octets}" --base base
check "  reports both" "2" "$(grep -c '^notes/a.py:1: ipv4' "${r}.out" || :)"

# A value several scan variants find (the line holds an escape, but not
# next to the value) is one finding, not one per variant.
new_case
put notes/a.c "log(\"peer ${ip}\\n\");"
put "$allowlist" "notes/a.c ipv4 1 A synthetic peer address."
commit
check "a value on a line with an escape counts once" "0" "$(scan "${r}.out" --base base)"
check "  and the entry's count is exact in a tree scan" "0" "$(scan "${r}.tree.out" --tree)"

new_case
put notes/a.txt "peer ${ip}"
put "$allowlist" "notes/a.txt ipv4 1 A synthetic peer address."
g commit -q -m "Reach ${ip}" || die "commit failed"
expect_finding "the allowlist does not cover a commit message" ipv4 "$ip" --base base

new_case
put notes/a.txt "clean"
put "$allowlist" "notes/a.txt ipv4 1 An entry with nothing to accept."
commit
check "an unused entry passes a change" "0" "$(scan "${r}.out" --base base)"
expect_finding "an unused entry in a tree scan" "stale-allowlist (0 found)" "" --tree

new_case
put notes/a.txt "peer ${ip}"
put "$allowlist" "notes/a.txt ipv4 2 One more than there is."
commit
expect_finding "an entry whose count exceeds the file's in a tree scan" "stale-allowlist (1 found)" "" --tree

new_case
put notes/a.txt "peer ${ip}"
commit
mkdir -p "$(dirname "${r}/${allowlist}")" || die "mkdir failed"
printf 'notes/a.txt ipv4 1 Not committed.\n' >"${r}/${allowlist}" || die "could not write the allowlist"
expect_finding "an allowlist entry that is not committed" ipv4 "$ip" --base base

for bad in "notes/a.txt credential 1 Never." "notes/a.txt mac 1 An evidence-only class." \
  "notes/a.txt ipv4 1" "notes/a.txt ipv5 1 A typo." \
  "notes/a.txt ipv4 some A missing count." "notes/a.txt ipv4 0 A zero count." \
  $'notes/a.txt ipv4 1 One.\nnotes/a.txt ipv4 1 Two.'; do
  new_case
  put notes/a.txt "peer ${ip}"
  put "$allowlist" "$bad"
  commit
  check "a malformed allowlist is no verdict ($(printf '%s' "$bad" | head -n1 | cut -d' ' -f2-4))" "2" \
    "$(scan "${r}.out" --base base)"
  check "  and says which line" "yes" "$(has "allowlist line" "${r}.out")"
done

# The entry's count covers both of the file's findings, and the allowlist
# accepts its own address, so only the rule that a masked path is never
# accepted can leave the file's two findings.
new_case
put "notes/${ip}.txt" "peer ${ip}"
put "$allowlist" "notes/${ip}.txt ipv4 2 An entry for a masked path."$'\n'"${allowlist} ipv4 1 The entry above names the address."
commit
expect_finding "an entry for a file whose name is a finding" ipv4 "$ip" --base base
check "  cannot accept that file's findings" "2" "$(grep -c '^changed-path#1:[01]: ipv4' "${r}.out" || :)"
check "  and nothing else is reported" "2" "$(grep -cE ':[0-9]+: [a-z]' "${r}.out" || :)"

# --- Arguments and faults.
new_case
put notes/a.txt "clean"
commit
check "no arguments is no verdict" "2" "$(scan "${r}.out")"
check "--help is no verdict" "2" "$(scan "${r}.out" --help)"
check "--base and --tree together is no verdict" "2" "$(scan "${r}.out" --base base --tree)"
check "an unknown argument is no verdict" "2" "$(scan "${r}.out" --base base --verbose)"
check "a --base that names no commit is no verdict" "2" "$(scan "${r}.out" --base "no-such-$(random_word 6)")"
check "an empty range is no verdict" "2" "$(scan "${r}.out" --base HEAD)"
check "a missing --message file is no verdict" "2" \
  "$(scan "${r}.out" --base base --message "${work}/absent.md")"
printf '\377\376\n' >"${r}.latin" || die "could not write a message"
check "a --message file that is not UTF-8 is no verdict" "2" \
  "$(scan "${r}.out" --base base --message "${r}.latin")"
outside="${work}/not-a-repository"
mkdir -p "$outside" || die "mkdir failed"
outside_status=0
(cd "$outside" && GIT_CEILING_DIRECTORIES="$work" "$scanner" --tree) >/dev/null 2>&1 || outside_status=$?
check "a scan outside a repository is no verdict" "2" "$outside_status"

if [[ "$failures" -eq 0 ]]; then
  printf '\nall checks passed\n'
  exit 0
fi
# Not the count: an exit status is taken modulo 256.
printf '\n%s check(s) failed\n' "$failures"
exit 1
