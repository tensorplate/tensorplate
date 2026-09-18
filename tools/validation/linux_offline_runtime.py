#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Offline-runtime checks for the systemd lifecycle harnesses.

The offline stage denies both TensorPlate services, and every CLI call it
makes, all IP traffic but the two loopback host addresses, then requires
the appliance to keep working. This module renders the per-unit denial,
reads the denial back from systemd, probes the network from inside a
denied transient unit, from inside each denied service's own control
group and from inside each transient unit a CLI call runs in -- running
the call there only once that unit's probe passed -- classifies every
result against an undenied control, and checks that the row still
resolved from whatever the row resolves from with the metadata service
unreachable -- a boot-bound machine-type record on Compute Engine, local
facts alone on a row that has no metadata service at all.

It is named for the mechanism, not for a row. The drop-in, the policy
readback, the probes and the classification carry no row in them. What
is row-specific is supplied as options, so another systemd harness
adopts this file unchanged rather than editing it:

  * `probe`, `control`, `probe-unit`, `control-unit` and `run-denied`
    take `--metadata-address`, and `none` omits the metadata operations
    on a host that has no metadata service;
  * `classify` takes `--metadata-operation absent`, which then requires
    those operations to be absent from both documents rather than letting
    a missing operation read as one that passed;
  * `identity-check` takes `--expect-source none`, for a row whose agent
    establishes no machine type and records none;
  * `doctor-check` and `identity-check` take the tokens the row expects
    its agent and its doctor to say.

Every default is the Compute Engine row's, because that is the row this
module was first written for; tools/validation/jetson-lifecycle.sh runs
its offline stage to exactly this bar and supplies its own, as a row with
no metadata service and no machine-type record of any kind.

Every subcommand prints its JSON result, writes it to --out when given,
and exits non-zero naming the checks that failed. Results written to the
evidence directory carry no host addresses, unit paths or process ids.
Nothing this module prints quotes a path either: what it writes to
stderr is filed in the stage log, so a failure it did not anticipate is
reported by subcommand and exception type alone, with no message and no
traceback (EXIT_UNEXPECTED).

The fail-open shapes this closes, because each of them turns a stage
that proves nothing into a stage that passes:

  * `systemctl show` answers for a unit that does not exist, is not
    loaded, or is dead, and answers with EMPTY property values. A denial
    readback that only looks for unexpected entries therefore passes for
    a unit nobody denied. Every readback here requires the unit to be
    loaded, active and carrying an invocation id first.
  * `systemctl show` answers with the unit's LOADED configuration, which
    counts a drop-in from `daemon-reload` onwards whether or not anything
    restarted under it. So the readback also compares the invocation id
    against the one from before, and a policy that never reached a
    running instance fails.
  * `IPAddressDeny=` is silently inert where systemd cannot install its
    BPF filter, and systemd installs it per unit on a best-effort basis
    (src/core/cgroup.c, cgroup_apply_firewall, ignores the result).
    Reading the property back proves the configuration, never the
    enforcement, and a transient unit that was filtered says nothing
    about a service, or another transient unit, whose own attach failed.
    So the probe runs in a transient unit, inside each service's control
    group, and inside every transient unit a CLI call runs in, before
    the call and as the condition for making it -- each against its own
    control -- and a probe that could not run is a failure, never a skip.
  * systemd prints `IPAddressDeny=` and `IPAddressAllow=` from a hash set
    (src/core/dbus-cgroup.c walks it with SET_FOREACH), whose order
    changes with each PID 1 start. The readback compares the prefixes
    as sets and files them in one canonical order, so a correct denial
    is not refused on a boot that happens to print them the other way.
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

# Prefix for the unit directories, overridable only so the stage can be
# driven against a stubbed appliance in CI. A harness whose assertions
# only ever run on a machine CI cannot reach is a harness whose assertions
# nobody has seen fire. It moves the directories and nothing else: the
# rendered text, the rule and every check are the same either way.
UNIT_ROOT = os.environ.get("TP_OFFLINE_UNIT_ROOT", "")
# Where the unified cgroup hierarchy is mounted, overridable for the same
# reason and nothing else. Unset, it is read from the mount table.
CGROUP_ROOT = os.environ.get("TP_OFFLINE_CGROUP_ROOT", "")
MOUNT_TABLE = "/proc/self/mountinfo"
# The unified-hierarchy line of this file names the control group the
# process is in, which is how joining one is read back.
PROC_SELF_CGROUP = "/proc/self/cgroup"

# Runtime only. A drop-in under /etc would outlive the run, and outlive a
# reboot, on a host the operator did not agree to leave denied.
RUNTIME_UNIT_DIR = UNIT_ROOT + "/run/systemd/system"
DROP_IN_NAME = "10-tensorplate-validation-offline.conf"
# Never written by this module; named so a check can refuse a path there.
PERSISTENT_UNIT_DIR = UNIT_ROOT + "/etc/systemd/system"

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
# unreachable, so the control has to reach it: a refusal under denial
# means nothing unless the same operation succeeded moments earlier. A row
# with no metadata service passes --metadata-address none and omits both
# operations.
METADATA_ADDRESS = "169.254.169.254"
METADATA_PORT = 80
METADATA_OPERATION = "tcp_gce_metadata"
# The same address as a datagram, which is where the refusal is
# attributable: see REFUSED and SILENCED below.
METADATA_UDP_OPERATION = "udp_gce_metadata"
METADATA_OPERATIONS = (METADATA_OPERATION, METADATA_UDP_OPERATION)
# A datagram port nothing needs to listen on; the send is the operation.
DISCARD_PORT = 9

# A policy refusal, as against a routing or listener outcome.
#
# systemd's filter is a cgroup_skb egress program, and a packet it drops
# comes back from the IP output path as -EPERM (kernel/bpf/cgroup.c,
# bpf_prog_run_array_cg; net/ipv4/ip_output.c, ip_finish_output). A UDP
# sendto() returns that to the caller synchronously, so a datagram is
# where the refusal is attributable. EACCES is accepted because it is the
# same class of answer and no routing failure produces either.
REFUSED = ("EPERM", "EACCES")

# What a TCP connect reports when the filter drops its SYN: nothing.
#
# tcp_connect() (net/ipv4/tcp_output.c) returns a transmit error only
# when it is -ECONNREFUSED; for any other it leaves the SYN on the
# retransmit queue and connect() in progress, so the caller waits out its
# own timeout. Such an outcome is attributable to the denial only against
# a control whose same connect was ANSWERED within that timeout -- one
# that timed out too says nothing -- and it is filed apart from the
# synchronous refusals, as what it is.
SILENCED = ("timeout", "ETIMEDOUT")
ANSWERED = ("ok", "ECONNREFUSED")
TCP_OPERATIONS = (METADATA_OPERATION, "tcp_resolver_stub")

# What a control has to have reported for a refusal of the same operation
# under the denial to mean anything: the operation completed.
#
# A datagram completed when sendto() returned, which `attempt` records as
# `ok`, and the child's send completed when the child ran, exited 0 and
# printed that. A connect completed when the far end answered it. Every
# other outcome is a control that sent nothing anyone saw -- a child that
# exited non-zero or never ran, a timeout, an exception's name, a string
# this module does not produce -- and a control that sent nothing cannot
# show that the later refusal was this denial's doing. So the controls are
# checked against these sets rather than against a list of the failures
# someone thought of. The unroutable outcomes below are the one deliberate
# exception, and are handled apart.
DELIVERED = ("ok",)


def completed_outcomes(name):
    """The control outcomes that count as `name` having completed."""
    return ANSWERED if name in TCP_OPERATIONS else DELIVERED

# Outcomes that mean the send never reached the address filter at all.
#
# On Linux the cgroup egress filter runs after the route lookup, so a
# destination the host has no route for answers the same way with and
# without the drop-in. An IPv4-only VM -- the default Compute Engine VPC
# is IPv4-only -- answers every global IPv6 destination this way. Such an
# operation discriminates nothing on that host: it is named in the result
# rather than counted as a refusal or as a failure to refuse, and all
# that can be required of the probe is that the denial did not make it
# start working. Refusals are tested first, so an EPERM is never read as
# one of these.
UNROUTABLE = ("ENETUNREACH", "EHOSTUNREACH", "ENETDOWN", "EAFNOSUPPORT",
              "EADDRNOTAVAIL", "EPFNOSUPPORT")
# Loopback destinations every Linux host routes. An unroutable control for
# one of these is a broken host rather than a limit of it, and these are
# the operations that prove the `localhost` shorthand was not used, so
# they are never excused as operations this host cannot send.
ALWAYS_ROUTABLE = ("tcp_resolver_stub", "udp_resolver_stub", "udp_loopback_alias")

# Long enough for a loopback listener or the metadata service to answer
# the control; the denied connects wait out all of it, twice.
CONNECT_TIMEOUT_SECONDS = 3

# Operations the denial must refuse, and the undenied control must not.
DENIED_NAMES = (
    METADATA_OPERATION,
    METADATA_UDP_OPERATION,
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
# What runs inside a service's own control group: the datagrams only.
# Each is refused synchronously, so the probe takes no timeout per unit,
# and none of them needs the agent socket or a serving port.
UNIT_DENIED_NAMES = tuple(name for name in DENIED_NAMES if name.startswith("udp_"))
UNIT_ALLOWED_NAMES = tuple(name for name in ALLOWED_NAMES if name.startswith("udp_"))
SCOPES = {
    "transient": (DENIED_NAMES, ALLOWED_NAMES),
    "unit": (UNIT_DENIED_NAMES, UNIT_ALLOWED_NAMES),
}

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
# What the agent logs where it established no machine type at all.
NO_SOURCE = "none"

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
    status = 1


class NotEnforced(CheckFailed):
    """`run-denied` refused to run its command: the unit it is in did not
    show that it enforces the denial."""
    status = 71


# Exit statuses. Every subcommand exits 0 when its checks passed,
# EXIT_CHECKS_FAILED naming the checks that did not, and 2 on a usage
# error (argparse's). EXIT_UNEXPECTED is anything else that went wrong
# inside this module, named by the exception's type alone: its message
# and its traceback can quote the checkout's path or the evidence
# directory's, and what this module prints to stderr is filed in the
# stage log, which is published. EXIT_NOT_ENFORCED is `run-denied`
# refusing to run its command. The last two are outside the CLI's
# documented exit codes (0-6, 10, 11), which `run-denied` passes through
# once it has run the command.
EXIT_CHECKS_FAILED = CheckFailed.status
EXIT_UNEXPECTED = 70
EXIT_NOT_ENFORCED = NotEnforced.status


def _read_error(error):
    """Why a file could not be read, without the path str(OSError) quotes.
    A JSON decoding error names a line and a column and nothing else."""
    if isinstance(error, OSError):
        return error.strerror or type(error).__name__
    return str(error)


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


def check_drop_in(unit, text, path):
    """The bytes read back from the installed drop-in, and where it sits.

    `path` is where the caller installed the file, not where this module
    would have put it: a check against a path this function computed
    itself could never fail, and the thing worth refusing is a stage that
    installed the denial somewhere else."""
    failures = list(lint_drop_in(text))
    if text != DROP_IN_TEXT:
        failures.append("drop_in_bytes_match_the_rendered_text")
    if not path.startswith(RUNTIME_UNIT_DIR + "/"):
        failures.append("drop_in_is_a_runtime_unit_file")
    if path.startswith(PERSISTENT_UNIT_DIR):
        failures.append("drop_in_is_not_persistent")
    if path != drop_in_path(unit):
        failures.append("drop_in_is_the_path_for_this_unit")
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
    """The prefix list systemd prints, as networks, in one canonical order.

    systemd prints these from a hash set whose order changes with each
    PID 1 start, so the order carries nothing and is not kept: IPv4
    before IPv6, then by address, then by length. An unparseable entry is
    kept as its text, after every network, so a check names it rather
    than dropping it. A repeated entry is kept too, so a list that is
    longer than the set it should be does not compare equal to it."""
    items = []
    for token in value.split():
        try:
            items.append(ipaddress.ip_network(token, strict=False))
        except ValueError:
            items.append(token)
    return sorted(items, key=_prefix_order)


def _prefix_order(item):
    if isinstance(item, str):
        return (2, 0, 0, item)
    return (0 if item.version == 4 else 1, int(item.network_address), item.prefixlen, "")


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


def _unit_was_replaced(show, previous, failures, name):
    """The running instance is not the one from before this policy change.

    `systemctl show` answers with the unit's LOADED configuration: a
    drop-in counts from `daemon-reload` onwards, whether or not anything
    restarted under it, and a removed drop-in stops counting the same
    way. The invocation id is the only property that says which instance
    is running, so a stage whose restart never happened -- or never took
    -- is caught here rather than certified from configuration."""
    if previous is None:
        return
    if show.get("InvocationID") == previous:
        failures.append(name)


def check_denial(unit, text, previous=None):
    """The unit is running, denies every address, and allows exactly the
    two host addresses -- not a prefix that also covers the resolver.

    Both lists are compared in canonical order, which DENY_ANY_PREFIXES
    and ALLOWED_PREFIXES are already written in."""
    show = parse_show(text)
    failures = []
    _unit_is_live(show, failures)
    _unit_was_replaced(show, previous, failures, "restarted_under_the_denial")
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


def check_no_denial(unit, text, previous=None):
    """After cleanup: the unit is running again and carries no policy."""
    show = parse_show(text)
    failures = []
    _unit_is_live(show, failures)
    _unit_was_replaced(show, previous, failures, "restarted_without_the_denial")
    if (show.get("IPAddressDeny") or "").strip():
        failures.append("deny_list_is_empty_again")
    if (show.get("IPAddressAllow") or "").strip():
        failures.append("allow_list_is_empty_again")
    return ({"unit": unit_service_name(unit), "denied": False}, sorted(set(failures)))


def check_policy(expect, shows, absent=(), previous=()):
    """Both services' effective policy in one answer, plus the drop-in
    paths that must be gone.

    `shows` is (unit, `systemctl show` output), `previous` is (unit, the
    invocation id that unit carried before this policy change) and
    `absent` is drop-in paths. A path that still exists, or that names
    anything but a runtime unit file, is a host this run changed and did
    not change back. The persistent drop-in is checked on every call, not
    only after cleanup: nothing here may ever write under
    /etc/systemd/system."""
    check = check_denial if expect == "denied" else check_no_denial
    was = {}
    for unit, value in previous:
        was[unit_service_name(unit)] = value
    units, failures, observed = [], [], []
    persistent_found = 0
    for unit, text in shows:
        result, unit_failures = check(unit, text, was.get(unit_service_name(unit)))
        units.append(result["unit"])
        if "allowed_prefixes" in result:
            observed.append(result["allowed_prefixes"])
        failures += ["{}:{}".format(result["unit"], name) for name in unit_failures]
        persistent = os.path.join(PERSISTENT_UNIT_DIR, result["unit"] + ".d", DROP_IN_NAME)
        if os.path.lexists(persistent):
            persistent_found += 1
            failures.append("{}:no_persistent_drop_in".format(result["unit"]))
    if not units:
        failures.append("units_read")
    # A unit named in `previous` that was not read back would leave its
    # restart unchecked, which is the shape this whole comparison exists
    # to refuse.
    for name in was:
        if name not in units:
            failures.append("{}:read_back".format(name))
    removed = 0
    for path in absent:
        name = os.path.basename(os.path.dirname(path))
        if not path.startswith(RUNTIME_UNIT_DIR + "/"):
            failures.append("removed_path_is_a_runtime_unit_file:" + name)
        elif os.path.lexists(path):
            failures.append("drop_in_removed:" + name)
        else:
            removed += 1
    result = {"units": units, "denied": expect == "denied",
              "persistent_drop_ins_found": persistent_found}
    if expect == "denied":
        if any(item != observed[0] for item in observed[1:]):
            failures.append("both_units_allow_the_same_addresses")
        # What systemd reported, not what this module asked for: the
        # evidence document states the policy the host was under.
        result["allowed_prefixes"] = observed[0] if observed else []
    if was:
        result["units_restarted"] = sorted(was)
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


def _was_pair(text):
    """`<unit>=<invocation>`, as --was takes it."""
    if "=" not in text:
        raise argparse.ArgumentTypeError(
            "expected <unit>=<invocation>, found {!r}".format(text))
    unit, invocation = text.split("=", 1)
    if not re.fullmatch(r"[0-9a-f]{32}", invocation):
        raise argparse.ArgumentTypeError(
            "not an invocation id: {!r}".format(invocation))
    return unit, invocation


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


def denied_operations(metadata_address=METADATA_ADDRESS):
    operations = []
    if metadata_address:
        operations += [
            (METADATA_OPERATION,
             lambda: tcp_connect(metadata_address, METADATA_PORT)),
            (METADATA_UDP_OPERATION,
             lambda: udp_send(metadata_address, DISCARD_PORT)),
        ]
    operations += [
        ("tcp_resolver_stub", lambda: tcp_connect(RESOLVER_STUB, 53)),
        ("udp_resolver_stub", lambda: udp_send(RESOLVER_STUB, 53)),
        ("udp_loopback_alias", lambda: udp_send("127.0.0.2", DISCARD_PORT)),
        ("udp_test_net_v4", lambda: udp_send("192.0.2.1", DISCARD_PORT)),
        ("udp_documentation_v6",
         lambda: udp_send("2001:db8::1", DISCARD_PORT, socket.AF_INET6)),
        ("udp_test_net_v4_from_child", child_udp_send),
    ]
    return operations


def allowed_operations(agent_socket, serving_port):
    return [
        ("unix_agent_socket", lambda: unix_connect(agent_socket)),
        ("tcp_loopback_serving_port", lambda: tcp_connect("127.0.0.1", serving_port)),
        ("udp_loopback_allowed_v4", lambda: udp_send("127.0.0.1", DISCARD_PORT)),
        ("udp_loopback_allowed_v6", lambda: udp_send("::1", DISCARD_PORT, socket.AF_INET6)),
    ]


def run_probe(agent_socket, serving_port, metadata_address=METADATA_ADDRESS):
    return {
        "denied": dict((name, attempt(operation))
                       for name, operation in denied_operations(metadata_address)),
        "allowed": dict((name, attempt(operation))
                        for name, operation in allowed_operations(agent_socket, serving_port)),
    }


def run_unit_probe(metadata_address=METADATA_ADDRESS):
    """The datagram operations, from whatever control group this process
    is in by now. No agent socket and no serving port: see
    UNIT_DENIED_NAMES."""
    return {
        "denied": dict((name, attempt(operation))
                       for name, operation in denied_operations(metadata_address)
                       if name in UNIT_DENIED_NAMES),
        "allowed": dict((name, attempt(operation))
                        for name, operation in allowed_operations("", 0)
                        if name in UNIT_ALLOWED_NAMES),
    }


# --- running a CLI call in a unit that proved its own enforcement --------

# The TensorPlate CLI calls the offline stage makes, in the order it makes
# them, each in its own denied transient unit.
CLI_CALLS = ("status", "doctor", "deploy", "status-after-deploy", "infer")


def cli_evidence_name(call):
    """`offline-cli-probe-<call>.json`: the probe the transient unit that
    ran `call` took of itself, before running it. `run-denied` files it
    under this name and the certificate reads it back by it, so the name
    is worked out here and nowhere else."""
    if call not in CLI_CALLS:
        raise CheckFailed("not an offline CLI call: {!r}".format(call))
    return "offline-cli-probe-{}.json".format(call)


def _write_document(path, document):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(document, indent=2, sort_keys=True) + "\n")


def run_denied(call, control, evidence_dir, command, metadata_address=METADATA_ADDRESS,
               execute=os.execvp):
    """Show that the unit this process is in enforces the denial, then
    become `command`.

    systemd attaches the address filter to each unit separately and does
    not act on a failure to (cgroup_apply_firewall), so what a CLI call
    runs in has to show its own filter works. A probe in another unit
    shows nothing about this one, and neither does reading back the
    properties this unit was created with: that is configuration. So the
    probe runs here, and the command runs here after it -- exec'd, not
    started as a child, so it is this process in this control group, and
    the unit's exit status is the command's.

    The probe is the datagram set a service's probe sends: each datagram
    is refused synchronously, so it costs no timeout. It is classified
    against the stage's undenied transient control, and the probe
    document is written to `evidence_dir` as `cli_evidence_name(call)`
    whatever it says, as every probe is. The command runs only if the
    classification passed. Nothing is written to stdout, which is the
    command's.

    A control that cannot be a baseline is refused before anything is
    sent, as a failed check (EXIT_CHECKS_FAILED) that names the control:
    EXIT_NOT_ENFORCED says this unit's filter did not work, and a
    control that never completed says nothing about this unit."""
    if not command:
        raise CheckFailed("run-denied needs the command to run after --")
    out = os.path.join(evidence_dir, cli_evidence_name(call))
    baseline = _load_json(control)
    if not isinstance(baseline, dict):
        raise CheckFailed("{} is not a JSON object".format(os.path.basename(control)))
    metadata = _metadata_operation(metadata_address)
    unusable = control_failures(baseline, metadata, "unit")
    if unusable:
        raise CheckFailed("{} is not a control the {} unit can be classified against, "
                          "so {} was not run: {}".format(
                              os.path.basename(control), call,
                              os.path.basename(command[0]), ", ".join(unusable)))
    document = run_unit_probe(metadata_address)
    document["call"] = call
    _write_document(out, document)
    _, failures = classify(document, baseline, metadata, "unit")
    if failures:
        raise NotEnforced("the {} unit does not enforce the denial, so {} was not run: {}".format(
            call, os.path.basename(command[0]), ", ".join(failures)))
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        execute(command[0], command)
    except OSError as error:
        raise CheckFailed("cannot run {}: {}".format(
            os.path.basename(command[0]), _read_error(error)))


# --- probing from inside a service's control group -----------------------


def cgroup2_mount():
    """Where the unified hierarchy is mounted: systemd attaches its address
    filter there, whatever else the host mounts."""
    if CGROUP_ROOT:
        return CGROUP_ROOT
    try:
        with open(MOUNT_TABLE, encoding="utf-8") as handle:
            for line in handle:
                fields = line.split()
                if "-" not in fields:
                    continue
                separator = fields.index("-")
                if len(fields) > separator + 1 and fields[separator + 1] == "cgroup2":
                    return fields[4]
    except OSError as error:
        raise CheckFailed("cannot read the mount table: {}".format(error.strerror))
    raise CheckFailed("no cgroup2 hierarchy is mounted, so no service can carry "
                      "systemd's address filter")


def control_group_path(unit, control_group):
    """The directory of `unit`'s own control group, from the ControlGroup
    systemd reported for it.

    Refused unless it is an absolute path with no empty, `.` or `..`
    component that ends in exactly this unit: the process is about to be
    moved there, and a path that named any other group would probe --
    and alter -- a unit this stage did not deny. An empty value is what
    systemd reports for a unit with no running instance."""
    name = unit_service_name(unit)
    parts = control_group.split("/")
    if (not control_group.startswith("/")
            or any(part in ("", ".", "..") for part in parts[1:])
            or parts[-1] != name):
        raise CheckFailed(
            "{} is not the control group of a running {}".format(
                control_group or "an empty ControlGroup", name))
    return os.path.join(cgroup2_mount(), control_group.lstrip("/"))


def join_control_group(control_group, path):
    """Move this process into `path`, then read the move back.

    A socket is charged to the control group of the process that created
    it, at creation, so everything this process sends from here on passes
    the filter attached to that group -- which is the point. Refused
    rather than assumed: a write that the kernel took and a process that
    is somewhere else would probe the wrong filter."""
    try:
        with open(os.path.join(path, "cgroup.procs"), "w", encoding="ascii") as handle:
            handle.write("{}\n".format(os.getpid()))
        with open(PROC_SELF_CGROUP, encoding="utf-8") as handle:
            current = [line.rstrip("\n")[3:] for line in handle if line.startswith("0::")]
    except OSError as error:
        raise CheckFailed("cannot join the control group {}: {}".format(
            control_group, _read_error(error)))
    if current != [control_group]:
        raise CheckFailed("this process did not join {}: it is in {}".format(
            control_group, current or "no unified control group"))


def drop_privileges(uid, gid, ops=os):
    """Joining a service's control group takes root; probing does not, and
    the probe is not left running as root. `ops` is the os module except
    in the tests, which must never change the credentials of the process
    running them."""
    if uid == 0 or gid == 0:
        raise CheckFailed("refusing to probe as root: pass the operator's uid and gid")
    if ops.geteuid() != 0:
        raise CheckFailed("joining a service's control group needs root; run this under sudo")
    ops.setgroups([])
    ops.setgid(gid)
    ops.setuid(uid)
    if (ops.getuid(), ops.geteuid(), ops.getgid(), ops.getegid()) != (uid, uid, gid, gid):
        raise CheckFailed("the probe did not drop to uid {} gid {}".format(uid, gid))


def _outcomes(document, key):
    value = document.get(key) if isinstance(document, dict) else None
    return value if isinstance(value, dict) else {}


def control_failures(control, metadata="required", scope="transient"):
    """Why `control` cannot be the baseline a probe of `scope` is
    classified against, whatever that probe turns out to say.

    Checked when the control is taken, before anything is denied, so a
    host that cannot provide a baseline fails the stage before it is
    changed -- and again by `classify` and `run-denied`, which never take
    a control on trust. Every name returned starts with `control_`,
    except `metadata_operation_not_probed`, which a row with no metadata
    service applies to both documents.

    An operation the host cannot route is not a failure here: it is
    excused by `classify`, which names it, unless it is a loopback
    destination every host can send to. Every other operation has to
    have completed (`completed_outcomes`): a refusal under the denial is
    attributable only to an operation that went through a moment
    earlier. The metadata operations must succeed outright rather than
    merely not be refused: the stage's whole claim is that this service
    was reachable and the denial is what made it unreachable."""
    failures = []
    denied_names, allowed_names = SCOPES[scope]
    denied = _outcomes(control, "denied")
    allowed = _outcomes(control, "allowed")
    names = list(denied_names)
    if metadata == "absent":
        names = [name for name in names if name not in METADATA_OPERATIONS]
        # Absent because the row has no metadata service, not absent
        # because a probe dropped it: an operation that quietly vanished
        # from a document must never read as one that passed.
        for name in METADATA_OPERATIONS:
            if name in denied:
                failures.append("metadata_operation_not_probed:" + name)
    for name in names:
        outcome = denied.get(name)
        if name in METADATA_OPERATIONS:
            if outcome != "ok":
                failures.append("control_metadata_service_reachable:" + name)
        elif outcome in UNROUTABLE:
            if name in ALWAYS_ROUTABLE:
                failures.append("control_routable:" + name)
        elif outcome in REFUSED:
            # Refused by something else on the host.
            failures.append("control_not_refused:" + name)
        elif outcome not in completed_outcomes(name):
            # Missing, a child that failed or never ran, a timeout, or
            # anything else that is not a send or connect that happened.
            failures.append("control_completed:" + name)
    for name in allowed_names:
        if allowed.get(name) != "ok":
            failures.append("control_allowed:" + name)
    return sorted(set(failures))


def classify(probe, control, metadata="required", scope="transient"):
    """The probe proves the denial only against a control that completed
    the same operations. Both halves are required: a control that was
    refused by something else on the host, or that never sent at all,
    makes the probe's refusal unattributable (`control_failures`), and a
    probe that was not refused means the denial did nothing.

    The control also decides which operations can prove anything here. An
    operation the host could not perform with nothing denied cannot be
    refused by the denial either; it is named in the result rather than
    reported as a denial that failed to bite -- except for a loopback
    destination, which every host can send to.

    A datagram has to be refused outright (REFUSED). A TCP connect cannot
    be: the kernel reports nothing for a dropped SYN, so it is accepted as
    silenced (SILENCED) -- which the control's answered connect is what
    makes attributable -- and filed apart from the refusals.

    `scope` names the operation set: `transient` for the probe in a
    denied transient unit, `unit` for the datagram subset run inside a
    service's own control group."""
    failures = control_failures(control, metadata, scope)
    denied_names, allowed_names = SCOPES[scope]
    control_denied = _outcomes(control, "denied")
    probe_denied = _outcomes(probe, "denied")
    probe_allowed = _outcomes(probe, "allowed")
    names = list(denied_names)
    if metadata == "absent":
        names = [name for name in names if name not in METADATA_OPERATIONS]
        for name in METADATA_OPERATIONS:
            if name in probe_denied:
                failures.append("metadata_operation_not_probed:" + name)
    refused_names, silenced_names, unroutable_names = [], [], []
    for name in names:
        outcome = control_denied.get(name)
        probed = probe_denied.get(name)
        if (outcome in UNROUTABLE and name not in METADATA_OPERATIONS
                and name not in ALWAYS_ROUTABLE):
            unroutable_names.append(name)
            # Nothing here to refuse, so the only thing to require is
            # that the denial did not make it start working.
            if probed != outcome:
                failures.append("probe_matches_the_unroutable_control:" + name)
            continue
        if probed in REFUSED:
            refused_names.append(name)
        elif name in TCP_OPERATIONS and probed in SILENCED:
            # Attributable because the control's same connect was
            # answered, which control_failures required: a connect that
            # timed out with nothing denied as well fails there.
            silenced_names.append(name)
        else:
            failures.append("refused:" + name)
    for name in allowed_names:
        if probe_allowed.get(name) != "ok":
            failures.append("allowed:" + name)
    result = {
        "ip_traffic_denied_except_the_two_host_addresses": True,
        "scope": scope,
        "metadata_operation": metadata,
        "operations_refused_under_the_denial": sorted(refused_names),
        "operations_silenced_under_the_denial": sorted(silenced_names),
        "operations_this_host_cannot_send": sorted(unroutable_names),
    }
    return result, sorted(set(failures))


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


def doctor_check(document, status, exact_row,
                 host_os_phrase=DOCTOR_RECORDED_PHRASE,
                 forbidden_host_os_phrase=DOCTOR_LIVE_PHRASE):
    """Doctor resolves the row with nothing failing, and says the machine
    type came from where the row expects it to under a denial.

    The two phrases are the row's, not this module's: the defaults are
    the Compute Engine ones, and an empty forbidden phrase forbids
    nothing. The result files the phrase that was required, and no
    phrase at all where none was: it states what was checked, never a
    machine-type source this call did not establish."""
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
    if host_os_phrase not in host_os:
        failures.append("host_os_machine_type_from_the_record")
    if forbidden_host_os_phrase and forbidden_host_os_phrase in host_os:
        failures.append("host_os_machine_type_not_from_live_metadata")
    return ({"doctor": "pass", "platform_row": exact_row,
             "host_os_phrase_required": host_os_phrase or None,
             "host_os_phrase_forbidden": forbidden_host_os_phrase or None},
            sorted(set(failures)))


def identity_check(journal_text, expect_source=RECORDED_SOURCE,
                   expect_record=RECORD_NOT_APPLICABLE, forbid_source=LIVE_SOURCE):
    """The agent's own account of where this start's machine type came from.

    Exactly one line, from this invocation: a second means the agent
    restarted, and the earlier line might have been the online one.

    The expected tokens are the row's. They default to the Compute Engine
    ones; a row that expects no machine type at all passes
    `--expect-source none`, and a detected machine type is then not
    required either."""
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
        "machine_type_from_the_record": source == expect_source,
        # A live answer would mean the denial let the metadata query
        # through, whatever the probe said.
        "metadata_service_was_not_reached": not forbid_source or source != forbid_source,
        # Nothing was recorded, because there was no live answer to record.
        "record_not_rewritten_while_denied": record == expect_record,
    }
    if expect_source != NO_SOURCE:
        checks["machine_type_detected"] = machine_type not in (None, NO_SOURCE)
    return ({"machine_type_source": source, "record": record},
            sorted(name for name, passed in checks.items() if not passed))


# --- evidence -----------------------------------------------------------


def evidence(directory, deployment):
    """The sanitized offline-runtime.json, derived from the stage's results.

    Every verdict here is read back from a document the stage filed, and
    `_emit` writes a document only after its own checks passed -- so a
    document that is present is a check that passed, and one that is
    missing refuses this command rather than being restated as a pass.
    The enforcement verdict is re-derived by classifying the same probe
    and control the certificate carries: a certificate that asserts what
    its own evidence contradicts is worse than no certificate at all.

    Carries outcome names and prefixes, never a host address the scanner
    would refuse, a unit path, or a process id."""

    def load(name):
        try:
            with open(os.path.join(directory, name), encoding="utf-8") as handle:
                document = json.load(handle)
        except (OSError, ValueError) as error:
            raise CheckFailed("cannot read {}: {}".format(name, _read_error(error)))
        if not isinstance(document, dict):
            raise CheckFailed("{} is not a JSON object".format(name))
        return document

    def field(document, name, key):
        if key not in document:
            raise CheckFailed("{} records no {}".format(name, key))
        return document[key]

    def verdict(name, key):
        document = load(name)
        if document.get(key) != "pass":
            raise CheckFailed("{} does not record a pass".format(name))
        return "pass"

    def classified(probe_name, control_name, metadata, scope):
        classification, failures = classify(
            load(probe_name), load(control_name), metadata, scope)
        if failures:
            raise CheckFailed(
                "{} and {} do not classify as an enforced denial: {}".format(
                    probe_name, control_name, ", ".join(failures)))
        return classification

    units = load("offline-denial.json")
    restored = load("offline-restored.json")
    filed = load("offline-classification.json")
    if units.get("denied") is not True:
        raise CheckFailed("offline-denial.json does not record a denied appliance")
    if restored.get("denied") is not False:
        raise CheckFailed("offline-restored.json does not record a restored appliance")
    denied_units = field(units, "offline-denial.json", "units")
    restored_units = field(restored, "offline-restored.json", "units")
    if not denied_units or sorted(restored_units) != sorted(denied_units):
        raise CheckFailed(
            "offline-restored.json restores {}, not the units denied: {}".format(
                restored_units, denied_units))
    # Every unit the stage denied had its running instance replaced under
    # the denial, and replaced again without it. A readback that compared
    # no invocation certifies the unit's loaded configuration and nothing
    # about the service that was running, so it is refused here rather
    # than published as a denial that took effect.
    for document, name in ((units, "offline-denial.json"),
                           (restored, "offline-restored.json")):
        restarted = document.get("units_restarted") or []
        if sorted(restarted) != sorted(document["units"]):
            raise CheckFailed(
                "{} does not record every unit's instance as replaced: "
                "read back {}, restart-checked {}".format(
                    name, document["units"], restarted))
        # A unit file under /etc outlives the run and the next reboot,
        # whichever side of the stage found it.
        if field(document, name, "persistent_drop_ins_found") != 0:
            raise CheckFailed(
                "{} found a persistent drop-in under /etc/systemd/system".format(name))
    # One runtime drop-in per denied unit, each read back as gone.
    removed = field(restored, "offline-restored.json", "drop_ins_removed")
    if removed != len(denied_units):
        raise CheckFailed(
            "offline-restored.json read back {} drop-in(s) as removed for {} "
            "denied unit(s)".format(removed, len(denied_units)))

    metadata = filed.get("metadata_operation", "required")
    classification = classified(
        "offline-probe.json", "offline-control.json", metadata, "transient")
    # And inside each service's own control group, because systemd attaches
    # the filter to each unit on a best-effort basis and the transient
    # unit's filter says nothing about theirs.
    per_unit = {}
    for unit in denied_units:
        per_unit[unit] = {
            "control": load(unit_evidence_name("control", unit)),
            "probe": load(unit_evidence_name("probe", unit)),
            "classification": classified(
                unit_evidence_name("probe", unit),
                unit_evidence_name("control", unit), metadata, "unit"),
        }
    # And inside each transient unit a CLI call ran in, from that unit's
    # own probe, for the same reason: another unit's filter says nothing
    # about this one's. `run-denied` ran the call only after this same
    # classification passed; it is derived again here rather than taken
    # on trust, against the transient control.
    per_call = {}
    for call in CLI_CALLS:
        name = cli_evidence_name(call)
        probe = load(name)
        if probe.get("call") != call:
            raise CheckFailed("{} records the probe of {!r}, not of {}".format(
                name, probe.get("call"), call))
        per_call[call] = {
            "probe": probe,
            "classification": classified(name, "offline-control.json", metadata, "unit"),
        }

    allow = units.get("allowed_prefixes") or []
    if not allow:
        raise CheckFailed(
            "offline-denial.json carries no allow list read back from systemd")
    resolver = ipaddress.ip_address(RESOLVER_STUB)
    try:
        if not isinstance(allow, list) or not all(isinstance(item, str) for item in allow):
            raise ValueError("{!r} is not a list of prefixes".format(allow))
        resolver_allowed = any(resolver in ipaddress.ip_network(prefix, strict=False)
                               for prefix in allow)
    except ValueError as error:
        raise CheckFailed("the allow list read back from systemd is unparseable: "
                          "{}".format(error))
    if resolver_allowed:
        raise CheckFailed(
            "the allow list read back from systemd admits " + RESOLVER_STUB)
    if [str(item) for item in prefixes(" ".join(allow))] != list(ALLOWED_PREFIXES):
        raise CheckFailed(
            "the allow list read back from systemd is not exactly {}: {}".format(
                " and ".join(ALLOWED_PREFIXES), allow))
    return {
        "mechanism": {
            "per_unit_drop_in": DROP_IN_NAME,
            "drop_in_scope": "runtime",
            "deny": "any",
            "allow": allow,
            "resolver_stub_allowed": resolver_allowed,
        },
        "units_denied": denied_units,
        "units_restarted_under_the_denial": units["units_restarted"],
        "transient_unit_properties": list(DENIAL_PROPERTIES),
        "control": load("offline-control.json"),
        "probe": load("offline-probe.json"),
        "classification": classification,
        "units_probed_in_their_own_control_group": per_unit,
        "cli_units_probed_before_each_call": per_call,
        "enforced": all(
            item["ip_traffic_denied_except_the_two_host_addresses"]
            for item in [classification]
            + [entry["classification"] for entry in per_unit.values()]
            + [entry["classification"] for entry in per_call.values()]),
        "identity": load("offline-identity.json"),
        "deployment_id": deployment,
        "cli_under_denial": {
            "status": verdict("offline-status-check.json", "status"),
            "doctor": verdict("offline-doctor-check.json", "doctor"),
            "deploy": verdict("offline-deploy-check.json", "deploy"),
            "infer": verdict("offline-infer-check.json", "infer"),
        },
        "restore": {"drop_ins_removed": removed,
                    "units_undenied": restored_units,
                    "units_restarted_without_the_denial": restored["units_restarted"],
                    "persistent_unit_files_written":
                        restored["persistent_drop_ins_found"]},
    }


def unit_evidence_name(kind, unit):
    """`offline-unit-<kind>-<unit>.service.json`, for the probe and control
    run inside that unit's control group."""
    return "offline-unit-{}-{}.json".format(kind, unit_service_name(unit))


# --- command line -------------------------------------------------------


def _load_json(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError) as error:
        raise CheckFailed("cannot read {} as JSON: {}".format(
            os.path.basename(path), _read_error(error)))


def _read(path):
    if path == "-":
        return sys.stdin.read()
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read()
    except OSError as error:
        raise CheckFailed("cannot read {}: {}".format(
            os.path.basename(path), _read_error(error)))


def _emit(result, out, failures=(), what="checks"):
    text = json.dumps(result, indent=2, sort_keys=True)
    print(text)
    if failures:
        raise CheckFailed(what + " failed: " + ", ".join(failures))
    if out:
        with open(out, "w", encoding="utf-8") as handle:
            handle.write(text + "\n")


def _metadata_address(value):
    """`none` names a row with no metadata service; anything else is one."""
    return "" if value == "none" else value


def _metadata_operation(metadata_address):
    """What `classify` requires of the metadata operations, for a probe
    or control taken with `metadata_address`."""
    return "required" if metadata_address else "absent"


def build_parser():
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

    probe_options = (
        opt("--agent-socket", required=True),
        opt("--serving-port", type=int, required=True),
        opt("--metadata-address", type=_metadata_address, default=METADATA_ADDRESS),
    )
    unit_probe_options = (
        opt("--unit", required=True),
        opt("--control-group", required=True),
        opt("--uid", type=int, required=True),
        opt("--gid", type=int, required=True),
        opt("--metadata-address", type=_metadata_address, default=METADATA_ADDRESS),
    )

    commands.add_parser("drop-in-text")
    commands.add_parser("transient-properties")
    commands.add_parser("child-udp")
    commands.add_parser("drop-in-path").add_argument("--unit", required=True)
    evidence_name = commands.add_parser("unit-evidence-name")
    evidence_name.add_argument("--kind", choices=("control", "probe", "classification"),
                               required=True)
    evidence_name.add_argument("--unit", required=True)
    command("check-drop-in", opt("--unit", required=True),
            opt("--print", dest="print_file", required=True))
    command("check-policy", opt("--expect", choices=("denied", "none"), required=True),
            opt("--show", type=_show_pair, action="append", default=[], required=True),
            opt("--was", type=_was_pair, action="append", default=[]),
            opt("--absent", action="append", default=[]))
    command("serving-port", opt("--status", required=True))
    command("probe", *probe_options)
    command("control", *probe_options)
    command("probe-unit", *unit_probe_options)
    command("control-unit", *unit_probe_options)
    command("classify", opt("--probe", required=True), opt("--control", required=True),
            opt("--metadata-operation", choices=("required", "absent"), default="required"),
            opt("--scope", choices=sorted(SCOPES), default="transient"))
    command("status-check", opt("--status", required=True), opt("--deployment", required=True))
    command("deploy-check", opt("--deploy", required=True), opt("--deployment", required=True))
    command("infer-request", opt("--request-id", required=True))
    command("infer-check", opt("--request", required=True), opt("--response", required=True))
    command("doctor-check", opt("--doctor", required=True),
            opt("--status", type=int, required=True), opt("--exact-row", required=True),
            opt("--host-os-phrase", default=DOCTOR_RECORDED_PHRASE),
            opt("--forbid-host-os-phrase", default=DOCTOR_LIVE_PHRASE))
    command("identity-check", opt("--agent-journal", required=True),
            opt("--expect-source", default=RECORDED_SOURCE),
            opt("--expect-record", default=RECORD_NOT_APPLICABLE),
            opt("--forbid-source", default=LIVE_SOURCE))
    command("evidence", opt("--dir", required=True), opt("--deployment", required=True))
    # run-denied ... -- COMMAND...: the command is split off before
    # parsing rather than left to argparse, whose handling of `--` has
    # changed between Python releases.
    wrapper = commands.add_parser("run-denied")
    wrapper.add_argument("--call", choices=CLI_CALLS, required=True)
    wrapper.add_argument("--control", required=True)
    wrapper.add_argument("--evidence-dir", required=True)
    wrapper.add_argument("--metadata-address", type=_metadata_address,
                         default=METADATA_ADDRESS)
    return parser


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    command = []
    if "--" in argv:
        split = argv.index("--")
        argv, command = argv[:split], argv[split + 1:]
    parser = build_parser()
    args = parser.parse_args(argv)
    if (args.command == "run-denied") != bool(command):
        parser.error("run-denied, and only run-denied, takes a command after --")
    args.exec_argv = command
    return _run_bounded(args)


def _run_bounded(args):
    """The one boundary every subcommand runs behind.

    A named check that failed says which. Anything else -- a bug, a file
    that vanished, an interrupt -- is named by its type and the
    subcommand, and nothing more: no message, no traceback. This stderr is
    filed in the stage log and copied into the report's detail, and both
    of those would otherwise quote the checkout's path or the evidence
    directory's into evidence that is published."""
    try:
        return _run(args)
    except CheckFailed as error:
        print("error: {}".format(error), file=sys.stderr)
        return error.status
    except (Exception, KeyboardInterrupt) as error:
        print("error: {} failed unexpectedly: {}".format(
            args.command, type(error).__name__), file=sys.stderr)
        return EXIT_UNEXPECTED


def _taken_control_failures(name, document, args, scope):
    """A control is checked as it is taken and filed only if it can be the
    baseline its probe is classified against, so a host that cannot
    provide one fails the stage before anything is denied. A probe is
    filed whatever it says, and classified against its control later."""
    if name not in ("control", "control-unit"):
        return ()
    return control_failures(document, _metadata_operation(args.metadata_address), scope)


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
        result, failures = check_drop_in(
            args.unit, _read(args.print_file), args.print_file)
        _emit(result, args.out, failures, "drop-in checks for " + args.unit)
    elif name == "check-policy":
        shows = [(unit, _read(path)) for unit, path in args.show]
        result, failures = check_policy(args.expect, shows, args.absent, args.was)
        _emit(result, args.out, failures,
              "{} address-policy readback checks".format(args.expect))
    elif name == "serving-port":
        _, failures, port = status_check(_load_json(args.status), None)
        if port is None or "serving_url_on_the_allowed_loopback_address" in failures:
            raise CheckFailed("status reports no serving URL on the allowed loopback address")
        print(port)
    elif name == "unit-evidence-name":
        print(unit_evidence_name(args.kind, args.unit))
    elif name == "run-denied":
        run_denied(args.call, args.control, args.evidence_dir, args.exec_argv,
                   args.metadata_address)
    elif name in ("probe", "control"):
        document = run_probe(args.agent_socket, args.serving_port, args.metadata_address)
        _emit(document, args.out, _taken_control_failures(name, document, args, "transient"),
              "transient control checks")
    elif name in ("probe-unit", "control-unit"):
        # Validated before anything moves: the path is where this process
        # is about to be written.
        path = control_group_path(args.unit, args.control_group)
        join_control_group(args.control_group, path)
        drop_privileges(args.uid, args.gid)
        document = run_unit_probe(args.metadata_address)
        document["unit"] = unit_service_name(args.unit)
        _emit(document, args.out, _taken_control_failures(name, document, args, "unit"),
              "unit control checks")
    elif name == "classify":
        result, failures = classify(_load_json(args.probe), _load_json(args.control),
                                    args.metadata_operation, args.scope)
        _emit(result, args.out, failures, "offline denial enforcement checks")
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
        result, failures = doctor_check(_load_json(args.doctor), args.status, args.exact_row,
                                        args.host_os_phrase, args.forbid_host_os_phrase)
        _emit(result, args.out, failures, "doctor checks")
    elif name == "identity-check":
        result, failures = identity_check(_read(args.agent_journal), args.expect_source,
                                          args.expect_record, args.forbid_source)
        _emit(result, args.out, failures, "platform identity checks")
    elif name == "evidence":
        _emit(evidence(args.dir, args.deployment), args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
