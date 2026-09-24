#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Is this change safe to publish in a public repository?
#
# Everything pushed to this repository is public the moment it is pushed,
# and stays public: a force-push does not withdraw a commit that forks,
# pull request refs and caches already hold, and a pull request's body
# becomes the squash-merge commit message. So a change is scanned before
# it is pushed, and again on its pull request. The scanner fails closed
# and never prints a value it matched.
#
# What is scanned, all of it as committed (never the working tree):
#
#   files     every file the change adds or modifies, as it is at HEAD and
#             as each commit in the range wrote it, merges included, so a
#             value one commit adds and a later one removes is still
#             found; and the name of every such file
#   commits   the message, author and committer of every commit in the
#             range
#   messages  the name of the branch checked out, if any, and any further
#             text the caller names with --message: a pull request's
#             title and body, and in CI its branch name
#
# Two tiers, because recorded evidence and source need different rules:
#
#   evidence  every file under EVIDENCE_PREFIXES below, where recorded
#             evidence and recorded fixtures live, goes through
#             tools/validation/check-evidence-publication.sh unchanged,
#             name and content: patterns only by default, the operator's
#             private literal file with --literals. The source policy
#             applies to it too.
#   source    every file and all commit and message text is checked for a
#             narrow set of shapes that never belong in a public source
#             tree: credentials, cloud service accounts and project
#             numbers, home directories, device UUIDs, network addresses,
#             and references to private planning material. The evidence
#             scanner's host, journal, UUID, serial, e-mail, MAC,
#             machine-id, cloud-project and planning-identifier classes are
#             deliberately not part of it: negative tests, synthetic
#             identities and the scanners' own patterns carry those
#             legitimately.
#
# Lines are scanned as written, with escapes and terminal control
# sequences blanked, and with escapes decoded. A file that is not UTF-8
# text or contains NUL is a finding (it cannot be reviewed as text), and
# its printable runs are still checked for the long shapes (BINARY_CLASSES
# below); a submodule is a finding. A
# symlink is scanned by its target, and under an evidence prefix it is a
# finding, as the evidence scanner rules, as is one that stands in for an
# evidence directory or a directory above one.
#
# The one override is tools/validation/public-hygiene-allowlist.txt, read
# as committed at HEAD and reviewed like code: one `path class count
# reason` entry per line, exact paths only. An entry accepts up to that
# many findings of that class in that file, counting every occurrence,
# and nothing else; a version with more reports them all. It bounds how
# many values are accepted, not which. It never applies to commit or
# message text or to a file whose name is itself a finding, and neither a
# credential finding, nor one only the evidence scanner makes, nor any
# finding in a file under an evidence prefix can be allowlisted: evidence
# is sanitized (rule 3), never excepted. --tree reports an entry
# whose count no longer matches the file at HEAD as stale. A path that is
# not plain and relative (an entry named `..`, which git plumbing can
# write) is no verdict.
#
# Findings print as `<ref>:<line>: <class> (<n> chars)`. `<ref>` is a
# repository path, `<path>@<commit>` for a version of the file that is
# not the one at HEAD, `changed-path#N` when either tier finds the path
# itself (N counts the sorted scanned paths), `commit <commit>` for a
# commit's message, with lines `author` and `committer` for its identity
# lines, `branch` for the checked-out branch name, or `message#N` for the
# Nth --message file. Line 0 means the finding is about the name or the
# whole file. Findings from the evidence scanner keep its own class names.
#
# Exit 0 when the change is publishable, 1 when there are findings, and 2
# when the scan did not reach a verdict (bad arguments or --help, a git
# failure, an unreadable input, a malformed allowlist, nothing to scan,
# an unanswered --local lookup, an evidence scanner fault, an internal
# error).
#
# Usage:
#   check-public-hygiene.sh --base REF [--message FILE]... [--literals FILE] [--local]
#   check-public-hygiene.sh --tree [--literals FILE] [--local]
#
# --base REF scans the change from REF to HEAD: files that differ between
# their merge base and HEAD, and the commits in REF..HEAD. --tree scans
# every file at HEAD and checks the allowlist for stale entries.
# --literals FILE names a private literal file kept outside the
# repository, as check-evidence-publication.sh reads it. --local adds this
# machine's user name, short host name and gcloud project as literals,
# for a scan before push; CI never passes it. The user name is not looked
# for in commit author and committer lines, which carry the identity the
# contributor chose to publish.
#
# The repository scanned is the one containing the current directory.

set -Eeuo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)" || {
  printf 'check-public-hygiene: cannot locate the scanner directory\n' >&2
  exit 2
}

command -v python3 >/dev/null 2>&1 || {
  printf 'check-public-hygiene: python3 is required\n' >&2
  exit 2
}

# As in check-evidence-publication.sh: Python exits 1 on an uncaught
# exception, so the findings verdict is exit 3, mapped to 1 below.
scan_status=0
python3 - "$script_dir" "$@" <<'PY' || scan_status=$?
# Keep this compatible with Python 3.9: that is /usr/bin/python3 on the
# macOS hosts changes are pushed from.
import bisect
import getpass
import ipaddress
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
from collections import Counter

FINDINGS = 3
FAULT = 2
INTERNAL = 4

USAGE = """usage: check-public-hygiene.sh --base REF [--message FILE]... [--literals FILE] [--local]
       check-public-hygiene.sh --tree [--literals FILE] [--local]

Exit 0: publishable. Exit 1: findings. Exit 2: no verdict."""

ALLOWLIST_PATH = "tools/validation/public-hygiene-allowlist.txt"
# Recorded lifecycle evidence, and recorded or transcribed fixtures of
# real machines. Every platform row's evidence.location is under one of
# these or under a git-ignored build path that is never committed;
# test/validation/public_hygiene_test.sh asserts it. Matched in any
# letter case.
EVIDENCE_PREFIXES = ("docs/validation/evidence/", "test/platform/")

SYNTHETIC_OPERATOR = "tp-synthetic-operator"
# Account and host names that identify nobody: containers, CI runners,
# stock images and device images share them, and as literals they would
# match ordinary words.
GENERIC_NAMES = frozenset((
    "admin", "codespace", "jetson", "localhost", "nvidia", "root", "runner",
    "ubuntu", "user", "vscode"))
MIN_LITERAL = 3
SUBSTRING_LITERAL = 8

MODE_SYMLINK = "120000"
MODE_SUBMODULE = "160000"
MODE_FILES = ("100644", "100755")
DIFF_OPTIONS = ["--raw", "-z", "--no-renames", "--no-abbrev", "--diff-filter=d",
                "--ignore-submodules=none"]

# Private keys in PEM and PGP armour, and token prefixes their issuers
# document: GitHub classic and fine-grained tokens, Google OAuth access
# tokens, API keys and OAuth client secrets, AWS access key ids, PyPI,
# Anthropic, Hugging Face and NVIDIA NGC tokens.
CREDENTIAL = re.compile(
    r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----"
    r"|(?<![A-Za-z0-9_])gh[pousr]_[A-Za-z0-9]{20,}"
    r"|(?<![A-Za-z0-9_])github_pat_[A-Za-z0-9_]{20,}"
    r"|(?<![A-Za-z0-9_.])ya29\.[A-Za-z0-9_-]{20,}"
    r"|(?<![A-Za-z0-9_-])AIza[0-9A-Za-z_-]{35}(?![0-9A-Za-z_-])"
    r"|(?<![A-Za-z0-9_-])GOCSPX-[A-Za-z0-9_-]{20,}"
    r"|(?<![A-Za-z0-9])(?:AKIA|ASIA)[0-9A-Z]{16}(?![0-9A-Za-z])"
    r"|(?<![A-Za-z0-9_-])pypi-AgE[A-Za-z0-9_-]{50,}"
    r"|(?<![A-Za-z0-9_-])sk-ant-[A-Za-z0-9_-]{20,}"
    r"|(?<![A-Za-z0-9_])hf_[A-Za-z0-9]{30,}"
    r"|(?<![A-Za-z0-9_-])nvapi-[A-Za-z0-9_-]{40,}")


def keyed_credential(space):
    """A kubeconfig's embedded key and an HTTP credential: a value after
    its key, with `space` between the parts. An HTTP credential needs no
    prefix, only a value long enough not to be a placeholder word. Between
    key and value YAML allows node properties (`&anchor`, `!!binary`), a
    block scalar header (`|`, `>-`, `|2`) and comments. A client-key-data
    "value" followed by a colon on its line is the next mapping key."""
    parts = {"s": space}
    parts["between"] = (r"(?:[&!][^\s,]*%(s)s+)*(?:[|>][0-9+-]{0,2}(?![^\s#]))?"
                        r"(?:%(s)s*#[^\n]*)*") % parts
    return re.compile(
        (r"(?<![A-Za-z0-9_-])client-key-data\\*[\"']?%(s)s*:%(s)s*%(between)s%(s)s*"
         r"\\*[\"']?%(s)s*[A-Za-z0-9+/]{20,}(?![A-Za-z0-9+/]*[^\S\n]*:)"
         r"|(?<![A-Za-z0-9_-])(?i:(?:Proxy-)?Authorization)\\*[\"']?%(s)s*[:=]%(s)s*%(between)s%(s)s*"
         r"\\*[\"']?%(s)s*(?i:Bearer|Basic)%(s)s+[A-Za-z0-9._~+/-]{16,}=*") % parts)


KEYED_CREDENTIAL = keyed_credential(r"[ \t]")
# Pretty-printed JSON puts a value on the line after its key, and a YAML
# block scalar always does. Lines are split before the per-line shapes
# run, so the keyed forms are matched across the whole text as well, with
# any white space between the parts, as a finding on the key's line.
KEYED_CREDENTIAL_ACROSS_LINES = keyed_credential(r"\s")

SERVICE_ACCOUNT = re.compile(
    r"[A-Za-z0-9._%+-]+@(?:[A-Za-z0-9-]+\.)*gserviceaccount\.com(?![A-Za-z0-9-])",
    re.IGNORECASE)
# A project number, in a resource path or keyed as one. The synthetic
# projects/1/ bodies in platform tests and projects/REDACTED/ are not
# numbers of six or more digits.
PROJECT_NUMBER = re.compile(
    r"(?<![A-Za-z0-9_-])projects/[0-9]{6,}(?![0-9])"
    r"|(?i:project_?number)[\"']?[ \t]*[:=][ \t]*[\"']?[0-9]{6,}(?![0-9])")

# A name directly under /home or /Users, under C:\Users, or in the
# -Users-<name> and -home-<name> forms tools use to encode a home
# directory in a single path component. A name must start like an
# account, so /home/.config is not one. GitHub-hosted runners check out
# under /home/runner.
HOME_PATH = re.compile(r"/(?:home|Users)/([A-Za-z0-9_][^/\s\"'`:;,()<>\[\]{}|\\=*?$]*)")
WINDOWS_HOME = re.compile(
    r"(?<![A-Za-z0-9])[A-Za-z]:\\+Users\\+([A-Za-z0-9_][^\\/\s\"'`:;,()<>]*)", re.IGNORECASE)
# Not after a letter, a digit or a dash, so an option (--home-dir) or a
# word (work-from-home-) is not one. The name ends at the next dash, so the
# synthetic operator is recognised by its full name.
ENCODED_HOME = re.compile(r"(?<![A-Za-z0-9_-])-(?:Users|home)-([A-Za-z0-9_][A-Za-z0-9_.]*)")
HOME_ALLOWED = frozenset((SYNTHETIC_OPERATOR, "Shared", "runner"))

DEVICE_UUID = re.compile(
    r"(?<![0-9A-Za-z])(?:GPU|MIG)-([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}"
    r"-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})(?![0-9A-Za-z])")
ZERO_UUID_PREFIX = "00000000-0000-0000-0000-"

# The IPv4 rules are check-evidence-publication.sh's: a pip pin, a Debian
# revision or a wheel name makes a dotted quad a version.
IPV4 = re.compile(
    r"(?<![A-Za-z0-9~+])(?<!\d\.)(?<![=<>!~]=)(\d{1,3}(?:\.\d{1,3}){3})"
    r"(?![A-Za-z0-9~+])(?!\.\d)")
DEBIAN_REVISION = re.compile(r"-\d(?!\d{0,2}(?:\.\d{1,3}){3}(?![0-9]))")
PACKAGE_NAME_DASH = re.compile(r"[A-Za-z0-9]-$")
PACKAGE_FILE_SUFFIX = re.compile(
    r"(?:\.dist-info|-(?:\d[A-Za-z0-9_]*-)?"
    r"[A-Za-z0-9_.]+-[A-Za-z0-9_.]+-[A-Za-z0-9_.]+\.whl)"
    r"(?=$|[/\\\s\"':;,()\[\]{}])")
# 169.254.169.254 is the metadata address every cloud instance shares.
IPV4_ALLOWED = frozenset(("169.254.169.254",))
# Where a colon-hex run starts: not inside a word (x86_64::), a number or
# another run; `_2001:db8::1_` in Markdown emphasis is not inside a word.
# ipv6_address() decides what the run is.
IPV6 = re.compile(r"(?<![0-9A-Za-z:.])(?<![0-9A-Za-z]_)(?=[0-9A-Fa-f]{0,4}:[0-9A-Fa-f]{0,4}:)")
ASCII_WORD = frozenset("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz")
HEX_COLON = frozenset("0123456789abcdefABCDEF:")
# fe80::1 is the link-local address macOS configures on lo0 on every Mac.
IPV6_ALLOWED = frozenset((ipaddress.ip_address("fe80::1"),))
# A bracket directly after a name opens a subscript, so `given[1::2]` is a
# slice. A bracketed address in a URL follows `//` or a space instead.
SUBSCRIPT = re.compile(r"[A-Za-z0-9_\])]\[\Z")
# Loopback, unspecified and "this network" (RFC 1122: no host has an
# address there), and the RFC 5737 and RFC 3849 documentation ranges.
ALLOWED_NETWORKS = tuple(ipaddress.ip_network(n) for n in (
    "127.0.0.0/8", "0.0.0.0/8", "192.0.2.0/24", "198.51.100.0/24",
    "203.0.113.0/24", "::1/128", "::/128", "2001:db8::/32"))

# Private planning material, as generic shapes only: a sibling
# repository's name, and label forms its documents use for ledger rows,
# hardware gates and numbered items. The shapes name no particular label
# and no count of them. Boundaries are alphanumeric, not \b, so a label
# beside an underscore still matches.
PRIVATE_REPOSITORY = re.compile(r"[A-Za-z0-9]+-internals", re.IGNORECASE)
PRIVATE_LABEL = re.compile(
    r"(?<![A-Za-z0-9])(?:PR-[0-9]{1,3}[a-z]?|HW-[A-Z][0-9]{1,2}|XM[0-9]{2}|[CD][0-9]{2})"
    r"(?![A-Za-z0-9])")

SOURCE_CLASSES = frozenset((
    "credential", "service-account", "cloud-project-number", "home-path",
    "device-uuid", "ipv4", "ipv6", "private-repository", "private-label",
    "binary", "symlink", "submodule"))
# check-evidence-publication.sh's classes, for its findings on evidence.
EVIDENCE_CLASSES = frozenset((
    "journal-field", "journal-host", "hostname", "machine-id", "home-path",
    "device-uuid", "uuid", "ipv4", "ipv6", "mac", "email", "cloud-project",
    "internal-dns", "serial", "credential", "planning-id", "symlink",
    "special-file", "binary"))
NEVER_ALLOWED = frozenset(("credential",))

# As in check-evidence-publication.sh: a control sequence or an escape
# directly before a value hides it from every rule that needs a boundary.
CONTROL = re.compile(r"(?:\x1b|\\(?:u001[bB]|x1[bB]|033|e))\[[0-?]*[ -/]*[@-~]")
TEXT_ESCAPE = re.compile(r"\\(?:u[0-9a-fA-F]{4}|x[0-9a-fA-F]{2}|[0-7]{1,3}|\S)")
# And an escaped separator hides a path: `\/Users\/<name>` is a home path.
# An escaped line break separates a key from its value as a real one does.
DECODABLE = re.compile(r"\\(?:u([0-9a-fA-F]{4})|x([0-9a-fA-F]{2})|(/)|([nrt]))")
ESCAPED_BREAKS = {"n": "\n", "r": "\r", "t": "\t"}
# The printable runs of a binary file, as `strings` would show them. In
# compressed data, short shapes (addresses, planning labels, short
# literals) occur by chance, so a run is checked for the long, specific
# ones only.
PRINTABLE_RUN = re.compile(rb"[\x20-\x7e\t]{6,}")
BINARY_CLASSES = frozenset((
    "credential", "service-account", "cloud-project-number", "home-path", "device-uuid"))
LITERAL_CLASSES = ("operator-literal", "local-identity")

EVIDENCE_FINDING = re.compile(r"^(.*):(\d+): (.+?)(?: \((\d+) chars\))?$")


def fault(message):
    print("check-public-hygiene: " + message, file=sys.stderr)
    raise SystemExit(FAULT)


def allowed_address(address):
    return any(address.version == net.version and address in net for net in ALLOWED_NETWORKS)


def is_version(line, start, end):
    """Whether the dotted quad at line[start:end] is a package version."""
    if DEBIAN_REVISION.match(line, end):
        return True
    return bool(PACKAGE_NAME_DASH.search(line[max(0, start - 2):start])
                and PACKAGE_FILE_SUFFIX.match(line, end))


def ipv6_address(line, start):
    """(address, written) for the colon-hex run at line[start:], or None.

    The run is an address as written, or one followed by `:` and the text
    after it (`: refused`), or by `:<port>` when unbracketed. A run that
    goes on into a dotted quad is mixed notation, judged from its quad
    (mixed_before); one that goes on into a word is an identifier
    (`x86_64::_mm`); one longer than any address (a fingerprint, a dump)
    parses as nothing.
    """
    end = start
    while end < len(line) and line[end] in HEX_COLON:
        end += 1
    run = line[start:end]
    after = line[end:end + 2]
    if after[:1] == "." and after[1:2].isdigit():
        return None
    if after[:1] in ASCII_WORD or (after[:1] == "_" and after[1:2] in ASCII_WORD):
        return None
    trimmed = run[:-1] if run.endswith(":") and not run.endswith("::") else run
    head, _, last = trimmed.rpartition(":")
    candidates = [run, trimmed]
    if last.isdigit() and int(last) <= 65535 and head and not head.endswith(":"):
        candidates.append(head)
    for candidate in candidates:
        try:
            return ipaddress.IPv6Address(candidate), candidate
        except ValueError:
            pass
    return None


def mixed_before(line, start, end):
    """(address, written) for the IPv6 address in mixed notation whose
    dotted quad is line[start:end], or None: the longest colon-hex run
    before the quad that parses with it. The notation does not make an
    address IPv4-mapped, only its value does."""
    if start == 0 or line[start - 1] != ":":
        return None
    first = start - 1
    while first > 0 and line[first - 1] in HEX_COLON:
        first -= 1
    for begin in range(first, start - 1):
        try:
            return ipaddress.IPv6Address(line[begin:end]), line[begin:end]
        except ValueError:
            pass
    return None


def scan_variant(line, literals):
    """(class, length, value) for each finding on one variant of a line."""
    found = []

    def add(cls, value):
        found.append((cls, len(value), value))

    for pattern in (CREDENTIAL, KEYED_CREDENTIAL):
        for m in pattern.finditer(line):
            add("credential", m.group(0))
    for m in SERVICE_ACCOUNT.finditer(line):
        add("service-account", m.group(0))
    for m in PROJECT_NUMBER.finditer(line):
        add("cloud-project-number", m.group(0))
    for pattern in (HOME_PATH, WINDOWS_HOME, ENCODED_HOME):
        for m in pattern.finditer(line):
            if pattern is ENCODED_HOME and line.startswith(SYNTHETIC_OPERATOR, m.start(1)):
                continue
            # A name that ends a sentence keeps its full stop out.
            if m.group(1).rstrip(".") not in HOME_ALLOWED:
                add("home-path", m.group(1))
    for m in DEVICE_UUID.finditer(line):
        if not m.group(1).startswith(ZERO_UUID_PREFIX):
            add("device-uuid", m.group(1))
    # An address is judged by its value, never by its spelling. One that is
    # IPv4-mapped is its IPv4 address, reported as ipv4 whichever way it is
    # written; any other is IPv6. A dotted quad that ends an IPv6 address
    # is part of it, not an IPv4 address of its own, and no package
    # version either. The quad is found wherever IPV4 finds one, including
    # where IPV6 starts no run: after a word, a colon or an escape
    # (`peer:2001:db8::192.0.2.1`, `\e2001:db8::192.0.2.1`).
    def judge(address, written, quad):
        mapped = address.ipv4_mapped
        if mapped is not None:
            if str(mapped) not in IPV4_ALLOWED and not allowed_address(mapped):
                add("ipv4", quad or written)
        elif address not in IPV6_ALLOWED and not allowed_address(address):
            # The written form, as the evidence scanner measures it, so a
            # value both tiers find is one finding where they agree on it.
            add("ipv6", written)

    for m in IPV6.finditer(line):
        if SUBSCRIPT.search(line, 0, m.start()):
            continue
        parsed = ipv6_address(line, m.start())
        if parsed is not None:
            judge(parsed[0], parsed[1], None)
    for m in IPV4.finditer(line):
        value = m.group(1)
        mixed = mixed_before(line, m.start(1), m.end(1))
        if mixed is not None:
            judge(mixed[0], mixed[1], value)
            continue
        if value in IPV4_ALLOWED or is_version(line, m.start(1), m.end(1)):
            continue
        try:
            address = ipaddress.ip_address(value)
        except ValueError:
            continue
        if not allowed_address(address):
            add("ipv4", value)
    for m in PRIVATE_REPOSITORY.finditer(line):
        add("private-repository", m.group(0))
    for m in PRIVATE_LABEL.finditer(line):
        add("private-label", m.group(0))
    for cls, pattern, length in literals:
        for m in pattern.finditer(line):
            found.append((cls, length, m.group(0).lower()))
    return found


def decode_escapes(line):
    def decoded(m):
        if m.group(3):
            return "/"
        if m.group(4):
            return ESCAPED_BREAKS[m.group(4)]
        return chr(int(m.group(1) or m.group(2), 16))
    return CONTROL.sub("", DECODABLE.sub(decoded, line))


def scan_line(line, literals):
    """Counter of (class, matched length) on one line: as written, with
    escapes and control sequences blanked, and with escapes decoded. A
    value every variant finds counts once; two values count twice, even
    when one variant finds each: the variants are merged by the value
    matched, which is never printed."""
    variants = {line}
    if "\\" in line or "\x1b" in line:
        variants.add(TEXT_ESCAPE.sub(" ", CONTROL.sub("", line)))
        variants.add(TEXT_ESCAPE.sub(" ", CONTROL.sub(" ", line)))
        variants.add(decode_escapes(line))
    values = Counter()
    for variant in variants:
        values |= Counter(scan_variant(variant, literals))
    found = Counter()
    for (cls, length, _), count in values.items():
        found[(cls, length)] += count
    return found


# scan_line's variants, other than the line as written.
LINE_VARIANTS = (
    lambda line: TEXT_ESCAPE.sub(" ", CONTROL.sub("", line)),
    lambda line: TEXT_ESCAPE.sub(" ", CONTROL.sub(" ", line)),
    decode_escapes,
)


def keyed_across_lines(lines):
    """Counter of (line, class, length) for the keyed credentials in the
    whole text, in each variant scan_line reads, on the line each key is
    on. A decoded escape can itself be the line break. A match within one
    line is the finding scan_line makes, with the same key, so the union
    in scan_text keeps it once."""
    variants = [lines]
    if any("\\" in line or "\x1b" in line for line in lines):
        variants.extend([variant(line) for line in lines] for variant in LINE_VARIANTS)
    found = Counter()
    for variant in variants:
        starts, offset = [], 0
        for line in variant:
            starts.append(offset)
            offset += len(line) + 1
        here = Counter()
        for m in KEYED_CREDENTIAL_ACROSS_LINES.finditer("\n".join(variant)):
            here[(bisect.bisect_right(starts, m.start()), "credential", len(m.group(0)))] += 1
        found |= here
    return found


def scan_text(text, literals):
    """Counter of (line, class, length) over every line of text, and the
    keyed credentials that span lines."""
    found = Counter()
    lines = text.split("\n")
    for number, line in enumerate(lines, 1):
        for (cls, length), count in scan_line(line, literals).items():
            found[(number, cls, length)] = count
    return found | keyed_across_lines(lines)


def run(args, root, data=None):
    """stdout of a git command in root. Its stderr is never shown: it can
    quote the ref or path it was given."""
    try:
        result = subprocess.run(["git", "-C", root] + args, input=data,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    except OSError:
        fault("git is required")
    if result.returncode != 0:
        fault("git %s failed; no verdict" % args[0])
    return result.stdout


def commit_id(root, ref, what):
    try:
        result = subprocess.run(
            ["git", "-C", root, "rev-parse", "--verify", "--quiet", "--end-of-options",
             ref + "^{commit}"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    except OSError:
        fault("git is required")
    if result.returncode != 0:
        fault("%s does not name a commit in this repository" % what)
    return result.stdout.decode().strip()


def split_raw(output):
    """(mode, blob, path) of each entry in -z --raw output, plain or
    combined. A combined (merge) entry lists every parent's mode and blob,
    then the result's; the result is what the merge wrote."""
    fields = output.decode("utf-8", "surrogateescape").split("\0")
    entries = []
    index = 0
    while index + 1 < len(fields) and fields[index]:
        meta = fields[index]
        parents = len(meta) - len(meta.lstrip(":"))
        parts = meta[parents:].split()
        if parents < 1 or len(parts) != 2 * (parents + 1) + 1:
            fault("unexpected git diff output; no verdict")
        mode, blob = parts[parents], parts[2 * parents + 1]
        if set(mode) != {"0"}:
            entries.append((mode, blob, fields[index + 1]))
        index += 2
    return entries


def read_blobs(root, ids):
    """Contents of the given objects, read in one git cat-file --batch."""
    ids = sorted(set(ids))
    if not ids:
        return {}
    output = run(["cat-file", "--batch"], root, ("\n".join(ids) + "\n").encode())
    blobs = {}
    position = 0
    for object_id in ids:
        end = output.find(b"\n", position)
        header = output[position:end].split()
        if end < 0 or len(header) != 3 or header[1] != b"blob":
            fault("an object in the range cannot be read; no verdict")
        size = int(header[2])
        blobs[object_id] = output[end + 1:end + 1 + size]
        position = end + 1 + size + 1
    return blobs


def repository_roots(root):
    """root and every enclosing checkout: a worktree can sit inside another
    checkout, and a literal file there is one `git add` from a commit."""
    roots = [root]
    current = os.path.realpath(root)
    while True:
        parent = os.path.dirname(current)
        if parent == current:
            return roots
        current = parent
        if os.path.lexists(os.path.join(current, ".git")):
            roots.append(current)


def literal_pattern(value):
    body = re.escape(value)
    if len(value) < SUBSTRING_LITERAL:
        # A short literal as a substring would flag ordinary words.
        body = r"(?<![A-Za-z0-9])" + body + r"(?![A-Za-z0-9])"
    return re.compile(body, re.IGNORECASE)


def load_literals(path, roots):
    try:
        info = os.stat(path)
    except OSError:
        fault("the literal file does not exist or cannot be read")
    if not stat.S_ISREG(info.st_mode):
        fault("the literal file is not a regular file")
    current = os.path.realpath(path)
    while True:
        if any(os.path.samefile(current, root) for root in roots):
            fault("the literal file is inside the repository; keep it outside, "
                  "where it cannot be committed")
        parent = os.path.dirname(current)
        if parent == current:
            break
        current = parent
    try:
        with open(path, encoding="utf-8") as handle:
            raw = handle.read()
    except (OSError, UnicodeDecodeError):
        fault("the literal file cannot be read as UTF-8 text")
    literals = []
    for number, line in enumerate(raw.split("\n"), 1):
        value = line.strip()
        if not value or value.startswith("#"):
            continue
        if len(value) < MIN_LITERAL:
            fault("literal file line %d is shorter than %d characters" % (number, MIN_LITERAL))
        literals.append(("operator-literal literal #%d" % number, literal_pattern(value), len(value)))
    if not literals:
        fault("the literal file lists no literals")
    return literals


USER_LITERAL = "local-identity (user)"


def local_literals():
    """This machine's user name, short host name and gcloud project. A
    lookup that does not answer is no verdict, not a scan without it."""
    try:
        values = [("user", getpass.getuser())]
    except Exception:
        fault("--local cannot determine the user name; no verdict")
    values.append(("host", socket.gethostname().split(".")[0]))
    gcloud = shutil.which("gcloud")
    if gcloud:
        try:
            result = subprocess.run([gcloud, "config", "get-value", "project"],
                                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                    timeout=60)
        except (OSError, subprocess.TimeoutExpired):
            fault("--local: gcloud did not answer for the project; no verdict")
        if result.returncode != 0:
            fault("--local: gcloud failed to report the project; no verdict")
        project = result.stdout.decode("utf-8", "replace").strip()
        if project and project != "(unset)":
            values.append(("project", project))
    return [("local-identity (%s)" % kind, literal_pattern(value), len(value))
            for kind, value in values
            if len(value) >= MIN_LITERAL and value.lower() not in GENERIC_NAMES]


def load_allowlist(data):
    """{(path, class): (count, line)} from the allowlist text."""
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        fault("the allowlist is not UTF-8 text")
    entries = {}
    for number, raw in enumerate(text.split("\n"), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split(None, 3)
        if len(fields) < 4:
            fault("allowlist line %d needs a path, a class, a count and a reason" % number)
        path, cls, count = fields[0], fields[1], fields[2]
        if cls not in SOURCE_CLASSES | EVIDENCE_CLASSES:
            fault("allowlist line %d names an unknown class" % number)
        if cls in NEVER_ALLOWED:
            fault("allowlist line %d: a %s finding can never be allowlisted" % (number, cls))
        if cls not in SOURCE_CLASSES:
            # The evidence scanner reports one finding per class, line and
            # length, so its occurrences cannot be counted; and evidence is
            # made publishable by sanitizing it (rule 3), never by exception.
            fault("allowlist line %d: an evidence-only %s finding is sanitized, "
                  "never allowlisted" % (number, cls))
        if is_evidence(path):
            # Evidence is sanitized (rule 3), never excepted. And the
            # evidence scanner reports one finding per line, class and
            # length, so a value only it decodes (a byte array, a JSON
            # escape) beside one the source policy finds would not be
            # counted against the entry.
            fault("allowlist line %d names a file under an evidence path, which is "
                  "sanitized, never allowlisted" % number)
        if not count.isdigit() or int(count) < 1:
            fault("allowlist line %d needs a count of one or more" % number)
        if (path, cls) in entries:
            fault("allowlist line %d repeats line %d" % (number, entries[(path, cls)][1]))
        entries[(path, cls)] = (int(count), number)
    return entries


def parse_arguments(argv):
    options = {"base": None, "tree": False, "messages": [], "literals": None, "local": False}
    index = 0

    def value(flag):
        if index + 1 >= len(argv) or not argv[index + 1]:
            fault("%s requires a value" % flag)
        return argv[index + 1]

    while index < len(argv):
        arg = argv[index]
        if arg in ("-h", "--help"):
            # Not 0: exit 0 means "publishable", and nothing was scanned.
            print(USAGE)
            raise SystemExit(FAULT)
        if arg == "--base":
            if options["base"] is not None:
                fault("--base given twice")
            options["base"] = value(arg)
            index += 1
        elif arg == "--message":
            options["messages"].append(value(arg))
            index += 1
        elif arg == "--literals":
            if options["literals"] is not None:
                fault("--literals given twice")
            options["literals"] = value(arg)
            index += 1
        elif arg == "--tree":
            options["tree"] = True
        elif arg == "--local":
            options["local"] = True
        else:
            fault("unknown argument %d\n%s" % (index + 1, USAGE))
        index += 1
    if options["tree"] == (options["base"] is not None):
        fault("choose one of --base REF or --tree\n" + USAGE)
    if options["tree"] and options["messages"]:
        fault("--message applies to --base scans only")
    return options


def evidence_findings(scanner, root_dir, literal_file):
    """(ref, line, class, length) from the evidence scanner over root_dir."""
    mode = ["--literals", literal_file] if literal_file else ["--patterns-only"]
    try:
        result = subprocess.run([scanner] + mode + [root_dir],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError:
        fault("the evidence scanner cannot be run")
    if result.returncode not in (0, 1):
        # Its own messages never quote scanned text; pass them on.
        sys.stderr.write(result.stderr.decode("utf-8", "replace"))
        fault("the evidence scanner reached no verdict")
    findings = []
    for line in result.stdout.decode("utf-8", "replace").splitlines():
        if result.returncode == 0 or not line:
            continue
        m = EVIDENCE_FINDING.match(line)
        if m is None:
            fault("unexpected evidence scanner output; no verdict")
        length = int(m.group(4)) if m.group(4) else None
        findings.append((m.group(1), int(m.group(2)), m.group(3), length))
    if result.returncode == 1 and not findings:
        fault("the evidence scanner reported findings it did not list; no verdict")
    return findings


def is_evidence(path):
    # Case-insensitively: a checkout on a case-insensitive disk, as on
    # macOS, puts test/Platform/x in test/platform/.
    return path.lower().startswith(EVIDENCE_PREFIXES)


def holds_evidence(path):
    """Whether path is under an evidence prefix or is one of the
    directories above one: a symlink there moves evidence elsewhere."""
    folded = path.lower() + "/"
    return is_evidence(path) or any(prefix.startswith(folded) for prefix in EVIDENCE_PREFIXES)


def listing(root):
    """Every entry below root, as check-evidence-publication.sh numbers them:
    relative paths of files and directories, sorted."""
    entries = []
    for directory, names, files in os.walk(root):
        for name in names + files:
            entries.append(os.path.relpath(os.path.join(directory, name), root))
    return sorted(entries)


def main(argv):
    script_dir, rest = argv[0], argv[1:]
    options = parse_arguments(rest)
    evidence_scanner = os.path.join(script_dir, "check-evidence-publication.sh")
    if not os.access(evidence_scanner, os.X_OK):
        fault("check-evidence-publication.sh is missing beside this scanner")

    try:
        top = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    except OSError:
        fault("git is required")
    if top.returncode != 0:
        fault("run this inside the repository to scan")
    root = top.stdout.decode().strip()
    head = commit_id(root, "HEAD", "HEAD")

    roots = repository_roots(root)
    script_root = os.path.dirname(os.path.dirname(script_dir))
    if not any(os.path.samefile(script_root, r) for r in roots):
        roots.extend(repository_roots(script_root))
    literals = load_literals(options["literals"], roots) if options["literals"] else []
    if options["local"]:
        literals.extend(local_literals())
    identity_literals = [literal for literal in literals if literal[0] != USER_LITERAL]

    # Every version of every file to scan: (path, mode, blob, commit), the
    # commit None for the version at HEAD. Earlier versions are the ones a
    # commit in the range wrote that HEAD no longer has; for a merge, the
    # files its result differs from every parent in.
    versions = []
    commits = []
    messages = []
    if options["tree"]:
        output = run(["ls-tree", "-r", "-z", "--full-tree", head], root)
        for entry in output.decode("utf-8", "surrogateescape").split("\0"):
            if not entry:
                continue
            meta, path = entry.split("\t", 1)
            mode, _, blob = meta.split()
            versions.append((path, mode, blob, None))
    else:
        base = commit_id(root, options["base"], "--base")
        merge_base = run(["merge-base", base, head], root).decode().strip()
        seen = set()
        for mode, blob, path in split_raw(run(["diff"] + DIFF_OPTIONS + [merge_base, head], root)):
            versions.append((path, mode, blob, None))
            seen.add((path, blob))
        listed = run(["rev-list", "--reverse", "--parents", head, "^" + base], root)
        for line in listed.decode().splitlines():
            commit, parents = line.split()[0], line.split()[1:]
            combined = ["-c"] if len(parents) > 1 else []
            output = run(["diff-tree", "-r", "--root", "--no-commit-id"] + combined
                         + DIFF_OPTIONS + [commit], root)
            for mode, blob, path in split_raw(output):
                if (path, blob) not in seen:
                    versions.append((path, mode, blob, commit))
                    seen.add((path, blob))
        log = run(["log", "-z", "--no-color",
                   "--format=tformat:%H%n%an <%ae>%n%cn <%ce>%n%B", head, "^" + base], root)
        for record in log.decode("utf-8", "surrogateescape").split("\0"):
            record = record.lstrip("\n")
            if record:
                commits.append((record.split("\n", 3) + ["", "", ""])[:4])
        branch = subprocess.run(["git", "-C", root, "symbolic-ref", "-q", "--short", "HEAD"],
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        if branch.returncode == 0:
            messages.append(("branch", branch.stdout.decode("utf-8", "surrogateescape").strip()))

    # A tree entry can be named `..` with git plumbing, and git only warns.
    # Such a path is neither reviewable nor safe to stage.
    for path, _, _, _ in versions:
        parts = path.split("/")
        if path.startswith("/") or any(part in ("", ".", "..") for part in parts):
            fault("a path in the range is not a plain relative path; no verdict")

    for position, path in enumerate(options["messages"], 1):
        try:
            with open(path, "rb") as handle:
                data = handle.read()
        except OSError:
            fault("--message file %d cannot be read" % position)
        try:
            messages.append(("message#%d" % position, data.decode("utf-8")))
        except UnicodeDecodeError:
            fault("--message file %d is not UTF-8 text" % position)

    if not versions and not commits and not options["messages"]:
        fault("nothing to scan; a scan of nothing is not a verdict")

    blobs = read_blobs(root, [blob for _, mode, blob, _ in versions if mode != MODE_SUBMODULE])
    allowlist_blob = run(["ls-tree", "-z", head, "--", ALLOWLIST_PATH], root)
    allowlist = {}
    if allowlist_blob:
        allowlist = load_allowlist(read_blobs(root, [allowlist_blob.split()[2].decode()])
                                   .popitem()[1])

    # Every finding, keyed (group, ordinal, line, class, length, subject)
    # with how many times it occurs. A file's subject is (path, commit) and
    # becomes a ref only when printed, once every name finding is known: a
    # path either tier flags is never printed. The two tiers can report the
    # same finding on evidence, so a key keeps the larger count, not a sum.
    # Where they class or measure a value differently (the source policy
    # judges a mixed-notation or IPv4-mapped IPv6 address by value), it
    # prints once per tier; evidence is never allowlisted, so only the
    # count printed differs.
    results = Counter()

    def add(key, count=1):
        results[key] = max(results[key], count)

    paths = sorted(set(path for path, _, _, _ in versions))
    ordinal_of = dict((path, n) for n, path in enumerate(paths, 1))
    masked = set()
    for path in paths:
        for (cls, length), count in scan_line("/" + path, literals).items():
            masked.add(path)
            add((0, ordinal_of[path], 0, cls, length, (path, None)), count)

    evidence = []
    for path, mode, blob, commit in versions:
        def report(line, cls, length, count=1):
            add((0, ordinal_of[path], line, cls, length, (path, commit)), count)

        data = b"" if mode == MODE_SUBMODULE else blobs[blob]
        if is_evidence(path):
            # Every evidence version reaches the evidence scanner, so its
            # name is judged there whatever the content; a symlink or a
            # submodule goes as an empty stand-in.
            evidence.append((path, commit, data if mode in MODE_FILES else b""))
        if mode == MODE_SUBMODULE:
            report(0, "submodule", None)
            continue
        if mode == MODE_SYMLINK and holds_evidence(path):
            report(0, "symlink", None)
        try:
            if b"\0" in data:
                raise UnicodeDecodeError("utf-8", data, 0, 1, "NUL")
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            report(0, "binary", None)
            runs = Counter()
            for run_bytes in PRINTABLE_RUN.findall(data):
                runs.update(scan_line(run_bytes.decode("ascii"), literals))
            for (cls, length), count in runs.items():
                if cls in BINARY_CLASSES or (
                        cls.startswith(LITERAL_CLASSES) and length >= SUBSTRING_LITERAL):
                    report(0, cls, length, count)
            continue
        for (line, cls, length), count in scan_text(text, literals).items():
            report(line, cls, length, count)

    if evidence:
        # One directory per version, so names that differ only in case or
        # normalization never collide on a case-insensitive disk.
        work = os.path.realpath(tempfile.mkdtemp(prefix="public-hygiene-"))
        try:
            for index, (path, _, data) in enumerate(evidence):
                target = os.path.join(work, str(index), path)
                # Unreachable while the plain-relative check above holds;
                # kept so a write never leaves the staging directory even
                # if that check is ever loosened.
                if not os.path.realpath(target).startswith(work + os.sep):
                    fault("an evidence path leaves the staging directory; no verdict")
                os.makedirs(os.path.dirname(target), exist_ok=True)
                with open(target, "wb") as handle:
                    handle.write(data)
            entries = listing(work)
            for ref, line, cls, length in evidence_findings(
                    evidence_scanner, work, options["literals"]):
                if ref.startswith("path#"):
                    # The evidence scanner masked an entry whose name is
                    # itself a finding: N counts its sorted listing.
                    number = int(ref[len("path#"):])
                    if not 1 <= number <= len(entries):
                        fault("unexpected evidence scanner output; no verdict")
                    ref = entries[number - 1]
                index, _, rest = ref.partition("/")
                if not index.isdigit() or int(index) >= len(evidence) or not rest:
                    fault("unexpected evidence scanner output; no verdict")
                path, commit, _ = evidence[int(index)]
                if rest != path and not path.startswith(rest + "/"):
                    fault("unexpected evidence scanner output; no verdict")
                # A line-0 finding other than about the content is about a
                # name, the file's or a directory's above it. The evidence
                # scanner judges each entry by its full path, so a flagged
                # directory flags every file below it as well.
                if line == 0 and cls not in ("binary", "symlink", "special-file"):
                    masked.add(path)
                add((0, ordinal_of[path], line, cls, length, (path, commit)))
        finally:
            shutil.rmtree(work, ignore_errors=True)

    for ordinal, (commit, author, committer, body) in enumerate(commits, 1):
        subject = "commit " + commit[:12]
        for label, text in (("author", author), ("committer", committer)):
            for (cls, length), count in scan_line(text, identity_literals).items():
                add((1, ordinal, label, cls, length, subject), count)
        for (line, cls, length), count in scan_text(body, literals).items():
            add((1, ordinal, line, cls, length, subject), count)
    for position, (label, text) in enumerate(messages, 1):
        for (line, cls, length), count in scan_text(text, literals).items():
            add((2, position, line, cls, length, label), count)

    def ref_of(subject):
        if not isinstance(subject, tuple):
            return subject
        path, commit = subject
        name = "changed-path#%d" % ordinal_of[path] if path in masked else path
        return name if commit is None else name + "@" + commit[:12]

    # How many findings an allowlist entry could accept, per file version:
    # an entry accepts a version's findings of its class when there are no
    # more of them than its count, counting every occurrence. A finding in
    # a commit or a message, or in a file whose name is a finding, is never
    # accepted.
    counted = Counter()
    for (group, ordinal, line, cls, length, subject), count in results.items():
        if isinstance(subject, tuple) and subject[0] not in masked \
                and (subject[0], cls) in allowlist:
            counted[(subject, cls)] += count
    accepted = set(key for key, found in counted.items()
                   if found <= allowlist[(key[0][0], key[1])][0])

    kept = []
    for (group, ordinal, line, cls, length, subject), count in results.items():
        if (subject, cls) not in accepted:
            kept.extend([(group, ordinal, line, cls, length, ref_of(subject))] * count)
    if options["tree"]:
        for (path, cls), (count, line) in allowlist.items():
            found = counted.get(((path, None), cls), 0)
            if found != count:
                kept.append((3, 0, line, "stale-allowlist (%d found)" % found, None, ALLOWLIST_PATH))

    if kept:
        def order(r):
            line = r[2] if isinstance(r[2], int) else -1
            return (r[0], r[1], r[5], line, str(r[2]), r[3], -1 if r[4] is None else r[4])

        for _, _, line, cls, length, ref in sorted(kept, key=order):
            suffix = "" if length is None else " (%d chars)" % length
            print("%s:%s: %s%s" % (ref, line, cls, suffix))
        sys.stdout.flush()
        print("\n%d finding(s): not publishable. Rewrite the branch so that no commit "
              "carries them before it is pushed: a later commit that removes a value "
              "does not clear it. See rule 10 of "
              "docs/validation/fixture-and-evidence-rules.md." % len(kept), file=sys.stderr)
        return FINDINGS
    print("publishable: %d file version(s), %d commit(s), %d message(s) scanned, no findings"
          % (len(versions), len(commits), len(messages)))
    return 0


try:
    sys.exit(main(sys.argv[1:]))
except SystemExit:
    raise
except BaseException as error:
    # A traceback could quote the text being scanned. Name the error only.
    print("check-public-hygiene: internal error (%s)" % type(error).__name__, file=sys.stderr)
    sys.exit(INTERNAL)
PY

case "$scan_status" in
  0) exit 0 ;;
  3) exit 1 ;;
  2) exit 2 ;;
  *)
    printf 'check-public-hygiene: no verdict (python exit %s)\n' "$scan_status" >&2
    exit 2
    ;;
esac
