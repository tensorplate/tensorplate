#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Offline-runtime checks for tools/validation/macos-homebrew-lifecycle.sh.

The harness runs the TensorPlate launchd services, the CLI, the MPS probe
and a network probe under one sandbox-exec profile. This module renders
that profile, derives the sandboxed launchd plists, probes the network
from inside the sandbox, reads sandbox state back from running processes,
and classifies every result. It also resolves each service's launchd
label, which every launchd stage of the harness uses.

Every subcommand prints its JSON result, which the harness keeps in the
local stage log, writes it to --out when given, and exits non-zero naming
the checks that failed. Results written to the evidence directory carry
no process ids, paths or host addresses.

sandbox-exec and its profile language are undocumented and deprecated.
Nothing here trusts them: the preflight subcommand proves the semantics
this module relies on before any Homebrew change, and the stage repeats
the probe against the running services.

Standard library only; Python 3.9 or newer.
"""

import argparse
import contextlib
import ctypes
import errno
import hashlib
import http.server
import json
import os
import plistlib
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
import xml.parsers.expat

SANDBOX_EXEC = "/usr/bin/sandbox-exec"
MDNS_SOCKET = "/var/run/mDNSResponder"
PREFLIGHT_DEPLOYMENT = "offline-profile-preflight"

# The launchd labels Homebrew gives a formula's service, its current
# default first. Homebrew 6.0.22 (commit 963b69f634, "services: default
# to canonical macOS service labels") made sh.brew.<formula> the default
# and still recognises homebrew.mxcl.<formula>, the label of a service
# installed or loaded by an earlier Homebrew: Library/Homebrew/service.rb
# canonical_plist_name, legacy_plist_name and plist_names. A formula that
# names its own label (`name macos:` in its service block) gets neither
# form, and no TensorPlate formula does.
LABEL_FORMS = ("sh.brew.{}", "homebrew.mxcl.{}")

# Loopback on the two serving ports is the only IP traffic allowed. SBPL
# `localhost` matches every address configured on the host, whatever the
# interface, so a rule for all ports would also admit fe80::1 (configured
# on lo0) on other interfaces; the port scope confines that residual gap
# to the serving ports. The resolver deny must name the /private path and
# come last: the unresolved /var/run path silently matches nothing, and a
# later unix-socket allow would override an earlier deny.
PROFILE_TEMPLATE = (
    "(version 1)(allow default)(deny network*)"
    '(allow network-inbound (local ip "localhost:{serving}")'
    ' (local ip "localhost:{candidate}"))'
    '(allow network-outbound (remote ip "localhost:{serving}")'
    ' (remote ip "localhost:{candidate}"))'
    "(allow network-bind network-inbound (local unix-socket))"
    "(allow network-outbound (remote unix-socket))"
    '(deny network-outbound (remote unix-socket (path-literal "/private/var/run/mDNSResponder")))'
)

# Matched against a process's full argument string by pgrep -f. Only the
# first argument is anchored, so an operator path that merely contains a
# service name does not match.
TENSORPLATE_PROCESS_PATTERN = (
    r"^[^ ]*/tensorplate-(agent|observability|serving)( |$)"
    r"|^[^ ]* -m tensorplate_pytorch_backend( |$)"
)

# <netinet/in.h> IP_BOUND_IF and <netinet6/in6.h> IPV6_BOUND_IF. Every
# probe socket for a non-loopback destination is pinned to lo0, so no
# probe -- sandboxed, unsandboxed control, or under a profile that fails
# to deny -- can put a packet on a real network. The sandbox decides on
# the destination address before routing: pinned, an allowed send fails
# with ENETUNREACH or EHOSTUNREACH and a denied one with EPERM.
IP_BOUND_IF = 25
IPV6_BOUND_IF = 125

# Operations the profile must refuse with EPERM, named by role. The same
# operations made without the sandbox must not be refused, or EPERM under
# the sandbox would prove nothing about the sandbox; mDNSResponder must
# answer the unsandboxed control outright.
DENIAL_NAMES = (
    "tcp_public_v4",
    "tcp_metadata_link_local_v4",
    "udp_test_net_v4",
    "udp_documentation_v6",
    "udp_fe80_1_unlisted_port",
    "udp_loopback_unlisted_port",
    "tcp_loopback_unlisted_port",
    "tcp_test_net_v4_serving_port",
    "udp_test_net_v4_serving_port",
    "tcp_documentation_v6_serving_port",
    "udp_documentation_v6_serving_port",
    "tcp_test_net_v4_candidate_port",
    "udp_test_net_v4_candidate_port",
    "tcp_documentation_v6_candidate_port",
    "udp_documentation_v6_candidate_port",
    "tcp_listen_wildcard_unlisted_port",
    "tcp_listen_loopback_unlisted_port",
    "udp_bind_wildcard_unlisted_port",
    "udp_test_net_v4_from_child",
    "unix_mdnsresponder",
)

# What a control has to have reported for the sandbox's EPERM on the same
# operation to mean anything: the operation completed.
#
# The sandbox decides on the destination address before the route lookup
# (see IP_BOUND_IF above), so a control the kernel carried as far as a
# routing answer got past the point where the sandbox would have refused
# it. That leaves three shapes a control can legitimately take, one set
# per operation:
#
#   DELIVERED -- the send, bind or listen returned, which `attempt`
#                records as `ok`, and the child's send completed when the
#                child ran, exited 0 and printed such an outcome;
#   ANSWERED  -- a connect the far end answered, which on a loopback port
#                with nothing listening on it is ECONNREFUSED;
#   ROUTED    -- a destination pinned to lo0, which lo0 cannot route.
#                These are the outcomes the recorded probes carry
#                (test/packaging/fixtures/macos-offline/): IPv4 answers
#                ENETUNREACH and IPv6 EHOSTUNREACH.
#
# Every other outcome is a control that never completed its operation --
# a socket that could not be pinned, a timeout, a child that exited
# non-zero or never ran, an exception's name, a name missing from the
# document, a string this module does not produce -- and a control that
# never ran the operation cannot show that the sandbox is what stopped
# the probe. So the controls are checked against these sets rather than
# against a list of the failures someone thought of.
DELIVERED = ("ok",)
ANSWERED = DELIVERED + ("ECONNREFUSED",)
ROUTED = DELIVERED + ("ENETUNREACH", "EHOSTUNREACH")

# A refusal by policy: what the sandboxed probe must report and the
# unsandboxed control must not. EPERM from something else on this host --
# an application firewall, an enclosing sandbox -- would make the
# sandbox's own EPERM prove nothing.
REFUSED = ("EPERM",)

# Denied operations addressed off this host, whose sockets are pinned to
# lo0: their control is answered by the route lookup rather than by
# anything on the far end. Listed one name at a time rather than matched
# against the destinations in the names: a substring rule reads a new
# operation to a destination already listed -- the likely addition --
# straight into this, the loosest set, which is the excuse-inheritance
# `completed_outcomes` says cannot happen. Spelling the names out means
# an operation added later is placed here by hand or not at all.
OFF_HOST_NAMES = (
    "tcp_public_v4",
    "tcp_metadata_link_local_v4",
    "udp_test_net_v4",
    "udp_documentation_v6",
    "tcp_test_net_v4_serving_port",
    "udp_test_net_v4_serving_port",
    "tcp_documentation_v6_serving_port",
    "udp_documentation_v6_serving_port",
    "tcp_test_net_v4_candidate_port",
    "udp_test_net_v4_candidate_port",
    "tcp_documentation_v6_candidate_port",
    "udp_documentation_v6_candidate_port",
    "udp_test_net_v4_from_child",
)
# The one denied connect to an address lo0 does route: a loopback port
# with nothing listening on it, which the far end refuses.
CONNECT_NAMES = ("tcp_loopback_unlisted_port",)

DOCTOR_FINDINGS_OK = (
    "platform_row",
    "platform_profile",
    "agent_reachable",
    "agent_service_state",
    "observability_service_state",
)

ADMISSION_LINE = re.compile(
    r"platform admission: row=(\S+) reason=(\S+) posture=.*? "
    r"evidence=(.+?) max_resident_model_memory=(\d+)"
)
OBSERVABILITY_STARTUP_PREFIX = "tensorplate-observability primary_source="


class CheckFailed(Exception):
    """A check refused; the message says which and why."""


class ProcessGone(CheckFailed):
    """The process exited or was replaced while it was being read."""


class PinFailed(Exception):
    """A probe socket could not be pinned to lo0."""


# --- profile -----------------------------------------------------------


def serving_ports(config):
    """The (serving, candidate) ports from an installed agent.json."""
    worker = config.get("worker") if isinstance(config, dict) else None
    if not isinstance(worker, dict):
        raise CheckFailed("the agent config has no worker section")
    host = worker.get("serving_bind_host")
    if host != "127.0.0.1":
        raise CheckFailed(
            f"worker.serving_bind_host is {host!r}; the offline profile requires 127.0.0.1"
        )
    ports = []
    for key in ("serving_bind_port", "serving_candidate_bind_port"):
        value = worker.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or not 1024 <= value <= 65535:
            raise CheckFailed(f"worker.{key} must be an integer in 1024-65535, found {value!r}")
        ports.append(value)
    if ports[0] == ports[1]:
        raise CheckFailed("worker.serving_bind_port and serving_candidate_bind_port must differ")
    return ports[0], ports[1]


def _top_level_forms(text):
    forms, depth, start, quoted = [], 0, 0, False
    for index, char in enumerate(text):
        if quoted:
            quoted = char != '"'
        elif char == '"':
            quoted = True
        elif char == "(":
            if depth == 0:
                start = index
            depth += 1
        elif char == ")":
            depth -= 1
            if depth < 0:
                return None
            if depth == 0:
                forms.append(text[start:index + 1])
        elif depth == 0 and not char.isspace():
            return None
    return forms if depth == 0 and not quoted else None


def _children(form):
    """The parenthesised forms directly inside one form."""
    inner = form[1:-1]
    head = re.match(r"[^()\"]*", inner).group(0)
    return _top_level_forms(inner[len(head):]) or []


def _nested_forms(form):
    found = [form]
    for child in _children(form):
        found.extend(_nested_forms(child))
    return found


def lint_profile(text):
    """Names of the structural rules a profile breaks; empty when sound.

    Written independently of PROFILE_TEMPLATE, so a change to the
    template that weakens it is refused at render time. The named rules
    say what broke; only_expected_rules also refuses a rule added with
    any other filter, such as `(remote tcp ...)` or a bare operation.
    """
    problems = set()
    if "\n" in text or "\r" in text:
        problems.add("single_line")
    forms = _top_level_forms(text)
    if not forms:
        return sorted(problems | {"balanced_forms"})
    if forms[:3] != ["(version 1)", "(allow default)", "(deny network*)"]:
        problems.add("base_network_deny_first")
    rules = forms[3:]
    ports = []
    for rule in rules:
        words = re.match(r"\(([^()\"]*)", rule).group(1).split()
        if not words or words[0] not in ("allow", "deny"):
            problems.add("known_actions")
            continue
        if words[0] == "allow" and "network*" in words[1:]:
            problems.add("no_allow_network_star")
        for form in _nested_forms(rule):
            if re.match(r"\((local|remote) unix-socket\b", form) and len(_children(form)) > 1:
                problems.add("unix_socket_single_filter")
        for value in re.findall(r'\((?:local|remote) ip "([^"]*)"\)', rule):
            port = re.fullmatch(r"localhost:([0-9]+)", value)
            if not port:
                problems.add("ip_rules_port_scoped")
            elif int(port.group(1)) not in ports:
                ports.append(int(port.group(1)))
    if len(ports) != 2:
        problems.add("two_serving_ports")
    elif forms != [
        "(version 1)",
        "(allow default)",
        "(deny network*)",
        f'(allow network-inbound (local ip "localhost:{ports[0]}") (local ip "localhost:{ports[1]}"))',
        f'(allow network-outbound (remote ip "localhost:{ports[0]}") (remote ip "localhost:{ports[1]}"))',
        "(allow network-bind network-inbound (local unix-socket))",
        "(allow network-outbound (remote unix-socket))",
        '(deny network-outbound (remote unix-socket (path-literal "/private/var/run/mDNSResponder")))',
    ]:
        problems.add("only_expected_rules")
    for direction, filter_kind in (("inbound", "local"), ("outbound", "remote")):
        wanted = {f'({filter_kind} ip "localhost:{port}")' for port in ports}
        if not any(
            rule.startswith(f"(allow network-{direction} (")
            and set(_children(rule)) == wanted
            for rule in rules
        ):
            problems.add(f"loopback_{direction}_allowed")
    if "(allow network-bind network-inbound (local unix-socket))" not in rules or \
            "(allow network-outbound (remote unix-socket))" not in rules:
        problems.add("unix_sockets_allowed")
    if not rules or rules[-1] != (
        "(deny network-outbound (remote unix-socket "
        '(path-literal "/private/var/run/mDNSResponder")))'
    ):
        problems.add("resolver_deny_last")
    return sorted(problems)


def render_profile(serving, candidate):
    text = PROFILE_TEMPLATE.format(serving=serving, candidate=candidate)
    problems = lint_profile(text)
    if problems:
        raise CheckFailed("the offline profile template breaks: " + ", ".join(problems))
    return text


def profile_ports(text):
    """The two serving ports a rendered profile allows, in order."""
    if lint_profile(text):
        raise CheckFailed("the offline profile is not the rendered template")
    ports = []
    for port in re.findall(r'"localhost:([0-9]+)"', text):
        if int(port) not in ports:
            ports.append(int(port))
    return ports


def write_profile(path, text):
    with contextlib.suppress(FileNotFoundError):
        os.chmod(path, 0o644)
        os.unlink(path)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    os.chmod(path, 0o444)
    return path


def read_profile(path):
    try:
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
    except OSError as error:
        raise CheckFailed(f"cannot read the offline profile: {error.strerror}")
    if "\n" in text or not text.startswith("(version 1)"):
        raise CheckFailed("the offline profile must be one line starting with (version 1)")
    return text


# --- launchd -----------------------------------------------------------


def label_forms(service):
    """Every launchd label Homebrew may have loaded `service` under."""
    return [form.format(service) for form in LABEL_FORMS]


def service_label(keg, service):
    """The launchd label Homebrew generated for an installed service.

    The formula installer writes the service definition into the keg as
    <label>.plist, carrying that Label (formula_installer.rb
    install_service, formula.rb launchd_service_path, service.rb
    to_plist). `brew services start` installs the same generated
    definition as ~/Library/LaunchAgents/<label>.plist and loads it under
    its Label (services/cli.rb install_service_file, launchctl_load), so
    under one Homebrew the keg's name, its Label, the LaunchAgents file
    and the loaded job agree. Exactly one keg plist named for a known
    form must exist, and its Label must be that name.
    """
    present = [label for label in label_forms(service)
               if os.path.isfile(os.path.join(keg, f"{label}.plist"))]
    names = " or ".join(f"{label}.plist" for label in label_forms(service))
    if not present:
        raise CheckFailed(f"the {service} keg holds no launchd plist named {names}")
    if len(present) > 1:
        raise CheckFailed(f"the {service} keg holds both {names}; cannot tell which Homebrew loads")
    label = present[0]
    try:
        with open(os.path.join(keg, f"{label}.plist"), "rb") as handle:
            document = plistlib.load(handle)
    except (OSError, ValueError, xml.parsers.expat.ExpatError) as error:
        raise CheckFailed(f"cannot read {label}.plist in the {service} keg: {error}")
    found = document.get("Label") if isinstance(document, dict) else None
    if found != label:
        raise CheckFailed(f"{label}.plist in the {service} keg has Label {found!r}, expected {label!r}")
    return label


def derive_plist(document, label, program, profile_path):
    """The formula plist with ProgramArguments run under sandbox-exec."""
    if not isinstance(document, dict):
        raise CheckFailed("the formula plist is not a dictionary")
    if document.get("Label") != label:
        raise CheckFailed(f"the formula plist Label is {document.get('Label')!r}, expected {label!r}")
    if "Program" in document:
        raise CheckFailed("the formula plist sets Program, which launchd runs instead of ProgramArguments")
    arguments = document.get("ProgramArguments")
    if not isinstance(arguments, list) or not arguments or \
            not all(isinstance(item, str) for item in arguments):
        raise CheckFailed("the formula plist ProgramArguments is not a non-empty string list")
    if arguments[0] != program:
        raise CheckFailed(f"ProgramArguments[0] is {arguments[0]!r}, expected {program!r}")
    if any(os.path.basename(item) == "sandbox-exec" for item in arguments):
        raise CheckFailed("the formula plist already runs sandbox-exec")
    if not os.path.isabs(profile_path):
        raise CheckFailed("the profile path must be absolute")
    derived = dict(document)
    derived["ProgramArguments"] = [SANDBOX_EXEC, "-f", profile_path] + arguments
    return derived


def parse_launchd_job(text):
    """Top-level fields of `launchctl print gui/<uid>/<label>`.

    Only single-tab `key = value` lines and the top-level arguments block
    are read; nested blocks carry their own pid and state lines.
    """
    lines = text.splitlines()
    while lines and not lines[-1].strip():
        lines.pop()
    if len(lines) < 2 or not re.fullmatch(r"\S+ = \{", lines[0]) or lines[-1] != "}":
        raise CheckFailed("launchctl print output is not one job block")
    fields, arguments, index = {}, None, 1
    while index < len(lines) - 1:
        line = lines[index]
        block = re.fullmatch(r"\t([^\t=][^=]*?) = \{", line)
        if block:
            end = next((k for k in range(index + 1, len(lines) - 1) if lines[k] == "\t}"), None)
            if end is None:
                raise CheckFailed(f"launchctl print block {block.group(1)!r} is not closed")
            if block.group(1) == "arguments":
                body = lines[index + 1:end]
                if arguments is not None or not all(item.startswith("\t\t") for item in body):
                    raise CheckFailed("launchctl print arguments block has an unexpected shape")
                arguments = [item[2:] for item in body]
            index = end + 1
            continue
        field = re.fullmatch(r"\t([^\t=][^=]*?) = (.*)", line)
        if field:
            if field.group(1) in fields and field.group(1) in ("path", "program", "pid", "runs"):
                raise CheckFailed(f"launchctl print repeats the top-level {field.group(1)!r} field")
            fields.setdefault(field.group(1), field.group(2))
        index += 1

    def number(name):
        value = fields.get(name)
        if value is None:
            return None
        if not re.fullmatch(r"[0-9]+", value):
            raise CheckFailed(f"launchctl print has an invalid {name!r} field")
        return int(value)

    program = fields.get("program")
    return {
        "label": lines[0].rsplit(" = {", 1)[0].rsplit("/", 1)[-1],
        "path": fields.get("path"),
        "program": program,
        "arguments": arguments,
        "pid": number("pid"),
        "runs": number("runs"),
        # Loaded from a derived plist, whether or not launchd honours the
        # prefix: cleanup boots out any such job.
        "runs_sandbox_exec": SANDBOX_EXEC in (program, (arguments or [None])[0]),
    }


def expected_sandboxed_arguments(formula_document, profile_path):
    """What launchd must report for a sandboxed job, built from the formula
    plist and the literal prefix rather than from the derived file."""
    return ["/usr/bin/sandbox-exec", "-f", profile_path] + list(
        formula_document.get("ProgramArguments") or []
    )


def _same_file(first, second):
    # /var/folders is a symlink to /private/var/folders; launchd may print
    # either spelling of the plist it loaded.
    return isinstance(first, str) and isinstance(second, str) and \
        os.path.realpath(first) == os.path.realpath(second)


def check_launchd_job(job, program=None, path=None, arguments=None, runs=None, same_pid_as=None,
                      startup=False):
    """launchd's own view of a job. `brew services info` reports the same
    loaded file and pid, parsed from this output, so it adds nothing."""
    failures = []
    if program is not None and job["program"] != program:
        failures.append("job_program")
    if path is not None and not _same_file(job["path"], path):
        failures.append("job_path")
    if arguments is not None and job["arguments"] != arguments:
        failures.append("job_arguments")
    # bootstrap loads the definition before launchd necessarily starts its
    # first process. Only an absent PID with zero attempts is pending; a
    # missing PID after a run is a stopped/crashed process, not startup.
    pending = startup and job["pid"] is None and job["runs"] == 0
    if not job["pid"] and not pending:
        failures.append("job_running")
    # KeepAlive would hide a crash behind a running job: a restart counts
    # a run, and a job booted out and loaded again starts over at one run
    # with a new pid.
    if runs is not None and job["runs"] != runs:
        failures.append("job_runs")
    if startup and not pending and job["runs"] != 1:
        failures.append("job_runs")
    if same_pid_as is not None and job["pid"] != same_pid_as:
        failures.append("job_pid_unchanged")
    return failures


# --- processes and sandbox state ---------------------------------------


_SANDBOX_CHECK = []


def sandbox_check(pid, operation):
    """libsystem_sandbox's sandbox_check: 1 when `operation` is denied (or,
    with no operation, when the process is sandboxed). It also returns 1
    for a pid that does not exist, so callers must confirm identity."""
    if not _SANDBOX_CHECK:
        try:
            function = ctypes.CDLL("/usr/lib/libSystem.B.dylib").sandbox_check
        except (OSError, AttributeError) as error:
            raise CheckFailed(f"sandbox_check is unavailable: {error}")
        function.restype = ctypes.c_int
        function.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int]
        _SANDBOX_CHECK.append(function)
    return _SANDBOX_CHECK[0](pid, operation.encode("ascii") if operation else None, 0)


def process_status(pid):
    """(state, start time and executable) of a process ps lists, or None."""
    result = subprocess.run(["ps", "-o", "stat=,lstart=,comm=", "-p", str(pid)],
                            capture_output=True, text=True)
    fields = result.stdout.strip().split(None, 1) if result.returncode == 0 else []
    return (fields[0], fields[1]) if len(fields) == 2 else None


def process_identity(pid):
    """Start time and executable of a live process, or None. A zombie has
    exited: sandbox_check reads it as sandboxed whatever it ran under. The
    state is left out of the identity because a live process changes it."""
    status = process_status(pid)
    return None if status is None or "Z" in status[0] else status[1]


def process_arguments(pid):
    result = subprocess.run(["ps", "-o", "args=", "-p", str(pid)], capture_output=True, text=True)
    text = result.stdout.strip()
    return text if result.returncode == 0 and text else None


def read_sandbox_state(pid):
    before = process_identity(pid)
    if before is None:
        raise ProcessGone(f"process {pid} is not running")
    values = [sandbox_check(pid, operation)
              for operation in (None, "network-outbound", "network-inbound")]
    if process_identity(pid) != before:
        raise ProcessGone(f"process {pid} exited or was replaced during the sandbox readback")
    if any(value not in (0, 1) for value in values):
        raise CheckFailed(f"sandbox_check returned {values} for process {pid}")
    return {
        "sandboxed": values[0] == 1,
        "network_outbound_denied": values[1] == 1,
        "network_inbound_denied": values[2] == 1,
    }


# The four properties the readback must have on this host for any
# sandbox_check result to mean anything, and what an unproved one says.
# `evidence()` publishes the flags `discrimination_controls` returns
# verbatim, so each is derived there from the observation that proves it
# and none is written down as proved: a control that stops being taken
# reads False and fails the stage, rather than publishing a measured-
# looking `true` for a check nobody ran.
READBACK_CONTROLS = {
    "unsandboxed_process_reads_unsandboxed":
        "this unsandboxed process reads as sandboxed",
    "sandboxed_process_reads_network_denied":
        "a process started under the profile does not read as sandboxed with the network denied",
    "exited_process_rejected":
        "a reaped exited process passed the identity check",
    "unreaped_exited_process_rejected":
        "an unreaped exited process passed the identity check",
}


def readback_rejects(pid):
    """Whether the readback refuses to report on `pid` because the process
    it named is gone. The answer is the observation itself, so a control
    derived from it cannot report a check that was not made."""
    try:
        read_sandbox_state(pid)
    except ProcessGone:
        return True
    return False


def discrimination_controls(profile_path, attempts=100):
    """Prove, on this host and now, that the readback tells sandboxed from
    unsandboxed processes and rejects a process that has exited, whether
    or not its parent has reaped it yet.

    Each flag starts False and is assigned the result of the check that
    proves it, so deleting or weakening a check leaves its flag False
    rather than leaving a `True` behind; anything still unproved at the
    end fails here, before the stage can carry it into the evidence."""
    proved = dict.fromkeys(READBACK_CONTROLS, False)
    proved["unsandboxed_process_reads_unsandboxed"] = \
        not any(read_sandbox_state(os.getpid()).values())
    exited = subprocess.Popen(["/usr/bin/true"])
    try:
        # Not reaped until the finally clause: ps lists it as a zombie.
        for _ in range(attempts):
            status = process_status(exited.pid)
            if status and "Z" in status[0]:
                break
            time.sleep(0.05)
        else:
            raise CheckFailed("readback control: the exited process never showed as a zombie")
        proved["unreaped_exited_process_rejected"] = readback_rejects(exited.pid)
    finally:
        exited.wait()
    sleeper = subprocess.Popen(["sandbox-exec", "-f", profile_path, "/bin/sleep", "60"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(attempts):
            identity = process_identity(sleeper.pid)
            if identity and identity.split()[-1].endswith("sleep"):
                break
            if sleeper.poll() is not None:
                raise CheckFailed("readback control: the sandboxed sleeper exited early")
            time.sleep(0.05)
        else:
            raise CheckFailed("readback control: the sandboxed sleeper never started")
        proved["sandboxed_process_reads_network_denied"] = \
            all(read_sandbox_state(sleeper.pid).values())
    finally:
        sleeper.kill()
        sleeper.wait()
    proved["exited_process_rejected"] = readback_rejects(sleeper.pid)
    # Named in brackets so a test or an operator can tell
    # `exited_process_rejected` from `unreaped_exited_process_rejected`,
    # one of which reads as a substring of the other.
    unproved = [name for name in READBACK_CONTROLS if not proved[name]]
    if unproved:
        raise CheckFailed("readback control not proved: " + "; ".join(
            f"[{name}] {READBACK_CONTROLS[name]}" for name in unproved))
    return proved


def _read_states(required_pids, optional_pids, expect):
    failures, read = [], 0
    for pid, required in [(pid, True) for pid in required_pids] + \
            [(pid, False) for pid in optional_pids]:
        try:
            state = read_sandbox_state(pid)
        except ProcessGone:
            if required:
                raise
            continue
        read += 1
        if expect == "sandboxed" and not all(state.values()):
            failures.append("process_sandboxed_network_denied")
        if expect == "unsandboxed" and any(state.values()):
            failures.append("process_unsandboxed")
    if expect == "sandboxed" and read == 0:
        failures.append("processes_read")
    return read, sorted(set(failures))


def sandbox_states(profile_path, required_pids, optional_pids, expect):
    controls = discrimination_controls(profile_path)
    if expect == "sandboxed" and not required_pids and not optional_pids:
        raise CheckFailed("there are no processes to read back")
    read, failures = _read_states(required_pids, optional_pids, expect)
    result = {"readback_controls": controls, "processes_read": read}
    result["all_sandboxed" if expect == "sandboxed" else "none_sandboxed"] = not failures
    return result, failures


def no_sandboxed_tensorplate_process(profile_path, required_pids, attempts=30):
    """After the sandboxed jobs are booted out, launchd's SIGTERM may still
    be reaching their children; wait for every TensorPlate process that
    still reads as sandboxed to exit."""
    controls = discrimination_controls(profile_path)
    for remaining in range(attempts, 0, -1):
        read, failures = _read_states(required_pids, tensorplate_processes(), "unsandboxed")
        if not failures or remaining == 1:
            break
        time.sleep(1)
    return {"readback_controls": controls, "processes_read": read,
            "none_sandboxed": not failures}, failures


def child_pids(pid):
    result = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True)
    if result.returncode == 1:
        return []
    if result.returncode != 0:
        raise CheckFailed(f"pgrep -P {pid} failed with status {result.returncode}")
    return [int(item) for item in result.stdout.split()]


def process_tree(root):
    order, queue = [], [root]
    while queue:
        pid = queue.pop(0)
        if pid not in order:
            order.append(pid)
            queue.extend(child_pids(pid))
    return order


def process_role(arguments):
    tokens = (arguments or "").split()
    if tokens and os.path.basename(tokens[0]) == "tensorplate-serving":
        return "serving"
    if tokens[1:3] == ["-m", "tensorplate_pytorch_backend"]:
        return "sidecar"
    return None


def tensorplate_processes():
    result = subprocess.run(
        ["pgrep", "-u", str(os.getuid()), "-f", TENSORPLATE_PROCESS_PATTERN],
        capture_output=True, text=True,
    )
    if result.returncode == 1:
        return []
    if result.returncode != 0:
        raise CheckFailed(f"pgrep failed with status {result.returncode}: {result.stderr.strip()}")
    return [int(item) for item in result.stdout.split()]


def lsof(arguments):
    # -w: warnings off, so anything on stderr is an error.
    result = subprocess.run(["lsof", "-w"] + arguments, capture_output=True, text=True)
    # lsof exits 1 both when nothing matched and when any one of several
    # -p processes has no matching file, so the output decides; but it
    # also exits 1, with nothing on stdout, when it could not run the
    # query at all, and then only stderr tells.
    if result.returncode not in (0, 1) or result.stderr.strip():
        raise CheckFailed(f"lsof failed with status {result.returncode}: {result.stderr.strip()}")
    return result.stdout


def parse_lsof(text):
    """Sockets from `lsof -F pPnT` output."""
    sockets, pid, current = [], None, None
    for line in text.splitlines():
        if not line:
            continue
        tag, value = line[0], line[1:]
        if tag == "p":
            pid, current = int(value), None
        elif tag == "f":
            if pid is None:
                raise CheckFailed("lsof output names a file before its process")
            current = {"pid": pid, "protocol": None, "name": None, "state": None}
            sockets.append(current)
        elif current is None:
            raise CheckFailed(f"unexpected lsof field outside a file: {line!r}")
        elif tag == "P":
            current["protocol"] = value
        elif tag == "n":
            current["name"] = value
        elif tag == "T" and value.startswith("ST="):
            current["state"] = value[3:]
    return sockets


def check_listeners(sockets, pids, tree_pids, serving_port):
    """Every internet socket of the TensorPlate processes (`pids`) is on
    loopback, and the agent's process tree holds the serving listener."""
    failures, listener = set(), False
    for item in sockets:
        local = (item["name"] or "").split("->", 1)[0]
        host = local.rpartition(":")[0]
        if item["pid"] not in pids:
            failures.add("sockets_owned_by_tensorplate_processes")
        # `*:*` is a socket that was never bound or connected: it has no
        # port, so nothing can reach it. A wildcard address with a port is
        # a listener or bound socket any interface can reach.
        if host not in ("127.0.0.1", "[::1]") and local != "*:*":
            failures.add("loopback_only")
        if item["protocol"] == "TCP" and item["state"] == "LISTEN" and \
                local == f"127.0.0.1:{serving_port}" and item["pid"] in tree_pids:
            listener = True
    if not listener:
        failures.add("serving_listener_in_tree")
    return sorted(failures)


# --- network probe -----------------------------------------------------


def _pinned(family, kind):
    sock = socket.socket(family, kind)
    try:
        index = socket.if_nametoindex("lo0")
        if family == socket.AF_INET6:
            sock.setsockopt(socket.IPPROTO_IPV6, IPV6_BOUND_IF, index)
        else:
            sock.setsockopt(socket.IPPROTO_IP, IP_BOUND_IF, index)
    except OSError as error:
        sock.close()
        raise PinFailed(str(error))
    return sock


def udp_send(host, port, family=socket.AF_INET, scope_lo0=False):
    sock = _pinned(family, socket.SOCK_DGRAM)
    try:
        if family == socket.AF_INET6:
            scope = socket.if_nametoindex("lo0") if scope_lo0 else 0
            sock.sendto(b"x", (host, port, 0, scope))
        else:
            sock.sendto(b"x", (host, port))
    finally:
        sock.close()


def tcp_connect(host, port, family=socket.AF_INET):
    sock = _pinned(family, socket.SOCK_STREAM)
    sock.settimeout(3)
    try:
        sock.connect((host, port))
    finally:
        sock.close()


def bind_unlisted_port(host, kind):
    """Bind, and listen for TCP, on a port the kernel would pick: the
    request names port 0, which is not a serving port."""
    sock = socket.socket(socket.AF_INET, kind)
    try:
        sock.bind((host, 0))
        if kind == socket.SOCK_STREAM:
            sock.listen(1)
    finally:
        sock.close()


def unix_connect(path):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(3)
    try:
        sock.connect(path)
    finally:
        sock.close()


def loopback_listen_connect(port):
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("127.0.0.1", port))
        server.listen(1)
        client = socket.create_connection(("127.0.0.1", port), timeout=3)
        accepted, _ = server.accept()
        accepted.close()
        client.close()
    finally:
        server.close()


def http_get_json(url):
    # No proxy: an operator's HTTP_PROXY must not route a loopback request.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(url, timeout=5) as response:
        return json.load(response)


def child_udp_send():
    """The same send from a child process, which inherits the sandbox."""
    try:
        result = subprocess.run([sys.executable, os.path.abspath(__file__), "child-udp"],
                                capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.SubprocessError) as error:
        # A child that never ran sent nothing: its EPERM is not a refusal.
        return f"child_not_run_{type(error).__name__}"
    return result.stdout.strip() if result.returncode == 0 else f"child_exit_{result.returncode}"


def attempt(operation):
    try:
        outcome = operation()
    except socket.timeout:
        return "timeout"
    except PinFailed:
        return "pin_failed"
    except OSError as error:
        return errno.errorcode.get(error.errno, "OSError") if error.errno else type(error).__name__
    except Exception as error:  # a probe records every outcome by name
        return type(error).__name__
    return outcome if isinstance(outcome, str) else "ok"


def denial_operations(ports):
    """(name, operation) for every DENIAL_NAMES entry. `ports` are the two
    serving ports the profile allows on loopback: another host on those
    ports, and loopback on any other port, must still be refused."""
    operations = [
        ("tcp_public_v4", lambda: tcp_connect("1.1.1.1", 443)),
        ("tcp_metadata_link_local_v4", lambda: tcp_connect("169.254.169.254", 80)),
        ("udp_test_net_v4", lambda: udp_send("192.0.2.1", 9)),
        ("udp_documentation_v6", lambda: udp_send("2001:db8::1", 9, socket.AF_INET6)),
        ("udp_fe80_1_unlisted_port",
         lambda: udp_send("fe80::1", 9, socket.AF_INET6, scope_lo0=True)),
        ("udp_loopback_unlisted_port", lambda: udp_send("127.0.0.1", 9)),
        # Refused: EPERM. Allowed with nothing listening: ECONNREFUSED.
        ("tcp_loopback_unlisted_port", lambda: tcp_connect("127.0.0.1", 9)),
    ]
    for role, port in zip(("serving", "candidate"), ports):
        operations += [
            (f"tcp_test_net_v4_{role}_port", lambda port=port: tcp_connect("192.0.2.1", port)),
            (f"udp_test_net_v4_{role}_port", lambda port=port: udp_send("192.0.2.1", port)),
            (f"tcp_documentation_v6_{role}_port",
             lambda port=port: tcp_connect("2001:db8::1", port, socket.AF_INET6)),
            (f"udp_documentation_v6_{role}_port",
             lambda port=port: udp_send("2001:db8::1", port, socket.AF_INET6)),
        ]
    operations += [
        ("tcp_listen_wildcard_unlisted_port",
         lambda: bind_unlisted_port("0.0.0.0", socket.SOCK_STREAM)),
        ("tcp_listen_loopback_unlisted_port",
         lambda: bind_unlisted_port("127.0.0.1", socket.SOCK_STREAM)),
        ("udp_bind_wildcard_unlisted_port",
         lambda: bind_unlisted_port("0.0.0.0", socket.SOCK_DGRAM)),
        ("udp_test_net_v4_from_child", child_udp_send),
        ("unix_mdnsresponder", lambda: unix_connect(MDNS_SOCKET)),
    ]
    return operations


def run_control(ports):
    return {name: attempt(operation) for name, operation in denial_operations(ports)}


def run_probe(agent_socket, health_url, ports, listen_port=None):
    allowed = {"unix_agent_socket": attempt(lambda: unix_connect(agent_socket))}
    if listen_port:
        allowed["tcp_loopback_listen_candidate_port"] = attempt(
            lambda: loopback_listen_connect(listen_port))
    health = {}
    request = attempt(lambda: health.update(http_get_json(health_url)))
    return {
        "denied": {name: attempt(operation) for name, operation in denial_operations(ports)},
        "allowed": allowed,
        "health": {
            "request": request,
            "state": health.get("state"),
            "active_model_id": health.get("active_model_id"),
        },
    }


def completed_outcomes(name):
    """The control outcomes that count as `name` having completed.

    A name this module does not place gets the strictest set, so a denial
    added later has to be placed deliberately rather than inherit an
    excuse from a neighbour."""
    if name in OFF_HOST_NAMES:
        return ROUTED
    if name in CONNECT_NAMES:
        return ANSWERED
    return DELIVERED


def control_failures(control):
    """Why `control` cannot be the baseline a probe is classified
    against, whatever that probe turns out to say.

    Checked when the control is taken, before the profile is applied to
    anything, so a host that cannot provide a baseline fails the stage
    before it is changed -- and again by `classify`, which never takes a
    control on trust.

    Two things are asked of every denied operation. It must not have been
    refused by something else on this host (REFUSED): the stage's claim
    is that this profile is what refuses. And it must have completed
    (`completed_outcomes`): the sandbox's EPERM is attributable only to
    an operation that went through, unsandboxed, a moment earlier. A
    control that never completed says the operation could not be made
    here, not that the sandbox stopped it. mDNSResponder is asked for
    both at once: the stage's claim is that this socket was reachable and
    the profile's deny is what made it unreachable, so its control must
    answer outright."""
    outcomes = control if isinstance(control, dict) else {}
    failures = []
    for name in DENIAL_NAMES:
        outcome = outcomes.get(name)
        if name == "unix_mdnsresponder":
            if outcome != "ok":
                failures.append("control_mdnsresponder_reachable")
        elif outcome in REFUSED:
            failures.append(f"control_not_refused:{name}")
        elif outcome not in completed_outcomes(name):
            # Missing, a socket that could not be pinned, a timeout, a
            # child that failed or never ran, or anything else that is
            # not an operation that happened.
            failures.append(f"control_completed:{name}")
    return failures


def classify(probe, control, expected_deployment, listen_port_checked=False):
    """The probe proves the profile denies the network only against a
    control that completed the same operations without it. Both halves
    are required: a control that something else on the host refused, or
    that never completed its operation, makes the probe's EPERM
    unattributable (`control_failures`), and a probe that was not refused
    means the profile did nothing."""
    failures = control_failures(control)
    denied = probe.get("denied") if isinstance(probe.get("denied"), dict) else {}
    for name in DENIAL_NAMES:
        if denied.get(name) != "EPERM":
            failures.append(f"refused:{name}")
    allowed = probe.get("allowed") if isinstance(probe.get("allowed"), dict) else {}
    expected_allowed = ["unix_agent_socket"]
    if listen_port_checked:
        expected_allowed.append("tcp_loopback_listen_candidate_port")
    for name in expected_allowed:
        if allowed.get(name) != "ok":
            failures.append(f"allowed:{name}")
    health = probe.get("health") if isinstance(probe.get("health"), dict) else {}
    if health.get("request") != "ok" or health.get("state") != "ready":
        failures.append("serving_health_ready")
    if health.get("active_model_id") != expected_deployment:
        failures.append("serving_health_deployment")
    return failures


def free_loopback_ports():
    first, second = socket.socket(), socket.socket()
    try:
        first.bind(("127.0.0.1", 0))
        second.bind(("127.0.0.1", 0))
        return first.getsockname()[1], second.getsockname()[1]
    finally:
        first.close()
        second.close()


@contextlib.contextmanager
def loopback_services(port, socket_path, deployment):
    """An unsandboxed health endpoint and unix listener for the preflight."""

    class Health(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802 - the http.server hook name
            body = json.dumps({"state": "ready", "active_model_id": deployment}).encode()
            self.send_response(200 if self.path == "/health" else 404)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = http.server.HTTPServer(("127.0.0.1", port), Health)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    with contextlib.suppress(FileNotFoundError):
        os.unlink(socket_path)
    listener.bind(socket_path)
    listener.listen(4)
    stop = threading.Event()

    def accept():
        listener.settimeout(0.2)
        while not stop.is_set():
            try:
                connection, _ = listener.accept()
                connection.close()
            except OSError:
                continue

    threads = [threading.Thread(target=server.serve_forever, daemon=True),
               threading.Thread(target=accept, daemon=True)]
    for thread in threads:
        thread.start()
    try:
        yield
    finally:
        stop.set()
        server.shutdown()
        server.server_close()
        listener.close()
        for thread in threads:
            thread.join(timeout=5)


def preflight(work_dir, render=None):
    """Prove the profile semantics on this host with ephemeral ports: a
    sandboxed probe must be refused everything but loopback on those two
    ports and unix sockets other than mDNSResponder."""
    render = render or render_profile
    os.makedirs(work_dir, exist_ok=True)
    serving, candidate = free_loopback_ports()
    profile_path = write_profile(os.path.join(work_dir, "network-denied.sb"),
                                 render(serving, candidate))
    # A unix socket path is limited to 104 bytes, which a deep work
    # directory can exceed.
    socket_dir = tempfile.mkdtemp(prefix="tp-offline-", dir="/tmp")
    socket_path = os.path.join(socket_dir, "agent.sock")
    try:
        with loopback_services(serving, socket_path, PREFLIGHT_DEPLOYMENT):
            control = run_control((serving, candidate))
            controls = discrimination_controls(profile_path)
            child = subprocess.run(
                ["sandbox-exec", "-f", profile_path, sys.executable, os.path.abspath(__file__),
                 "probe", "--agent-socket", socket_path,
                 "--health-url", f"http://127.0.0.1:{serving}/health",
                 "--ports", f"{serving},{candidate}", "--listen-port", str(candidate)],
                capture_output=True, text=True, timeout=300,
            )
    finally:
        shutil.rmtree(socket_dir, ignore_errors=True)
    if child.returncode != 0:
        raise CheckFailed(
            f"the sandboxed probe exited {child.returncode}: {child.stderr.strip()[-400:]}")
    probe = json.loads(child.stdout)
    failures = classify(probe, control, PREFLIGHT_DEPLOYMENT, listen_port_checked=True)
    return {"control": control, "probe": probe, "readback_controls": controls}, failures


# --- CLI results -------------------------------------------------------


def status_check(document, expected_deployment, ports):
    payload = document.get("payload") if isinstance(document.get("payload"), dict) else {}
    agent = payload.get("agent") if isinstance(payload.get("agent"), dict) else {}
    active = agent.get("active") if isinstance(agent.get("active"), dict) else {}
    url = active.get("serving_url")
    parts = urllib.parse.urlsplit(url) if isinstance(url, str) else None
    try:
        port = parts.port if parts is not None else None
    except ValueError:
        port = None
    checks = {
        "status_command": document.get("command") == "status",
        "status_severity_ready": payload.get("severity") == "ready",
        "agent_state_ready": agent.get("agent_state") == "ready",
        "no_transaction_in_flight": agent.get("in_flight_transaction") is None,
        "active_deployment": active.get("deployment_id") == expected_deployment,
        "active_backend": active.get("backend") == "python_pytorch",
        "serving_url_on_allowed_loopback_port": (
            parts is not None and parts.scheme == "http" and parts.hostname == "127.0.0.1"
            and parts.path == "/infer" and port in ports
        ),
    }
    failures = [name for name, passed in checks.items() if not passed]
    return {"deployment_id": expected_deployment, "status": "pass"}, failures, url


def deploy_check(document, expected_deployment):
    payload = document.get("payload") if isinstance(document.get("payload"), dict) else {}
    checks = {
        "deploy_command": document.get("command") == "deploy",
        "deployment_phase_active": payload.get("phase") == "active",
        "deployment_id": payload.get("deployment_id") == expected_deployment,
    }
    return {"deploy": "pass"}, [name for name, passed in checks.items() if not passed]


def infer_request():
    import base64
    import struct
    payload = struct.pack("<4f", 1.0, 2.0, 3.0, 4.0)
    return {
        "schema_version": "0.1",
        "request_id": "macos-offline-runtime-1",
        "endpoint": "macos-offline-runtime",
        "inputs": [{
            "name": "probe",
            "tensor": {"dtype": "float32", "layout": "row_major", "shape": [1, 4]},
            "payload_b64": base64.b64encode(payload).decode("ascii"),
        }],
    }


def infer_check(request, response):
    if isinstance(response, dict) and "payload" in response:
        response = response["payload"]
    outputs = response.get("outputs") if isinstance(response, dict) else None
    by_name = {item.get("name"): item for item in outputs or [] if isinstance(item, dict)}
    echoed = by_name.get("echo_probe") or {}
    sent = request["inputs"][0]
    tensor = echoed.get("tensor") if isinstance(echoed.get("tensor"), dict) else {}
    checks = {
        # The fixture backend echoes each input as echo_<name>; the worker
        # adds byte_offset and byte_size, so only declared fields compare.
        "inference_echoed_the_input": all(tensor.get(key) == value
                                          for key, value in sent["tensor"].items()),
        "inference_preserved_the_payload": echoed.get("payload_b64") == sent["payload_b64"],
    }
    return {"infer": "pass"}, [name for name, passed in checks.items() if not passed]


def doctor_check(document, status, exact_row):
    payload = document.get("payload") if isinstance(document.get("payload"), dict) else {}
    findings = {item.get("id"): item for item in payload.get("findings") or []
                if isinstance(item, dict)}
    failures = []
    if status != 0:
        failures.append("doctor_exit_status")
    if document.get("command") != "doctor":
        failures.append("doctor_command")
    if payload.get("failing") != 0:
        failures.append("doctor_no_failing_findings")
    for finding in DOCTOR_FINDINGS_OK:
        if (findings.get(finding) or {}).get("status") != "ok":
            failures.append(f"{finding}_ok")
    if (findings.get("platform_row") or {}).get("message") != \
            f"resolves to support row `{exact_row}`":
        failures.append("platform_row_exact")
    return {"doctor": "pass", "platform_row": exact_row}, failures


def read_since(path, offset):
    try:
        with open(path, "rb") as handle:
            handle.seek(offset)
            return handle.read().decode("utf-8", "replace")
    except OSError as error:
        raise CheckFailed(f"cannot read {os.path.basename(path)}: {error.strerror}")


def admission_check(agent_output, observability_output, exact_row):
    """This run's launchd output must hold exactly one admission decision,
    for the exact row with validated evidence, and one observability
    startup line: a second of either means a service restarted."""
    lines = [line for line in agent_output.splitlines() if line.startswith("platform admission:")]
    match = ADMISSION_LINE.fullmatch(lines[0]) if len(lines) == 1 else None
    row, reason, evidence, memory = match.groups() if match else (None, None, None, None)
    startups = [line for line in observability_output.splitlines()
                if line.startswith(OBSERVABILITY_STARTUP_PREFIX)]
    checks = {
        "admission_logged_once": len(lines) == 1,
        "admission_line_parsed": match is not None,
        "admission_exact_row": row == exact_row["row_id"],
        "admission_reason_none": reason == "none",
        "admission_evidence_validated": evidence == "validated",
        "admission_memory_ceiling": memory is not None and
        int(memory) == exact_row["accelerator"]["memory_bytes"],
        "observability_started_once": len(startups) == 1,
    }
    result = {"row": row, "reason": reason, "evidence": evidence,
              "observability_started": len(startups) == 1}
    return result, [name for name, passed in checks.items() if not passed]


def evidence(directory, deployment):
    """The sanitized offline-runtime.json, built from the stage's results."""

    def load(name):
        with open(os.path.join(directory, name), encoding="utf-8") as handle:
            return json.load(handle)

    profile = load("profile.json")
    with open(os.path.join(directory, "network-denied.sb"), "rb") as handle:
        if hashlib.sha256(handle.read()).hexdigest() != profile["profile_sha256"]:
            raise CheckFailed("the offline profile changed during the stage")
    services = {}
    for name in ("agent", "observability"):
        services[name] = {
            "launchd_program": "sandbox-exec",
            "loaded_from": "derived plist",
            "launchd_runs_through_stage": load(f"{name}-final.json")["runs"],
            "restored_launchd_program": "service binary",
            "restored_loaded_from": "LaunchAgents plist",
        }
    service_state = load("services-sandbox.json")
    tree = load("tree.json")
    tree_state = load("tree-sandbox.json")
    restored = load("restored-sandbox.json")
    admission = load("admission.json")
    return {
        "profile": {
            "sha256": profile["profile_sha256"],
            "allowed_loopback_ports": [profile["serving_port"], profile["candidate_port"]],
            "unix_sockets": "allowed except mDNSResponder",
        },
        "control": load("control.json"),
        "probe": load("probe.json"),
        "services": services,
        "services_sandboxed_network_denied": service_state["all_sandboxed"],
        "readback_controls": service_state["readback_controls"],
        "process_tree": {
            "processes": tree["processes"],
            "serving_workers": tree["serving_workers"],
            "backend_sidecars": tree["backend_sidecars"],
            "all_sandboxed_network_denied": tree_state["all_sandboxed"],
        },
        "listeners": load("listeners.json"),
        "admission": {key: admission[key] for key in ("row", "reason", "evidence")},
        "deployment_id": deployment,
        "cli_under_profile": {"status": "pass", "deploy": "pass", "infer": "pass",
                              "doctor": "pass", "mps_probe": "pass"},
        "restore": {"normal_launchd_supervision": "pass",
                    "no_sandboxed_process_remains": restored["none_sandboxed"]},
    }


# --- command line ------------------------------------------------------


def _load_json(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError) as error:
        raise CheckFailed(f"cannot read {os.path.basename(path)} as JSON: {error}")


def _job_pid(path):
    pid = _load_json(path).get("pid")
    if not isinstance(pid, int) or pid <= 0:
        raise CheckFailed(f"{os.path.basename(path)} records no running pid")
    return pid


def _pids_file(path):
    with open(path, encoding="utf-8") as handle:
        return [int(item) for item in handle.read().split()]


def _emit(result, out, failures=(), what="checks"):
    text = json.dumps(result, indent=2, sort_keys=True)
    print(text)
    if failures:
        raise CheckFailed(f"{what} failed: " + ", ".join(failures))
    if out:
        with open(out, "w", encoding="utf-8") as handle:
            handle.write(text + "\n")


def _port_pair(text):
    """`serving,candidate`, as profile-ports prints them."""
    items = text.split(",")
    if len(items) != 2 or not all(re.fullmatch(r"[0-9]+", item) for item in items):
        raise argparse.ArgumentTypeError(f"expected two ports as serving,candidate, found {text!r}")
    return int(items[0]), int(items[1])


def _read_print(path):
    if path == "-":
        return sys.stdin.read()
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)

    def command(name, *options):
        sub = commands.add_parser(name)
        for option in options:
            flags, kwargs = option
            sub.add_argument(*flags, **kwargs)
        sub.add_argument("--out")
        return sub

    def opt(*flags, **kwargs):
        return flags, kwargs

    command("render-profile", opt("--agent-config", required=True),
            opt("--profile", required=True))
    command("profile-ports", opt("--profile", required=True))
    command("label-forms", opt("--service", required=True))
    command("service-label", opt("--keg", required=True), opt("--service", required=True))
    command("derive-plist", opt("--formula-plist", required=True), opt("--label", required=True),
            opt("--program", required=True), opt("--profile", required=True),
            opt("--plist-out", required=True))
    command("launchd-job", opt("--print", dest="print_file", required=True),
            opt("--field"), opt("--program"), opt("--path"),
            opt("--sandboxed-arguments-from"), opt("--profile"),
            opt("--runs", type=int), opt("--same-pid-as"), opt("--startup", action="store_true"))
    command("sandbox-state", opt("--profile", required=True),
            opt("--expect", choices=("sandboxed", "unsandboxed"), required=True),
            opt("--job", action="append", default=[]),
            opt("--pids-file"), opt("--all-tensorplate-processes", action="store_true"))
    command("process-tree", opt("--job", required=True), opt("--pids-out", required=True))
    command("quiesced", opt("--profile", required=True), opt("--attempts", type=int, default=30))
    command("listeners", opt("--pids-file", required=True), opt("--status", required=True))
    command("control", opt("--ports", type=_port_pair, required=True))
    command("probe", opt("--agent-socket", required=True), opt("--health-url"),
            opt("--status"), opt("--ports", type=_port_pair, required=True),
            opt("--listen-port", type=int))
    command("classify", opt("--probe", required=True), opt("--control", required=True),
            opt("--deployment", required=True))
    command("preflight", opt("--work-dir", required=True))
    command("status-check", opt("--status", required=True), opt("--deployment", required=True),
            opt("--profile", required=True))
    command("deploy-check", opt("--deploy", required=True), opt("--deployment", required=True))
    command("infer-request")
    command("infer-check", opt("--request", required=True), opt("--response", required=True))
    command("doctor-check", opt("--doctor", required=True),
            opt("--status", type=int, required=True), opt("--exact-row", required=True))
    command("log-size", opt("--log", required=True))
    command("admission-check", opt("--agent-log", required=True),
            opt("--agent-offset", type=int, required=True),
            opt("--observability-log", required=True),
            opt("--observability-offset", type=int, required=True),
            opt("--exact-row", required=True))
    command("evidence", opt("--dir", required=True), opt("--deployment", required=True))
    commands.add_parser("child-udp")

    args = parser.parse_args(argv)
    try:
        return _run(args)
    except CheckFailed as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


def _run(args):
    name = args.command
    if name == "child-udp":
        print(attempt(lambda: udp_send("192.0.2.1", 9)))
    elif name == "render-profile":
        text = render_profile(*serving_ports(_load_json(args.agent_config)))
        write_profile(args.profile, text)
        serving, candidate = profile_ports(text)
        _emit({"profile_sha256": hashlib.sha256(text.encode()).hexdigest(),
               "serving_port": serving, "candidate_port": candidate}, args.out)
    elif name == "profile-ports":
        print(",".join(str(port) for port in profile_ports(read_profile(args.profile))))
    elif name == "label-forms":
        print("\n".join(label_forms(args.service)))
    elif name == "service-label":
        print(service_label(args.keg, args.service))
    elif name == "derive-plist":
        read_profile(args.profile)
        with open(args.formula_plist, "rb") as handle:
            document = plistlib.load(handle)
        derived = derive_plist(document, args.label, args.program, args.profile)
        with open(args.plist_out, "wb") as handle:
            plistlib.dump(derived, handle)
        _emit({"derived": os.path.basename(args.plist_out)}, args.out)
    elif name == "launchd-job":
        job = parse_launchd_job(_read_print(args.print_file))
        if args.field:
            value = job[args.field]
            if isinstance(value, bool):
                value = "yes" if value else "no"
            print("" if value is None else value)
            return 0
        arguments = None
        if args.sandboxed_arguments_from:
            if not args.profile:
                raise CheckFailed("--sandboxed-arguments-from needs --profile")
            with open(args.sandboxed_arguments_from, "rb") as handle:
                arguments = expected_sandboxed_arguments(plistlib.load(handle), args.profile)
        failures = check_launchd_job(
            job, program=args.program, path=args.path, arguments=arguments, runs=args.runs,
            same_pid_as=_job_pid(args.same_pid_as) if args.same_pid_as else None,
            startup=args.startup)
        _emit(job, args.out, failures, f"launchd job {job['label']} checks")
        if args.startup and job["pid"] is None:
            # The shell retries only this result. Wrong definitions,
            # malformed output and failed launches still return 1.
            return 75
    elif name == "sandbox-state":
        required = [_job_pid(path) for path in args.job]
        required += _pids_file(args.pids_file) if args.pids_file else []
        if args.all_tensorplate_processes:
            if args.expect != "unsandboxed":
                raise CheckFailed("--all-tensorplate-processes reads back an unsandboxed expectation")
            result, failures = no_sandboxed_tensorplate_process(args.profile, required)
        else:
            result, failures = sandbox_states(args.profile, required, [], args.expect)
        _emit(result, args.out, failures, "sandbox readback checks")
    elif name == "process-tree":
        pids = process_tree(_job_pid(args.job))
        roles = [process_role(process_arguments(pid)) for pid in pids]
        with open(args.pids_out, "w", encoding="utf-8") as handle:
            handle.write("".join(f"{pid}\n" for pid in pids))
        result = {"processes": len(pids), "serving_workers": roles.count("serving"),
                  "backend_sidecars": roles.count("sidecar")}
        failures = [check for check, count in (("tree_has_serving_worker", result["serving_workers"]),
                                               ("tree_has_backend_sidecar", result["backend_sidecars"]))
                    if count == 0]
        _emit(result, args.out, failures, "process tree checks")
    elif name == "quiesced":
        ports = ",".join(str(port) for port in profile_ports(read_profile(args.profile)))
        for remaining in range(args.attempts, 0, -1):
            pids = tensorplate_processes()
            listening = lsof(["-nP", f"-iTCP:{ports}", "-sTCP:LISTEN"]).strip()
            if not pids and not listening:
                _emit({"tensorplate_processes": 0, "serving_port_listeners": 0}, args.out)
                return 0
            if remaining > 1:
                time.sleep(1)
        print(listening)
        raise CheckFailed(
            f"{len(pids)} TensorPlate processes and "
            f"{'a' if listening else 'no'} serving-port listener remain after the services stopped")
    elif name == "listeners":
        tree = _pids_file(args.pids_file)
        if not tree:
            raise CheckFailed("the process tree is empty")
        # Every TensorPlate process, not only the agent's tree: the
        # observability service and anything else running a TensorPlate
        # binary.
        pids = list(dict.fromkeys(tree + tensorplate_processes()))
        _, _, url = status_check(_load_json(args.status), None, ())
        try:
            serving_port = urllib.parse.urlsplit(url).port
        except (TypeError, ValueError):
            serving_port = None
        output = lsof(["-nP", "-F", "pPnT", "-a", "-p", ",".join(str(pid) for pid in pids), "-i"])
        print(output, end="")
        sockets = parse_lsof(output)
        failures = check_listeners(sockets, set(pids), set(tree), serving_port)
        _emit({"tensorplate_processes": len(pids), "internet_sockets": len(sockets),
               "loopback_only": "loopback_only" not in failures,
               "serving_listener_in_tree": "serving_listener_in_tree" not in failures},
              args.out, failures, "listener checks")
    elif name == "control":
        # A control is checked as it is taken and filed only if it can be
        # the baseline its probe is classified against, so a host that
        # cannot provide one fails the stage before anything is denied. A
        # probe is filed whatever it says, and classified against its
        # control later.
        document = run_control(args.ports)
        _emit(document, args.out, control_failures(document), "unsandboxed control checks")
    elif name == "probe":
        health_url = args.health_url
        if args.status:
            _, _, url = status_check(_load_json(args.status), None, ())
            parts = urllib.parse.urlsplit(url or "")
            health_url = urllib.parse.urlunsplit((parts.scheme, parts.netloc, "/health", "", ""))
        if not health_url:
            raise CheckFailed("probe needs --health-url or --status")
        _emit(run_probe(args.agent_socket, health_url, args.ports, args.listen_port), args.out)
    elif name == "classify":
        failures = classify(_load_json(args.probe), _load_json(args.control), args.deployment)
        _emit({"network_denied_except_loopback_serving_ports": True}, args.out, failures,
              "offline profile enforcement checks")
    elif name == "preflight":
        result, failures = preflight(args.work_dir)
        _emit(result, args.out, failures, "offline profile preflight checks")
    elif name == "status-check":
        ports = profile_ports(read_profile(args.profile))
        result, failures, _ = status_check(_load_json(args.status), args.deployment, ports)
        _emit(result, args.out, failures, "status checks")
    elif name == "deploy-check":
        result, failures = deploy_check(_load_json(args.deploy), args.deployment)
        _emit(result, args.out, failures, "deploy checks")
    elif name == "infer-request":
        _emit(infer_request(), args.out)
    elif name == "infer-check":
        result, failures = infer_check(_load_json(args.request), _load_json(args.response))
        _emit(result, args.out, failures, "inference checks")
    elif name == "doctor-check":
        result, failures = doctor_check(_load_json(args.doctor), args.status, args.exact_row)
        _emit(result, args.out, failures, "doctor checks")
    elif name == "log-size":
        print(os.path.getsize(args.log) if os.path.exists(args.log) else 0)
    elif name == "admission-check":
        exact_row = _load_json(args.exact_row)
        result, failures = admission_check(
            read_since(args.agent_log, args.agent_offset),
            read_since(args.observability_log, args.observability_offset),
            exact_row,
        )
        _emit(result, args.out, failures, "admission checks")
    elif name == "evidence":
        _emit(evidence(args.dir, args.deployment), args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
