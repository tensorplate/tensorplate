#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for tools/validation/linux_offline_runtime.py.

Run by verify_linux_offline_runtime.sh. The module is the mechanism both
systemd lifecycle harnesses use for their offline stage, so what it
refuses is what those stages refuse. Everything here is synthetic: no
unit file is written outside a temporary directory, no systemd command is
run, no socket is opened, no process is moved between control groups, and
no credential of the process running the tests is changed.

The harness end of the same mechanism -- the stage body, its cleanup and
its signal handling -- is exercised against the stubbed appliance in
verify_ubuntu_l4_cloud_lifecycle.sh.
"""

import ipaddress
import json
import os
import pathlib
import subprocess
import sys
import tempfile

REPO = pathlib.Path(__file__).resolve().parents[2]
MODULE = REPO / "tools/validation/linux_offline_runtime.py"
sys.path.insert(0, str(MODULE.parent))

import linux_offline_runtime as m  # noqa: E402

ROW = "ubuntu2404-x86-l4-g2s8"


def passed(name):
    print(f"linux offline runtime: {name}: pass")


def refused(operation, fragment):
    try:
        operation()
    except m.CheckFailed as error:
        assert fragment in str(error), (fragment, str(error))
        return
    raise AssertionError(f"not refused: expected {fragment!r}")


# --- the drop-in --------------------------------------------------------


def test_drop_in():
    assert m.lint_drop_in(m.DROP_IN_TEXT) == [], m.lint_drop_in(m.DROP_IN_TEXT)
    path = m.drop_in_path("tensorplate-agent")
    assert path == os.path.join(
        m.RUNTIME_UNIT_DIR, "tensorplate-agent.service.d", m.DROP_IN_NAME), path
    # A unit named with or without its suffix is the same unit.
    assert m.drop_in_path("tensorplate-agent.service") == path
    assert not path.startswith(m.PERSISTENT_UNIT_DIR), path
    refused(lambda: m.drop_in_path("tensorplate agent; rm -rf /"), "not a unit name")

    # The shorthand the owner ruling refuses, and every other way the file
    # could be written and not deny what it claims.
    shorthand = m.DROP_IN_TEXT.replace(
        "IPAddressAllow=127.0.0.1/32\nIPAddressAllow=::1/128\n",
        "IPAddressAllow=localhost\n")
    assert m.lint_drop_in(shorthand) == [
        "drop_in_allows_exactly_the_two_host_addresses",
        "drop_in_uses_the_localhost_shorthand",
    ], m.lint_drop_in(shorthand)
    for mutant, expected in (
        (m.DROP_IN_TEXT.replace("IPAddressDeny=any\n", ""), "drop_in_denies_any"),
        (m.DROP_IN_TEXT.replace("127.0.0.1/32", "127.0.0.0/8"),
         "drop_in_allows_exactly_the_two_host_addresses"),
        (m.DROP_IN_TEXT.replace("IPAddressAllow=::1/128\n", ""),
         "drop_in_allows_exactly_the_two_host_addresses"),
        (m.DROP_IN_TEXT.replace("[Service]", "[Unit]"), "drop_in_service_section"),
        (m.DROP_IN_TEXT + "ExecStartPre=/bin/true\n",
         "drop_in_unexpected_directive:ExecStartPre"),
    ):
        assert expected in m.lint_drop_in(mutant), (expected, m.lint_drop_in(mutant))

    result, failures = m.check_drop_in("tensorplate-agent", m.DROP_IN_TEXT, path)
    assert failures == [], failures
    assert result["allowed_prefixes"] == list(m.ALLOWED_PREFIXES), result
    # Bytes that lint clean but are not the bytes this release renders:
    # the file on the host is what systemd read, not what we meant.
    altered = m.DROP_IN_TEXT.replace("Runtime only", "runtime only")
    assert altered != m.DROP_IN_TEXT
    assert m.lint_drop_in(altered) == []
    assert m.check_drop_in("tensorplate-agent", altered, path)[1] == [
        "drop_in_bytes_match_the_rendered_text"]
    # The path is the caller's, not one this function recomputed: a
    # denial installed somewhere else is a host change this stage does
    # not remove, and a check against a path derived here could never
    # say so.
    persistent = os.path.join(m.PERSISTENT_UNIT_DIR, "tensorplate-agent.service.d",
                              m.DROP_IN_NAME)
    assert m.check_drop_in("tensorplate-agent", m.DROP_IN_TEXT, persistent)[1] == [
        "drop_in_is_a_runtime_unit_file", "drop_in_is_not_persistent",
        "drop_in_is_the_path_for_this_unit",
    ], m.check_drop_in("tensorplate-agent", m.DROP_IN_TEXT, persistent)[1]
    # The right tree, the wrong unit.
    assert m.check_drop_in("tensorplate-agent", m.DROP_IN_TEXT,
                           m.drop_in_path("tensorplate-observability"))[1] == [
        "drop_in_is_the_path_for_this_unit"]
    passed("drop-in rendering, linting and readback")


# --- reading the denial back --------------------------------------------


def show(deny="0.0.0.0/0 ::/0", allow="127.0.0.1/32 ::1/128", load="loaded",
         active="active", invocation="0" * 32):
    return (f"LoadState={load}\nActiveState={active}\nInvocationID={invocation}\n"
            f"IPAddressDeny={deny}\nIPAddressAllow={allow}\n")


def test_check_denial():
    result, failures = m.check_denial("tensorplate-agent", show())
    assert failures == [], failures
    assert result == {"unit": "tensorplate-agent.service", "denied": True,
                      "allowed_prefixes": ["127.0.0.1/32", "::1/128"]}, result

    # THE fail-open case: `systemctl show` answers for a unit that does
    # not exist, and answers with empty values. Read only for unexpected
    # allow entries, that is a unit nobody denied reading as denied.
    empty = show(deny="", allow="", load="not-found", active="inactive", invocation="")
    assert m.check_denial("tensorplate-agent", empty)[1] == [
        "allows_exactly_the_two_host_addresses", "denies_every_address",
        "unit_active", "unit_has_an_invocation", "unit_loaded",
    ], m.check_denial("tensorplate-agent", empty)[1]

    for text, expected in (
        (show(load="masked"), "unit_loaded"),
        (show(active="failed"), "unit_active"),
        (show(invocation=""), "unit_has_an_invocation"),
        (show(invocation="not-an-invocation-id"), "unit_has_an_invocation"),
        (show(deny=""), "denies_every_address"),
        (show(deny="0.0.0.0/0"), "denies_every_address"),
        # What `IPAddressAllow=localhost` reads back as. The resolver stub
        # sits inside 127.0.0.0/8, so an offline stage that accepted this
        # left DNS reachable.
        (show(allow="127.0.0.0/8 ::1/128"), "resolver_stub_is_not_allowed"),
        (show(allow="127.0.0.0/8 ::1/128"), "allows_exactly_the_two_host_addresses"),
        (show(allow="127.0.0.1/32"), "allows_exactly_the_two_host_addresses"),
        (show(allow="127.0.0.1/32 ::1/128 169.254.169.254/32"),
         "allows_exactly_the_two_host_addresses"),
        (show(allow="not-a-prefix"), "allow_entry_unparseable:not-a-prefix"),
    ):
        failures = m.check_denial("tensorplate-agent", text)[1]
        assert expected in failures, (expected, failures)

    assert ipaddress.ip_address(m.RESOLVER_STUB) in ipaddress.ip_network("127.0.0.0/8")
    assert ipaddress.ip_address(m.RESOLVER_STUB) not in ipaddress.ip_network("127.0.0.1/32")

    # systemd prints both lists from a hash set whose order changes with
    # each PID 1 start (src/core/dbus-cgroup.c, SET_FOREACH). The same
    # policy in the other order is the same policy, and is filed in one
    # canonical order so the evidence does not depend on the boot.
    for deny, allow in (("::/0 0.0.0.0/0", "127.0.0.1/32 ::1/128"),
                        ("0.0.0.0/0 ::/0", "::1/128 127.0.0.1/32"),
                        ("::/0 0.0.0.0/0", "::1/128 127.0.0.1/32")):
        result, failures = m.check_denial("tensorplate-agent", show(deny=deny, allow=allow))
        assert failures == [], (deny, allow, failures)
        assert result["allowed_prefixes"] == ["127.0.0.1/32", "::1/128"], result
    # The canonical order is the order the constants are written in, which
    # is what lets a readback compare equal to them.
    assert [str(p) for p in m.prefixes(" ".join(m.ALLOWED_PREFIXES))] == \
        list(m.ALLOWED_PREFIXES)
    assert [str(p) for p in m.prefixes(" ".join(m.DENY_ANY_PREFIXES))] == \
        list(m.DENY_ANY_PREFIXES)
    # Order-insensitive is not count-insensitive: a repeated entry is not
    # the two-entry set.
    for text, expected in (
        (show(deny="0.0.0.0/0 ::/0 0.0.0.0/0"), "denies_every_address"),
        (show(allow="::1/128 127.0.0.1/32 ::1/128"), "allows_exactly_the_two_host_addresses"),
    ):
        failures = m.check_denial("tensorplate-agent", text)[1]
        assert expected in failures, (expected, failures)
    # An unparseable entry sorts last and is still named.
    assert m.prefixes("zz ::1/128 127.0.0.1/32")[-1] == "zz"

    refused(lambda: m.parse_show("LoadState=loaded\nLoadState=masked\n"),
            "printed LoadState twice")
    refused(lambda: m.parse_show("Warning: unit not found\n"), "not a property")

    # THE second fail-open case: `systemctl show` answers with the unit's
    # LOADED configuration, so a drop-in that was installed and reloaded
    # but never restarted under reads back exactly like an enforced one.
    # The invocation id is the only property that says otherwise.
    before = "0" * 32
    assert m.check_denial("tensorplate-agent", show(invocation=before), before)[1] == [
        "restarted_under_the_denial"]
    assert m.check_denial("tensorplate-agent", show(invocation="1" * 32), before)[1] == []
    passed("denial readback refuses a unit that is not running")


def test_check_no_denial():
    result, failures = m.check_no_denial("tensorplate-agent", show(deny="", allow=""))
    assert failures == [] and result["denied"] is False, (result, failures)
    assert m.check_no_denial("tensorplate-agent", show())[1] == [
        "allow_list_is_empty_again", "deny_list_is_empty_again"]
    # A dead unit answers empty too, so "no policy" needs a live unit as
    # much as "denied" does.
    assert "unit_loaded" in m.check_no_denial(
        "tensorplate-agent", show(deny="", allow="", load="not-found"))[1]
    # Removing the drop-in and reloading empties the unit's loaded policy
    # whether or not anything restarted, so the restore side needs the
    # invocation comparison as much as the denial side does.
    before = "0" * 32
    assert m.check_no_denial(
        "tensorplate-agent", show(deny="", allow="", invocation=before), before)[1] == [
            "restarted_without_the_denial"]
    passed("restored readback refuses a unit that is not running")


def test_check_policy():
    shows = [("tensorplate-agent", show()), ("tensorplate-observability", show())]
    result, failures = m.check_policy("denied", shows)
    assert failures == [], failures
    assert result == {"units": ["tensorplate-agent.service",
                                "tensorplate-observability.service"],
                      "denied": True, "persistent_drop_ins_found": 0,
                      "allowed_prefixes": ["127.0.0.1/32", "::1/128"]}, result
    assert m.check_policy("denied", [])[1] == ["units_read"]
    # The prefixes in the filed result are the ones systemd reported, so
    # the evidence document states the policy the host was under rather
    # than the one this module asked for.
    mixed = [("tensorplate-agent", show()),
             ("tensorplate-observability", show(allow="127.0.0.1/32"))]
    assert "both_units_allow_the_same_addresses" in m.check_policy("denied", mixed)[1]
    # Two units printing the same set in different orders allow the same
    # addresses.
    reordered = [("tensorplate-agent", show()),
                 ("tensorplate-observability",
                  show(deny="::/0 0.0.0.0/0", allow="::1/128 127.0.0.1/32"))]
    result, failures = m.check_policy("denied", reordered)
    assert failures == [], failures
    assert result["allowed_prefixes"] == ["127.0.0.1/32", "::1/128"], result

    # The invocation comparison, over both units at once. A unit named in
    # `previous` whose policy was never read back leaves the claim
    # unmade, which is the shape the comparison exists to refuse.
    before = "0" * 32
    restarted = [("tensorplate-agent", show(invocation="1" * 32)),
                 ("tensorplate-observability", show(invocation="2" * 32))]
    previous = [("tensorplate-agent", before), ("tensorplate-observability", before)]
    result, failures = m.check_policy("denied", restarted, previous=previous)
    assert failures == [], failures
    assert result["units_restarted"] == ["tensorplate-agent.service",
                                         "tensorplate-observability.service"], result
    stale = [("tensorplate-agent", show(invocation=before)),
             ("tensorplate-observability", show(invocation="2" * 32))]
    assert m.check_policy("denied", stale, previous=previous)[1] == [
        "tensorplate-agent.service:restarted_under_the_denial"]
    assert "tensorplate-observability.service:read_back" in m.check_policy(
        "denied", restarted[:1], previous=previous)[1]
    # One unit denied and the other not is not a denied appliance.
    half = [("tensorplate-agent", show()),
            ("tensorplate-observability", show(deny="", allow=""))]
    assert "tensorplate-observability.service:denies_every_address" in \
        m.check_policy("denied", half)[1]

    with tempfile.TemporaryDirectory() as root:
        runtime = pathlib.Path(root) / "run/systemd/system/tensorplate-agent.service.d"
        runtime.mkdir(parents=True)
        gone = runtime / m.DROP_IN_NAME
        saved = m.RUNTIME_UNIT_DIR, m.PERSISTENT_UNIT_DIR
        m.RUNTIME_UNIT_DIR = os.path.join(root, "run/systemd/system")
        m.PERSISTENT_UNIT_DIR = os.path.join(root, "etc/systemd/system")
        try:
            clean = [("tensorplate-agent", show(deny="", allow=""))]
            result, failures = m.check_policy("none", clean, [str(gone)])
            assert failures == [] and result["drop_ins_removed"] == 1, (result, failures)
            assert result["persistent_drop_ins_found"] == 0, result
            # Still there: the host is left denied and the run says so.
            gone.write_text(m.DROP_IN_TEXT)
            assert m.check_policy("none", clean, [str(gone)])[1] == [
                "drop_in_removed:tensorplate-agent.service.d"]
            gone.unlink()
            # A dangling symlink is a file that is still there.
            gone.symlink_to("/nowhere")
            assert m.check_policy("none", clean, [str(gone)])[1] == [
                "drop_in_removed:tensorplate-agent.service.d"]
            gone.unlink()
            # Removing something that was never a runtime unit file is not
            # this run cleaning up after itself.
            elsewhere = os.path.join(root, "etc/systemd/system/x.service.d", m.DROP_IN_NAME)
            assert m.check_policy("none", clean, [elsewhere])[1] == [
                "removed_path_is_a_runtime_unit_file:x.service.d"]
            # A persistent drop-in outlives the run whatever the runtime
            # one did, so it fails while denied as well as after cleanup.
            persistent = pathlib.Path(m.PERSISTENT_UNIT_DIR) / "tensorplate-agent.service.d"
            persistent.mkdir(parents=True)
            (persistent / m.DROP_IN_NAME).write_text(m.DROP_IN_TEXT)
            assert "tensorplate-agent.service:no_persistent_drop_in" in \
                m.check_policy("none", clean, [str(gone)])[1]
            found = m.check_policy("none", clean, [str(gone)])[0]
            # Counted from the filesystem, so the certificate's
            # "persistent_unit_files_written" is a readback rather than a
            # restated zero.
            assert found["persistent_drop_ins_found"] == 1, found
            assert "tensorplate-agent.service:no_persistent_drop_in" in \
                m.check_policy("denied", [("tensorplate-agent", show())])[1]
        finally:
            m.RUNTIME_UNIT_DIR, m.PERSISTENT_UNIT_DIR = saved
    passed("policy readback counts removals and refuses persistent drop-ins")


# --- classification -----------------------------------------------------


def outcomes(denied, allowed, scope="transient"):
    denied_names, allowed_names = m.SCOPES[scope]
    return {"denied": dict.fromkeys(denied_names, denied),
            "allowed": dict.fromkeys(allowed_names, allowed)}


def kernel_probe(scope="transient", refusal="EPERM", silence="timeout"):
    """What a Linux kernel answers under the denial: every datagram
    refused outright, every TCP connect left waiting for a SYN the filter
    dropped (net/ipv4/tcp_output.c, tcp_connect, reports only
    -ECONNREFUSED)."""
    probe = outcomes(refusal, "ok", scope)
    for name in m.TCP_OPERATIONS:
        if name in probe["denied"]:
            probe["denied"][name] = silence
    return probe


def verdict(probe, control, metadata="required", scope="transient"):
    return m.classify(probe, control, metadata, scope)[1]


UDP_DENIED = sorted(name for name in m.DENIED_NAMES if name not in m.TCP_OPERATIONS)


def test_classify():
    control = outcomes("ok", "ok")
    probe = kernel_probe()
    result, failures = m.classify(probe, control)
    assert failures == [], failures
    assert result["operations_refused_under_the_denial"] == UDP_DENIED, result
    assert result["operations_silenced_under_the_denial"] == sorted(m.TCP_OPERATIONS), result
    assert result["operations_this_host_cannot_send"] == [], result
    assert result["scope"] == "transient", result
    # The metadata address is refused outright, as a datagram, whatever
    # its TCP connect could show.
    assert m.METADATA_UDP_OPERATION in result["operations_refused_under_the_denial"]
    # EACCES is the same class of answer; no routing failure produces it.
    assert verdict(kernel_probe(refusal="EACCES"), control) == []
    # ETIMEDOUT is what a connect that outlived the kernel's own retries
    # would say; it is the same silence.
    assert verdict(kernel_probe(silence="ETIMEDOUT"), control) == []
    # A refused connect, where some kernel reports one, is a refusal.
    result, failures = m.classify(outcomes("EPERM", "ok"), control)
    assert failures == [] and result["operations_silenced_under_the_denial"] == [], \
        (result, failures)

    # A denial that did nothing. `IPAddressDeny=` is silently inert where
    # systemd cannot install its filter, so this is the outcome the
    # property readback alone would have missed.
    assert verdict(outcomes("ok", "ok"), control) == \
        sorted(f"refused:{name}" for name in m.DENIED_NAMES)
    # A routing failure is not a refusal.
    assert "refused:udp_gce_metadata" in verdict(outcomes("ENETUNREACH", "ok"), control)
    # Nor is a datagram that timed out: only a TCP connect can be silenced.
    assert "refused:udp_resolver_stub" in verdict(kernel_probe(refusal="timeout"), control)
    # A connect answered under the denial -- accepted, or reset by a host
    # with no listener -- means packets flowed.
    for leaked in ("ok", "ECONNREFUSED"):
        leaking = kernel_probe()
        leaking["denied"]["tcp_resolver_stub"] = leaked
        assert verdict(leaking, control) == ["refused:tcp_resolver_stub"], leaked

    # A TCP timeout is attributable only against a control whose same
    # connect was answered. One that timed out with nothing denied says
    # nothing about the denial. A resolver stub with no TCP listener
    # answers with a reset, which is an answer.
    slow = outcomes("ok", "ok")
    slow["denied"]["tcp_resolver_stub"] = "timeout"
    assert verdict(kernel_probe(), slow) == ["control_completed:tcp_resolver_stub"]
    # Whatever the probe says: a refusal is no more attributable than a
    # silence to a connect nobody answered.
    refusing = kernel_probe()
    refusing["denied"]["tcp_resolver_stub"] = "EPERM"
    assert verdict(refusing, slow) == ["control_completed:tcp_resolver_stub"]
    reset = outcomes("ok", "ok")
    reset["denied"]["tcp_resolver_stub"] = "ECONNREFUSED"
    assert verdict(kernel_probe(), reset) == []

    # A control has to have COMPLETED its operation, not merely not been
    # refused. The case a review reproduced: a child that exited 3 sent
    # nothing, so the child's later refusal was certified against a
    # baseline that established nothing. Every outcome that is not a send
    # or a connect that happened is named, in both scopes, for the child
    # and for a datagram the probe process sends itself.
    not_completed = ("child_exit_3", "child_exit_1", "child_not_run_TimeoutExpired",
                     "child_not_run_FileNotFoundError", "RuntimeError", "OSError",
                     "timeout", "ETIMEDOUT", "ECONNREFUSED", "", "EPERM\n", "OK",
                     "something this module never prints")
    for scope in m.SCOPES:
        for name in ("udp_test_net_v4_from_child", "udp_test_net_v4"):
            for value in not_completed:
                failed = outcomes("ok", "ok", scope)
                failed["denied"][name] = value
                assert verdict(kernel_probe(scope), failed, scope=scope) == [
                    "control_completed:" + name], (scope, name, value)
            # Not a string at all.
            for value in (None, 0, ["ok"], {"ok": True}):
                failed = outcomes("ok", "ok", scope)
                failed["denied"][name] = value
                assert verdict(kernel_probe(scope), failed, scope=scope) == [
                    "control_completed:" + name], (scope, name, value)
            # Missing from the control altogether.
            failed = outcomes("ok", "ok", scope)
            del failed["denied"][name]
            assert verdict(kernel_probe(scope), failed, scope=scope) == [
                "control_completed:" + name], (scope, name)
        # The denied child failing is not a refusal either, whatever it
        # printed on the way out: only a child that ran and was refused is.
        for value in ("child_exit_3", "child_not_run_TimeoutExpired", "RuntimeError", "ok"):
            probe_child = kernel_probe(scope)
            probe_child["denied"]["udp_test_net_v4_from_child"] = value
            assert verdict(probe_child, outcomes("ok", "ok", scope), scope=scope) == [
                "refused:udp_test_net_v4_from_child"], (scope, value)
        # Both children failing: each half is named.
        both = kernel_probe(scope)
        both["denied"]["udp_test_net_v4_from_child"] = "child_exit_3"
        failed = outcomes("ok", "ok", scope)
        failed["denied"]["udp_test_net_v4_from_child"] = "child_exit_3"
        assert verdict(both, failed, scope=scope) == [
            "control_completed:udp_test_net_v4_from_child",
            "refused:udp_test_net_v4_from_child"], scope
    # A connect is complete when it was answered, and only then.
    assert m.completed_outcomes("tcp_resolver_stub") == m.ANSWERED
    assert m.completed_outcomes(m.METADATA_OPERATION) == m.ANSWERED
    for name in m.DENIED_NAMES:
        if name not in m.TCP_OPERATIONS:
            assert m.completed_outcomes(name) == ("ok",), name

    # A control refused by something else on the host -- an application
    # firewall, a missing route -- makes the probe's refusal unattributable.
    refused_control = outcomes("EPERM", "ok")
    failures = verdict(probe, refused_control)
    assert "control_metadata_service_reachable:tcp_gce_metadata" in failures
    assert "control_metadata_service_reachable:udp_gce_metadata" in failures
    assert "control_not_refused:udp_test_net_v4" in failures
    # The metadata controls must answer outright, not merely not be
    # refused: the stage's claim is that the denial is what made the
    # service unreachable.
    unreachable = outcomes("ok", "ok")
    unreachable["denied"][m.METADATA_UDP_OPERATION] = "EHOSTUNREACH"
    assert verdict(probe, unreachable) == [
        "control_metadata_service_reachable:" + m.METADATA_UDP_OPERATION]
    # The connect that never reached the service cannot be silenced by the
    # denial either, and is not excused as an operation this host cannot
    # send: the metadata service is the one it must reach.
    for value in ("EHOSTUNREACH", "timeout", "ECONNREFUSED", "child_exit_3"):
        unreachable = outcomes("ok", "ok")
        unreachable["denied"][m.METADATA_OPERATION] = value
        assert verdict(probe, unreachable) == [
            "control_metadata_service_reachable:" + m.METADATA_OPERATION], value

    # An operation the host cannot perform with nothing denied. On Linux
    # the cgroup egress filter runs after the route lookup, so an
    # IPv4-only host -- the default Compute Engine VPC -- answers every
    # global IPv6 destination ENETUNREACH with and without the drop-in.
    # That operation refuses nothing and proves nothing; it is named,
    # and blaming the denial for it would fail the stage on a stock VM.
    ipv4_only_control = outcomes("ok", "ok")
    ipv4_only_control["denied"]["udp_documentation_v6"] = "ENETUNREACH"
    ipv4_only_probe = kernel_probe()
    ipv4_only_probe["denied"]["udp_documentation_v6"] = "ENETUNREACH"
    result, failures = m.classify(ipv4_only_probe, ipv4_only_control)
    assert failures == [], failures
    assert result["operations_this_host_cannot_send"] == ["udp_documentation_v6"], result
    assert "udp_documentation_v6" not in result["operations_refused_under_the_denial"]
    # The one thing such an operation can still show: the denial did not
    # make it start working.
    worked = json.loads(json.dumps(ipv4_only_probe))
    worked["denied"]["udp_documentation_v6"] = "ok"
    assert verdict(worked, ipv4_only_control) == [
        "probe_matches_the_unroutable_control:udp_documentation_v6"]
    # Every other operation still has to be refused.
    assert "refused:udp_test_net_v4" in verdict(
        outcomes("ok", "ok"), ipv4_only_control)
    # Loopback is routable on every host, and these are the operations
    # that show the `localhost` shorthand was not used: an unroutable
    # control there is a broken host, never an excuse.
    for name in m.ALWAYS_ROUTABLE:
        broken = outcomes("ok", "ok")
        broken["denied"][name] = "ENETUNREACH"
        assert "control_routable:" + name in verdict(kernel_probe(), broken), name

    # A row with no metadata service drives the same mechanism with the
    # operations omitted -- and omitted on purpose, not merely missing.
    jetson_control = outcomes("ok", "ok")
    jetson_probe = kernel_probe()
    for name in m.METADATA_OPERATIONS:
        del jetson_control["denied"][name]
        del jetson_probe["denied"][name]
    assert verdict(jetson_probe, jetson_control, "absent") == []
    assert verdict(probe, control, "absent") == [
        "metadata_operation_not_probed:" + name for name in m.METADATA_OPERATIONS]
    only_udp = json.loads(json.dumps(jetson_probe))
    only_udp["denied"][m.METADATA_UDP_OPERATION] = "EPERM"
    assert verdict(only_udp, jetson_control, "absent") == [
        "metadata_operation_not_probed:" + m.METADATA_UDP_OPERATION]
    assert "control_metadata_service_reachable:tcp_gce_metadata" in \
        verdict(jetson_probe, jetson_control)
    # The case a review found certified: no metadata operation, and every
    # other control unroutable. Nothing was refused, so nothing is
    # certified.
    nothing_routes = {"denied": {name: "ENETUNREACH" for name in m.DENIED_NAMES
                                 if name not in m.METADATA_OPERATIONS},
                      "allowed": dict.fromkeys(m.ALLOWED_NAMES, "ok")}
    failures = verdict(json.loads(json.dumps(nothing_routes)), nothing_routes, "absent")
    assert failures == sorted("control_routable:" + name for name in m.ALWAYS_ROUTABLE) + \
        sorted("refused:" + name for name in m.ALWAYS_ROUTABLE), failures

    # Loopback and the agent socket have to keep working.
    blocked = kernel_probe()
    blocked["allowed"] = dict.fromkeys(m.ALLOWED_NAMES, "EPERM")
    assert verdict(blocked, control) == \
        sorted(f"allowed:{name}" for name in m.ALLOWED_NAMES)
    assert "control_allowed:unix_agent_socket" in verdict(probe, outcomes("ok", "EPERM"))

    # A probe that did not report an operation is a failure, never a skip:
    # a probe whose failure is swallowed certifies nothing.
    missing = kernel_probe()
    del missing["denied"]["udp_resolver_stub"]
    del missing["allowed"]["tcp_loopback_serving_port"]
    failures = verdict(missing, control)
    assert "refused:udp_resolver_stub" in failures
    assert "allowed:tcp_loopback_serving_port" in failures
    assert verdict({}, {}) != []
    assert verdict({"denied": "not a mapping"}, control) != []

    # Inside a service's control group: the datagrams only, with the
    # same rules.
    unit_control = outcomes("ok", "ok", "unit")
    unit_probe = kernel_probe("unit")
    result, failures = m.classify(unit_probe, unit_control, scope="unit")
    assert failures == [], failures
    assert result["scope"] == "unit"
    assert result["operations_refused_under_the_denial"] == sorted(m.UNIT_DENIED_NAMES)
    assert not any(name.startswith(("tcp_", "unix_")) for name in
                   m.UNIT_DENIED_NAMES + m.UNIT_ALLOWED_NAMES)
    assert m.METADATA_UDP_OPERATION in m.UNIT_DENIED_NAMES
    # A service whose filter never attached: configured, restarted, and
    # sending freely.
    assert verdict(outcomes("ok", "ok", "unit"), unit_control, scope="unit") == \
        sorted(f"refused:{name}" for name in m.UNIT_DENIED_NAMES)
    # The transient scope's operations are not required of a unit probe,
    # and a transient probe cannot stand in for a unit's.
    assert verdict(probe, control, scope="unit") == []
    assert "refused:tcp_gce_metadata" in verdict(unit_probe, control)
    passed("classification needs a completed control and a refused probe")


def test_probe_outcomes():
    # attempt() names every outcome rather than letting one disappear.
    import errno
    import socket

    def raises(error):
        def operation():
            raise error
        return operation

    assert m.attempt(lambda: None) == "ok"
    assert m.attempt(lambda: "already named") == "already named"
    assert m.attempt(raises(OSError(errno.EPERM, "denied"))) == "EPERM"
    assert m.attempt(raises(OSError(errno.ENETUNREACH, "no route"))) == "ENETUNREACH"
    assert m.attempt(raises(socket.timeout())) == "timeout"
    assert m.attempt(raises(RuntimeError("boom"))) == "RuntimeError"
    # An OSError with no errno still gets a name rather than passing as ok.
    assert m.attempt(raises(OSError())) == "OSError"

    names = [name for name, _ in m.denied_operations()]
    assert names == list(m.DENIED_NAMES), names
    # A row with no metadata service probes everything else.
    without = [name for name, _ in m.denied_operations("")]
    assert without == [name for name in m.DENIED_NAMES
                       if name not in m.METADATA_OPERATIONS], without
    allowed = [name for name, _ in m.allowed_operations("/run/agent.sock", 18080)]
    assert allowed == list(m.ALLOWED_NAMES), allowed
    # The resolver stub is probed on both protocols, because that address
    # is exactly what the `localhost` shorthand would have admitted.
    assert m.RESOLVER_STUB == "127.0.0.53"
    assert "udp_test_net_v4_from_child" in m.DENIED_NAMES
    assert set(m.TCP_OPERATIONS) == {name for name in m.DENIED_NAMES
                                     if name.startswith("tcp_")}

    # The probe run inside a service's control group sends the datagrams
    # and nothing else, and says which operations it ran by name.
    sent = []
    saved = m.udp_send, m.tcp_connect, m.unix_connect, m.child_udp_send
    try:
        m.udp_send = lambda host, port, family=None: sent.append(host)
        m.tcp_connect = m.unix_connect = None  # called, they would raise
        m.child_udp_send = lambda: "EPERM"
        document = m.run_unit_probe()
    finally:
        m.udp_send, m.tcp_connect, m.unix_connect, m.child_udp_send = saved
    assert sorted(document["denied"]) == sorted(m.UNIT_DENIED_NAMES), document
    assert sorted(document["allowed"]) == sorted(m.UNIT_ALLOWED_NAMES), document
    assert m.METADATA_ADDRESS in sent and m.RESOLVER_STUB in sent, sent

    # A child that never ran sent nothing, so its silence is not EPERM.
    saved = sys.executable
    try:
        sys.executable = "/nonexistent/python3"
        assert m.child_udp_send().startswith("child_not_run_")
        # Nor is a child that printed EPERM and then failed: its exit
        # status is the outcome, not whatever it said on the way out.
        with tempfile.TemporaryDirectory() as work:
            lying = pathlib.Path(work) / "python3"
            lying.write_text("#!/bin/sh\necho EPERM\nexit 3\n")
            lying.chmod(0o755)
            sys.executable = str(lying)
            assert m.child_udp_send() == "child_exit_3", m.child_udp_send()
    finally:
        sys.executable = saved
    passed("probe outcomes are named, including a child that never ran")


class FakeCredentials:
    """os, as far as drop_privileges uses it, for a process that is root
    until told otherwise. The real calls would change the credentials of
    the process running these tests."""

    def __init__(self, euid=0, ignore_setuid=False):
        self.uid = self.euid = euid
        self.gid = self.egid = 0
        self.groups = [0, 4]
        self.ignore_setuid = ignore_setuid
        self.calls = []

    def geteuid(self):
        return self.euid

    def getuid(self):
        return self.uid

    def getgid(self):
        return self.gid

    def getegid(self):
        return self.egid

    def setgroups(self, groups):
        self.calls.append("setgroups")
        self.groups = list(groups)

    def setgid(self, gid):
        self.calls.append("setgid")
        self.gid = self.egid = gid

    def setuid(self, uid):
        self.calls.append("setuid")
        if not self.ignore_setuid:
            self.uid = self.euid = uid


def test_unit_probe_mechanics():
    agent = "/system.slice/tensorplate-agent.service"
    with tempfile.TemporaryDirectory() as root:
        root = pathlib.Path(root)
        group = root / "cgroup" / agent.lstrip("/")
        group.mkdir(parents=True)
        (group / "cgroup.procs").write_text("")
        saved = m.CGROUP_ROOT, m.PROC_SELF_CGROUP, m.MOUNT_TABLE
        try:
            m.CGROUP_ROOT = str(root / "cgroup")
            # Only a path that ends in exactly this unit, with nothing
            # that could climb out of it, is a place this process may be
            # moved to. The empty value is systemd's answer for a unit
            # with no running instance.
            assert m.control_group_path("tensorplate-agent", agent) == str(group)
            for unit, bad in (
                ("tensorplate-agent", ""),
                ("tensorplate-agent", "system.slice/tensorplate-agent.service"),
                ("tensorplate-agent", "/system.slice/../tensorplate-agent.service"),
                ("tensorplate-agent", "/system.slice/./tensorplate-agent.service"),
                ("tensorplate-agent", "/system.slice//tensorplate-agent.service"),
                ("tensorplate-agent", "/system.slice/tensorplate-agent.service/"),
                ("tensorplate-agent", "/system.slice/tensorplate-observability.service"),
                ("tensorplate-agent", "/"),
            ):
                refused(lambda: m.control_group_path(unit, bad), "is not the control group")

            # Joining writes this pid, and is read back from the unified
            # line of /proc/self/cgroup.
            proc = root / "cgroup-self"
            m.PROC_SELF_CGROUP = str(proc)
            proc.write_text("0::" + agent + "\n")
            m.join_control_group(agent, str(group))
            assert (group / "cgroup.procs").read_text() == f"{os.getpid()}\n"
            # A write the kernel took and a process that is somewhere else.
            for elsewhere in ("0::/user.slice/session-1.scope\n",
                              "12:pids:" + agent + "\n", ""):
                proc.write_text(elsewhere)
                refused(lambda: m.join_control_group(agent, str(group)),
                        "did not join")
            refused(lambda: m.join_control_group(agent, str(root / "absent")),
                    "cannot join")

            # The mount point comes from the mount table when nothing
            # overrides it, and no unified hierarchy is a refusal.
            m.CGROUP_ROOT = ""
            table = root / "mountinfo"
            m.MOUNT_TABLE = str(table)
            table.write_text(
                "22 1 0:21 / /proc rw,nosuid shared:12 - proc proc rw\n"
                "30 25 0:26 / /sys/fs/cgroup rw,nosuid shared:9 - cgroup2 cgroup2 "
                "rw,nsdelegate\n")
            assert m.cgroup2_mount() == "/sys/fs/cgroup"
            table.write_text("22 1 0:21 / /proc rw,nosuid shared:12 - proc proc rw\n"
                             "31 25 0:27 / /sys/fs/cgroup/cpu rw - cgroup cgroup rw,cpu\n")
            refused(m.cgroup2_mount, "no cgroup2 hierarchy")
            m.MOUNT_TABLE = str(root / "absent-mountinfo")
            refused(m.cgroup2_mount, "cannot read the mount table")
        finally:
            m.CGROUP_ROOT, m.PROC_SELF_CGROUP, m.MOUNT_TABLE = saved

    # Probing does not need root and is not left running as it.
    ops = FakeCredentials()
    m.drop_privileges(1000, 1000, ops)
    assert (ops.uid, ops.euid, ops.gid, ops.egid, ops.groups) == (1000, 1000, 1000, 1000, [])
    # Supplementary groups first, then the group, then the user: after
    # setuid nothing else can be dropped.
    assert ops.calls == ["setgroups", "setgid", "setuid"], ops.calls
    refused(lambda: m.drop_privileges(0, 1000, FakeCredentials()), "refusing to probe as root")
    refused(lambda: m.drop_privileges(1000, 0, FakeCredentials()), "refusing to probe as root")
    refused(lambda: m.drop_privileges(1000, 1000, FakeCredentials(euid=1000)), "needs root")
    refused(lambda: m.drop_privileges(1000, 1000, FakeCredentials(ignore_setuid=True)),
            "did not drop")

    assert m.unit_evidence_name("probe", "tensorplate-agent") == \
        "offline-unit-probe-tensorplate-agent.service.json"
    passed("a unit probe joins only that unit's control group, then drops root")


# --- what the appliance answered ----------------------------------------


def doctor_document(**overrides):
    findings = {
        "host_os": "ubuntu 24.04 on g2-standard-8 (" + m.DOCTOR_RECORDED_PHRASE + ")",
        "platform_row": f"resolves to support row `{ROW}`",
    }
    findings.update(overrides.pop("messages", {}))
    statuses = {name: "ok" for name in m.DOCTOR_FINDINGS_OK}
    statuses["platform_row"] = "ok"
    statuses.update(overrides.pop("statuses", {}))
    payload = {"failing": 0, "findings": [
        {"id": name, "status": statuses.get(name, "ok"),
         "message": findings.get(name, "ok")}
        for name in set(list(statuses) + list(findings))]}
    payload.update(overrides)
    return {"command": "doctor", "payload": payload}


def test_doctor_check():
    result, failures = m.doctor_check(doctor_document(), 0, ROW)
    assert failures == [], failures
    # What was required and what was forbidden, as the row supplied them.
    assert result == {"doctor": "pass", "platform_row": ROW,
                      "host_os_phrase_required": m.DOCTOR_RECORDED_PHRASE,
                      "host_os_phrase_forbidden": m.DOCTOR_LIVE_PHRASE}, result
    for kwargs, status, expected in (
        ({}, 10, "doctor_exit_status"),
        ({"failing": 2}, 0, "doctor_no_failing_findings"),
        ({"statuses": {"platform_row": "warning"}}, 0, "platform_row_ok"),
        ({"statuses": {"agent_reachable": "fail"}}, 0, "agent_reachable_ok"),
        ({"messages": {"platform_row": "resolves to support row `another-row`"}}, 0,
         "platform_row_exact"),
        # Doctor saying the shape came from a live answer means the denial
        # let the metadata query through, whatever the probe said.
        ({"messages": {"host_os": "ubuntu 24.04 on g2-standard-8 "
                                  + m.DOCTOR_LIVE_PHRASE}}, 0,
         "host_os_machine_type_not_from_live_metadata"),
        ({"messages": {"host_os": "ubuntu 24.04"}}, 0,
         "host_os_machine_type_from_the_record"),
    ):
        failures = m.doctor_check(doctor_document(**kwargs), status, ROW)[1]
        assert expected in failures, (expected, failures)
    assert "doctor_command" in m.doctor_check({"command": "status"}, 0, ROW)[1]

    # A row whose doctor says something else drives the same check with
    # its own tokens, rather than editing this file. An empty forbidden
    # phrase forbids nothing, which is what a row with no live-metadata
    # spelling needs.
    jetson = doctor_document(messages={
        "host_os": "ubuntu 22.04 on NVIDIA Jetson Orin Nano",
        "platform_row": "resolves to support row `jetson-orin-nano-8gb`"})
    result, failures = m.doctor_check(jetson, 0, "jetson-orin-nano-8gb",
                                      "NVIDIA Jetson Orin Nano", "")
    assert failures == [], failures
    assert result["host_os_phrase_required"] == "NVIDIA Jetson Orin Nano", result
    assert result["host_os_phrase_forbidden"] is None, result
    assert "host_os_machine_type_from_the_record" in m.doctor_check(
        jetson, 0, "jetson-orin-nano-8gb")[1]
    # A row that requires no phrase at all is filed as having checked
    # none, never as a machine type recorded from anywhere.
    result, failures = m.doctor_check(jetson, 0, "jetson-orin-nano-8gb", "", "")
    assert failures == [], failures
    assert result == {"doctor": "pass", "platform_row": "jetson-orin-nano-8gb",
                      "host_os_phrase_required": None,
                      "host_os_phrase_forbidden": None}, result
    assert "recorded" not in json.dumps(result)
    passed("doctor must resolve the row from the recorded machine type")


def journal(*messages, invocation="0" * 32):
    return "".join(json.dumps({
        "MESSAGE": message, "_SYSTEMD_UNIT": "tensorplate-agent.service",
        "_SYSTEMD_INVOCATION_ID": invocation}) + "\n" for message in messages)


def test_identity_check():
    recorded = ("platform identity: machine_type=g2-standard-8 "
                "source=recorded_gce_metadata record=not_applicable")
    result, failures = m.identity_check(journal("fixture service started", recorded))
    assert failures == [], failures
    assert result == {"machine_type_source": "recorded_gce_metadata",
                      "record": "not_applicable"}, result

    live = ("platform identity: machine_type=g2-standard-8 "
            "source=gce_metadata record=written")
    failures = m.identity_check(journal(live))[1]
    assert "machine_type_from_the_record" in failures
    assert "metadata_service_was_not_reached" in failures
    # A record written while denied would mean the agent either reached
    # the service or recorded the record from itself.
    rewritten = recorded.replace("record=not_applicable", "record=written")
    assert m.identity_check(journal(rewritten))[1] == ["record_not_rewritten_while_denied"]
    undetected = ("platform identity: machine_type=none source=none "
                  "record=not_applicable")
    failures = m.identity_check(journal(undetected))[1]
    assert "machine_type_detected" in failures
    # Two lines means the agent restarted, and the earlier one may have
    # been the online start.
    assert "identity_logged_once" in m.identity_check(journal(recorded, recorded))[1]
    # None at all is not a pass.
    assert "identity_logged_once" in m.identity_check(journal("nothing here"))[1]
    assert "identity_line_parsed" in m.identity_check(journal("platform identity: garbled"))[1]
    refused(lambda: m.identity_check("not json\n"), "is not a JSON record")

    # A row whose agent establishes no machine type at all -- every
    # source in platform/src/machine_type_record.rs is a GCE one -- drives
    # the same check with its own expected tokens, and a detected machine
    # type is then not required either.
    assert m.identity_check(journal(undetected), expect_source="none",
                            expect_record="not_applicable", forbid_source="")[1] == []
    assert "machine_type_from_the_record" in m.identity_check(
        journal(recorded), expect_source="none")[1]
    passed("platform identity must be logged once, from the record")


def test_cli_documents():
    status = {"command": "status", "payload": {"severity": "ready", "agent": {
        "agent_state": "ready",
        "active": {"deployment_id": "d-offline", "backend": "python_pytorch",
                   "serving_url": "http://127.0.0.1:18080/infer"}}}}
    result, failures, port = m.status_check(status, "d-offline")
    assert failures == [] and port == 18080, (failures, port)
    off_host = json.loads(json.dumps(status))
    off_host["payload"]["agent"]["active"]["serving_url"] = "http://10.0.0.5:18080/infer"
    assert "serving_url_on_the_allowed_loopback_address" in m.status_check(off_host, "d-offline")[1]
    assert "active_deployment" in m.status_check(status, "other")[1]
    # Each field on its own, so no one of them can be dropped behind the
    # others: the document has to be a status, and a ready one.
    for path, value, expected in (
        (("command",), "doctor", "status_command"),
        (("payload", "severity"), "degraded", "status_severity_ready"),
        (("payload", "agent", "agent_state"), "starting", "agent_state_ready"),
    ):
        changed = json.loads(json.dumps(status))
        target = changed
        for key in path[:-1]:
            target = target[key]
        target[path[-1]] = value
        assert m.status_check(changed, "d-offline")[1] == [expected], (path, value)

    deploy = {"command": "deploy", "payload": {"phase": "active", "deployment_id": "d-offline"}}
    assert m.deploy_check(deploy, "d-offline")[1] == []
    assert "deployment_id" in m.deploy_check(deploy, "d")[1]
    assert m.deploy_check(dict(deploy, command="status"), "d-offline")[1] == ["deploy_command"]
    assert "deployment_phase_active" in m.deploy_check(
        {"command": "deploy", "payload": {"phase": "failed", "deployment_id": "d-offline"}},
        "d-offline")[1]

    request = m.infer_request("cloud-offline-1")
    echoed = {"outputs": [{"name": "echo_probe",
                           "tensor": dict(request["inputs"][0]["tensor"],
                                          byte_offset=0, byte_size=16),
                           "payload_b64": request["inputs"][0]["payload_b64"]}]}
    assert m.infer_check(request, echoed)[1] == []
    garbled = json.loads(json.dumps(echoed))
    garbled["outputs"][0]["payload_b64"] = "AAAA"
    assert m.infer_check(request, garbled)[1] == ["inference_preserved_the_payload"]
    assert m.infer_check(request, {"outputs": []})[1] == [
        "inference_echoed_the_input", "inference_preserved_the_payload"]
    passed("status, deploy and inference documents")


# --- the command line ---------------------------------------------------


def run(*args, **kwargs):
    return subprocess.run([sys.executable, str(MODULE)] + list(args),
                          capture_output=True, text=True, **kwargs)


def test_command_line():
    assert run("drop-in-text").stdout == m.DROP_IN_TEXT
    properties = run("transient-properties").stdout.split()
    assert properties == ["--property=" + value for value in m.DENIAL_PROPERTIES], properties
    # The transient units and the drop-in deny and allow the same thing.
    assert [value.split("=", 1)[1] for value in m.DENIAL_PROPERTIES
            if value.startswith("IPAddressAllow=")] == list(m.ALLOWED_PREFIXES)

    with tempfile.TemporaryDirectory() as work:
        work = pathlib.Path(work)
        (work / "show").write_text(show())
        out = work / "denial.json"
        result = run("check-policy", "--expect", "denied",
                     "--show", f"tensorplate-agent={work / 'show'}", "--out", str(out))
        assert result.returncode == 0, result.stderr
        assert json.loads(out.read_text())["denied"] is True

        # --was is the invocation the unit carried before the drop-in
        # went in, and the same value back means nothing restarted.
        result = run("check-policy", "--expect", "denied",
                     "--show", f"tensorplate-agent={work / 'show'}",
                     "--was", "tensorplate-agent=" + "0" * 32)
        assert result.returncode == 1, result.stdout
        assert "restarted_under_the_denial" in result.stderr, result.stderr
        result = run("check-policy", "--expect", "denied",
                     "--show", f"tensorplate-agent={work / 'show'}",
                     "--was", "tensorplate-agent=not-an-invocation")
        assert result.returncode == 2, result.stdout
        assert "not an invocation id" in result.stderr, result.stderr

        # A failing check exits non-zero AND writes nothing: a stage that
        # read the file afterwards must not find a stale pass.
        (work / "bad").write_text(show(allow="127.0.0.0/8 ::1/128"))
        missing = work / "not-written.json"
        result = run("check-policy", "--expect", "denied",
                     "--show", f"tensorplate-agent={work / 'bad'}", "--out", str(missing))
        assert result.returncode == 1, result.stdout
        assert "resolver_stub_is_not_allowed" in result.stderr, result.stderr
        assert not missing.exists()

        status = work / "status.json"
        status.write_text(json.dumps({"command": "status", "payload": {
            "severity": "ready", "agent": {"agent_state": "ready", "active": {
                "deployment_id": "d", "serving_url": "http://127.0.0.1:18080/infer"}}}}))
        assert run("serving-port", "--status", str(status)).stdout.strip() == "18080"
        # A port that exists but is not on the allowed loopback address is
        # not a port the probe may be pointed at.
        status.write_text(json.dumps({"command": "status", "payload": {
            "severity": "ready", "agent": {"agent_state": "ready", "active": {
                "deployment_id": "d", "serving_url": "http://10.0.0.5:18080/infer"}}}}))
        result = run("serving-port", "--status", str(status))
        assert result.returncode == 1 and "loopback" in result.stderr, (result.stdout,
                                                                         result.stderr)
        assert result.stdout.strip() == "", result.stdout
        status.write_text(json.dumps({"command": "status", "payload": {}}))
        result = run("serving-port", "--status", str(status))
        assert result.returncode == 1 and "loopback" in result.stderr, result.stderr
        result = run("serving-port", "--status", str(work / "absent.json"))
        assert result.returncode == 1 and "cannot read" in result.stderr, result.stderr

        # The evidence names the harness files the unit probes under.
        result = run("unit-evidence-name", "--kind", "control", "--unit", "tensorplate-agent")
        assert result.stdout.strip() == m.unit_evidence_name("control", "tensorplate-agent")

        # classify takes the scope; a unit's datagram-only documents pass
        # as a unit and fail as a transient probe.
        (work / "unit-probe.json").write_text(json.dumps(kernel_probe("unit")))
        (work / "unit-control.json").write_text(json.dumps(outcomes("ok", "ok", "unit")))
        classify = ["classify", "--probe", str(work / "unit-probe.json"),
                    "--control", str(work / "unit-control.json")]
        result = run(*classify, "--scope", "unit", "--out", str(work / "unit-class.json"))
        assert result.returncode == 0, result.stderr
        assert json.loads((work / "unit-class.json").read_text())["scope"] == "unit"
        result = run(*classify)
        assert result.returncode == 1 and "refused:tcp_gce_metadata" in result.stderr, \
            result.stderr

        # A unit probe refuses a control group that is not the unit's
        # before it moves anything: the process is never written into it.
        elsewhere = work / "cgroup" / "system.slice" / "sshd.service"
        elsewhere.mkdir(parents=True)
        (elsewhere / "cgroup.procs").write_text("")
        result = run("probe-unit", "--unit", "tensorplate-agent",
                     "--control-group", "/system.slice/sshd.service",
                     "--uid", "1000", "--gid", "1000",
                     env=dict(os.environ, TP_OFFLINE_CGROUP_ROOT=str(work / "cgroup")))
        assert result.returncode == 1 and "is not the control group" in result.stderr, \
            result.stderr
        assert (elsewhere / "cgroup.procs").read_text() == ""
    passed("subcommands report failure through their exit status")


UNITS = ["tensorplate-agent.service", "tensorplate-observability.service"]


def denial_document(**changes):
    document = {"units": UNITS, "denied": True, "persistent_drop_ins_found": 0,
                "allowed_prefixes": list(m.ALLOWED_PREFIXES), "units_restarted": UNITS}
    document.update(changes)
    return document


def restored_document(**changes):
    document = {"units": UNITS, "denied": False, "drop_ins_removed": 2,
                "persistent_drop_ins_found": 0, "units_restarted": UNITS}
    document.update(changes)
    return document


def evidence_directory(work, drop=(), **overrides):
    """A stage's filed results, as the harness writes them. `drop` names
    keys to delete from a document: `(file, key)`."""
    documents = {
        "offline-denial.json": denial_document(),
        "offline-restored.json": restored_document(),
        "offline-control.json": outcomes("ok", "ok"),
        "offline-probe.json": kernel_probe(),
        "offline-classification.json": {"metadata_operation": "required"},
        "offline-identity.json": {"machine_type_source": "recorded_gce_metadata",
                                  "record": "not_applicable"},
        "offline-status-check.json": {"deployment_id": "d-offline", "status": "pass"},
        "offline-doctor-check.json": {"doctor": "pass"},
        "offline-deploy-check.json": {"deploy": "pass"},
        "offline-infer-check.json": {"infer": "pass"},
    }
    for unit in UNITS:
        documents[m.unit_evidence_name("control", unit)] = outcomes("ok", "ok", "unit")
        documents[m.unit_evidence_name("probe", unit)] = kernel_probe("unit")
    documents.update(overrides)
    for name, key in drop:
        del documents[name][key]
    for name, document in documents.items():
        (work / name).write_text(json.dumps(document))
    return str(work)


def test_evidence():
    with tempfile.TemporaryDirectory() as work:
        result = m.evidence(evidence_directory(pathlib.Path(work)), "d-offline")
    # Read back from what systemd reported, not restated from the module's
    # own constants -- the certificate states the policy the host was
    # under.
    assert result["mechanism"]["allow"] == list(m.ALLOWED_PREFIXES)
    assert result["mechanism"]["resolver_stub_allowed"] is False
    assert result["mechanism"]["drop_in_scope"] == "runtime"
    assert result["units_denied"] == UNITS
    assert result["units_restarted_under_the_denial"] == UNITS
    assert result["enforced"] is True
    assert result["cli_under_denial"] == {"status": "pass", "doctor": "pass",
                                          "deploy": "pass", "infer": "pass"}
    assert result["classification"]["operations_refused_under_the_denial"] == UDP_DENIED
    assert result["classification"]["operations_silenced_under_the_denial"] == \
        sorted(m.TCP_OPERATIONS)
    # Both services were probed from inside their own control groups.
    per_unit = result["units_probed_in_their_own_control_group"]
    assert sorted(per_unit) == UNITS, per_unit
    for unit in UNITS:
        # The documents the verdict was derived from, as filed.
        assert per_unit[unit]["probe"] == kernel_probe("unit"), per_unit[unit]
        assert per_unit[unit]["control"] == outcomes("ok", "ok", "unit"), per_unit[unit]
        assert per_unit[unit]["classification"]["scope"] == "unit"
        assert per_unit[unit]["classification"]["operations_refused_under_the_denial"] == \
            sorted(m.UNIT_DENIED_NAMES)
    assert result["restore"]["drop_ins_removed"] == 2
    assert result["restore"]["persistent_unit_files_written"] == 0
    assert result["restore"]["units_restarted_without_the_denial"] == UNITS
    # Nothing in the evidence carries a path off the host or a process id.
    text = json.dumps(result)
    for forbidden in ("/run/systemd", "/etc/systemd", "pid", "system.slice", "cgroup/"):
        assert forbidden not in text, forbidden

    unit = UNITS[1]
    # Every verdict is derived, so a directory whose own documents
    # contradict the certificate is refused rather than certified.
    for overrides, drop, fragment in (
        # Nothing was denied, and the control was refused: the exact
        # input `classify` rejects.
        ({"offline-probe.json": outcomes("ok", "ok"),
          "offline-control.json": outcomes("EPERM", "ok")}, (), "do not classify"),
        # One service whose own filter never attached, while the
        # transient unit's did.
        ({m.unit_evidence_name("probe", unit): outcomes("ok", "ok", "unit")}, (),
         "do not classify"),
        # A control whose child never sent anything, on either scope.
        ({"offline-control.json": dict(outcomes("ok", "ok"), denied=dict(
            outcomes("ok", "ok")["denied"], udp_test_net_v4_from_child="child_exit_3"))},
         (), "control_completed:udp_test_net_v4_from_child"),
        ({m.unit_evidence_name("control", unit): dict(
            outcomes("ok", "ok", "unit"), denied=dict(
                outcomes("ok", "ok", "unit")["denied"],
                udp_test_net_v4_from_child="child_not_run_TimeoutExpired"))},
         (), "control_completed:udp_test_net_v4_from_child"),
        # A service probed against the transient control instead of its
        # own, or not probed at all.
        ({}, (), None),
        # The readback saw the shorthand's expansion, which admits the
        # resolver stub and the DNS namespace behind it.
        ({"offline-denial.json": denial_document(
            allowed_prefixes=["127.0.0.0/8", "::1/128"])}, (), "admits 127.0.0.53"),
        # Not the resolver, and still not the two host addresses.
        ({"offline-denial.json": denial_document(allowed_prefixes=["127.0.0.1/32"])}, (),
         "is not exactly"),
        ({"offline-denial.json": denial_document(
            allowed_prefixes=["127.0.0.1/32", "::1/128", "169.254.169.254/32"])}, (),
         "is not exactly"),
        ({"offline-denial.json": denial_document(allowed_prefixes=[])}, (),
         "carries no allow list"),
        ({"offline-denial.json": denial_document(allowed_prefixes=["not-a-prefix"])}, (),
         "unparseable"),
        ({"offline-denial.json": denial_document(allowed_prefixes=[2130706433])}, (),
         "unparseable"),
        # A readback that compared no invocation certifies the unit's
        # loaded configuration and nothing about the running service --
        # on either side of the stage.
        ({}, (("offline-denial.json", "units_restarted"),), "instance as replaced"),
        ({}, (("offline-restored.json", "units_restarted"),), "instance as replaced"),
        # Documents that say something other than what their names claim.
        ({"offline-denial.json": denial_document(denied=False)}, (),
         "does not record a denied appliance"),
        ({}, (("offline-denial.json", "denied"),), "does not record a denied appliance"),
        ({"offline-restored.json": restored_document(denied=True)}, (),
         "does not record a restored appliance"),
        ({}, (("offline-restored.json", "denied"),), "does not record a restored appliance"),
        # A restore of other units than the ones denied, or of none.
        ({"offline-restored.json": restored_document(
            units=UNITS[:1], units_restarted=UNITS[:1])}, (), "not the units denied"),
        ({"offline-denial.json": denial_document(units=[], units_restarted=[]),
          "offline-restored.json": restored_document(units=[], units_restarted=[])}, (),
         "not the units denied"),
        ({}, (("offline-denial.json", "units"),), "records no units"),
        # A persistent drop-in outlives the run, whichever side found it.
        ({"offline-restored.json": restored_document(persistent_drop_ins_found=1)}, (),
         "persistent drop-in"),
        ({"offline-denial.json": denial_document(persistent_drop_ins_found=2)}, (),
         "persistent drop-in"),
        ({}, (("offline-restored.json", "persistent_drop_ins_found"),),
         "records no persistent_drop_ins_found"),
        # A restore that read back fewer removals than there were drop-ins,
        # or never counted them.
        ({"offline-restored.json": restored_document(drop_ins_removed=0)}, (),
         "0 drop-in(s) as removed for 2"),
        ({"offline-restored.json": restored_document(drop_ins_removed=1)}, (),
         "1 drop-in(s) as removed for 2"),
        ({}, (("offline-restored.json", "drop_ins_removed"),),
         "records no drop_ins_removed"),
        # Not JSON objects at all.
        ({"offline-restored.json": ["not", "an", "object"]}, (), "is not a JSON object"),
    ):
        with tempfile.TemporaryDirectory() as work:
            directory = evidence_directory(pathlib.Path(work), drop, **overrides)
            if fragment is None:
                # The in-unit documents are required, not optional.
                for kind in ("control", "probe"):
                    os.unlink(os.path.join(directory, m.unit_evidence_name(kind, unit)))
                    refused(lambda: m.evidence(directory, "d-offline"), "cannot read")
                    (pathlib.Path(directory) / m.unit_evidence_name(kind, unit)).write_text(
                        json.dumps(kernel_probe("unit") if kind == "probe"
                                   else outcomes("ok", "ok", "unit")))
                continue
            refused(lambda: m.evidence(directory, "d-offline"), fragment)

    # A check that never passed filed no document, so its absence refuses
    # the certificate rather than being restated as a pass.
    for missing in ("offline-doctor-check.json", "offline-classification.json",
                    "offline-infer-check.json", "offline-identity.json"):
        with tempfile.TemporaryDirectory() as work:
            directory = evidence_directory(pathlib.Path(work))
            os.unlink(os.path.join(directory, missing))
            refused(lambda: m.evidence(directory, "d-offline"), "cannot read " + missing)
    # A filed document that records something other than a pass.
    with tempfile.TemporaryDirectory() as work:
        directory = evidence_directory(pathlib.Path(work),
                                       **{"offline-deploy-check.json": {"deploy": "fail"}})
        refused(lambda: m.evidence(directory, "d-offline"), "does not record a pass")
    # A row with no metadata service files `absent`, and its documents are
    # classified that way on both scopes.
    with tempfile.TemporaryDirectory() as work:
        without = {}
        for name, document in (("offline-control.json", outcomes("ok", "ok")),
                               ("offline-probe.json", kernel_probe())):
            for key in m.METADATA_OPERATIONS:
                del document["denied"][key]
            without[name] = document
        for unit_name in UNITS:
            for kind, document in (("control", outcomes("ok", "ok", "unit")),
                                   ("probe", kernel_probe("unit"))):
                del document["denied"][m.METADATA_UDP_OPERATION]
                without[m.unit_evidence_name(kind, unit_name)] = document
        without["offline-classification.json"] = {"metadata_operation": "absent"}
        directory = evidence_directory(pathlib.Path(work), **without)
        assert m.evidence(directory, "d")["classification"]["metadata_operation"] == "absent"
    passed("evidence is derived from the filed results, not restated")


def main():
    for test in (test_drop_in, test_check_denial, test_check_no_denial, test_check_policy,
                 test_classify, test_probe_outcomes, test_unit_probe_mechanics,
                 test_doctor_check, test_identity_check, test_cli_documents,
                 test_command_line, test_evidence):
        test()
    print("linux offline runtime: all checks pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
