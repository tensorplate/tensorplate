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

    result, failures = m.check_drop_in("tensorplate-agent", m.DROP_IN_TEXT)
    assert failures == [], failures
    assert result["allowed_prefixes"] == list(m.ALLOWED_PREFIXES), result
    # Bytes that lint clean but are not the bytes this release renders:
    # the file on the host is what systemd read, not what we meant.
    altered = m.DROP_IN_TEXT.replace("Runtime only", "runtime only")
    assert altered != m.DROP_IN_TEXT
    assert m.lint_drop_in(altered) == []
    assert m.check_drop_in("tensorplate-agent", altered)[1] == [
        "drop_in_bytes_match_the_rendered_text"]
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
    passed("restored readback refuses a unit that is not running")


def test_check_policy():
    shows = [("tensorplate-agent", show()), ("tensorplate-observability", show())]
    result, failures = m.check_policy("denied", shows)
    assert failures == [], failures
    assert result == {"units": ["tensorplate-agent.service",
                                "tensorplate-observability.service"], "denied": True}, result
    assert m.check_policy("denied", [])[1] == ["units_read"]
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
            assert "tensorplate-agent.service:no_persistent_drop_in" in \
                m.check_policy("denied", [("tensorplate-agent", show())])[1]
        finally:
            m.RUNTIME_UNIT_DIR, m.PERSISTENT_UNIT_DIR = saved
    passed("policy readback counts removals and refuses persistent drop-ins")


# --- classification -----------------------------------------------------


def outcomes(denied, allowed):
    return {"denied": dict.fromkeys(m.DENIED_NAMES, denied),
            "allowed": dict.fromkeys(m.ALLOWED_NAMES, allowed)}


def test_classify():
    control = outcomes("ok", "ok")
    probe = outcomes("EPERM", "ok")
    assert m.classify(probe, control) == []
    # EACCES is the same class of answer; no routing failure produces it.
    assert m.classify(outcomes("EACCES", "ok"), control) == []

    # A denial that did nothing. `IPAddressDeny=` is silently inert where
    # systemd cannot install its filter, so this is the outcome the
    # property readback alone would have missed.
    assert m.classify(outcomes("ok", "ok"), control) == \
        sorted(f"refused:{name}" for name in m.DENIED_NAMES)
    # A routing failure is not a refusal.
    assert "refused:tcp_gce_metadata" in m.classify(outcomes("ENETUNREACH", "ok"), control)

    # A control refused by something else on the host -- an application
    # firewall, a missing route -- makes the probe's EPERM unattributable.
    refused_control = outcomes("EPERM", "ok")
    failures = m.classify(probe, refused_control)
    assert "control_metadata_service_reachable" in failures
    assert "control_not_refused:udp_test_net_v4" in failures
    # The metadata control must answer outright, not merely not be
    # refused: the stage's claim is that the denial is what made it
    # unreachable.
    unreachable = outcomes("ok", "ok")
    unreachable["denied"]["tcp_gce_metadata"] = "EHOSTUNREACH"
    assert m.classify(probe, unreachable) == ["control_metadata_service_reachable"]

    # Loopback and the agent socket have to keep working.
    blocked = outcomes("EPERM", "EPERM")
    assert m.classify(blocked, control) == \
        sorted(f"allowed:{name}" for name in m.ALLOWED_NAMES)
    assert "control_allowed:unix_agent_socket" in m.classify(probe, outcomes("ok", "EPERM"))

    # A probe that did not report an operation is a failure, never a skip:
    # a probe whose failure is swallowed certifies nothing.
    missing = outcomes("EPERM", "ok")
    del missing["denied"]["udp_resolver_stub"]
    del missing["allowed"]["tcp_loopback_serving_port"]
    failures = m.classify(missing, control)
    assert "refused:udp_resolver_stub" in failures
    assert "allowed:tcp_loopback_serving_port" in failures
    assert m.classify({}, {}) != []
    assert m.classify({"denied": "not a mapping"}, control) != []
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


def test_evidence():
    with tempfile.TemporaryDirectory() as work:
        work = pathlib.Path(work)
        units = ["tensorplate-agent.service", "tensorplate-observability.service"]
        (work / "offline-denial.json").write_text(json.dumps({"units": units, "denied": True}))
        (work / "offline-restored.json").write_text(json.dumps(
            {"units": units, "denied": False, "drop_ins_removed": 2}))
        (work / "offline-control.json").write_text(json.dumps(outcomes("ok", "ok")))
        (work / "offline-probe.json").write_text(json.dumps(outcomes("EPERM", "ok")))
        (work / "offline-identity.json").write_text(json.dumps(
            {"machine_type_source": "recorded_gce_metadata", "record": "not_applicable"}))
        result = m.evidence(str(work), "d-offline")
    assert result["mechanism"]["allow"] == list(m.ALLOWED_PREFIXES)
    assert result["mechanism"]["localhost_shorthand_used"] is False
    assert result["mechanism"]["drop_in_scope"] == "runtime"
    assert result["units_denied"] == units
    assert result["restore"]["drop_ins_removed"] == 2
    assert result["restore"]["persistent_unit_files_written"] == 0
    # Nothing in the evidence carries a path off the host or a process id.
    text = json.dumps(result)
    for forbidden in ("/run/systemd", "/etc/systemd", "pid"):
        assert forbidden not in text, forbidden
    passed("evidence carries outcomes and prefixes, not paths")


def main():
    for test in (test_drop_in, test_check_denial, test_check_no_denial, test_check_policy,
                 test_classify, test_probe_outcomes, test_doctor_check, test_identity_check,
                 test_cli_documents, test_command_line, test_evidence):
        test()
    print("linux offline runtime: all checks pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
