#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for the macOS offline-runtime stages.

Run by verify_macos_offline_runtime.sh. Four parts:

- unit tests of tools/validation/macos_offline_runtime.py on synthetic
  documents and on probe outcomes recorded under the real profile and
  its mutants (fixtures/macos-offline/);
- the harness's own offline-runtime stage and cleanup, extracted from
  macos-homebrew-lifecycle.sh and run against fixtures/macos-offline/
  fake_host.py, including INT, TERM and HUP delivered mid-stage and
  copies of the harness with one guard removed, each of which must fail;
- static ordering, mapping and transcript checks;
- on macOS only, the preflight against the real sandbox-exec with the
  rendered profile and each mutant. Every probe socket is pinned to lo0,
  so this sends nothing off the host.
"""

import concurrent.futures
import copy
import errno
import json
import os
import pathlib
import platform
import plistlib
import re
import shutil
import signal
import pty
import subprocess
import sys
import tempfile
import threading
import time

REPO = pathlib.Path(__file__).resolve().parents[2]
HARNESS = REPO / "tools/validation/macos-homebrew-lifecycle.sh"
FIXTURES = REPO / "test/packaging/fixtures/macos-offline"
FAKE_HOST = FIXTURES / "fake_host.py"
sys.path.insert(0, str(REPO / "tools/validation"))

import macos_offline_runtime as m  # noqa: E402

SOURCE = HARNESS.read_text(encoding="utf-8")
EXACT_ROW = json.loads((REPO / "config/platform/rows/macos26-m1pro-16gb.json").read_text())
AGENT, OBSERVABILITY = "tensorplate-agent", "tensorplate-observability"


def passed(name):
    print(f"macOS offline runtime: {name}: pass")


def refused(operation, fragment):
    try:
        operation()
    except m.CheckFailed as error:
        assert fragment in str(error), (fragment, str(error))
        return
    raise AssertionError(f"not refused: expected {fragment!r}")


# --- profile -----------------------------------------------------------


def test_profile():
    config = json.loads((REPO / "packaging/homebrew/conf/agent.json.in").read_text())
    assert m.serving_ports(config) == (18080, 18081), "the packaged agent config ports changed"
    text = m.render_profile(18080, 18081)
    assert "\n" not in text and m.lint_profile(text) == [], m.lint_profile(text)
    assert m.profile_ports(text) == [18080, 18081]
    for mutate, fragment in (
        (lambda w: w.update(serving_bind_host="0.0.0.0"), "serving_bind_host"),
        (lambda w: w.update(serving_bind_host="localhost"), "serving_bind_host"),
        (lambda w: w.update(serving_candidate_bind_port=18080), "must differ"),
        (lambda w: w.update(serving_bind_port=80), "1024-65535"),
        (lambda w: w.update(serving_candidate_bind_port=70000), "1024-65535"),
        (lambda w: w.update(serving_bind_port=True), "1024-65535"),
        (lambda w: w.update(serving_bind_port="18080"), "1024-65535"),
        (lambda w: w.pop("serving_candidate_bind_port"), "serving_candidate_bind_port"),
    ):
        broken = copy.deepcopy(config)
        mutate(broken["worker"])
        refused(lambda: m.serving_ports(broken), fragment)
    refused(lambda: m.serving_ports({"worker": None}), "no worker section")

    unix = ("(allow network-bind network-inbound (local unix-socket))"
            "(allow network-outbound (remote unix-socket))")
    resolver = ('(deny network-outbound (remote unix-socket '
                '(path-literal "/private/var/run/mDNSResponder")))')
    lint_cases = {
        "base_network_deny_first": text.replace("(deny network*)", ""),
        "no_allow_network_star": text.replace(
            '(allow network-inbound (local ip "localhost:18080") (local ip "localhost:18081"))',
            '(allow network* (local ip "localhost:18080") (local ip "localhost:18081"))'),
        "ip_rules_port_scoped": text.replace('"localhost:18081"', '"localhost:*"'),
        "resolver_deny_last": text.replace(resolver, "").replace(
            "(deny network*)", "(deny network*)" + resolver),
        "unix_socket_single_filter": text.replace(
            "(local unix-socket)",
            '(local unix-socket (subpath "/private/tmp") (subpath "/private/var/folders"))'),
        "loopback_inbound_allowed": text.replace(
            '(allow network-inbound (local ip "localhost:18080") (local ip "localhost:18081"))', ""),
        "loopback_outbound_allowed": text.replace(
            '(allow network-outbound (remote ip "localhost:18080") (remote ip "localhost:18081"))', ""),
        "unix_sockets_allowed": text.replace(unix, ""),
        "single_line": text.replace(")(", ")\n(", 1),
        "two_serving_ports": text.replace(
            '(local ip "localhost:18081"))', '(local ip "localhost:18081") (local ip "localhost:18082"))'),
    }
    lint_cases["resolver_deny_last (unresolved path)"] = text.replace(
        "/private/var/run/mDNSResponder", "/var/run/mDNSResponder")
    lint_cases["ip_rules_port_scoped (address literal)"] = text.replace(
        '"localhost:18080"', '"127.0.0.1:18080"')
    lint_cases["known_actions"] = text.replace(resolver, "(debug deny)" + resolver)
    # A rule added with a filter the named rules do not parse, or with no
    # filter at all, widens the profile without breaking any of them.
    for extra in ("(allow network-outbound)", "(allow network-inbound)", "(allow network-bind)",
                  '(allow network-outbound (remote tcp "*:443"))', "(allow network-outbound (remote udp))",
                  '(allow network-inbound (local tcp "*:*"))',
                  '(allow network-outbound (remote tcp "localhost:*"))',
                  '(allow network-outbound (remote tcp "*:18080") (remote tcp "*:18081"))'):
        lint_cases[f"only_expected_rules {extra}"] = text.replace(resolver, extra + resolver)
    lint_cases["only_expected_rules (reordered)"] = text.replace(unix, "").replace(
        "(deny network*)", "(deny network*)" + unix)
    for problem, mutant in lint_cases.items():
        found = m.lint_profile(mutant)
        assert problem.split(" ")[0] in found, (problem, found, mutant)
    refused(lambda: m.profile_ports(lint_cases["ip_rules_port_scoped"]), "not the rendered template")
    saved = m.PROFILE_TEMPLATE
    try:
        for weakened in (saved.replace('"localhost:{candidate}"', '"localhost:*"'),
                         saved.replace(resolver, "(allow network-outbound)" + resolver)):
            m.PROFILE_TEMPLATE = weakened
            refused(lambda: m.render_profile(18080, 18081), "template breaks")
    finally:
        m.PROFILE_TEMPLATE = saved
    passed("profile rendering and lint")


# --- launchd -----------------------------------------------------------


def formula_document(prefix="/opt/homebrew", service=AGENT):
    return {
        "Label": f"homebrew.mxcl.{service}",
        "ProgramArguments": [f"{prefix}/opt/{service}/bin/{service}", "--config",
                             f"{prefix}/etc/tensorplate/agent.json"],
        "RunAtLoad": True, "KeepAlive": {"SuccessfulExit": False}, "ThrottleInterval": 5,
        "EnvironmentVariables": {"PATH": "/usr/bin:/bin"},
        "WorkingDirectory": f"{prefix}/var/tensorplate",
    }


def test_derive_plist():
    document = formula_document()
    program = document["ProgramArguments"][0]
    profile = "/private/tmp/offline-denial/network-denied.sb"
    derived = m.derive_plist(document, "homebrew.mxcl.tensorplate-agent", program, profile)
    assert {key for key in derived if derived[key] != document.get(key)} == {"ProgramArguments"}
    assert derived["ProgramArguments"] == ["/usr/bin/sandbox-exec", "-f", profile] + \
        document["ProgramArguments"]
    assert derived["ProgramArguments"] == m.expected_sandboxed_arguments(document, profile)
    for mutate, fragment in (
        (lambda d: d.update(Label="homebrew.mxcl.other"), "Label"),
        (lambda d: d.update(Program=program), "sets Program"),
        (lambda d: d["ProgramArguments"].__setitem__(0, "/usr/local/bin/tensorplate-agent"),
         "ProgramArguments[0]"),
        (lambda d: d.update(ProgramArguments=["/usr/bin/sandbox-exec", "-f", profile, program]),
         "ProgramArguments[0]"),
        (lambda d: d["ProgramArguments"].append("/usr/bin/sandbox-exec"), "already runs sandbox-exec"),
        (lambda d: d.update(ProgramArguments=[]), "non-empty string list"),
    ):
        broken = copy.deepcopy(document)
        mutate(broken)
        refused(lambda: m.derive_plist(broken, "homebrew.mxcl.tensorplate-agent", program, profile),
                fragment)
    refused(lambda: m.derive_plist(document, "homebrew.mxcl.tensorplate-agent", program,
                                   "network-denied.sb"), "absolute")
    with tempfile.TemporaryDirectory(prefix="tp-offline-derive-") as directory:
        root = pathlib.Path(directory)
        with open(root / "formula.plist", "wb") as handle:
            plistlib.dump(document, handle)
        base = ["derive-plist", "--formula-plist", str(root / "formula.plist"),
                "--label", "homebrew.mxcl.tensorplate-agent", "--program", program,
                "--plist-out", str(root / "derived.plist")]
        assert m.main(base + ["--profile", str(root / "missing.sb")]) == 1
        (root / "multi.sb").write_text("(version 1)\n(allow default)")
        assert m.main(base + ["--profile", str(root / "multi.sb")]) == 1
        assert not (root / "derived.plist").exists()
        m.write_profile(str(root / "good.sb"), m.render_profile(18080, 18081))
        assert m.main(base + ["--profile", str(root / "good.sb")]) == 0
        with open(root / "derived.plist", "rb") as handle:
            assert plistlib.load(handle)["ProgramArguments"][:3] == [
                "/usr/bin/sandbox-exec", "-f", str(root / "good.sb")]
    passed("plist derivation refusals")


def test_launchd_job():
    text = (FIXTURES / "launchctl-print-sandboxed-agent.txt").read_text()
    job = m.parse_launchd_job(text)
    profile = "/private/var/folders/xx/T/tmp.synthetic/offline-denial/network-denied.sb"
    formula = formula_document()
    expected = m.expected_sandboxed_arguments(formula, profile)
    derived_path = "/private/var/folders/xx/T/tmp.synthetic/offline-denial/homebrew.mxcl.tensorplate-agent.plist"
    assert job == {"label": "homebrew.mxcl.tensorplate-agent", "path": derived_path,
                   "program": "/usr/bin/sandbox-exec", "arguments": expected,
                   "pid": 4242, "runs": 1, "runs_sandbox_exec": True}, job
    # Cleanup's test for a job it must boot out: either launchd's program or
    # the first argument is sandbox-exec.
    service = "/opt/homebrew/opt/tensorplate-agent/bin/tensorplate-agent"
    program_overridden = text.replace("\tprogram = /usr/bin/sandbox-exec", f"\tprogram = {service}")
    assert m.parse_launchd_job(program_overridden)["runs_sandbox_exec"]
    normal = program_overridden.replace("\t\t/usr/bin/sandbox-exec\n\t\t-f\n", "").replace(
        f"\t\t{profile}\n", "")
    assert m.parse_launchd_job(normal)["arguments"] == formula["ProgramArguments"]
    assert not m.parse_launchd_job(normal)["runs_sandbox_exec"]
    good = dict(program="/usr/bin/sandbox-exec", path=derived_path, arguments=expected,
                runs=1, same_pid_as=4242)
    assert m.check_launchd_job(job, **good) == []
    cases = {
        "job_arguments": dict(arguments=expected[:-1] + [expected[-1] + "x"]),
        "job_program": dict(program="/opt/homebrew/opt/tensorplate-agent/bin/tensorplate-agent"),
        "job_path": dict(path="/Users/operator/Library/LaunchAgents/homebrew.mxcl.tensorplate-agent.plist"),
        "job_runs": dict(runs=2),
        "job_pid_unchanged": dict(same_pid_as=4243),
    }
    for failure, override in cases.items():
        found = m.check_launchd_job(job, **dict(good, **override))
        assert found == [failure], (failure, found)
    assert m.check_launchd_job(dict(job, pid=None)) == ["job_running"]
    # launchd may print the loaded plist through a symlinked directory
    # (/var/folders is /private/var/folders); both spellings are one file.
    with tempfile.TemporaryDirectory(prefix="tp-offline-link-") as directory:
        real = pathlib.Path(directory) / "private" / "work"
        real.mkdir(parents=True)
        (pathlib.Path(directory) / "var").symlink_to(real)
        linked_job = dict(job, path=f"{directory}/var/job.plist")
        assert m.check_launchd_job(linked_job, path=f"{real}/job.plist") == []
        assert m.check_launchd_job(linked_job, path=f"{real}/other.plist") == ["job_path"]
    # A derivation that dropped the prefix would load the formula's own
    # arguments; the expectation is built independently and refuses it.
    assert "job_arguments" in m.check_launchd_job(
        dict(job, arguments=formula["ProgramArguments"]), arguments=expected)
    for broken, fragment in (
        (text.replace("\t\t}\n\t}\n\n\tproperties", "\t\t}\n\n\tproperties", 1), "not closed"),
        (text.replace("\t\t/opt/homebrew/etc/tensorplate/agent.json\n\t}\n",
                      "\t\t/opt/homebrew/etc/tensorplate/agent.json\n", 1), "unexpected shape"),
        (text.rstrip("}\n"), "not one job block"),
        (text.replace("\tpid = 4242\n", "\tpid = 4242\n\tprogram = /bin/sh\n"), "repeats"),
        (text.replace("\t\t/usr/bin/sandbox-exec", " /usr/bin/sandbox-exec"), "unexpected shape"),
    ):
        refused(lambda: m.parse_launchd_job(broken), fragment)
    passed("launchctl print parsing and job checks")


# --- sandbox readback --------------------------------------------------


def with_patches(patches, operation):
    saved = {name: getattr(m, name) for name in patches}
    try:
        for name, value in patches.items():
            setattr(m, name, value)
        return operation()
    finally:
        for name, value in saved.items():
            setattr(m, name, value)


def test_sandbox_readback():
    identities = {10: "Mon Jan  5 00:00:00 2026 /bin/agent", 11: "Mon Jan  5 00:00:01 2026 /bin/serving"}
    sandboxed = {10: 1, 11: 0}

    def primitive(pid, operation):
        return sandboxed.get(pid, 1)  # like libsystem_sandbox: a missing pid reads 1

    patches = {"sandbox_check": primitive, "process_identity": identities.get}
    assert with_patches(patches, lambda: m.read_sandbox_state(10)) == {
        "sandboxed": True, "network_outbound_denied": True, "network_inbound_denied": True}
    assert not any(with_patches(patches, lambda: m.read_sandbox_state(11)).values())
    # A pid that is not running reads 1/1 from sandbox_check itself.
    with_patches(patches, lambda: refused(lambda: m.read_sandbox_state(99999), "not running"))
    # The pid was reused between the two identity reads.
    calls = []

    def reused(pid):
        calls.append(pid)
        return f"Mon Jan  5 00:00:0{len(calls)} 2026 /bin/agent"

    with_patches({"sandbox_check": primitive, "process_identity": reused},
                 lambda: refused(lambda: m.read_sandbox_state(10), "exited or was replaced"))
    with_patches({"sandbox_check": lambda pid, op: -1, "process_identity": identities.get},
                 lambda: refused(lambda: m.read_sandbox_state(10), "returned [-1, -1, -1]"))
    # A child that has exited but is not reaped yet is still listed by ps,
    # and sandbox_check reads it as sandboxed.
    zombie = subprocess.Popen(["/usr/bin/true"])
    try:
        deadline = time.time() + 10
        while "Z" not in (m.process_status(zombie.pid) or ("",))[0]:
            assert time.time() < deadline, "the exited child never showed as a zombie"
            time.sleep(0.05)
        assert m.process_identity(zombie.pid) is None
        with_patches({"sandbox_check": lambda pid, op: 1},
                     lambda: refused(lambda: m.read_sandbox_state(zombie.pid), "not running"))
    finally:
        zombie.wait()

    # A probe child that could not be started sent nothing, whatever the
    # errno of the failure.
    def spawn_refused(*args, **kwargs):
        raise PermissionError(errno.EPERM, "Operation not permitted")
    outcome = with_patches({"subprocess": type("S", (), {"run": staticmethod(spawn_refused),
                                                         "SubprocessError": subprocess.SubprocessError})},
                           lambda: m.attempt(m.child_udp_send))
    assert outcome == "child_not_run_PermissionError", outcome

    # The discrimination controls run a real sleeper under a stand-in
    # sandbox-exec that only records which pid it started.
    with tempfile.TemporaryDirectory(prefix="tp-offline-readback-") as directory:
        root = pathlib.Path(directory)
        (root / "bin").mkdir()
        (root / "bin/sandbox-exec").write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >>\"$TP_SANDBOXED_PIDS\"\nshift 2\nexec \"$@\"\n")
        (root / "bin/sandbox-exec").chmod(0o755)
        (root / "sandboxed").write_text("")
        profile = m.write_profile(str(root / "p.sb"), m.render_profile(18080, 18081))
        saved_env = dict(os.environ)
        os.environ["PATH"] = f"{root / 'bin'}{os.pathsep}{os.environ['PATH']}"
        os.environ["TP_SANDBOXED_PIDS"] = str(root / "sandboxed")

        def recorded(pid, operation):
            if int(pid) in [int(item) for item in (root / "sandboxed").read_text().split()]:
                return 1
            try:
                os.kill(pid, 0)
            except OSError:
                return 1
            return 0

        try:
            controls = with_patches({"sandbox_check": recorded},
                                    lambda: m.discrimination_controls(profile))
            assert all(controls.values()), controls
            # A primitive that reads every process as sandboxed.
            with_patches({"sandbox_check": lambda pid, op: 1},
                         lambda: refused(lambda: m.discrimination_controls(profile),
                                         "unsandboxed process reads as sandboxed"))
            # One that cannot see the sandbox at all.
            with_patches({"sandbox_check": lambda pid, op: 0 if pid == os.getpid() else 0},
                         lambda: refused(lambda: m.discrimination_controls(profile),
                                         "does not read as sandboxed"))
            # An identity check that notices a reaped process but not a
            # zombie.
            with_patches({"sandbox_check": recorded,
                          "process_identity": lambda pid: (m.process_status(pid) or (None, None))[1]},
                         lambda: refused(lambda: m.discrimination_controls(profile),
                                         "unreaped exited process passed the identity check"))
            # An identity check that never notices an exit.
            real_identity = m.process_identity
            with_patches({"sandbox_check": recorded,
                          "process_identity": lambda pid: real_identity(pid) or "Mon /bin/sleep"},
                         lambda: refused(lambda: m.discrimination_controls(profile),
                                         "exited process passed the identity check"))

            live = subprocess.Popen(["sleep", "30"])
            try:
                result, failures = with_patches(
                    {"sandbox_check": recorded},
                    lambda: m.sandbox_states(profile, [live.pid], [], "sandboxed"))
                assert failures == ["process_sandboxed_network_denied"], failures
                result, failures = with_patches(
                    {"sandbox_check": recorded},
                    lambda: m.sandbox_states(profile, [live.pid], [99999], "unsandboxed"))
                assert failures == [] and result["processes_read"] == 1, (result, failures)
                with_patches({"sandbox_check": recorded},
                             lambda: refused(lambda: m.sandbox_states(profile, [99999], [], "unsandboxed"),
                                             "not running"))
                with_patches({"sandbox_check": recorded},
                             lambda: refused(lambda: m.sandbox_states(profile, [], [], "sandboxed"),
                                             "no processes"))
                # Only optional processes, all gone: nothing was read.
                result, failures = with_patches(
                    {"sandbox_check": recorded}, lambda: m.sandbox_states(profile, [], [99999], "sandboxed"))
                assert failures == ["processes_read"] and result["processes_read"] == 0, (result, failures)
                result, failures = with_patches(
                    {"sandbox_check": lambda pid, op: 1 if pid == live.pid else recorded(pid, op)},
                    lambda: m.sandbox_states(profile, [], [live.pid], "unsandboxed"))
                assert failures == ["process_unsandboxed"], failures
                # A sandboxed leftover that exits within the wait passes;
                # one that stays fails.
                listings = [[live.pid], [live.pid], []]
                result, failures = with_patches(
                    {"sandbox_check": lambda pid, op: 1 if pid == live.pid else recorded(pid, op),
                     "tensorplate_processes": lambda: listings.pop(0) if len(listings) > 1 else listings[0]},
                    lambda: m.no_sandboxed_tensorplate_process(profile, [], attempts=5))
                assert failures == [] and result["none_sandboxed"], (result, failures)
                result, failures = with_patches(
                    {"sandbox_check": lambda pid, op: 1 if pid == live.pid else recorded(pid, op),
                     "tensorplate_processes": lambda: [live.pid]},
                    lambda: m.no_sandboxed_tensorplate_process(profile, [], attempts=3))
                assert failures == ["process_unsandboxed"] and not result["none_sandboxed"], failures
            finally:
                live.kill()
                live.wait()
        finally:
            os.environ.clear()
            os.environ.update(saved_env)
    passed("sandbox readback identity and discrimination controls")


def test_processes_and_listeners():
    pattern = re.compile(m.TENSORPLATE_PROCESS_PATTERN)
    for arguments, role, matches in (
        ("/opt/homebrew/opt/tensorplate-agent/bin/tensorplate-agent --config a.json", None, True),
        ("/opt/homebrew/opt/tensorplate-observability/bin/tensorplate-observability", None, True),
        ("/opt/homebrew/opt/tensorplate-serving/libexec/tensorplate-serving --config w", "serving", True),
        ("/opt/homebrew/opt/pytorch/libexec/bin/python -m tensorplate_pytorch_backend --socket s",
         "sidecar", True),
        ("/bin/bash tools/validation/macos-homebrew-lifecycle.sh --evidence-dir /tmp/tensorplate-agent",
         None, False),
        ("python3 -c tensorplate-serving", None, False),
    ):
        assert m.process_role(arguments) == role, arguments
        assert bool(pattern.search(arguments)) == matches, arguments

    good = ("p101\nf3\nPTCP\nn127.0.0.1:18080\nTST=LISTEN\nTQR=0\nTQS=0\n"
            "p102\nf7\nPTCP\nn127.0.0.1:52296->127.0.0.1:18080\nTST=ESTABLISHED\n"
            "f8\nPUDP\nn[::1]:18081\n")
    sockets = m.parse_lsof(good)
    assert len(sockets) == 3 and sockets[0] == {
        "pid": 101, "protocol": "TCP", "name": "127.0.0.1:18080", "state": "LISTEN"}
    tree = {101, 102}
    assert m.check_listeners(sockets, tree | {200}, tree, 18080) == []
    # pid 200 is a TensorPlate process outside the agent's tree, such as
    # the observability service.
    observability = "p200\nf4\nPTCP\nn*:18081\nTST=LISTEN\n"
    for document, pids, tree_pids, failures in (
        (good.replace("n127.0.0.1:18080\nTST=LISTEN", "n*:18080\nTST=LISTEN"), tree, tree,
         ["loopback_only", "serving_listener_in_tree"]),
        (good + "f9\nPTCP\nn192.0.2.10:18081\nTST=LISTEN\n", tree, tree, ["loopback_only"]),
        (good + "f9\nPUDP\nn*:5353\n", tree, tree, ["loopback_only"]),
        (good + observability, tree | {200}, tree, ["loopback_only"]),
        # Never bound or connected: no port, nothing can reach it.
        (good + "f9\nPUDP\nn*:*\n", tree, tree, []),
        (good + "f9\nPTCP\nn*:*\nTST=CLOSED\n", tree, tree, []),
        ("", {101}, {101}, ["serving_listener_in_tree"]),
        (good, {102}, {102}, ["serving_listener_in_tree", "sockets_owned_by_tensorplate_processes"]),
        # The serving listener must belong to the agent's tree.
        (good, tree, {102}, ["serving_listener_in_tree"]),
        (good.replace("n127.0.0.1:18080\nTST=LISTEN", "n[::1]:18080\nTST=LISTEN"), tree, tree,
         ["serving_listener_in_tree"]),
        (good.replace("TST=LISTEN", "TST=CLOSED"), tree, tree, ["serving_listener_in_tree"]),
    ):
        assert m.check_listeners(m.parse_lsof(document), pids, tree_pids, 18080) == failures, \
            (document, failures)
    refused(lambda: m.parse_lsof("f3\nPTCP\n"), "before its process")
    refused(lambda: m.parse_lsof("p1\nPTCP\n"), "outside a file")

    # pgrep and lsof exit 1 for "nothing matched"; anything else is an
    # error, never an empty answer.
    with tempfile.TemporaryDirectory(prefix="tp-offline-tools-") as directory:
        root = pathlib.Path(directory)
        for tool in ("pgrep", "lsof"):
            (root / tool).write_text("#!/bin/sh\nprintf '%s' \"${TP_TOOL_STDERR:-}\" >&2\n"
                                     "exit \"${TP_TOOL_STATUS}\"\n")
            (root / tool).chmod(0o755)
        saved_env = dict(os.environ)
        os.environ["PATH"] = f"{root}{os.pathsep}{os.environ['PATH']}"
        try:
            os.environ["TP_TOOL_STATUS"] = "1"
            assert m.tensorplate_processes() == [] and m.child_pids(1) == [] and m.lsof(["-i"]) == ""
            # lsof also exits 1, with nothing on stdout, when its query is
            # not understood; quiesced would read that as no listener.
            os.environ["TP_TOOL_STDERR"] = "lsof: unknown service notaport for tcp in: -i TCP:notaport"
            refused(lambda: m.lsof(["-iTCP:notaport", "-sTCP:LISTEN"]), "lsof failed with status 1")
            del os.environ["TP_TOOL_STDERR"]
            os.environ["TP_TOOL_STATUS"] = "2"
            refused(m.tensorplate_processes, "pgrep failed with status 2")
            refused(lambda: m.child_pids(1), "pgrep -P 1 failed with status 2")
            refused(lambda: m.lsof(["-i"]), "lsof failed with status 2")
        finally:
            os.environ.clear()
            os.environ.update(saved_env)
    passed("process roles and loopback-only listeners")


# --- classification ----------------------------------------------------


UNLISTED_BINDS = ["refused:tcp_listen_wildcard_unlisted_port", "refused:tcp_listen_loopback_unlisted_port",
                  "refused:udp_bind_wildcard_unlisted_port"]
RECORDED = {
    "final": [],
    "deny-network-only": ["allowed:unix_agent_socket", "allowed:tcp_loopback_listen_candidate_port",
                          "serving_health_ready", "serving_health_deployment"],
    "draft-localhost-any-port": ["refused:udp_fe80_1_unlisted_port", "refused:udp_loopback_unlisted_port",
                                 "refused:tcp_loopback_unlisted_port"] + UNLISTED_BINDS,
    "allow-all-local-ip": [f"refused:{name}" for name in m.DENIAL_NAMES
                           if name != "unix_mdnsresponder"],
    "resolver-deny-first": ["refused:unix_mdnsresponder"],
    "resolver-unresolved-path": ["refused:unix_mdnsresponder"],
    "no-base-deny": UNLISTED_BINDS + ["refused:unix_mdnsresponder"],
    "no-inbound-allow": ["allowed:tcp_loopback_listen_candidate_port"],
    "tcp-loopback-any-port": ["refused:tcp_loopback_unlisted_port"],
    "tcp-any-host-serving-ports": [f"refused:tcp_{host}_{role}_port" for role in ("serving", "candidate")
                                   for host in ("test_net_v4", "documentation_v6")],
    "udp-any-host-serving-ports": [f"refused:udp_{host}_{role}_port" for role in ("serving", "candidate")
                                   for host in ("test_net_v4", "documentation_v6")],
    "tcp-listen-any-address": UNLISTED_BINDS[:2],
    "udp-bind-any-address": UNLISTED_BINDS[2:],
}


def test_classify():
    recorded = {}
    for variant, expected in RECORDED.items():
        document = json.loads((FIXTURES / f"probe-{variant}.json").read_text())
        assert document["profile_variant"] == variant
        recorded[variant] = document
        found = m.classify(document["probe"], document["control"], m.PREFLIGHT_DEPLOYMENT,
                           listen_port_checked=True)
        assert found == expected, (variant, found)
    final = recorded["final"]
    # The allow-all mutant's sends failed only because lo0 cannot route
    # them; anything but EPERM is not a refusal.
    assert set(recorded["allow-all-local-ip"]["probe"]["denied"].values()) & {"ENETUNREACH", "EHOSTUNREACH"}
    in_stage = copy.deepcopy(final["probe"])
    del in_stage["allowed"]["tcp_loopback_listen_candidate_port"]
    assert m.classify(in_stage, final["control"], m.PREFLIGHT_DEPLOYMENT) == []
    # The probe and the control run the same operations, one per name.
    assert [name for name, _ in m.denial_operations((18080, 18081))] == list(m.DENIAL_NAMES)
    # Every operation's control counts: EPERM, or a socket that could not
    # be pinned, outside the sandbox would make the sandbox's EPERM moot.
    for name in m.DENIAL_NAMES:
        for outcome in ("EPERM", "pin_failed"):
            control = dict(final["control"], **{name: outcome})
            expected = ["control_mdnsresponder_reachable"] if name == "unix_mdnsresponder" else \
                [f"control_not_refused:{name}"]
            assert m.classify(in_stage, control, m.PREFLIGHT_DEPLOYMENT) == expected, (name, outcome)
    for mutate, expected in (
        (lambda probe, control: control.pop("udp_fe80_1_unlisted_port"),
         ["control_not_refused:udp_fe80_1_unlisted_port"]),
        (lambda probe, control: control.update(unix_mdnsresponder="EPERM"),
         ["control_mdnsresponder_reachable"]),
        (lambda probe, control: probe["denied"].pop("udp_test_net_v4_from_child"),
         ["refused:udp_test_net_v4_from_child"]),
        (lambda probe, control: probe["denied"].update(tcp_public_v4="pin_failed"),
         ["refused:tcp_public_v4"]),
        (lambda probe, control: probe["denied"].update(tcp_public_v4="timeout"),
         ["refused:tcp_public_v4"]),
        (lambda probe, control: probe["health"].update(active_model_id="smoke-1"),
         ["serving_health_deployment"]),
        (lambda probe, control: probe["health"].update(request="URLError", state=None),
         ["serving_health_ready"]),
        (lambda probe, control: probe["allowed"].update(unix_agent_socket="ECONNREFUSED"),
         ["allowed:unix_agent_socket"]),
    ):
        probe, control = copy.deepcopy(in_stage), copy.deepcopy(final["control"])
        mutate(probe, control)
        assert m.classify(probe, control, m.PREFLIGHT_DEPLOYMENT) == expected, expected
    for document in recorded.values():
        text = json.dumps(document)
        assert not re.search(r"[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+|/", text), "fixture carries an address or path"
    passed("probe classification on recorded outcomes")


def doctor_document(**overrides):
    # Listed here rather than taken from m.DOCTOR_FINDINGS_OK, so dropping
    # a finding from the helper's list is caught.
    findings = {name: {"id": name, "status": "ok", "severity": "info", "message": "ok"}
                for name in ("platform_row", "platform_profile", "agent_reachable",
                             "agent_service_state", "observability_service_state")}
    findings["platform_row"]["message"] = "resolves to support row `macos26-m1pro-16gb`"
    for name, change in overrides.items():
        findings[name].update(change)
    return {"command": "doctor", "status": "ok",
            "payload": {"failing": 0, "total": len(findings), "findings": list(findings.values())}}


def test_cli_checks():
    assert m.doctor_check(doctor_document(), 0, "macos26-m1pro-16gb") == (
        {"doctor": "pass", "platform_row": "macos26-m1pro-16gb"}, [])
    failing = doctor_document()
    failing["payload"]["failing"] = 1
    for document, status, expected in (
        (failing, 10, ["doctor_exit_status", "doctor_no_failing_findings"]),
        (doctor_document(platform_row={"message": "resolves to support row `macos26-apple-m-series-preview`"}),
         0, ["platform_row_exact"]),
        (doctor_document(platform_row={"status": "unsupported"}), 0, ["platform_row_ok"]),
        (doctor_document(platform_profile={"status": "warn"}), 0, ["platform_profile_ok"]),
        # Nonzero with nothing failing: the exit status is checked on its own.
        (doctor_document(), 10, ["doctor_exit_status"]),
        (doctor_document(agent_service_state={"status": "warn", "message": "is stopped"}), 0,
         ["agent_service_state_ok"]),
        (doctor_document(observability_service_state={"status": "skipped"}), 0,
         ["observability_service_state_ok"]),
        (doctor_document(agent_reachable={"status": "skipped",
                                          "message": "agent probe skipped via --skip-agent"}), 0,
         ["agent_reachable_ok"]),
        (dict(doctor_document(), command="status"), 0, ["doctor_command"]),
    ):
        assert m.doctor_check(document, status, "macos26-m1pro-16gb")[1] == expected, expected

    status = {"command": "status", "payload": {"severity": "ready", "agent": {
        "agent_state": "ready", "active": {"deployment_id": "offline-1", "backend": "python_pytorch",
                                           "serving_url": "http://127.0.0.1:18081/infer"}}}}
    assert m.status_check(status, "offline-1", [18080, 18081])[1] == []
    for mutate, expected in (
        (lambda s: s["payload"]["agent"]["active"].update(deployment_id="smoke-1"), ["active_deployment"]),
        (lambda s: s["payload"]["agent"].update(in_flight_transaction={"deployment_id": "x"}),
         ["no_transaction_in_flight"]),
        (lambda s: s["payload"]["agent"]["active"].update(serving_url="http://127.0.0.1:9999/infer"),
         ["serving_url_on_allowed_loopback_port"]),
        (lambda s: s["payload"]["agent"]["active"].update(serving_url="http://0.0.0.0:18080/infer"),
         ["serving_url_on_allowed_loopback_port"]),
        (lambda s: s["payload"].update(severity="degraded"), ["status_severity_ready"]),
        (lambda s: s["payload"]["agent"].update(agent_state="starting"), ["agent_state_ready"]),
        (lambda s: s.update(command="deploy"), ["status_command"]),
        (lambda s: s["payload"]["agent"]["active"].update(backend="cpu"), ["active_backend"]),
    ):
        broken = copy.deepcopy(status)
        mutate(broken)
        assert m.status_check(broken, "offline-1", [18080, 18081])[1] == expected, expected

    deploy = {"command": "deploy", "payload": {"phase": "active", "deployment_id": "offline-1"}}
    assert m.deploy_check(deploy, "offline-1")[1] == []
    for broken, expected in (
        (dict(deploy, payload={"phase": "failed", "deployment_id": "offline-1"}), ["deployment_phase_active"]),
        (dict(deploy, payload={"phase": "active", "deployment_id": "smoke-1"}), ["deployment_id"]),
        (dict(deploy, command="status"), ["deploy_command"]),
    ):
        assert m.deploy_check(broken, "offline-1")[1] == expected, expected

    request = m.infer_request()
    sent = request["inputs"][0]
    response = {"outputs": [{"name": "echo_probe", "payload_b64": sent["payload_b64"],
                             "tensor": dict(sent["tensor"], byte_offset=0, byte_size=16)}]}
    assert m.infer_check(request, response)[1] == []
    assert m.infer_check(request, {"payload": response})[1] == []
    wrong = copy.deepcopy(response)
    wrong["outputs"][0]["payload_b64"] = "AAAA"
    assert m.infer_check(request, wrong)[1] == ["inference_preserved_the_payload"]
    wrong["outputs"][0]["tensor"]["shape"] = [4]
    assert m.infer_check(request, wrong)[1] == ["inference_echoed_the_input",
                                                "inference_preserved_the_payload"]
    passed("doctor, status, deploy and inference checks")


def admission_line():
    """The line agent/src/main.rs prints, rendered from its format string."""
    agent = (REPO / "agent/src/main.rs").read_text(encoding="utf-8")
    literal = re.search(r'"(platform admission:(?:[^"\\]|\\.)*)"', agent, re.S)
    template = re.sub(r"\\\n\s*", "", literal.group(1))
    values = iter(("macos26-m1pro-16gb", "none", "17179869184"))
    named = {"posture": "technical_prerequisites", "posture_from": "row floor", "evidence": "validated"}
    return re.sub(r"\{(\w*)\}", lambda match: named[match.group(1)] if match.group(1) else next(values),
                  template)


def test_admission():
    line = admission_line()
    startup = "tensorplate-observability primary_source=internal interval=1000ms ros2_enabled=false"
    result, failures = m.admission_check(f"registry\n{line}\nlistening\n", startup + "\n", EXACT_ROW)
    assert failures == [] and result == {"row": "macos26-m1pro-16gb", "reason": "none",
                                         "evidence": "validated", "observability_started": True}
    for agent_output, observability_output, expected in (
        (f"{line}\n{line}\n", startup, ["admission_logged_once", "admission_line_parsed",
                                        "admission_exact_row", "admission_reason_none",
                                        "admission_evidence_validated", "admission_memory_ceiling"]),
        (line.replace("evidence=validated", "evidence=unvalidated (admitted on technical prerequisites)"),
         startup, ["admission_evidence_validated"]),
        (line.replace("reason=none", "reason=row_memory_exceeded"), startup, ["admission_reason_none"]),
        (line.replace("row=macos26-m1pro-16gb", "row=macos26-apple-m-series-preview"), startup,
         ["admission_exact_row"]),
        (line.replace("17179869184", "8589934592"), startup, ["admission_memory_ceiling"]),
        ("", startup, ["admission_logged_once", "admission_line_parsed", "admission_exact_row",
                       "admission_reason_none", "admission_evidence_validated",
                       "admission_memory_ceiling"]),
        (line, f"{startup}\n{startup}", ["observability_started_once"]),
        (line, "", ["observability_started_once"]),
    ):
        assert m.admission_check(agent_output, observability_output, EXACT_ROW)[1] == expected, expected
    with tempfile.TemporaryDirectory(prefix="tp-offline-admission-") as directory:
        root = pathlib.Path(directory)
        (root / "row.json").write_text(json.dumps(EXACT_ROW))
        (root / "agent.log").write_text(line + "\n")
        (root / "observability.log").write_text(startup + "\n")
        size = len(line) + 1
        args = ["admission-check", "--agent-log", str(root / "agent.log"),
                "--observability-log", str(root / "observability.log"),
                "--observability-offset", "0", "--exact-row", str(root / "row.json")]
        assert m.main(args + ["--agent-offset", "0"]) == 0
        # The only admission line precedes the offset: an earlier run's.
        assert m.main(args + ["--agent-offset", str(size)]) == 1
    passed("admission and observability startup lines")


# --- the harness stage against a fake host ------------------------------


def function(source, name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^\}\n", source, re.M | re.S)
    assert match, f"missing harness function: {name}"
    return match.group(0)


def heredoc_function(source, name):
    match = re.search(r"^" + name + r"\(\) \{\n.*?^PY\n\}\n", source, re.M | re.S)
    assert match, f"missing harness function: {name}"
    return match.group(0)


STAGE_FUNCTIONS = (
    "die", "note", "pass", "run_stage", "offline_helper", "restore_agent_config", "tell_operator",
    "restore_formula_trust", "purge_offline_job", "restore_offline_supervision", "cleanup",
    "wait_for_service", "wait_for_agent_ready", "run_denied", "enter_offline_denial",
    "wait_for_denied_status", "verify_normal_supervision", "verify_offline_runtime",
    "verify_offline_profile",
)

GLOBALS = r'''
set -Eeuo pipefail
evidence_dir="$TP_ROOT/evidence"
work_dir="$TP_ROOT/work"
stage_results="$evidence_dir/stages.tsv"
tap_repo="${TP_TAP_REPO:-}"
baseline_version="${TP_BASELINE_VERSION:-}"
candidate_active=1
agent_config="$TP_ROOT/prefix/etc/tensorplate/agent.json"
agent_config_backup=""
trust_added=()
active_stage=""
active_stage_log=""
active_stage_started=""
lifecycle_marker=""
offline_services_stopped=0
denial_dir=""
offline_profile=""
bundle_dir="$TP_ROOT/bundle"
smoke_deployment_id=smoke-1
offline_deployment_id=offline-1
offline_helper_path="$TP_ROOT/helper.py"
python_bin=python3
restore_tap() { printf 'restore_tap\n' >>"$TP_ROOT/cleanup-calls"; }
restore_baseline() {
  printf 'restore_baseline\n' >>"$TP_ROOT/cleanup-calls"
  if [[ -n "${TP_SLOW_BASELINE:-}" ]]; then
    : >"$TP_ROOT/baseline-started"
    /bin/sleep 10
  fi
}
linked_formula_version() { printf '0.1.2\n'; }
'''

HELPER_WRAPPER = r'''
import errno
import json
import os
import sys
import time

sys.path.insert(0, os.path.join(os.environ["TP_REPO"], "tools", "validation"))
import macos_offline_runtime as m

ROOT = os.environ["TP_ROOT"]
MODES = set(filter(None, os.environ.get("TP_MODE", "").split(",")))
_sleep = time.sleep
m.time.sleep = lambda seconds: _sleep(min(seconds, 0.01))


def state():
    with open(os.path.join(ROOT, "state.json"), encoding="utf-8") as handle:
        return json.load(handle)


def sandboxed():
    return os.environ.get("TP_FAKE_SANDBOXED") == "1"


def sandbox_check(pid, operation):
    current = state()
    proc = current["procs"].get(str(pid))
    if proc is not None:
        return 1 if proc["sandboxed"] else 0
    if pid in current["sandboxed_pids"]:
        return 1
    try:
        os.kill(pid, 0)
    except OSError:
        return 1
    return 0


def refuse(tcp=False):
    if sandboxed() and "probe-leaks" not in MODES:
        raise OSError(errno.EPERM, "Operation not permitted")
    if not sandboxed() and ("control-refused" in MODES or tcp and "control-refused-tcp" in MODES):
        # An application firewall refusing the unsandboxed control.
        raise OSError(errno.EPERM, "Operation not permitted")


def udp_send(host, port, *args, **kwargs):
    refuse()
    if host in ("127.0.0.1", "fe80::1"):
        return None
    raise OSError(errno.ENETUNREACH, "Network is unreachable")


def tcp_connect(host, port, *args, **kwargs):
    refuse(tcp=True)
    raise OSError(errno.ECONNREFUSED if host == "127.0.0.1" else errno.ENETUNREACH, "not reached")


def bind_unlisted_port(host, kind):
    refuse()


def unix_connect(path):
    if path == m.MDNS_SOCKET:
        if sandboxed():
            raise OSError(errno.EPERM, "Operation not permitted")
        return None
    if "homebrew.mxcl.tensorplate-agent" not in state()["labels"]:
        raise OSError(errno.ECONNREFUSED, "Connection refused")
    return None


m.sandbox_check = sandbox_check
m.udp_send = udp_send
m.tcp_connect = tcp_connect
m.bind_unlisted_port = bind_unlisted_port
m.unix_connect = unix_connect
m.child_udp_send = lambda: "EPERM" if sandboxed() and "probe-leaks" not in MODES else "ENETUNREACH"
m.http_get_json = lambda url: {
    "state": "starting" if "health-not-ready" in MODES else "ready",
    "active_model_id": state()["active"],
}
if "probe-crashes" in MODES and sandboxed():
    def crash(*args, **kwargs):
        raise RuntimeError("the probe crashed")
    m.run_probe = crash
sys.exit(m.main())
'''


def trap_block(source):
    match = re.search(r"^# The terminal, kept for tell_operator.*?^trap cleanup EXIT\n", source, re.M | re.S)
    assert match, "the harness no longer saves the terminal and installs its traps before cleanup"
    return match.group(0)


def stage_script(source=SOURCE, body="run_stage offline-runtime verify_offline_runtime\n"):
    functions = "".join(function(source, name) for name in STAGE_FUNCTIONS)
    functions += heredoc_function(source, "probe_mps")
    return functions + GLOBALS + trap_block(source) + body + "trap - EXIT\n"


def tool_env(root, mode=""):
    return dict(os.environ, TP_ROOT=str(root), TP_MODE=mode, TP_REPO=str(REPO),
                HOME=str(root / "home"), PATH=f"{root / 'bin'}{os.pathsep}{os.environ['PATH']}")


def fake(root, *args, mode=""):
    result = subprocess.run([str(root / "bin" / args[0])] + list(args[1:]), env=tool_env(root, mode),
                            capture_output=True, text=True)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout


def make_world(root, scenario="normal", mode=""):
    """A Homebrew prefix with both services running from their normal
    LaunchAgents plists, or in another starting state for purge tests."""
    modes = set(mode.split(","))
    for name in ("bin", "evidence", "work/offline-denial", "home/Library/LaunchAgents",
                 "prefix/etc/tensorplate", "prefix/var/log/tensorplate", "prefix/var/run/tensorplate",
                 "prefix/share/tensorplate/platform/rows", "prefix/opt/pytorch/libexec/bin", "bundle"):
        (root / name).mkdir(parents=True, exist_ok=True)
    for tool in ("brew", "launchctl", "lsof", "pgrep", "ps", "sandbox-exec", "tensorplate", "sleep"):
        (root / "bin" / tool).symlink_to(FAKE_HOST)
    (root / "prefix/opt/pytorch/libexec/bin/python").symlink_to(FAKE_HOST)
    prefix = root / "prefix"
    agent_config = (REPO / "packaging/homebrew/conf/agent.json.in").read_text().replace(
        "@HOMEBREW_PREFIX@", str(prefix))
    if "agent-config-wildcard-host" in modes:
        agent_config = agent_config.replace('"serving_bind_host": "127.0.0.1"', '"serving_bind_host": "0.0.0.0"')
        assert '"0.0.0.0"' in agent_config
    (prefix / "etc/tensorplate/agent.json").write_text(agent_config)
    (prefix / "share/tensorplate/platform/rows/macos26-m1pro-16gb.json").write_text(json.dumps(EXACT_ROW))
    (root / "bundle/manifest.json").write_text("{}\n")
    (root / "helper.py").write_text(HELPER_WRAPPER)
    # Output an earlier run left in the append-only launchd logs.
    (prefix / "var/log/tensorplate/agent.error.log").write_text(
        admission_line().replace("evidence=validated", "evidence=unvalidated (earlier run)") + "\n")
    (prefix / "var/log/tensorplate/observability.error.log").write_text(
        "tensorplate-observability primary_source=internal interval=1000ms (earlier run)\n")
    for service in (AGENT, OBSERVABILITY):
        (prefix / "opt" / service).mkdir(parents=True)
        document = formula_document(str(prefix), service)
        if "formula-plist-program-key" in modes:
            document["Program"] = document["ProgramArguments"][0]
        with open(prefix / "opt" / service / f"homebrew.mxcl.{service}.plist", "wb") as handle:
            plistlib.dump(document, handle)
    # Fake pids start above every OS pid range (macOS 99998, Linux at most
    # 4194304): sandbox_check and ps answer from this table first, so a
    # real helper pid equal to a fake one would read as that process.
    (root / "state.json").write_text(json.dumps(
        {"labels": {}, "procs": {}, "next_pid": 5000000, "active": "smoke-1", "sandboxed_pids": []}))
    if scenario == "empty":
        return
    for service in (OBSERVABILITY, AGENT):
        fake(root, "brew", "services", "start", service)
    if scenario == "normal":
        return
    profile = m.write_profile(str(root / "work/offline-denial/network-denied.sb"),
                              m.render_profile(18080, 18081))
    sandboxed = {"agent-sandboxed": (AGENT,), "both-sandboxed": (AGENT, OBSERVABILITY)}[scenario]
    for service in sandboxed:
        fake(root, "brew", "services", "stop", "--keep", service)
        document = formula_document(str(prefix), service)
        derived = root / "work/offline-denial" / f"homebrew.mxcl.{service}.plist"
        with open(derived, "wb") as handle:
            plistlib.dump(m.derive_plist(document, f"homebrew.mxcl.{service}",
                                         document["ProgramArguments"][0], profile), handle)
        fake(root, "brew", "services", "run", service, f"--file={derived}")
    if scenario == "agent-sandboxed":
        fake(root, "brew", "services", "stop", "--keep", OBSERVABILITY)


CASE = threading.local()


def new_world_root():
    """A directory for one world, removed by run_parallel when its case
    passes and kept for debugging when it fails."""
    root = pathlib.Path(tempfile.mkdtemp(prefix="tp-offline-"))
    getattr(CASE, "roots", []).append(root)
    return root


def run_world(script, mode="", scenario="normal", extra_env=None, signals=None, timeout=240):
    root = new_world_root()
    make_world(root, scenario, mode)
    # Only what the harness does is recorded, not the world's setup.
    (root / "calls.jsonl").write_text("")
    (root / "probe.sh").write_text(script)
    env = dict(tool_env(root, mode), **(extra_env or {}))
    process = subprocess.Popen(["bash", str(root / "probe.sh")], env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    if signals:
        signals(root, process)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate()
        raise AssertionError(f"stage run timed out: {mode}\n{stderr[-2000:]}")
    return World(root, process.returncode, stdout, stderr)


class World:
    def __init__(self, root, returncode, stdout, stderr):
        self.root, self.returncode, self.stdout, self.stderr = root, returncode, stdout, stderr
        self.state = json.loads((root / "state.json").read_text())
        rows = root / "evidence/stages.tsv"
        self.rows = rows.read_text() if rows.exists() else ""
        log = root / "evidence/offline-runtime.log"
        self.log = log.read_text() if log.exists() else ""
        calls = root / "calls.jsonl"
        self.calls = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
        cleanup = root / "cleanup-calls"
        self.cleanup_calls = cleanup.read_text().split() if cleanup.exists() else []

    def context(self):
        return (f"rc={self.returncode}\n--- stage log\n{self.log[-3000:]}\n--- stderr\n"
                f"{self.stderr[-3000:]}\n--- rows\n{self.rows}")

    def assert_normal_supervision(self):
        for service in (AGENT, OBSERVABILITY):
            job = self.state["labels"].get(f"homebrew.mxcl.{service}")
            launch_agent = str(self.root / "home/Library/LaunchAgents" / f"homebrew.mxcl.{service}.plist")
            assert job and job["file"] == launch_agent and job["arguments"][0] != "/usr/bin/sandbox-exec", \
                (service, job, self.context())
        leftovers = [proc for proc in self.state["procs"].values() if proc["sandboxed"]]
        assert not leftovers, (leftovers, self.context())

    def bootouts(self):
        return [call["args"][1].rsplit("/", 1)[-1] for call in self.calls
                if call["tool"] == "launchctl" and call["args"][0] == "bootout"]


def check_clean_run(world):
    assert world.returncode == 0, world.context()
    assert "offline-runtime\tpass\t" in world.rows, world.context()
    world.assert_normal_supervision()
    text = (world.root / "evidence/offline-runtime.json").read_text()
    assert "/" not in text, f"offline-runtime.json carries a path: {text}"
    assert not re.search(r"\b5[0-9]{6}\b", text), f"offline-runtime.json carries a pid: {text}"
    evidence = json.loads(text)
    assert evidence["services"]["agent"]["launchd_runs_through_stage"] == 1
    assert evidence["services_sandboxed_network_denied"]
    assert evidence["process_tree"] == {"processes": 3, "serving_workers": 1, "backend_sidecars": 1,
                                        "all_sandboxed_network_denied": True}, evidence["process_tree"]
    assert set(evidence["probe"]["denied"].values()) == {"EPERM"}, evidence["probe"]
    assert evidence["admission"] == {"row": "macos26-m1pro-16gb", "reason": "none", "evidence": "validated"}
    assert evidence["listeners"]["loopback_only"] and evidence["restore"]["no_sandboxed_process_remains"]
    denied = [call for call in world.calls if call["tool"] == "sandbox-exec"]
    commands = [" ".join(call["args"][2:4]) for call in denied]
    for expected in ("tensorplate status", "tensorplate deploy", "tensorplate infer", "tensorplate doctor",
                     "python3 " + str(world.root / "helper.py"), "/usr/bin/env PYTHONPATH="):
        assert any(command.startswith(expected) for command in commands), (expected, commands)
    assert not any(call["args"][2] == "brew" for call in denied), "Homebrew ran under the profile"
    # Record-first: the raw documents are in the local stage log.
    for raw in ('"command": "doctor"', '"command": "deploy"', "\tprogram = /usr/bin/sandbox-exec"):
        assert raw in world.log, raw
    runs = [call for call in world.calls if call["tool"] == "brew" and call["args"][:2] == ["services", "run"]]
    assert [call["args"][2] for call in runs] == [OBSERVABILITY, AGENT], runs


FAILURE_MODES = {
    # mode: (message in the stage log, whether cleanup can restore normal supervision)
    "stop-leaves-loaded": ("tensorplate-agent is still loaded after brew services stop --keep", True),
    # The agent is already stopped when observability fails to stop, so
    # cleanup must start the agent again.
    "stop-leaves-observability-loaded": ("tensorplate-observability is still loaded after brew services stop "
                                         "--keep", True),
    "port-collision": ("a TensorPlate process or serving-port listener outlived the stopped services", True),
    "stop-leaves-process": ("a TensorPlate process or serving-port listener outlived the stopped services", True),
    "run-fails": ("brew services run --file failed for tensorplate-observability", True),
    "stop-reloads": ("tensorplate-agent is not running as the sandboxed launchd job", True),
    "unsandboxed-sidecar": ("a process in the service trees does not read back as sandboxed with the "
                            "network denied", True),
    "never-ready": ("the agent did not recover the deploy-smoke deployment under the offline profile", True),
    "admission-unvalidated": ("the sandboxed services did not start once on the exact row with validated "
                              "evidence", True),
    "admission-twice": ("the sandboxed services did not start once on the exact row with validated "
                        "evidence", True),
    "deploy-fails": ("tensorplate deploy failed under the offline profile with status 4", True),
    "deploy-keeps-smoke": ("status under the offline profile does not report the new deployment", True),
    "status-exit-nonzero-after-deploy": ("tensorplate status failed under the offline profile after the deploy", True),
    "infer-no-echo": ("inference under the offline profile did not echo the request", True),
    "infer-exit-nonzero": ("tensorplate infer failed under the offline profile", True),
    "probe-leaks": ("the offline profile did not refuse the network as required", True),
    "health-not-ready": ("the offline profile did not refuse the network as required", True),
    "no-sidecar": ("the agent's process tree lacks a serving worker or backend sidecar", True),
    "pgrep-empty": ("the agent's process tree lacks a serving worker or backend sidecar", True),
    "wildcard-listener": ("a TensorPlate process holds a non-loopback socket, or the agent's tree holds no "
                          "serving listener", True),
    "observability-wildcard-listener": ("a TensorPlate process holds a non-loopback socket, or the agent's tree "
                                        "holds no serving listener", True),
    "doctor-failing": ("doctor under the offline profile is not green on the exact row", True),
    "doctor-exit-nonzero": ("doctor under the offline profile is not green on the exact row", True),
    "crash-during-doctor": ("tensorplate-agent restarted during the offline stage", True),
    "launchagents-drift": ("the tensorplate-agent LaunchAgents plist differs from the formula plist", True),
    "agent-not-ready-after": ("the agent did not answer outside the sandbox after the offline stage", True),
    "agent-config-wildcard-host": ("cannot render the offline profile from the installed agent config", True),
    "control-refused": ("the offline profile did not refuse the network as required", True),
    "control-refused-tcp": ("the offline profile did not refuse the network as required", True),
    "formula-plist-program-key": ("cannot derive the sandboxed tensorplate-observability launchd plist", True),
    "run-rewrites-arguments": ("tensorplate-agent is not running as the sandboxed launchd job", True),
    "run-copies-plist": ("tensorplate-agent is not running as the sandboxed launchd job", True),
    "run-sets-program": ("tensorplate-agent is not running as the sandboxed launchd job", True),
    "unsandboxed-observability": ("a launchd service does not read back as sandboxed with the network denied",
                                  True),
    "deploy-phase-failed": ("the deploy under the offline profile did not activate offline-1", True),
    "probe-crashes": ("the network probe did not run under the offline profile", True),
    "mps-unavailable": ("the MPS probe failed under the offline profile", True),
    "crashed-at-start": ("tensorplate-agent restarted during the offline stage", True),
    "rebootstrap-during-doctor": ("tensorplate-agent restarted during the offline stage", True),
    "profile-changed": ("cannot write the offline-runtime evidence", True),
    "start-loads-formula-plist": ("tensorplate-agent is not back under its normal launchd job", False),
    "orphan-sidecar": ("a sandboxed TensorPlate process remains after the offline stage", False),
    "start-not-started": ("normal launchd supervision was not restored after the offline stage", False),
    "start-fails": ("normal launchd supervision was not restored after the offline stage", False),
    "bootout-fails": ("normal launchd supervision was not restored after the offline stage", False),
}


def check_failure(mode, world, message, restorable):
    assert world.returncode != 0, (mode, world.context())
    assert "offline-runtime\tfail\t" in world.rows and "offline-runtime\tpass\t" not in world.rows, \
        (mode, world.context())
    assert f"error: {message}" in world.log.splitlines(), (mode, message, world.context())
    assert f"error: stage offline-runtime failed with exit {world.returncode}" in world.stderr, \
        (mode, world.context())
    assert "restore_tap" in world.cleanup_calls, (mode, world.cleanup_calls)
    if restorable:
        world.assert_normal_supervision()
    if mode == "start-fails":
        assert "error: normal launchd supervision is not restored; run: brew services start " \
               "tensorplate-observability && brew services start tensorplate-agent" in world.stderr, world.context()
        assert not world.state["labels"], world.state["labels"]
    if mode == "bootout-fails":
        assert "remove it with: launchctl bootout gui/" in world.stderr, world.context()
    if mode == "orphan-sidecar":
        assert not [job for job in world.state["labels"].values()
                    if job["arguments"][0] == "/usr/bin/sandbox-exec"], world.state["labels"]


def mutated(old, new, count=1):
    assert SOURCE.count(old) == count, (old, SOURCE.count(old))
    return SOURCE.replace(old, new)


# Copies of the harness with one guard removed; each must fail the run that
# the guard protects.
UNSANDBOXED_CALLS = {
    "status wait": "if run_denied tensorplate status --output json",
    "status after deploy": "  run_denied tensorplate status --output json >",
    "deploy": "run_denied tensorplate deploy",
    "infer": "run_denied tensorplate infer",
    "doctor": "run_denied tensorplate doctor",
    "probe": 'run_denied "$python_bin"',
    "mps probe": "probe_mps run_denied",
}


def stage_cases():
    cases = {"clean run": lambda: check_clean_run(run_world(stage_script()))}
    cases["clean run with a sandboxed sidecar slow to exit after the bootout"] = lambda: check_clean_run(
        run_world(stage_script(), mode="slow-exit-sidecar"))
    cases["clean run with errexit suspended by the caller"] = lambda: check_clean_run(run_world(
        stage_script(body="run_stage offline-runtime verify_offline_runtime || exit 1\n")))
    for mode, (message, restorable) in FAILURE_MODES.items():
        cases[f"failure {mode}"] = (
            lambda mode=mode, message=message, restorable=restorable:
            check_failure(mode, run_world(stage_script(), mode=mode), message, restorable))
    for scenario, message in (
        ("empty", "tensorplate-agent is not loaded before the offline stage"),
        ("agent-sandboxed", "tensorplate-agent is not running under its normal launchd job before the offline stage"),
    ):
        cases[f"failure precondition {scenario}"] = (
            lambda scenario=scenario, message=message:
            check_failure(scenario, run_world(stage_script(), scenario=scenario), message, False))
    cases["failure precondition agent config still broken"] = lambda: check_failure(
        "config", run_world(stage_script(body='agent_config_backup="$TP_ROOT/missing-backup"\n'
                                            "run_stage offline-runtime verify_offline_runtime\n")),
        "the agent config is still replaced by the crash-loop stage", True)
    for mode in ("run-fails", "probe-leaks", "start-fails"):
        message, restorable = FAILURE_MODES[mode]

        def suspended(mode=mode, message=message, restorable=restorable):
            world = run_world(stage_script(body="run_stage offline-runtime verify_offline_runtime || true\n"),
                              mode=mode)
            check_failure(mode, world, message, restorable)
        cases[f"failure {mode} with errexit suspended by the caller"] = suspended

    for name, call in UNSANDBOXED_CALLS.items():
        replacement = call.replace("run_denied ", "").replace(" run_denied", "")

        def unsandboxed(name=name, call=call, replacement=replacement):
            world = run_world(stage_script(mutated(call, replacement)))
            assert world.returncode != 0 and "offline-runtime\tpass\t" not in world.rows, \
                (name, world.context())
            world.assert_normal_supervision()
        cases[f"guard: {name} without run_denied fails"] = unsandboxed

    def stopped_flag_removed():
        world = run_world(stage_script(mutated("  offline_services_stopped=1\n", "")), mode="probe-leaks")
        assert world.returncode != 0
        assert not world.state["labels"], ("normal jobs restarted without the stopped flag", world.state)
    cases["guard: without offline_services_stopped cleanup does not restart the services"] = \
        stopped_flag_removed

    def restore_call_removed():
        source = mutated('  restore_offline_supervision || { [[ "$status" -ne 0 ]] || status=1; }\n', "")
        world = run_world(stage_script(source), mode="probe-leaks")
        assert [job for job in world.state["labels"].values()
                if job["arguments"][0] == "/usr/bin/sandbox-exec"], "restore ran without the cleanup call"
    cases["guard: without the cleanup restore the sandboxed jobs stay loaded"] = restore_call_removed

    def agent_config_not_restored():
        # The crash-loop stage's copy of the good config, and a config
        # directory that cannot take it back.
        body = ('agent_config_backup="$work_dir/agent.json"\n'
                "printf '{\"good\": true}\\n' >\"$agent_config_backup\"\n"
                'rm -rf "$TP_ROOT/prefix/etc/tensorplate"\n: >"$TP_ROOT/prefix/etc/tensorplate"\n'
                "exit 0\n")
        world = run_world(stage_script(body=body))
        assert world.returncode == 1, ("cleanup did not fail the run", world.context())
        backup = world.root / "work/agent.json"
        assert backup.exists() and backup.read_text() == '{"good": true}\n', "the only good config is gone"
        assert f"error: the agent config was not restored; copy {world.root}/work/agent.json to " in world.stderr, \
            world.context()
        assert "restore_tap" in world.cleanup_calls, world.cleanup_calls
    cases["cleanup keeps the agent config backup and fails when it cannot restore it"] = agent_config_not_restored
    return cases


# --- purge from partial states ----------------------------------------


def purge_cases():
    cases = {}
    direct = ('offline_services_stopped="${TP_STOPPED}"\nstatus=0\n'
              'restore_offline_supervision || status=$?\n'
              'printf "restore status %s stopped %s\\n" "$status" "$offline_services_stopped"\n')
    for scenario, stopped, mode, expected_status, bootouts, normal in (
        ("empty", "0", "", 0, [], False),
        ("empty", "1", "", 0, [], True),
        ("agent-sandboxed", "1", "", 0, ["homebrew.mxcl.tensorplate-agent"], True),
        ("both-sandboxed", "1", "", 0, ["homebrew.mxcl.tensorplate-agent",
                                        "homebrew.mxcl.tensorplate-observability"], True),
        ("both-sandboxed", "1", "bootout-fails", 1, ["homebrew.mxcl.tensorplate-agent",
                                                     "homebrew.mxcl.tensorplate-observability"], False),
        ("both-sandboxed", "1", "print-error", 1, [], False),
        # launchctl print in a shape the parser does not know.
        ("both-sandboxed", "1", "print-unparsable", 1, [], False),
        # The agent's job goes, observability's stays.
        ("both-sandboxed", "1", "bootout-fails-observability", 1, ["homebrew.mxcl.tensorplate-agent",
                                                                   "homebrew.mxcl.tensorplate-observability"], False),
        ("normal", "0", "", 0, [], True),
        ("normal", "1", "", 0, [], True),
    ):
        def case(scenario=scenario, stopped=stopped, mode=mode, expected_status=expected_status,
                 bootouts=bootouts, normal=normal):
            script = stage_script(body=direct)
            world = run_world(script, mode=mode, scenario=scenario, extra_env={"TP_STOPPED": stopped})
            assert world.returncode == 0, world.context()
            expected_stopped = stopped if expected_status else "0"
            assert f"restore status {expected_status} stopped {expected_stopped}" in world.stdout, world.context()
            assert world.bootouts() == bootouts, (world.bootouts(), bootouts)
            if normal:
                world.assert_normal_supervision()
            elif scenario == "empty":
                assert not world.state["labels"], world.state
            starts = [call for call in world.calls
                      if call["tool"] == "brew" and call["args"][:2] == ["services", "start"]]
            if mode in ("bootout-fails", "bootout-fails-observability"):
                assert not starts, "normal jobs were started while a sandboxed job stayed loaded"
                assert re.search(r"remove it with: launchctl bootout gui/\d+/homebrew.mxcl.tensorplate-observability",
                                 world.stderr), world.context()
            if mode == "print-unparsable":
                assert not starts, "normal jobs were started while launchd could not be read"
                assert re.search(r"error: cannot tell whether gui/\d+/homebrew.mxcl.tensorplate-agent runs under "
                                 r"sandbox-exec", world.stderr), world.context()
            if mode == "print-error":
                assert not starts, "normal jobs were started while launchd could not be read"
                assert re.search(r"error: launchctl print gui/\d+/homebrew.mxcl.tensorplate-agent failed "
                                 r"with status 5", world.stderr), world.context()
        cases[f"purge {scenario} stopped={stopped} {mode or 'no fault'}"] = case

    def via_cleanup():
        body = 'offline_services_stopped=1\nexit 0\n'
        world = run_world(stage_script(body=body), mode="bootout-fails", scenario="both-sandboxed",
                          extra_env={"TP_TAP_REPO": "/", "TP_BASELINE_VERSION": "0.1.2"})
        assert world.returncode == 1, ("cleanup did not force a failing status", world.context())
        assert world.cleanup_calls == ["restore_baseline"], world.cleanup_calls
        assert "remove it with: launchctl bootout gui/" in world.stderr, world.context()
    cases["purge failure in cleanup still restores the baseline and fails the run"] = via_cleanup
    return cases


# --- signals -----------------------------------------------------------


def wait_for(path, timeout=120):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            return
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {path.name}")


def signal_run(source, sig, second=None, to_group=True, mode="infer-blocks"):
    def deliver(root, process):
        wait_for(root / "infer-blocked")
        if to_group:
            os.killpg(process.pid, sig)
        else:
            os.kill(process.pid, sig)
        if second is not None:
            wait_for(root / "bootout-started")
            os.killpg(process.pid, second)
    return run_world(stage_script(source), mode=mode if second is None else f"{mode},slow-bootout",
                     signals=deliver)


def check_signalled(world, sig):
    code = 128 + int(sig)
    assert world.returncode == code, (sig, world.context())
    assert "offline-runtime\tfail\t" in world.rows, (sig, world.context())
    world.assert_normal_supervision()
    message = f"error: stage offline-runtime failed with exit {code}"
    assert message in world.stderr, (sig, world.context())
    assert message not in world.log, ("cleanup wrote to the stage log", world.context())


def lost_terminal_run(source, sig, terminal):
    """The harness's output on a terminal that goes away mid-stage, then
    `sig` to its group: a pty whose master closes, as when a Terminal
    window closes or an ssh session drops, or a pipe whose reader exits,
    as when Ctrl-C also stops `| tee`."""
    root = new_world_root()
    make_world(root, "normal", "infer-blocks")
    (root / "calls.jsonl").write_text("")
    (root / "probe.sh").write_text(stage_script(source))
    reader, writer = pty.openpty() if terminal == "pty" else os.pipe()
    process = subprocess.Popen(["bash", str(root / "probe.sh")], env=tool_env(root, "infer-blocks"),
                               stdin=subprocess.DEVNULL, stdout=writer, stderr=writer, start_new_session=True)
    os.close(writer)
    try:
        wait_for(root / "infer-blocked")
    finally:
        os.close(reader)
    os.killpg(process.pid, sig)
    try:
        process.wait(timeout=240)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        raise AssertionError(f"stage run timed out after the terminal went away ({terminal})")
    return World(root, process.returncode, "", "")


def check_lost_terminal(world, sig):
    code = 128 + int(sig)
    cleanup_log = world.root / "evidence/cleanup.log"
    cleanup_text = cleanup_log.read_text() if cleanup_log.exists() else ""
    context = world.context() + "\n--- cleanup.log\n" + cleanup_text
    assert world.returncode == code, (sig, context)
    assert "offline-runtime\tfail\t" in world.rows, context
    world.assert_normal_supervision()
    assert f"error: stage offline-runtime failed with exit {code}" in cleanup_text, context
    assert "restore_tap" in world.cleanup_calls, (world.cleanup_calls, context)


def must_fail_lost_terminal(source, sig, terminal):
    """The lost-terminal test must fail on this copy of the harness."""
    def case():
        world = lost_terminal_run(source, sig, terminal)
        try:
            check_lost_terminal(world, sig)
        except AssertionError:
            return
        raise AssertionError("the lost-terminal test passed without the guard")
    return case


def rearmed_run(first, second):
    """`first` mid-stage, then `second` while cleanup restores the Homebrew
    baseline, which an operator must still be able to interrupt."""
    def deliver(root, process):
        wait_for(root / "infer-blocked")
        os.killpg(process.pid, first)
        wait_for(root / "baseline-started")
        os.killpg(process.pid, second)
    return run_world(stage_script(), mode="infer-blocks", signals=deliver,
                     extra_env={"TP_TAP_REPO": "/", "TP_BASELINE_VERSION": "0.1.2", "TP_SLOW_BASELINE": "1"})


def signal_cases():
    cases = {}
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        cases[f"{sig.name} to the process group"] = lambda sig=sig: check_signalled(signal_run(SOURCE, sig), sig)
        cases[f"{sig.name} then a second {sig.name} during the restore"] = (
            lambda sig=sig: check_signalled(signal_run(SOURCE, sig, second=sig), sig))
    cases["SIGTERM then SIGINT during the restore"] = (
        lambda: check_signalled(signal_run(SOURCE, signal.SIGTERM, second=signal.SIGINT), signal.SIGTERM))
    cases["SIGTERM to the harness pid only"] = lambda: check_signalled(
        signal_run(SOURCE, signal.SIGTERM, to_group=False, mode="infer-blocks-short"), signal.SIGTERM)
    cases["SIGHUP after the terminal hung up"] = lambda: check_lost_terminal(
        lost_terminal_run(SOURCE, signal.SIGHUP, "pty"), signal.SIGHUP)
    cases["SIGINT after the output pipe's reader exited"] = lambda: check_lost_terminal(
        lost_terminal_run(SOURCE, signal.SIGINT, "pipe"), signal.SIGINT)
    for first, second in ((signal.SIGTERM, signal.SIGINT), (signal.SIGINT, signal.SIGTERM),
                          (signal.SIGINT, signal.SIGHUP)):
        def rearmed(first=first, second=second):
            world = rearmed_run(first, second)
            assert world.returncode == 128 + int(second), (
                f"{second.name} did not end the baseline restore", world.context())
        cases[f"{first.name} then {second.name} during the baseline restore ends it"] = rearmed

    # Cleanup writing to the terminal itself, as it did before cleanup.log:
    # a closed pipe kills bash with SIGPIPE on any bash.
    cases["guard: without cleanup.log a lost output pipe stops the restore"] = must_fail_lost_terminal(
        mutated('  exec 1>>"${evidence_dir}/cleanup.log" 2>&1\n', "  exec 1>&4 2>&4\n"), signal.SIGINT, "pipe")
    cases["guard: a builtin write to the terminal in tell_operator stops the restore"] = must_fail_lost_terminal(
        mutated("  /usr/bin/printf '%s\\n' \"$*\" >&4 2>/dev/null || true\n",
                "  printf '%s\\n' \"$*\" >&4 2>/dev/null || true\n"), signal.SIGINT, "pipe")

    def must_fail(source, sig, symptom, second=None):
        """The signal test must fail on this copy, showing `symptom`."""
        def case():
            world = signal_run(source, sig, second=second)
            try:
                check_signalled(world, sig)
            except AssertionError:
                assert symptom(world), ("failed for another reason", world.context())
                return
            raise AssertionError("the signal test passed without the guard")
        return case

    def still_sandboxed(world):
        return any(job["arguments"][0] == "/usr/bin/sandbox-exec" for job in world.state["labels"].values()) \
            or len(world.state["labels"]) < 2

    # These guards answer how macOS /bin/bash 3.2 behaves, which is the only
    # shell the harness runs under: after an untrapped TERM or HUP it runs
    # the EXIT trap with status 0, a second signal during cleanup runs its
    # exit trap, and cleanup inherits the failed stage's redirection. Another
    # bash may differ, so the guards are removed and exercised under 3.2
    # only; test_static requires them in the harness on every bash.
    bash_version = subprocess.run(["bash", "-c", "printf %s \"${BASH_VERSINFO[0]}\""],
                                  capture_output=True, text=True).stdout
    if bash_version != "3":
        print(f"macOS offline runtime: signal guard discrimination: skipped (bash {bash_version}, "
              "not macOS /bin/bash 3.2)")
        return cases
    cases["guard: without the TERM trap the TERM test fails"] = must_fail(
        mutated("trap 'exit 143' TERM\ntrap 'exit 129' HUP\ntrap cleanup EXIT",
                "trap 'exit 129' HUP\ntrap cleanup EXIT"), signal.SIGTERM,
        lambda world: world.returncode != 143 or "offline-runtime\tfail\t" not in world.rows)
    cases["guard: without the HUP trap the HUP test fails"] = must_fail(
        mutated("trap 'exit 129' HUP\ntrap cleanup EXIT", "trap cleanup EXIT"), signal.SIGHUP,
        lambda world: world.returncode != 129 or "offline-runtime\tfail\t" not in world.rows)
    cases["guard: without ignoring signals in cleanup a second TERM interrupts the restore"] = must_fail(
        mutated("  trap '' INT TERM HUP\n", ""), signal.SIGTERM, still_sandboxed, second=signal.SIGTERM)
    # bash 3.2 keeps a failed builtin write buffered and writes it into the
    # next command substitution, which corrupts the launchctl target.
    cases["guard: without cleanup.log a hung-up terminal stops the restore"] = must_fail_lost_terminal(
        mutated('  exec 1>>"${evidence_dir}/cleanup.log" 2>&1\n', "  exec 1>&4 2>&4\n"), signal.SIGHUP, "pty")
    cases["guard: without the cleanup log redirection cleanup messages land in the stage log"] = must_fail(
        mutated('  exec 1>>"${evidence_dir}/cleanup.log" 2>&1\n', ""), signal.SIGTERM,
        lambda world: "error: stage offline-runtime failed with exit 143" in world.log)
    return cases


# --- static ------------------------------------------------------------


def test_static():
    lines = SOURCE.splitlines()

    def line_of(text):
        matches = [index for index, line in enumerate(lines) if line == text]
        assert len(matches) == 1, (text, matches)
        return matches[0]

    assert line_of("run_stage launchd-crash-loop exercise_crash_loop") < \
        line_of("run_stage offline-runtime verify_offline_runtime") < \
        line_of("run_stage uninstall uninstall_candidate"), "offline-runtime is out of order"
    assert line_of("run_stage tap-trust verify_tap_trust") < \
        line_of("run_stage offline-profile verify_offline_profile") < \
        line_of('if [[ "$preflight_only" == "1" ]]; then') < \
        line_of("run_stage clean-install install_candidate_clean"), \
        "offline-profile must run in the preflight path before any Homebrew change"
    assert not re.search(r"run_denied\s+(HOMEBREW_\w+=\S+\s+)*brew\b", SOURCE), "run_denied wraps brew"
    assert trap_block(SOURCE) == (
        "# The terminal, kept for tell_operator: a stage's output goes to its log.\n"
        "exec 4>&2\n"
        "# Without these, bash runs the EXIT trap with status 0 after TERM or HUP,\n"
        "# and the interrupted stage would record no fail row.\n"
        "trap 'exit 130' INT\ntrap 'exit 143' TERM\ntrap 'exit 129' HUP\ntrap cleanup EXIT\n"
    ), trap_block(SOURCE)
    # cleanup reaches these; die would skip the rest of the restore.
    for name in ("purge_offline_job", "restore_offline_supervision", "wait_for_service",
                 "offline_helper", "restore_agent_config", "tell_operator"):
        assert not re.search(r"\bdie\b", function(SOURCE, name)), f"{name} calls die"
    cleanup = function(SOURCE, "cleanup")
    assert cleanup.startswith("cleanup() {\n  status=$?\n"), "cleanup must read the exit status first"
    redirect = 'exec 1>>"${evidence_dir}/cleanup.log" 2>&1'
    # errexit would end cleanup at a failed redirection.
    assert cleanup.index("trap '' INT TERM HUP") < cleanup.index("set +e") < cleanup.index(redirect) < \
        cleanup.index("printf '%s\\tfail"), "cleanup must leave the stage log before it reports"
    assert cleanup.index("trap '' INT TERM HUP") < cleanup.index("restore_agent_config") < \
        cleanup.index("restore_offline_supervision") < cleanup.index("restore_baseline"), cleanup
    rearm = cleanup.index("trap 'exit 130' INT\n  trap 'exit 143' TERM\n  trap 'exit 129' HUP\n")
    assert cleanup.index("restore_offline_supervision") < rearm < cleanup.index("restore_baseline"), \
        "cleanup ignores signals outside the critical restore"
    # Only external commands write to the terminal copy.
    for function_name in ("cleanup", "purge_offline_job", "restore_offline_supervision"):
        body = function(SOURCE, function_name)
        assert not re.search(r"^\s*(printf|echo)\b.*>&[24]", body, re.M), f"{function_name} writes with a builtin"

    runbook = (REPO / "docs/validation/physical-row-runbooks.md").read_text()
    section = runbook.split("\n## MacBook Pro M1 Pro\n", 1)[1].split("\n## ", 1)[0]
    block = next(item for item in re.findall(r"```bash\n(.*?)```", section, re.S)
                 if "lifecycle-report-from-stages.sh" in item)
    assert "offline-runtime=offline" in block.split()
    assert not re.search(r"(?<!\S)offline-profile=", block), \
        "offline-profile is a preflight check; mapping it lets a preflight-only run claim offline"

    with tempfile.TemporaryDirectory(prefix="tp-offline-transcript-") as directory:
        root = pathlib.Path(directory)
        runtime = {"deployment_id": "offline-1", "services_sandboxed_network_denied": True}
        preflight = {"control": {"udp_test_net_v4": "ENETUNREACH"}}
        (root / "offline-runtime.json").write_text(json.dumps(runtime))
        (root / "offline-profile.json").write_text(json.dumps(preflight))
        (root / "stages.tsv").write_text(
            "stage\tstatus\tstarted_at\tfinished_at\tlog\n"
            "offline-profile\tpass\t2026-01-01T00:00:00Z\t2026-01-01T00:00:01Z\toffline-profile.log\n"
            "offline-runtime\tpass\t2026-01-01T00:00:00Z\t2026-01-01T00:00:01Z\toffline-runtime.log\n")
        script = heredoc_function(SOURCE, "write_sanitized_transcript") + (
            'set -Eeuo pipefail\nstage_results="$TP_ROOT/stages.tsv"\nevidence_dir="$TP_ROOT"\n'
            'baseline_version=0.1.2\ncandidate_version=0.2.1\nwrite_sanitized_transcript\n')
        (root / "probe.sh").write_text(script)
        result = subprocess.run(["bash", str(root / "probe.sh")], capture_output=True, text=True,
                                env=dict(os.environ, TP_ROOT=directory))
        assert result.returncode == 0, result.stderr
        stages = {item["stage"]: item["summary"] for item in
                  json.loads((root / "sanitized-transcript.json").read_text())["stages"]}
        assert stages == {"offline-profile": preflight, "offline-runtime": runtime}, stages
    passed("stage order, runbook mapping, cleanup shape and transcript")


def test_preflight_stage_body():
    """verify_offline_profile hands the helper a work directory and fails
    the stage when the helper does."""
    body = function(SOURCE, "verify_offline_profile")
    for helper_status, expect_pass in ((0, True), (1, False)):
        with tempfile.TemporaryDirectory(prefix="tp-offline-preflight-stage-") as directory:
            root = pathlib.Path(directory)
            (root / "evidence").mkdir()
            script = "".join(function(SOURCE, name) for name in ("die", "note", "pass", "run_stage")) + body + (
                'set -Eeuo pipefail\nevidence_dir="$TP_ROOT/evidence"\nwork_dir="$TP_ROOT/work"\n'
                'stage_results="$evidence_dir/stages.tsv"\n'
                'offline_helper() { printf "%s\\n" "$*" >"$TP_ROOT/args"; return "$TP_STATUS"; }\n'
                "run_stage offline-profile verify_offline_profile\n")
            (root / "probe.sh").write_text(script)
            result = subprocess.run(["bash", str(root / "probe.sh")], capture_output=True, text=True,
                                    env=dict(os.environ, TP_ROOT=directory, TP_STATUS=str(helper_status)))
            rows = (root / "evidence/stages.tsv").read_text() if (root / "evidence/stages.tsv").exists() else ""
            assert (root / "args").read_text().split() == [
                "preflight", "--work-dir", f"{directory}/work/offline-profile",
                "--out", f"{directory}/evidence/offline-profile.json"]
            assert (result.returncode == 0) == expect_pass, result.stderr
            assert ("offline-profile\tpass\t" in rows) == expect_pass, rows
    passed("offline-profile stage body")


# --- macOS only --------------------------------------------------------


BASE = "(version 1)(allow default)(deny network*)"
UNIX = "(allow network-bind network-inbound (local unix-socket))(allow network-outbound (remote unix-socket))"
RESOLVER = '(deny network-outbound (remote unix-socket (path-literal "/private/var/run/mDNSResponder")))'


def scoped(serving, candidate):
    return (f'(allow network-inbound (local ip "localhost:{serving}") (local ip "localhost:{candidate}"))'
            f'(allow network-outbound (remote ip "localhost:{serving}") (remote ip "localhost:{candidate}"))')


MUTANT_PROFILES = {
    "deny-network-only": lambda p, c: BASE,
    "draft-localhost-any-port": lambda p, c: BASE + '(allow network-inbound (local ip "localhost:*"))'
                                             '(allow network-outbound (remote ip "localhost:*"))' + UNIX + RESOLVER,
    "allow-all-local-ip": lambda p, c: BASE + '(allow network* (local ip "localhost:*"))' + UNIX + RESOLVER,
    "resolver-deny-first": lambda p, c: BASE + RESOLVER + scoped(p, c) + UNIX,
    "resolver-unresolved-path": lambda p, c: m.render_profile(p, c).replace(
        "/private/var/run/mDNSResponder", "/var/run/mDNSResponder"),
    "no-inbound-allow": lambda p, c: BASE + f'(allow network-outbound (remote ip "localhost:{p}") '
                                     f'(remote ip "localhost:{c}"))' + UNIX + RESOLVER,
    # Each adds one rule the lint refuses but a probe of only fixed
    # destinations would miss: port scoping lost for TCP, host scoping
    # lost on the serving ports, or binds on any address and port.
    "tcp-loopback-any-port": lambda p, c: BASE + scoped(p, c) + '(allow network-outbound (remote tcp "localhost:*"))'
                                          + UNIX + RESOLVER,
    "tcp-any-host-serving-ports": lambda p, c: BASE + scoped(p, c) + f'(allow network-outbound (remote tcp "*:{p}") '
                                               f'(remote tcp "*:{c}"))' + UNIX + RESOLVER,
    "udp-any-host-serving-ports": lambda p, c: BASE + scoped(p, c) + f'(allow network-outbound (remote udp "*:{p}") '
                                               f'(remote udp "*:{c}"))' + UNIX + RESOLVER,
    "tcp-listen-any-address": lambda p, c: BASE + scoped(p, c) + '(allow network-inbound (local tcp "*:*"))'
                                           + UNIX + RESOLVER,
    "udp-bind-any-address": lambda p, c: BASE + scoped(p, c) + '(allow network-inbound (local udp "*:*"))'
                                         + UNIX + RESOLVER,
}
NO_BASE_DENY = (lambda p, c: '(version 1)(allow default)(deny network-outbound (remote ip "*:*"))'
                f'(allow network-outbound (remote ip "localhost:{p}") (remote ip "localhost:{c}"))')


def test_darwin():
    if platform.system() != "Darwin" or not os.path.exists("/usr/bin/sandbox-exec"):
        print("macOS offline runtime: real sandbox-exec preflight: skipped (not macOS or no sandbox-exec)")
        return
    with tempfile.TemporaryDirectory(prefix="tp-offline-darwin-") as directory:
        result, failures = m.preflight(os.path.join(directory, "final"))
        assert failures == [], (failures, result)
        assert all(result["readback_controls"].values())
        for variant, render in MUTANT_PROFILES.items():
            result, failures = m.preflight(os.path.join(directory, variant), render=render)
            assert failures == RECORDED[variant], (variant, failures, result["probe"])
        # With no base deny, the probe is refused almost everything and only
        # the readback shows the profile does not deny inbound.
        refused(lambda: m.preflight(os.path.join(directory, "no-base-deny"), render=NO_BASE_DENY),
                "does not read as sandboxed with the network denied")
    passed("real sandbox-exec preflight against the rendered profile and its mutants")


def run_case(case):
    CASE.roots = []
    try:
        case()
    except Exception as error:
        error.world_roots = CASE.roots
        raise
    for root in CASE.roots:
        shutil.rmtree(root, ignore_errors=True)


def run_parallel(cases):
    failures = []
    workers = min(8, os.cpu_count() or 2)
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(run_case, case): name for name, case in cases.items()}
        for future in concurrent.futures.as_completed(futures):
            name = futures[future]
            try:
                future.result()
                passed(name)
            except Exception as error:  # report every failing case, then fail
                kept = " ".join(str(root) for root in getattr(error, "world_roots", []))
                failures.append(f"{name}: {type(error).__name__}: {error}\n(kept: {kept or 'nothing'})")
    if failures:
        raise SystemExit("FAIL:\n" + "\n\n".join(failures))


def main():
    only = sys.argv[1:]
    # Started in the background by a non-interactive shell, this process
    # inherits SIGINT ignored, and so would every harness it runs: bash
    # cannot trap a signal ignored at entry, and the INT cases would fail.
    # A handler, unlike SIG_IGN, is reset to the default across exec.
    if signal.getsignal(signal.SIGINT) is signal.SIG_IGN:
        signal.signal(signal.SIGINT, signal.default_int_handler)
    for test in (test_profile, test_derive_plist, test_launchd_job, test_sandbox_readback,
                 test_processes_and_listeners, test_classify, test_cli_checks, test_admission,
                 test_static, test_preflight_stage_body):
        if not only or test.__name__ in only:
            test()
    cases = {}
    for group in (stage_cases, purge_cases, signal_cases):
        if not only or group.__name__ in only:
            cases.update(group())
    run_parallel(cases)
    if not only or "test_darwin" in only:
        test_darwin()


if __name__ == "__main__":
    main()
