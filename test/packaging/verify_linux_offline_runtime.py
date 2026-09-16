#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for tools/validation/linux_offline_runtime.py.

Run by verify_linux_offline_runtime.sh. The module is the mechanism both
systemd lifecycle harnesses use for their offline stage, so what it
refuses is what those stages refuse. Everything here is synthetic: no
unit file is written outside a temporary directory, no systemd command is
run, and no socket is opened.

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


def outcomes(denied, allowed):
    return {"denied": dict.fromkeys(m.DENIED_NAMES, denied),
            "allowed": dict.fromkeys(m.ALLOWED_NAMES, allowed)}


def verdict(probe, control, metadata="required"):
    return m.classify(probe, control, metadata)[1]


def test_classify():
    control = outcomes("ok", "ok")
    probe = outcomes("EPERM", "ok")
    result, failures = m.classify(probe, control)
    assert failures == [], failures
    assert result["operations_refused_under_the_denial"] == sorted(m.DENIED_NAMES), result
    assert result["operations_this_host_cannot_send"] == [], result
    # EACCES is the same class of answer; no routing failure produces it.
    assert verdict(outcomes("EACCES", "ok"), control) == []

    # A denial that did nothing. `IPAddressDeny=` is silently inert where
    # systemd cannot install its filter, so this is the outcome the
    # property readback alone would have missed.
    assert verdict(outcomes("ok", "ok"), control) == \
        sorted(f"refused:{name}" for name in m.DENIED_NAMES)
    # A routing failure is not a refusal.
    assert "refused:tcp_gce_metadata" in verdict(outcomes("ENETUNREACH", "ok"), control)

    # A control refused by something else on the host -- an application
    # firewall, a missing route -- makes the probe's EPERM unattributable.
    refused_control = outcomes("EPERM", "ok")
    failures = verdict(probe, refused_control)
    assert "control_metadata_service_reachable" in failures
    assert "control_not_refused:udp_test_net_v4" in failures
    # The metadata control must answer outright, not merely not be
    # refused: the stage's claim is that the denial is what made it
    # unreachable.
    unreachable = outcomes("ok", "ok")
    unreachable["denied"]["tcp_gce_metadata"] = "EHOSTUNREACH"
    assert verdict(probe, unreachable) == ["control_metadata_service_reachable"]

    # An operation the host cannot perform with nothing denied. On Linux
    # the cgroup egress filter runs after the route lookup, so an
    # IPv4-only host -- the default Compute Engine VPC -- answers every
    # global IPv6 destination ENETUNREACH with and without the drop-in.
    # That operation refuses nothing and proves nothing; it is named,
    # and blaming the denial for it would fail the stage on a stock VM.
    ipv4_only_control = outcomes("ok", "ok")
    ipv4_only_control["denied"]["udp_documentation_v6"] = "ENETUNREACH"
    ipv4_only_probe = outcomes("EPERM", "ok")
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

    # A row with no metadata service drives the same mechanism with the
    # operation omitted -- and omitted on purpose, not merely missing.
    jetson_control = outcomes("ok", "ok")
    jetson_probe = outcomes("EPERM", "ok")
    del jetson_control["denied"]["tcp_gce_metadata"]
    del jetson_probe["denied"]["tcp_gce_metadata"]
    assert verdict(jetson_probe, jetson_control, "absent") == []
    assert verdict(probe, control, "absent") == ["metadata_operation_not_probed"]
    assert "control_metadata_service_reachable" in verdict(jetson_probe, jetson_control)

    # Loopback and the agent socket have to keep working.
    blocked = outcomes("EPERM", "EPERM")
    assert verdict(blocked, control) == \
        sorted(f"allowed:{name}" for name in m.ALLOWED_NAMES)
    assert "control_allowed:unix_agent_socket" in verdict(probe, outcomes("ok", "EPERM"))

    # A probe that did not report an operation is a failure, never a skip:
    # a probe whose failure is swallowed certifies nothing.
    missing = outcomes("EPERM", "ok")
    del missing["denied"]["udp_resolver_stub"]
    del missing["allowed"]["tcp_loopback_serving_port"]
    failures = verdict(missing, control)
    assert "refused:udp_resolver_stub" in failures
    assert "allowed:tcp_loopback_serving_port" in failures
    assert verdict({}, {}) != []
    assert verdict({"denied": "not a mapping"}, control) != []
    passed("classification needs an unrefused control and a refused probe")


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
                       if name != m.METADATA_OPERATION], without
    allowed = [name for name, _ in m.allowed_operations("/run/agent.sock", 18080)]
    assert allowed == list(m.ALLOWED_NAMES), allowed
    # The resolver stub is probed on both protocols, because that address
    # is exactly what the `localhost` shorthand would have admitted.
    assert m.RESOLVER_STUB == "127.0.0.53"
    assert "udp_test_net_v4_from_child" in m.DENIED_NAMES

    # A child that never ran sent nothing, so its silence is not EPERM.
    saved = sys.executable
    try:
        sys.executable = "/nonexistent/python3"
        assert m.child_udp_send().startswith("child_not_run_")
    finally:
        sys.executable = saved
    passed("probe outcomes are named, including a child that never ran")


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
    assert m.doctor_check(doctor_document(), 0, ROW)[1] == []
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
    assert m.doctor_check(jetson, 0, "jetson-orin-nano-8gb",
                          "NVIDIA Jetson Orin Nano", "")[1] == []
    assert "host_os_machine_type_from_the_record" in m.doctor_check(
        jetson, 0, "jetson-orin-nano-8gb")[1]
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

    deploy = {"command": "deploy", "payload": {"phase": "active", "deployment_id": "d-offline"}}
    assert m.deploy_check(deploy, "d-offline")[1] == []
    assert "deployment_id" in m.deploy_check(deploy, "d")[1]
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
        status.write_text(json.dumps({"command": "status", "payload": {}}))
        result = run("serving-port", "--status", str(status))
        assert result.returncode == 1 and "loopback" in result.stderr, result.stderr
        result = run("serving-port", "--status", str(work / "absent.json"))
        assert result.returncode == 1 and "cannot read" in result.stderr, result.stderr
    passed("subcommands report failure through their exit status")


UNITS = ["tensorplate-agent.service", "tensorplate-observability.service"]


def evidence_directory(work, **overrides):
    """A stage's filed results, as the harness writes them."""
    documents = {
        "offline-denial.json": {
            "units": UNITS, "denied": True, "persistent_drop_ins_found": 0,
            "allowed_prefixes": list(m.ALLOWED_PREFIXES), "units_restarted": UNITS},
        "offline-restored.json": {
            "units": UNITS, "denied": False, "drop_ins_removed": 2,
            "persistent_drop_ins_found": 0, "units_restarted": UNITS},
        "offline-control.json": outcomes("ok", "ok"),
        "offline-probe.json": outcomes("EPERM", "ok"),
        "offline-classification.json": {"metadata_operation": "required"},
        "offline-identity.json": {"machine_type_source": "recorded_gce_metadata",
                                  "record": "not_applicable"},
        "offline-status-check.json": {"deployment_id": "d-offline", "status": "pass"},
        "offline-doctor-check.json": {"doctor": "pass"},
        "offline-deploy-check.json": {"deploy": "pass"},
        "offline-infer-check.json": {"infer": "pass"},
    }
    documents.update(overrides)
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
    assert result["classification"]["operations_refused_under_the_denial"] == \
        sorted(m.DENIED_NAMES)
    assert result["restore"]["drop_ins_removed"] == 2
    assert result["restore"]["persistent_unit_files_written"] == 0
    assert result["restore"]["units_restarted_without_the_denial"] == UNITS
    # Nothing in the evidence carries a path off the host or a process id.
    text = json.dumps(result)
    for forbidden in ("/run/systemd", "/etc/systemd", "pid"):
        assert forbidden not in text, forbidden

    # Every verdict is derived, so a directory whose own documents
    # contradict the certificate is refused rather than certified. Each
    # of these produced `"enforced": true` while the claim was a literal.
    for overrides, fragment in (
        # Nothing was denied, and the control was refused: the exact
        # input `classify` rejects.
        ({"offline-probe.json": outcomes("ok", "ok"),
          "offline-control.json": outcomes("EPERM", "ok")}, "do not classify"),
        # The readback saw the shorthand's expansion, which admits the
        # resolver stub and the DNS namespace behind it.
        ({"offline-denial.json": {"units": UNITS, "denied": True,
                                  "persistent_drop_ins_found": 0,
                                  "allowed_prefixes": ["127.0.0.0/8", "::1/128"],
                                  "units_restarted": UNITS}}, "admits 127.0.0.53"),
        # A readback that compared no invocation certifies the unit's
        # loaded configuration and nothing about the running service.
        ({"offline-denial.json": {"units": UNITS, "denied": True,
                                  "persistent_drop_ins_found": 0,
                                  "allowed_prefixes": list(m.ALLOWED_PREFIXES)}},
         "instance as replaced"),
        # A persistent drop-in outlives the run.
        ({"offline-restored.json": {"units": UNITS, "denied": False,
                                    "drop_ins_removed": 2, "units_restarted": UNITS,
                                    "persistent_drop_ins_found": 1}}, "/etc/systemd"),
    ):
        with tempfile.TemporaryDirectory() as work:
            directory = evidence_directory(pathlib.Path(work), **overrides)
            if fragment == "/etc/systemd":
                # This one is a value, not a refusal: it is filed so the
                # certificate cannot silently say zero.
                assert m.evidence(directory, "d-offline")[
                    "restore"]["persistent_unit_files_written"] == 1
                continue
            refused(lambda: m.evidence(directory, "d-offline"), fragment)

    # A check that never passed filed no document, so its absence refuses
    # the certificate rather than being restated as a pass.
    for missing in ("offline-doctor-check.json", "offline-classification.json",
                    "offline-infer-check.json"):
        with tempfile.TemporaryDirectory() as work:
            directory = evidence_directory(pathlib.Path(work))
            os.unlink(os.path.join(directory, missing))
            try:
                m.evidence(directory, "d-offline")
            except (OSError, m.CheckFailed):
                continue
            raise AssertionError(f"evidence certified a directory without {missing}")
    # A filed document that records something other than a pass.
    with tempfile.TemporaryDirectory() as work:
        directory = evidence_directory(pathlib.Path(work),
                                       **{"offline-deploy-check.json": {"deploy": "fail"}})
        refused(lambda: m.evidence(directory, "d-offline"), "does not record a pass")
    passed("evidence is derived from the filed results, not restated")


def main():
    for test in (test_drop_in, test_check_denial, test_check_no_denial, test_check_policy,
                 test_classify, test_probe_outcomes, test_doctor_check, test_identity_check,
                 test_cli_documents, test_command_line, test_evidence):
        test()
    print("linux offline runtime: all checks pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
