#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Is this evidence safe to publish in a public repository?
#
# Lifecycle evidence is recorded on real machines and committed to a
# public repository. The stage logs, journal captures and failing-stage
# details carry whatever those machines printed: host names, account
# names, machine and boot ids, addresses, device UUIDs and serials. Once
# committed, a value stays in the branch history until that history is
# rewritten, and CI logs outlive the rewrite. So this scanner fails
# closed, and it never prints a value it matched.
#
# Two sources of truth, because neither is enough alone:
#
#   patterns  identifier shapes no published evidence may carry unless
#             they use the self-describing synthetic value named in
#             docs/validation/evidence/v0.2.1/README.md
#   literals  the operator's own host, account, project and instance
#             names, from a private file kept outside the repository;
#             a pattern cannot know that an ordinary word is a host name
#
# Every file and directory name below each PATH is scanned as well as
# every file's contents. Contents are scanned line by line, then again as
# the decoded strings of every JSON object or array found in them: a whole
# document, JSON Lines, concatenated pretty-printed records, or a record
# quoted after other text. JSON arrays of byte values are decoded too,
# because journalctl encodes a non-printable or non-UTF-8 field value that
# way. A symlink, a special file, and a file that is not UTF-8 text or
# contains NUL are findings: archives and terminal captures cannot be
# reviewed and are never publishable.
#
# Findings print as `<ref>:<line>: <class> (<n> chars)`. `<ref>` is the
# path relative to the PATH argument, or `path#N` when a component of the
# path is itself a finding; N is line N of
# `(cd PATH && find . -mindepth 1 | LC_ALL=C sort)`. A finding inside a
# JSON value is reported on the line the value starts on. Line 0 means the
# finding is about the name or the whole file rather than the contents.
# Literal findings name the literal only by its line number in the
# literal file.
#
# Exit 0 when the evidence is publishable, 1 when there are findings, and
# 2 when the scan did not reach a verdict (bad arguments or --help, an
# unreadable file, nothing to scan, an internal error).
#
# Usage:
#   check-evidence-publication.sh --literals FILE PATH...
#   check-evidence-publication.sh --patterns-only PATH...
#
# TP_EVIDENCE_REPO_ROOT overrides the repository root that a literal file
# must lie outside of. It exists for the scanner's own tests.

set -Eeuo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)" || {
  printf 'check-evidence-publication: cannot locate the scanner directory\n' >&2
  exit 2
}
repo_root="${TP_EVIDENCE_REPO_ROOT:-${script_dir}/../..}"

command -v python3 >/dev/null 2>&1 || {
  printf 'check-evidence-publication: python3 is required\n' >&2
  exit 2
}

# Python exits 1 on an uncaught exception, so 1 cannot mean "findings":
# a crash would then read as a verdict. Exit 3 is reserved for the
# findings verdict and mapped to 1 below; everything else is a fault.
scan_status=0
python3 - "$repo_root" "$@" <<'PY' || scan_status=$?
# Keep this compatible with Python 3.9: that is /usr/bin/python3 on the
# macOS hosts operators sanitize evidence on.
import ipaddress
import json
import os
import re
import stat
import sys

FINDINGS = 3
FAULT = 2
INTERNAL = 4

USAGE = """usage: check-evidence-publication.sh --literals FILE PATH...
       check-evidence-publication.sh --patterns-only PATH...

Exit 0: publishable. Exit 1: findings. Exit 2: no verdict."""

SYNTHETIC_HOST = "tp-synthetic-host"
SYNTHETIC_OPERATOR = "tp-synthetic-operator"
ZERO_ID = "0" * 32
MIN_LITERAL = 3
SUBSTRING_LITERAL = 8

# The journal fields a published capture may keep. Everything else
# systemd attaches (_HOSTNAME, _MACHINE_ID, _BOOT_ID, __CURSOR,
# _CMDLINE, ...) describes the machine rather than the service.
JOURNAL_KEYS = frozenset((
    "MESSAGE",
    "PRIORITY",
    "SYSLOG_IDENTIFIER",
    "UNIT",
    "_PID",
    "_SYSTEMD_UNIT",
    "_SYSTEMD_INVOCATION_ID",
    "__REALTIME_TIMESTAMP",
))
JOURNAL_TEXT_FIELD = re.compile(
    r"(?<![A-Za-z0-9_])(_HOSTNAME|_MACHINE_ID|_BOOT_ID|__CURSOR)=")
# The same fields as a quoted key, where no JSON value can be decoded
# around it: a record cut short by a log tail, or escaped inside a string.
JOURNAL_QUOTED_KEY = re.compile(r"\\*[\"'](_[A-Za-z0-9_]+)\\*[\"'][ \t]*:")

MONTH = r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)"
WEEKDAY = r"(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun)"
# journalctl's short, short-precise, short-iso(-precise) and short-full
# prefixes, followed by `<host> <identifier>[pid]: `. Unanchored, because
# a failing stage's report detail joins log lines with spaces.
JOURNAL_SHORT = re.compile(
    r"(?<![A-Za-z0-9])(?:"
    + MONTH + r" [ \d]\d \d\d:\d\d:\d\d(?:\.\d+)?"
    + r"|\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?[+-]\d\d:?\d\d"
    + r"|" + WEEKDAY + r" \d{4}-\d\d-\d\d \d\d:\d\d:\d\d(?:\.\d+)? \S+"
    + r") (\S+) [^\s:\[\]]+(?:\[\d+\])?:(?=\s|$)")
JOURNAL_BOOT = re.compile(r"-- Boot ([0-9a-fA-F]{32}) --")
JOURNAL_DIR = re.compile(r"/(?:var|run)/log/journal/([0-9a-fA-F]{32})(?![0-9A-Za-z])")

HOSTNAME_FIELD = re.compile(r"(?:Static|Transient) hostname:[ \t]*(\S+)")
PRETTY_HOSTNAME = re.compile(r"Pretty hostname:[ \t]*(\S.*?)\s*$")
UNAME = re.compile(
    r"(?<![A-Za-z0-9])(?:Linux|Darwin) (\S+) \d+\.\d+\S* (?:#|Darwin Kernel)")
MACHINE_ID = re.compile(r"(?:Machine|Boot) ID:[ \t]*([0-9a-fA-F]{32})(?![0-9A-Za-z])")

HOME_PATH = re.compile(r"/(?:home|Users)/([^/\s\"'`:;,()<>\[\]{}|\\=*?]+)")
HOME_ALLOWED = frozenset((SYNTHETIC_OPERATOR, "Shared"))

UUID = re.compile(
    r"(?<![0-9A-Za-z])([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}"
    r"-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})(?![0-9A-Za-z])")
# The all-zero namespace, as in platform/tests/accelerator_fixtures.rs.
ZERO_UUID_PREFIX = "00000000-0000-0000-0000-"
DEVICE_PREFIX = re.compile(r"(?:GPU|MIG)-$")
# Per-invocation random ids the product mints: cli/src/lib.rs (cli-),
# agent/src/coordinator.rs (tx-), cli/src/commands/deploy.rs (deploy-).
RANDOM_ID_PREFIX = re.compile(r"(?<![A-Za-z0-9_-])(?:cli|tx|deploy)-$")

# Excludes dotted-quad runs inside version strings: a Debian version such
# as 10.3.0.30-1, a longer dotted run, and a pip pin such as ==10.3.0.30.
IPV4 = re.compile(
    r"(?<![A-Za-z0-9~+-])(?<!\d\.)(?<![=<>!~]=)(\d{1,3}(?:\.\d{1,3}){3})"
    r"(?![A-Za-z0-9~+-])(?!\.\d)")
# A version string is never followed by a port, so an address with one is
# matched whatever precedes it: sshd's per-connection unit names put a dash
# before each endpoint (sshd@3-<local>:22-<peer>:<port>.service), which the
# version exclusion above would otherwise let through.
IPV4_WITH_PORT = re.compile(r"(?<![A-Za-z0-9.])(\d{1,3}(?:\.\d{1,3}){3}):\d{1,5}(?!\d)")
IPV4_ALLOWED = frozenset(("0.0.0.0", "169.254.169.254"))
IPV6 = re.compile(
    r"(?<![0-9A-Za-z:.])((?:[0-9A-Fa-f]{0,4}:){2,7}[0-9A-Fa-f]{0,4})(?![0-9A-Za-z:.])")
IPV6_ALLOWED = frozenset((ipaddress.ip_address("::1"), ipaddress.ip_address("::")))
# RFC 5737 and RFC 3849 documentation ranges.
DOCUMENTATION_NETWORKS = tuple(ipaddress.ip_network(n) for n in (
    "192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24", "2001:db8::/32"))

MAC = re.compile(
    r"(?<![0-9A-Za-z:])([0-9A-Fa-f]{2}(?::[0-9A-Fa-f]{2}){5})(?![0-9A-Za-z:])")
# RFC 7042 documentation block, plus the broadcast and all-zero addresses
# `ip link` prints beside a real one.
MAC_DOCUMENTATION_PREFIX = "00:00:5e:00:53:"
MAC_ALLOWED = frozenset(("ff:ff:ff:ff:ff:ff", "00:00:00:00:00:00"))

EMAIL = re.compile(
    r"(?<![A-Za-z0-9._%+-])[A-Za-z0-9._%+-]+@((?:[A-Za-z0-9-]+\.)+[A-Za-z][A-Za-z0-9-]*)"
    r"(?![A-Za-z0-9-])")
EMAIL_DOMAINS = frozenset(("example.com", "example.org", "example.net"))

# As platform/tests/host_identity.rs requires of published GCE fixtures.
CLOUD_PROJECT = re.compile(r"projects/([^/\s\"']+)/")
CLOUD_PROJECT_ALLOWED = "REDACTED"

INTERNAL_DNS = re.compile(
    r"(?<![A-Za-z0-9.-])((?:[A-Za-z0-9-]+\.)+(?:internal|local))(?![A-Za-z0-9-])(?!\.[A-Za-z0-9])")
INTERNAL_DNS_ALLOWED = "metadata.google.internal"

# Up to 16 other characters may sit between the keyword and the separator,
# plus any column padding: `nvidia-smi -q` aligns its separators.
SERIAL = re.compile(
    r"(?i)(?:serial(?:[\s_-]?(?:number|no\.?))?|(?<![a-z])udid)(?![a-z])"
    r"(?:[ \t]*[^:=\n \t]){0,16}?[ \t]*[:=][ \t]*[\"']?([^\s\"',;}]*)")
SERIAL_ALLOWED = re.compile(r"REDACTED|0+")

CREDENTIAL = re.compile(
    r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----"
    r"|(?<![A-Za-z0-9_])gh[pousr]_[A-Za-z0-9]{20,}"
    r"|(?<![A-Za-z0-9_])github_pat_[A-Za-z0-9_]{20,}"
    r"|(?<![A-Za-z0-9_.])ya29\.[A-Za-z0-9_-]{20,}")

# Planning identifiers belong in CHANGELOG.md only, with or without the
# epic segment.
PLANNING_ID = re.compile(r"(?<![A-Za-z0-9])V\d{2,3}(?:-[EFT]\d{2})+(?![A-Za-z0-9])")


def fault(message):
    print("check-evidence-publication: " + message, file=sys.stderr)
    raise SystemExit(FAULT)


def in_network(address, networks):
    return any(address.version == net.version and address in net for net in networks)


# A terminal control sequence (colour, bold), raw or escaped the way JSON,
# Python or C text prints it. It ends in a letter, so `\x1b[1m` directly
# before a value hides it from every rule that needs a boundary there.
CONTROL = re.compile(r"(?:\x1b|\\(?:u001[bB]|x1[bB]|033|e))\[[0-?]*[ -/]*[@-~]")
# A backslash escape in text that was not decoded: `\n` or `\t` before a
# value hides it the same way.
TEXT_ESCAPE = re.compile(r"\\(?:u[0-9a-fA-F]{4}|x[0-9a-fA-F]{2}|[0-7]{1,3}|\S)")


def scan_line(line, literals):
    """(class, matched length) for every finding on one line of text.

    The line is scanned as it is and with control sequences and escapes
    taken out. A control sequence is both removed and replaced with a
    space: removing it keeps `Sep 14 01:42:03<styling> host` one space
    apart, and a space keeps `request<styling><uuid>` two words.
    """
    variants = {line, TEXT_ESCAPE.sub(" ", CONTROL.sub("", line)),
                TEXT_ESCAPE.sub(" ", CONTROL.sub(" ", line))}
    found = set()
    for variant in variants:
        found.update(scan_variant(variant, literals))
    return sorted(found)


def scan_variant(line, literals):
    found = []

    def add(cls, value):
        found.append((cls, len(value)))

    for m in JOURNAL_TEXT_FIELD.finditer(line):
        add("journal-field", m.group(1))
    for m in JOURNAL_QUOTED_KEY.finditer(line):
        if m.group(1) not in JOURNAL_KEYS:
            add("journal-field", m.group(1))
    for m in JOURNAL_SHORT.finditer(line):
        if m.group(1) != SYNTHETIC_HOST:
            add("journal-host", m.group(1))
    for pattern in (JOURNAL_BOOT, JOURNAL_DIR):
        for m in pattern.finditer(line):
            if m.group(1) != ZERO_ID:
                add("journal-host", m.group(1))
    for pattern in (HOSTNAME_FIELD, PRETTY_HOSTNAME, UNAME):
        for m in pattern.finditer(line):
            if m.group(1) != SYNTHETIC_HOST:
                add("hostname", m.group(1))
    for m in MACHINE_ID.finditer(line):
        if m.group(1) != ZERO_ID:
            add("machine-id", m.group(1))
    for m in HOME_PATH.finditer(line):
        if m.group(1) not in HOME_ALLOWED:
            add("home-path", m.group(1))
    for m in UUID.finditer(line):
        value = m.group(1)
        before = line[max(0, m.start() - 9):m.start()]
        if DEVICE_PREFIX.search(before):
            if not value.startswith(ZERO_UUID_PREFIX):
                add("device-uuid", value)
        elif not RANDOM_ID_PREFIX.search(before) and not value.startswith(ZERO_UUID_PREFIX):
            add("uuid", value)
    ipv4 = {}
    for pattern in (IPV4, IPV4_WITH_PORT):
        for m in pattern.finditer(line):
            ipv4[m.start(1)] = m.group(1)
    for value in ipv4.values():
        try:
            address = ipaddress.ip_address(value)
        except ValueError:
            continue
        if not (address.is_loopback or value in IPV4_ALLOWED
                or in_network(address, DOCUMENTATION_NETWORKS)):
            add("ipv4", value)
    for m in IPV6.finditer(line):
        value = m.group(1)
        try:
            address = ipaddress.ip_address(value)
        except ValueError:
            continue
        if address not in IPV6_ALLOWED and not in_network(address, DOCUMENTATION_NETWORKS):
            add("ipv6", value)
    for m in MAC.finditer(line):
        value = m.group(1).lower()
        if not value.startswith(MAC_DOCUMENTATION_PREFIX) and value not in MAC_ALLOWED:
            add("mac", m.group(1))
    for m in EMAIL.finditer(line):
        if m.group(1).lower() not in EMAIL_DOMAINS:
            add("email", m.group(0))
    for m in CLOUD_PROJECT.finditer(line):
        if m.group(1) != CLOUD_PROJECT_ALLOWED:
            add("cloud-project", m.group(1))
    for m in INTERNAL_DNS.finditer(line):
        if m.group(1).lower() != INTERNAL_DNS_ALLOWED:
            add("internal-dns", m.group(1))
    for m in SERIAL.finditer(line):
        if m.group(1) and not SERIAL_ALLOWED.fullmatch(m.group(1)):
            add("serial", m.group(1))
    for m in CREDENTIAL.finditer(line):
        add("credential", m.group(0))
    for m in PLANNING_ID.finditer(line):
        add("planning-id", m.group(0))
    for number, pattern, length in literals:
        for _ in pattern.finditer(line):
            found.append(("operator-literal literal #%d" % number, length))
    return found


def is_byte_array(value):
    return bool(value) and all(
        type(item) is int and 0 <= item <= 255 for item in value)


def scan_json(document, literals):
    """Findings in the decoded strings, keys and byte arrays of a document."""
    found = []
    pending = [document]
    while pending:
        value = pending.pop()
        if isinstance(value, str):
            found.extend((cls, length) for _, cls, length in scan_text(value, literals))
        elif isinstance(value, dict):
            journal = "MESSAGE" in value and any(k.startswith("_") for k in value)
            for key, child in value.items():
                if key not in JOURNAL_KEYS and (journal or key.startswith("_")):
                    found.append(("journal-field", len(key)))
                found.extend((cls, length) for _, cls, length in scan_text(key, literals))
                pending.append(child)
        elif isinstance(value, list):
            if is_byte_array(value):
                text = bytes(value).decode("utf-8", "replace")
                found.extend((cls, length) for _, cls, length in scan_text(text, literals))
            else:
                pending.extend(value)
    return found


JSON_START = re.compile(r"[\[{]")


def json_values(text):
    """(first line, last line, value) for every JSON object or array in text.

    Not only a whole document or one record per line: jq's default output
    and `journalctl -o json-pretty` concatenate pretty-printed records, and
    a log line or a report detail can quote a record after other text.
    Each value is decoded once, outermost first; scan_json walks what is
    inside it.
    """
    decoder = json.JSONDecoder()
    line = 1
    counted = 0
    index = 0
    while True:
        match = JSON_START.search(text, index)
        if match is None:
            return
        start = match.start()
        try:
            value, end = decoder.raw_decode(text, start)
        except ValueError:
            index = start + 1
            continue
        line += text.count("\n", counted, start)
        first = line
        line += text.count("\n", start, end)
        counted = end
        index = end
        yield first, line, value


def scan_text(text, literals):
    """Set of (line, class, length) for text: its lines, then its JSON values.

    A finding inside a JSON value is reported on the line the value starts
    on, unless a line the value spans already reported the same finding.
    """
    found = set()
    for number, line in enumerate(text.split("\n"), 1):
        for cls, length in scan_line(line, literals):
            found.add((number, cls, length))
    for first, last, value in json_values(text):
        on_lines = set((cls, length) for number, cls, length in found
                       if first <= number <= last)
        for cls, length in scan_json(value, literals):
            if (cls, length) not in on_lines:
                found.add((first, cls, length))
    return found


def load_literals(path, repo_root):
    try:
        info = os.stat(path)
    except OSError:
        fault("the literal file does not exist or cannot be read")
    if not stat.S_ISREG(info.st_mode):
        fault("the literal file is not a regular file")
    # Identity, not string prefixes: a symlinked or differently spelled
    # path to a file inside the checkout is still inside it.
    current = os.path.realpath(path)
    while True:
        if os.path.samefile(current, repo_root):
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
            fault("literal file line %d is shorter than %d characters"
                  % (number, MIN_LITERAL))
        body = re.escape(value)
        if len(value) < SUBSTRING_LITERAL:
            # A short literal as a substring would flag ordinary words.
            body = r"(?<![A-Za-z0-9])" + body + r"(?![A-Za-z0-9])"
        literals.append((number, re.compile(body, re.IGNORECASE), len(value)))
    if not literals:
        fault("the literal file lists no literals")
    return literals


def parse_arguments(argv):
    mode = None
    literal_file = None
    paths = []
    options = True
    index = 0
    while index < len(argv):
        arg = argv[index]
        if options and arg == "--":
            options = False
        elif options and arg in ("-h", "--help"):
            # Not 0: exit 0 means "publishable", and nothing was scanned.
            print(USAGE)
            raise SystemExit(FAULT)
        elif options and arg in ("--literals", "--patterns-only"):
            if mode is not None:
                fault("choose exactly one of --literals FILE or --patterns-only")
            if arg == "--patterns-only":
                mode = "patterns"
            else:
                if index + 1 >= len(argv) or not argv[index + 1]:
                    fault("--literals requires a file")
                mode = "literals"
                literal_file = argv[index + 1]
                index += 1
        elif options and arg.startswith("-") and arg != "-":
            fault("unknown option %s\n%s" % (arg, USAGE))
        else:
            paths.append(arg)
        index += 1
    if mode is None:
        fault("choose one of --literals FILE or --patterns-only\n" + USAGE)
    if not paths:
        fault("name at least one PATH to scan\n" + USAGE)
    return literal_file, paths


def list_tree(root):
    """Sorted (relative path, lstat mode) for everything below root."""
    entries = []
    pending = [""]
    while pending:
        rel = pending.pop()
        directory = os.path.join(root, rel) if rel else root
        # os.walk would skip an unreadable directory silently.
        with os.scandir(directory) as listing:
            names = [entry.name for entry in listing]
        for name in names:
            child = os.path.join(rel, name) if rel else name
            mode = os.lstat(os.path.join(root, child)).st_mode
            entries.append((child, mode))
            if stat.S_ISDIR(mode):
                pending.append(child)
    entries.sort()
    return entries


def main(argv):
    repo_root, rest = argv[0], argv[1:]
    literal_file, paths = parse_arguments(rest)
    if not os.path.isdir(repo_root):
        fault("the repository root does not exist")
    literals = load_literals(literal_file, repo_root) if literal_file else []

    results = []
    files = 0
    for position, argument in enumerate(paths, 1):
        path = argument.rstrip(os.sep) or argument
        try:
            mode = os.lstat(path).st_mode
        except OSError:
            fault("PATH argument %d does not exist or cannot be read" % position)
        if stat.S_ISDIR(mode):
            root = path
            try:
                entries = list_tree(root)
            except OSError:
                fault("a directory under PATH argument %d cannot be listed" % position)
        else:
            root = os.path.dirname(path) or "."
            entries = [(os.path.basename(path), mode)]

        flagged = set()
        for ordinal, (rel, mode) in enumerate(entries, 1):
            parts = rel.split(os.sep)
            name_findings = scan_line(parts[-1], literals)
            if name_findings:
                flagged.add(rel)
            masked = any(os.sep.join(parts[:i]) in flagged for i in range(1, len(parts) + 1))
            ref = "path#%d" % ordinal if masked else rel

            def report(line, cls, length=None):
                results.append((position, ordinal, line, cls, length, ref))

            for cls, length in name_findings:
                report(0, cls, length)
            if stat.S_ISLNK(mode):
                report(0, "symlink")
                continue
            if stat.S_ISDIR(mode):
                continue
            if not stat.S_ISREG(mode):
                report(0, "special-file")
                continue
            try:
                with open(os.path.join(root, rel), "rb") as handle:
                    data = handle.read()
            except OSError:
                fault("%s under PATH argument %d cannot be read" % (ref, position))
            files += 1
            if b"\0" in data:
                report(0, "binary")
                continue
            try:
                text = data.decode("utf-8")
            except UnicodeDecodeError:
                report(0, "binary")
                continue
            for line, cls, length in scan_text(text, literals):
                report(line, cls, length)

    if results:
        order = lambda r: (r[0], r[1], r[2], r[3], -1 if r[4] is None else r[4])
        for _, _, line, cls, length, ref in sorted(results, key=order):
            suffix = "" if length is None else " (%d chars)" % length
            print("%s:%d: %s%s" % (ref, line, cls, suffix))
        sys.stdout.flush()
        print("\n%d finding(s): not publishable. Replace each value with its synthetic "
              "form (docs/validation/evidence/v0.2.1/README.md) and scan again."
              % len(results), file=sys.stderr)
        return FINDINGS
    if files == 0:
        fault("no files to scan; a scan of nothing is not a verdict")
    print("publishable: %d file(s) scanned, no findings" % files)
    return 0


try:
    sys.exit(main(sys.argv[1:]))
except SystemExit:
    raise
except BaseException as error:
    # A traceback could quote the text being scanned. Name the error only.
    print("check-evidence-publication: internal error (%s)" % type(error).__name__,
          file=sys.stderr)
    sys.exit(INTERNAL)
PY

case "$scan_status" in
  0) exit 0 ;;
  3) exit 1 ;;
  2) exit 2 ;;
  *)
    printf 'check-evidence-publication: no verdict (python exit %s)\n' "$scan_status" >&2
    exit 2
    ;;
esac
