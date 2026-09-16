#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Offline-runtime checks for the systemd lifecycle harnesses.

The offline stage denies both TensorPlate services, and every CLI call it
makes, all IP traffic but the two loopback host addresses, then requires
the appliance to keep working. This module renders the per-unit denial,
reads the denial back from systemd, probes the network from inside a
denied unit, classifies every result against an undenied control, and
checks that the row was resolved from the boot-bound machine-type record
rather than from a metadata service the denial made unreachable.

It is named for the mechanism, not for a row: the drop-in, the probe and
the classification are the same on a Compute Engine VM and on a Jetson,
and tools/validation/jetson-lifecycle.sh defers its offline stage with
exactly this bar.

Every subcommand prints its JSON result, writes it to --out when given,
and exits non-zero naming the checks that failed. Results written to the
evidence directory carry no host addresses, unit paths or process ids.

Three fail-open shapes this closes, because each of them turns a stage
that proves nothing into a stage that passes:

  * `systemctl show` answers for a unit that does not exist, is not
    loaded, or is dead, and answers with EMPTY property values. A denial
    readback that only looks for unexpected entries therefore passes for
    a unit nobody denied. Every readback here requires the unit to be
    loaded, active and carrying an invocation id first.
  * `IPAddressDeny=` is silently inert where systemd cannot install its
    BPF filter. Reading the property back proves the configuration, never
    the enforcement, so the probe is what decides -- and a probe that
    could not run is a failure, never a skip.
  * A drop-in left behind leaves the host denied after the run. The
    removal is asserted the same way it was applied: the file is gone and
    the effective properties are empty again.

Standard library only; Python 3.9 or newer.
"""

import argparse
import errno
import ipaddress
import json
import os
import re
import socket
import subprocess
import sys
import urllib.parse

# Runtime only. A drop-in under /etc would outlive the run, and outlive a
# reboot, on a host the operator did not agree to leave denied.
RUNTIME_UNIT_DIR = "/run/systemd/system"
DROP_IN_NAME = "10-tensorplate-validation-offline.conf"
# Never written by this module; named so a check can refuse a path there.
PERSISTENT_UNIT_DIR = "/etc/systemd/system"

# The two host addresses, as prefixes that match exactly one address each.
#
# NOT systemd's `localhost` shorthand, which expands to 127.0.0.0/8 and
# would admit 127.0.0.53 -- the systemd-resolved stub listener, and so the
# whole DNS namespace behind it. An offline stage that admits the resolver
# is an offline stage that never went offline.
ALLOWED_PREFIXES = ("127.0.0.1/32", "::1/128")
# What `IPAddressDeny=any` reads back as.
DENY_ANY_PREFIXES = ("0.0.0.0/0", "::/0")
FORBIDDEN_ALLOW_TOKEN = "localhost"
# The address the shorthand would have admitted, named so the refusal is
# about this host rather than about prefix arithmetic in the abstract.
RESOLVER_STUB = "127.0.0.53"

DROP_IN_TEXT = """\
# Written by tools/validation/linux_offline_runtime.py for the offline
# lifecycle stage, and removed on every exit path including SIGINT,
# SIGTERM and SIGHUP. Runtime only: nothing here survives a reboot.
#
# `localhost` is deliberately not used: it expands to 127.0.0.0/8, which
# admits the systemd-resolved stub at {resolver}.
[Service]
IPAddressDeny=any
IPAddressAllow={first}
IPAddressAllow={second}
""".format(resolver=RESOLVER_STUB, first=ALLOWED_PREFIXES[0], second=ALLOWED_PREFIXES[1])

# Properties every transient unit the stage runs a command in carries, so
# a denied CLI call and the denied probe are denied the same way the
# services are.
DENIAL_PROPERTIES = ("IPAddressDeny=any",) + tuple(
    "IPAddressAllow=" + prefix for prefix in ALLOWED_PREFIXES
)

# The GCE metadata service. The offline claim is about this address being
# unreachable, so the control has to reach it: EPERM under denial means
# nothing unless the same operation succeeded moments earlier.
METADATA_ADDRESS = "169.254.169.254"
METADATA_PORT = 80

# A policy refusal, as against a routing or listener outcome. The BPF
# filter answers the sending syscall with EPERM; EACCES is accepted
# because it is the same class of answer and no routing failure produces
# either.
REFUSED = ("EPERM", "EACCES")

CONNECT_TIMEOUT_SECONDS = 5

# Operations the denial must refuse, and the undenied control must not.
DENIED_NAMES = (
    "tcp_gce_metadata",
    "tcp_resolver_stub",
    "udp_resolver_stub",
    "udp_loopback_alias",
    "udp_test_net_v4",
    "udp_documentation_v6",
    "udp_test_net_v4_from_child",
)
# Operations that must succeed under the denial as well as without it.
ALLOWED_NAMES = (
    "unix_agent_socket",
    "tcp_loopback_serving_port",
    "udp_loopback_allowed_v4",
    "udp_loopback_allowed_v6",
)

# The agent start-up line that says where the machine type came from.
IDENTITY_LINE = re.compile(
    r"platform identity: machine_type=(\S+) source=(\S+) record=(.+)"
)
# platform/src/machine_type_record.rs: the token for a machine type read
# back from the record because the metadata service was unreachable.
RECORDED_SOURCE = "recorded_gce_metadata"
LIVE_SOURCE = "gce_metadata"
# No live answer to record, which is what an unreachable service means.
# A record written during the offline stage would mean the agent reached
# the metadata service, or recorded the record from itself.
RECORD_NOT_APPLICABLE = "not_applicable"

# cli/src/commands/doctor/mod.rs renders one of these into host_os.
DOCTOR_RECORDED_PHRASE = "recorded from GCE metadata by tensorplate-agent"
DOCTOR_LIVE_PHRASE = "(from GCE metadata)"

# Findings that must be ok while the host is denied. platform_row is
# checked separately, against the exact row.
DOCTOR_FINDINGS_OK = (
    "platform_profile",
    "host_os",
    "agent_reachable",
    "agent_socket",
    "platform_registry",
)


class CheckFailed(Exception):
    pass


# --- the drop-in -------------------------------------------------------


def unit_service_name(unit):
    """`tensorplate-agent` and `tensorplate-agent.service` name one unit."""
    if not re.fullmatch(r"[A-Za-z0-9@:_.-]+", unit):
        raise CheckFailed("not a unit name: {!r}".format(unit))
    return unit if unit.endswith(".service") else unit + ".service"


def drop_in_directory(unit):
    return os.path.join(RUNTIME_UNIT_DIR, unit_service_name(unit) + ".d")


def drop_in_path(unit):
    return os.path.join(drop_in_directory(unit), DROP_IN_NAME)


def lint_drop_in(text):
    """Every way this file could be written and not deny what it claims."""
    failures = []
    lines = [line.strip() for line in text.splitlines()]
    body = [line for line in lines if line and not line.startswith("#")]
    if body[:1] != ["[Service]"]:
        failures.append("drop_in_service_section")
    allow = [line.split("=", 1)[1] for line in body if line.startswith("IPAddressAllow=")]
    deny = [line.split("=", 1)[1] for line in body if line.startswith("IPAddressDeny=")]
    if deny != ["any"]:
        failures.append("drop_in_denies_any")
    if tuple(allow) != ALLOWED_PREFIXES:
        failures.append("drop_in_allows_exactly_the_two_host_addresses")
    if FORBIDDEN_ALLOW_TOKEN in allow:
        failures.append("drop_in_uses_the_localhost_shorthand")
    for line in body:
        if not line.startswith(("[Service]", "IPAddress")):
            failures.append("drop_in_unexpected_directive:" + line.split("=", 1)[0])
    return sorted(set(failures))


def check_drop_in(unit, text):
    """The bytes read back from the installed drop-in, and where it sits."""
    path = drop_in_path(unit)
    failures = list(lint_drop_in(text))
    if text != DROP_IN_TEXT:
        failures.append("drop_in_bytes_match_the_rendered_text")
    if not path.startswith(RUNTIME_UNIT_DIR + "/"):
        failures.append("drop_in_is_a_runtime_unit_file")
    if path.startswith(PERSISTENT_UNIT_DIR):
        failures.append("drop_in_is_not_persistent")
    return ({"unit": unit_service_name(unit), "runtime_drop_in": DROP_IN_NAME,
             "allowed_prefixes": list(ALLOWED_PREFIXES)}, sorted(set(failures)))


# --- reading the denial back from systemd -------------------------------


def parse_show(text):
    """`systemctl show -p ... UNIT` output as a mapping. A repeated key is
    refused rather than merged: systemd prints each property once, and
    silently keeping the last value would hide whatever produced two."""
    values = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        if "=" not in line:
            raise CheckFailed(
                "systemctl show printed a line that is not a property: {!r}".format(line))
        key, value = line.split("=", 1)
        if key in values:
            raise CheckFailed("systemctl show printed {} twice".format(key))
        values[key] = value
    return values


def prefixes(value):
    """The prefix list systemd prints, as networks. An unparseable entry is
    kept as its text so a check names it rather than dropping it."""
    items = []
    for token in value.split():
        try:
            items.append(ipaddress.ip_network(token, strict=False))
        except ValueError:
            items.append(token)
    return items


def _unit_is_live(show, failures):
    """A unit that is not loaded and running answers every property with an
    empty string, which reads as `nothing unexpected is allowed`. Refuse
    that before any property is interpreted."""
    if show.get("LoadState") != "loaded":
        failures.append("unit_loaded")
    if show.get("ActiveState") != "active":
        failures.append("unit_active")
    if not re.fullmatch(r"[0-9a-f]{32}", show.get("InvocationID") or ""):
        failures.append("unit_has_an_invocation")


def check_denial(unit, text):
    """The unit is running, denies every address, and allows exactly the
    two host addresses -- not a prefix that also covers the resolver."""
    show = parse_show(text)
    failures = []
    _unit_is_live(show, failures)
    deny = prefixes(show.get("IPAddressDeny") or "")
    allow = prefixes(show.get("IPAddressAllow") or "")
    if [str(item) for item in deny] != list(DENY_ANY_PREFIXES):
        failures.append("denies_every_address")
    if [str(item) for item in allow] != list(ALLOWED_PREFIXES):
        failures.append("allows_exactly_the_two_host_addresses")
    resolver = ipaddress.ip_address(RESOLVER_STUB)
    for item in allow:
        if isinstance(item, str):
            failures.append("allow_entry_unparseable:" + item)
        elif resolver in item:
            # An `IPAddressAllow=localhost` drop-in reads back as
            # 127.0.0.0/8 and lands here.
            failures.append("resolver_stub_is_not_allowed")
    return ({"unit": unit_service_name(unit), "denied": True,
             "allowed_prefixes": [str(item) for item in allow]}, sorted(set(failures)))


def check_no_denial(unit, text):
    """After cleanup: the unit is running again and carries no policy."""
    show = parse_show(text)
    failures = []
    _unit_is_live(show, failures)
    if (show.get("IPAddressDeny") or "").strip():
        failures.append("deny_list_is_empty_again")
    if (show.get("IPAddressAllow") or "").strip():
        failures.append("allow_list_is_empty_again")
    return ({"unit": unit_service_name(unit), "denied": False}, sorted(set(failures)))


def check_policy(expect, shows, absent=()):
    """Both services' effective policy in one answer, plus the drop-in
    paths that must be gone.

    `shows` is (unit, `systemctl show` output) and `absent` is drop-in
    paths. A path that still exists, or that names anything but a runtime
    unit file, is a host this run changed and did not change back. The
    persistent drop-in is checked on every call, not only after cleanup:
    nothing here may ever write under /etc/systemd/system."""
    check = check_denial if expect == "denied" else check_no_denial
    units, failures = [], []
    for unit, text in shows:
        result, unit_failures = check(unit, text)
        units.append(result["unit"])
        failures += ["{}:{}".format(result["unit"], name) for name in unit_failures]
        persistent = os.path.join(PERSISTENT_UNIT_DIR, result["unit"] + ".d", DROP_IN_NAME)
        if os.path.lexists(persistent):
            failures.append("{}:no_persistent_drop_in".format(result["unit"]))
    if not units:
        failures.append("units_read")
    removed = 0
    for path in absent:
        name = os.path.basename(os.path.dirname(path))
        if not path.startswith(RUNTIME_UNIT_DIR + "/"):
            failures.append("removed_path_is_a_runtime_unit_file:" + name)
        elif os.path.lexists(path):
            failures.append("drop_in_removed:" + name)
        else:
            removed += 1
    result = {"units": units, "denied": expect == "denied"}
    if absent:
        result["drop_ins_removed"] = removed
    return result, sorted(set(failures))


def _show_pair(text):
    """`<unit>=<path>`, as --show takes it."""
    if "=" not in text:
        raise argparse.ArgumentTypeError(
            "expected <unit>=<path>, found {!r}".format(text))
    unit, path = text.split("=", 1)
    return unit, path


# --- the network probe --------------------------------------------------


def udp_send(host, port, family=socket.AF_INET):
    sock = socket.socket(family, socket.SOCK_DGRAM)
    try:
        sock.sendto(b"x", (host, port))
    finally:
        sock.close()


def tcp_connect(host, port, family=socket.AF_INET):
    sock = socket.socket(family, socket.SOCK_STREAM)
    sock.settimeout(CONNECT_TIMEOUT_SECONDS)
    try:
        sock.connect((host, port))
    finally:
        sock.close()


def unix_connect(path):
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(CONNECT_TIMEOUT_SECONDS)
    try:
        sock.connect(path)
    finally:
        sock.close()


def child_udp_send():
    """The same send from a child process, which inherits the cgroup and so
    inherits the denial. The agent launches a serving worker, which
    launches a backend sidecar; a denial that stopped at the first process
    would say nothing about either."""
    try:
        result = subprocess.run(
            [sys.executable, os.path.abspath(__file__), "child-udp"],
            capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.SubprocessError) as error:
        # A child that never ran sent nothing, so its silence is not a
        # refusal.
        return "child_not_run_" + type(error).__name__
    if result.returncode != 0:
        return "child_exit_{}".format(result.returncode)
    return result.stdout.strip()


def attempt(operation):
    try:
        outcome = operation()
    except socket.timeout:
        return "timeout"
    except OSError as error:
        return errno.errorcode.get(error.errno, "OSError") if error.errno else type(error).__name__
    except Exception as error:  # a probe records every outcome by name
        return type(error).__name__
    return outcome if isinstance(outcome, str) else "ok"


def denied_operations():
    return [
        ("tcp_gce_metadata", lambda: tcp_connect(METADATA_ADDRESS, METADATA_PORT)),
        ("tcp_resolver_stub", lambda: tcp_connect(RESOLVER_STUB, 53)),
        ("udp_resolver_stub", lambda: udp_send(RESOLVER_STUB, 53)),
        ("udp_loopback_alias", lambda: udp_send("127.0.0.2", 9)),
        ("udp_test_net_v4", lambda: udp_send("192.0.2.1", 9)),
        ("udp_documentation_v6", lambda: udp_send("2001:db8::1", 9, socket.AF_INET6)),
        ("udp_test_net_v4_from_child", child_udp_send),
    ]


def allowed_operations(agent_socket, serving_port):
    return [
        ("unix_agent_socket", lambda: unix_connect(agent_socket)),
        ("tcp_loopback_serving_port", lambda: tcp_connect("127.0.0.1", serving_port)),
        ("udp_loopback_allowed_v4", lambda: udp_send("127.0.0.1", 9)),
        ("udp_loopback_allowed_v6", lambda: udp_send("::1", 9, socket.AF_INET6)),
    ]


def run_probe(agent_socket, serving_port):
    return {
        "denied": dict((name, attempt(operation)) for name, operation in denied_operations()),
        "allowed": dict((name, attempt(operation))
                        for name, operation in allowed_operations(agent_socket, serving_port)),
    }


def classify(probe, control):
    """The probe proves the denial only against a control that was not
    refused. Both halves are required: a control refused by something else
    on the host makes the probe's EPERM unattributable, and a probe that
    was not refused means the denial did nothing."""
    failures = []
    control_denied = control.get("denied") if isinstance(control.get("denied"), dict) else {}
    control_allowed = control.get("allowed") if isinstance(control.get("allowed"), dict) else {}
    probe_denied = probe.get("denied") if isinstance(probe.get("denied"), dict) else {}
    probe_allowed = probe.get("allowed") if isinstance(probe.get("allowed"), dict) else {}
    for name in DENIED_NAMES:
        outcome = control_denied.get(name)
        if name == "tcp_gce_metadata":
            # The one control that must succeed outright rather than merely
            # not be refused: the stage's whole claim is that this service
            # was reachable and the denial is what made it unreachable.
            if outcome != "ok":
                failures.append("control_metadata_service_reachable")
        elif outcome is None or outcome in REFUSED:
            failures.append("control_not_refused:" + name)
        if probe_denied.get(name) not in REFUSED:
            failures.append("refused:" + name)
    for name in ALLOWED_NAMES:
        if control_allowed.get(name) != "ok":
            failures.append("control_allowed:" + name)
        if probe_allowed.get(name) != "ok":
            failures.append("allowed:" + name)
    return sorted(set(failures))


# --- what the appliance answered while denied ---------------------------


def _payload(document):
    return document.get("payload") if isinstance(document.get("payload"), dict) else {}


def status_check(document, expected_deployment):
    """Status answered over the agent socket and names the offline
    deployment on an allowed loopback serving URL."""
    payload = _payload(document)
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
        "active_deployment": active.get("deployment_id") == expected_deployment,
        "serving_url_on_the_allowed_loopback_address": (
            parts is not None and parts.scheme == "http"
            and parts.hostname == "127.0.0.1" and isinstance(port, int)
        ),
    }
    failures = sorted(name for name, passed in checks.items() if not passed)
    return ({"deployment_id": expected_deployment, "status": "pass"}, failures, port)


def deploy_check(document, expected_deployment):
    payload = _payload(document)
    checks = {
        "deploy_command": document.get("command") == "deploy",
        "deployment_phase_active": payload.get("phase") == "active",
        "deployment_id": payload.get("deployment_id") == expected_deployment,
    }
    return ({"deploy": "pass"},
            sorted(name for name, passed in checks.items() if not passed))


def infer_request(request_id):
    import base64
    import struct
    payload = struct.pack("<4f", 1.0, 2.0, 3.0, 4.0)
    return {
        "schema_version": "0.1",
        "request_id": request_id,
        "endpoint": "linux-offline-runtime",
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
    by_name = dict((item.get("name"), item) for item in outputs or [] if isinstance(item, dict))
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
    return ({"infer": "pass"},
            sorted(name for name, passed in checks.items() if not passed))


def doctor_check(document, status, exact_row):
    """Doctor resolves the row with nothing failing, and says the machine
    type came from the record rather than from the metadata service."""
    payload = _payload(document)
    findings = dict((item.get("id"), item) for item in payload.get("findings") or []
                    if isinstance(item, dict))
    failures = []
    if status != 0:
        failures.append("doctor_exit_status")
    if document.get("command") != "doctor":
        failures.append("doctor_command")
    if payload.get("failing") != 0:
        failures.append("doctor_no_failing_findings")
    for finding in DOCTOR_FINDINGS_OK:
        if (findings.get(finding) or {}).get("status") != "ok":
            failures.append(finding + "_ok")
    row = findings.get("platform_row") or {}
    if row.get("status") != "ok":
        failures.append("platform_row_ok")
    if exact_row not in (row.get("message") or ""):
        failures.append("platform_row_exact")
    host_os = (findings.get("host_os") or {}).get("message") or ""
    if DOCTOR_RECORDED_PHRASE not in host_os:
        failures.append("host_os_machine_type_from_the_record")
    if DOCTOR_LIVE_PHRASE in host_os:
        failures.append("host_os_machine_type_not_from_live_metadata")
    return ({"doctor": "pass", "platform_row": exact_row,
             "machine_type_source": "recorded"}, sorted(set(failures)))


def identity_check(journal_text):
    """The agent's own account of where this start's machine type came from.

    Exactly one line, from this invocation: a second means the agent
    restarted, and the earlier line might have been the online one."""
    lines = []
    for number, line in enumerate(journal_text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            entry = json.loads(line)
        except ValueError:
            raise CheckFailed("agent journal line {} is not a JSON record".format(number))
        message = entry.get("MESSAGE") if isinstance(entry, dict) else None
        if isinstance(message, str) and message.startswith("platform identity:"):
            lines.append(message)
    match = IDENTITY_LINE.fullmatch(lines[0]) if len(lines) == 1 else None
    machine_type, source, record = match.groups() if match else (None, None, None)
    checks = {
        "identity_logged_once": len(lines) == 1,
        "identity_line_parsed": match is not None,
        "machine_type_detected": machine_type not in (None, "none"),
        "machine_type_from_the_record": source == RECORDED_SOURCE,
        # A live answer would mean the denial let the metadata query
        # through, whatever the probe said.
        "metadata_service_was_not_reached": source != LIVE_SOURCE,
        # Nothing was recorded, because there was no live answer to record.
        "record_not_rewritten_while_denied": record == RECORD_NOT_APPLICABLE,
    }
    return ({"machine_type_source": source, "record": record},
            sorted(name for name, passed in checks.items() if not passed))


# --- evidence -----------------------------------------------------------


def evidence(directory, deployment):
    """The sanitized offline-runtime.json, built from the stage's results.

    Carries outcome names and prefixes, never a host address the scanner
    would refuse, a unit path, or a process id."""

    def load(name):
        with open(os.path.join(directory, name), encoding="utf-8") as handle:
            return json.load(handle)

    units = load("offline-denial.json")
    restored = load("offline-restored.json")
    return {
        "mechanism": {
            "per_unit_drop_in": DROP_IN_NAME,
            "drop_in_scope": "runtime",
            "deny": "any",
            "allow": list(ALLOWED_PREFIXES),
            "localhost_shorthand_used": False,
        },
        "units_denied": units["units"],
        "transient_unit_properties": list(DENIAL_PROPERTIES),
        "control": load("offline-control.json"),
        "probe": load("offline-probe.json"),
        "enforced": True,
        "identity": load("offline-identity.json"),
        "deployment_id": deployment,
        "cli_under_denial": {"status": "pass", "doctor": "pass",
                             "deploy": "pass", "infer": "pass"},
        "restore": {"drop_ins_removed": restored["drop_ins_removed"],
                    "units_undenied": restored["units"],
                    "persistent_unit_files_written": 0},
    }


# --- command line -------------------------------------------------------


def _load_json(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError) as error:
        raise CheckFailed("cannot read {} as JSON: {}".format(os.path.basename(path), error))


def _read(path):
    if path == "-":
        return sys.stdin.read()
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read()
    except OSError as error:
        raise CheckFailed("cannot read {}: {}".format(os.path.basename(path), error.strerror))


def _emit(result, out, failures=(), what="checks"):
    text = json.dumps(result, indent=2, sort_keys=True)
    print(text)
    if failures:
        raise CheckFailed(what + " failed: " + ", ".join(failures))
    if out:
        with open(out, "w", encoding="utf-8") as handle:
            handle.write(text + "\n")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)

    def command(name, *options):
        sub = commands.add_parser(name)
        for flags, kwargs in options:
            sub.add_argument(*flags, **kwargs)
        sub.add_argument("--out")
        return sub

    def opt(*flags, **kwargs):
        return flags, kwargs

    commands.add_parser("drop-in-text")
    commands.add_parser("transient-properties")
    commands.add_parser("child-udp")
    commands.add_parser("drop-in-path").add_argument("--unit", required=True)
    command("check-drop-in", opt("--unit", required=True),
            opt("--print", dest="print_file", required=True))
    command("check-policy", opt("--expect", choices=("denied", "none"), required=True),
            opt("--show", type=_show_pair, action="append", default=[], required=True),
            opt("--absent", action="append", default=[]))
    command("serving-port", opt("--status", required=True))
    command("probe", opt("--agent-socket", required=True),
            opt("--serving-port", type=int, required=True))
    command("control", opt("--agent-socket", required=True),
            opt("--serving-port", type=int, required=True))
    command("classify", opt("--probe", required=True), opt("--control", required=True))
    command("status-check", opt("--status", required=True), opt("--deployment", required=True))
    command("deploy-check", opt("--deploy", required=True), opt("--deployment", required=True))
    command("infer-request", opt("--request-id", required=True))
    command("infer-check", opt("--request", required=True), opt("--response", required=True))
    command("doctor-check", opt("--doctor", required=True),
            opt("--status", type=int, required=True), opt("--exact-row", required=True))
    command("identity-check", opt("--agent-journal", required=True))
    command("evidence", opt("--dir", required=True), opt("--deployment", required=True))

    args = parser.parse_args(argv)
    try:
        return _run(args)
    except CheckFailed as error:
        print("error: {}".format(error), file=sys.stderr)
        return 1


def _run(args):
    name = args.command
    if name == "child-udp":
        print(attempt(lambda: udp_send("192.0.2.1", 9)))
    elif name == "drop-in-text":
        sys.stdout.write(DROP_IN_TEXT)
    elif name == "drop-in-path":
        print(drop_in_path(args.unit))
    elif name == "transient-properties":
        for value in DENIAL_PROPERTIES:
            print("--property=" + value)
    elif name == "check-drop-in":
        result, failures = check_drop_in(args.unit, _read(args.print_file))
        _emit(result, args.out, failures, "drop-in checks for " + args.unit)
    elif name == "check-policy":
        shows = [(unit, _read(path)) for unit, path in args.show]
        result, failures = check_policy(args.expect, shows, args.absent)
        _emit(result, args.out, failures,
              "{} address-policy readback checks".format(args.expect))
    elif name == "serving-port":
        _, failures, port = status_check(_load_json(args.status), None)
        if port is None or "serving_url_on_the_allowed_loopback_address" in failures:
            raise CheckFailed("status reports no serving URL on the allowed loopback address")
        print(port)
    elif name in ("probe", "control"):
        _emit(run_probe(args.agent_socket, args.serving_port), args.out)
    elif name == "classify":
        failures = classify(_load_json(args.probe), _load_json(args.control))
        _emit({"ip_traffic_denied_except_the_two_host_addresses": True}, args.out, failures,
              "offline denial enforcement checks")
    elif name == "status-check":
        result, failures, _ = status_check(_load_json(args.status), args.deployment)
        _emit(result, args.out, failures, "status checks")
    elif name == "deploy-check":
        result, failures = deploy_check(_load_json(args.deploy), args.deployment)
        _emit(result, args.out, failures, "deploy checks")
    elif name == "infer-request":
        _emit(infer_request(args.request_id), args.out)
    elif name == "infer-check":
        result, failures = infer_check(_load_json(args.request), _load_json(args.response))
        _emit(result, args.out, failures, "inference checks")
    elif name == "doctor-check":
        result, failures = doctor_check(_load_json(args.doctor), args.status, args.exact_row)
        _emit(result, args.out, failures, "doctor checks")
    elif name == "identity-check":
        result, failures = identity_check(_read(args.agent_journal))
        _emit(result, args.out, failures, "platform identity checks")
    elif name == "evidence":
        _emit(evidence(args.dir, args.deployment), args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
