#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""A fake macOS host for the offline-runtime stage tests.

Symlinked under the names brew, launchctl, lsof, pgrep, ps, sandbox-exec,
tensorplate, sleep and the PyTorch formula's python, it models just
enough of Homebrew services, launchd, the process table and the
TensorPlate CLI for the harness's offline stage to run against it. State
lives in $TP_ROOT/state.json; every call is appended to calls.jsonl.

TP_MODE is a comma-separated list of faults to inject. Nothing here
touches the real host: no launchd job, socket or Homebrew state.
"""

import json
import os
import plistlib
import re
import shutil
import signal
import sys
import time

ROOT = os.environ["TP_ROOT"]
MODES = set(filter(None, os.environ.get("TP_MODE", "").split(",")))
PREFIX = os.path.join(ROOT, "prefix")
LAUNCH_AGENTS = os.path.join(ROOT, "home", "Library", "LaunchAgents")
STATE = os.path.join(ROOT, "state.json")
SANDBOX_EXEC = "/usr/bin/sandbox-exec"
AGENT, OBSERVABILITY = "tensorplate-agent", "tensorplate-observability"
ADMISSION = ("platform admission: row={row} reason=none posture=technical_prerequisites "
             "(row floor) evidence={evidence} max_resident_model_memory=17179869184")


def label_of(service):
    return f"homebrew.mxcl.{service}"


def formula_plist(service):
    return os.path.join(PREFIX, "opt", service, f"homebrew.mxcl.{service}.plist")


def launch_agent(service):
    return os.path.join(LAUNCH_AGENTS, f"homebrew.mxcl.{service}.plist")


def load_state():
    with open(STATE, encoding="utf-8") as handle:
        return json.load(handle)


def save_state(state):
    temporary = STATE + f".{os.getpid()}"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(state, handle, indent=1, sort_keys=True)
    os.replace(temporary, STATE)


def record(name, args):
    with open(os.path.join(ROOT, "calls.jsonl"), "a", encoding="utf-8") as handle:
        handle.write(json.dumps({"tool": name, "args": args,
                                 "sandboxed": os.environ.get("TP_FAKE_SANDBOXED") == "1"}) + "\n")


def marker(name):
    with open(os.path.join(ROOT, name), "w", encoding="utf-8") as handle:
        handle.write("1\n")


def append_log(name, line):
    with open(os.path.join(PREFIX, "var", "log", "tensorplate", name), "a", encoding="utf-8") as handle:
        handle.write(line + "\n")


def denial_active(state):
    return any(job["arguments"][0] == SANDBOX_EXEC for job in state["labels"].values())


def new_process(state, args, sandboxed, parent, label):
    pid = state["next_pid"]
    state["next_pid"] += 1
    state["procs"][str(pid)] = {
        "args": " ".join(args), "comm": args[0], "sandboxed": sandboxed,
        "parent": parent, "label": label, "lstart": f"Mon Jan  5 00:00:{pid % 60:02d} 2026",
    }
    return pid


def load_job(state, service, path):
    with open(path, "rb") as handle:
        arguments = plistlib.load(handle)["ProgramArguments"]
    sandboxed = arguments[0] == SANDBOX_EXEC
    executed = arguments[3:] if sandboxed else arguments
    if sandboxed and "run-rewrites-arguments" in MODES:
        arguments = arguments[:2] + ["/private/tmp/other-profile.sb"] + arguments[3:]
    # A Program key would make launchd run the service binary directly,
    # outside the sandbox, whatever the arguments say.
    program = executed[0] if sandboxed and "run-sets-program" in MODES else arguments[0]
    label = label_of(service)
    pid = new_process(state, executed,
                      sandboxed and program == SANDBOX_EXEC and
                      not ("unsandboxed-observability" in MODES and service == OBSERVABILITY),
                      1, label)
    runs = 2 if sandboxed and service == AGENT and "crashed-at-start" in MODES else 1
    state["labels"][label] = {"file": path, "arguments": arguments, "program": program, "pid": pid,
                              "runs": runs}
    if service == AGENT:
        serving = new_process(
            state, [os.path.join(PREFIX, "opt/tensorplate-serving/libexec/tensorplate-serving"),
                    "--config", "worker.json"], sandboxed, pid, label)
        if "no-sidecar" not in MODES:
            new_process(state, [os.path.join(PREFIX, "opt/pytorch/libexec/bin/python"), "-m",
                                "tensorplate_pytorch_backend", "--socket", "sidecar.sock"],
                        sandboxed and "unsandboxed-sidecar" not in MODES, serving, label)
        evidence = "unvalidated (admitted on technical prerequisites)" \
            if "admission-unvalidated" in MODES else "validated"
        append_log("agent.error.log", ADMISSION.format(row="macos26-m1pro-16gb", evidence=evidence))
        if "admission-twice" in MODES and sandboxed:
            append_log("agent.error.log", ADMISSION.format(row="macos26-m1pro-16gb", evidence=evidence))
    else:
        append_log("observability.error.log",
                   "tensorplate-observability primary_source=internal interval=1000ms ros2_enabled=false")


def unload_job(state, label):
    state["labels"].pop(label, None)
    for pid, proc in list(state["procs"].items()):
        if proc["label"] != label:
            continue
        if MODES & {"orphan-sidecar", "slow-exit-sidecar"} and proc["sandboxed"] and \
                "-m tensorplate_pytorch_backend" in proc["args"]:
            proc.update(parent=1, label=None)
            state["orphaned_at_pgrep_call"] = state.get("pgrep_u_calls", 0)
            continue
        del state["procs"][pid]


def fake_brew(args):
    state = load_state()
    if args == ["--prefix"]:
        print(PREFIX)
        return 0
    if len(args) == 2 and args[0] == "--prefix":
        print(os.path.join(PREFIX, "opt", args[1]))
        return 0
    if args[:1] != ["services"]:
        return 9
    action, rest = args[1], args[2:]
    if action == "list":
        for label in sorted(state["labels"]):
            print(f"{label[len('homebrew.mxcl.'):]} started")
        return 0
    service = next(item for item in rest if not item.startswith("-"))
    label = label_of(service)
    job = state["labels"].get(label)
    if action == "info":
        print(json.dumps([{
            "name": service, "loaded": job is not None, "status": "started" if job else "none",
            "pid": job["pid"] if job else None, "loaded_file": job["file"] if job else None,
        }]))
        return 0
    if action == "stop":
        if "stop-leaves-loaded" not in MODES and not (
                "stop-leaves-observability-loaded" in MODES and service == OBSERVABILITY):
            unload_job(state, label)
            if "--keep" not in rest:
                os.unlink(launch_agent(service))
            if "stop-leaves-process" in MODES and service == AGENT:
                # A worker launchd no longer tracks, holding no listener.
                new_process(state, [os.path.join(PREFIX, "opt/tensorplate-serving/libexec/tensorplate-serving"),
                                    "--config", "worker.json"], False, 1, None)
        save_state(state)
        return 0
    if action == "run":
        path = next(item.split("=", 1)[1] for item in rest if item.startswith("--file="))
        if "stop-reloads" in MODES and job is None:
            load_job(state, service, launch_agent(service))
            save_state(state)
            job = state["labels"][label]
        if job is not None:
            print(f"Service `{service}` already running, use `brew services restart {service}` to restart.")
            return 0
        if "run-fails" in MODES and service == OBSERVABILITY:
            return 1
        if "run-copies-plist" in MODES:
            shutil.copyfile(path, launch_agent(service))
            path = launch_agent(service)
        load_job(state, service, path)
        save_state(state)
        return 0
    if action == "start":
        if job is not None:
            print(f"Service `{service}` already started, use `brew services restart {service}` to restart.")
            return 0
        if "start-fails" in MODES and state.get("offline_done"):
            return 1
        if "start-not-started" in MODES and state.get("offline_done"):
            return 0
        shutil.copyfile(formula_plist(service), launch_agent(service))
        if "launchagents-drift" in MODES and state.get("offline_done"):
            with open(launch_agent(service), "ab") as handle:
                handle.write(b"\n")
        loaded_from = formula_plist(service) if "start-loads-formula-plist" in MODES and \
            state.get("offline_done") else launch_agent(service)
        load_job(state, service, loaded_from)
        save_state(state)
        # Like Homebrew, report success after the job is loaded, and exit
        # non-zero when that report cannot be written.
        try:
            print(f"==> Successfully started `{service}` (label: {label})", flush=True)
        except OSError:
            return 1
        return 0
    return 9


def launchctl_document(state, label):
    job = state["labels"][label]
    arguments = "".join(f"\t\t{item}\n" for item in job["arguments"])
    return (
        f"gui/501/{label} = {{\n\tactive count = 1\n\tpath = {job['file']}\n"
        f"\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = {job['program']}\n"
        f"\targuments = {{\n{arguments}\t}}\n\n\tenvironment = {{\n\t\tXPC_SERVICE_NAME => {label}\n\t}}\n\n"
        f"\truns = {job['runs']}\n\tpid = {job['pid']}\n\tlast exit code = (never exited)\n\n"
        f"\tresource coalition = {{\n\t\tID = 1\n\t\tstate = active\n\t\tpid = 1\n\t}}\n}}\n"
    )


def fake_launchctl(args):
    state = load_state()
    if len(args) != 2 or args[0] not in ("print", "bootout"):
        return 9
    label = args[1].rsplit("/", 1)[-1]
    if label not in state["labels"]:
        print(f"Could not find service \"{label}\" in domain", file=sys.stderr)
        return 113
    if args[0] == "print":
        if "print-error" in MODES:
            print("Unexpected launchctl failure", file=sys.stderr)
            return 5
        document = launchctl_document(state, label)
        if "print-unparsable" in MODES:
            # A format the parser does not know: no closing brace.
            document = document[:document.rindex("}")]
        print(document, end="")
        return 0
    if "bootout-fails" in MODES or ("bootout-fails-observability" in MODES and label == label_of(OBSERVABILITY)):
        return 5
    if "slow-bootout" in MODES:
        marker("bootout-started")
        time.sleep(3)
        state = load_state()
    unload_job(state, label)
    save_state(state)
    return 0


def fake_lsof(args):
    state = load_state()
    if "-sTCP:LISTEN" in args:
        if "port-collision" in MODES:
            print("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME")
            print("python 99 user 3u IPv4 0x0 0t0 TCP 127.0.0.1:18080 (LISTEN)")
            return 0
        return 1
    pids = args[args.index("-p") + 1].split(",")
    for pid in pids:
        proc = state["procs"].get(pid)
        if proc and os.path.basename(proc["args"].split()[0]) == "tensorplate-serving":
            print(f"p{pid}\nf3\nPTCP\nn127.0.0.1:18080\nTST=LISTEN\nTQR=0\nTQS=0")
            print("f9\nPTCP\nn127.0.0.1:18080->127.0.0.1:50000\nTST=ESTABLISHED")
            if "wildcard-listener" in MODES:
                print("f11\nPTCP\nn*:18081\nTST=LISTEN")
        if proc and proc["label"] == label_of(OBSERVABILITY) and "observability-wildcard-listener" in MODES:
            print(f"p{pid}\nf5\nPUDP\nn*:18081")
    return 1


def fake_pgrep(args):
    state = load_state()
    if "pgrep-empty" in MODES:
        return 1
    if args[:1] == ["-P"]:
        found = [pid for pid, proc in state["procs"].items() if str(proc["parent"]) == args[1]]
    elif args[:1] == ["-u"] and args[2] == "-f":
        # A sandboxed sidecar outlives its job's bootout for a moment: it
        # is still listed the first time the restore looks.
        state["pgrep_u_calls"] = state.get("pgrep_u_calls", 0) + 1
        if "slow-exit-sidecar" in MODES and "orphaned_at_pgrep_call" in state and \
                state["pgrep_u_calls"] >= state["orphaned_at_pgrep_call"] + 2:
            state["procs"] = {pid: proc for pid, proc in state["procs"].items() if proc["label"]}
        save_state(state)
        found = [pid for pid, proc in state["procs"].items() if re.search(args[3], proc["args"])]
    else:
        return 2
    for pid in found:
        print(pid)
    return 0 if found else 1


def real_tool(name):
    fake_dir = os.path.dirname(os.path.abspath(sys.argv[0]))
    path = os.pathsep.join(item for item in os.environ["PATH"].split(os.pathsep)
                           if os.path.abspath(item) != fake_dir)
    return shutil.which(name, path=path)


def fake_ps(args):
    state = load_state()
    fields, pid = args[args.index("-o") + 1], args[args.index("-p") + 1]
    proc = state["procs"].get(pid)
    if proc is None:
        os.execv(real_tool("ps"), ["ps"] + args)
    if fields == "stat=,lstart=,comm=":
        print(f"S {proc['lstart']} {proc['comm']}")
    elif fields == "args=":
        print(proc["args"])
    else:
        return 2
    return 0


def fake_sandbox_exec(args):
    if len(args) < 3 or args[0] != "-f" or not os.path.isfile(args[1]):
        return 65
    state = load_state()
    state["sandboxed_pids"].append(os.getpid())
    save_state(state)
    command = args[2:]
    executable = command[0] if os.sep in command[0] else shutil.which(command[0])
    if not executable:
        return 71
    os.execve(executable, command, dict(os.environ, TP_FAKE_SANDBOXED="1"))
    return 71


def status_document(state):
    agent_job = state["labels"].get(label_of(AGENT))
    sandboxed = bool(agent_job) and agent_job["arguments"][0] == SANDBOX_EXEC
    ready = not ("never-ready" in MODES and sandboxed)
    return {
        "command": "status", "status": "ok",
        "payload": {
            "severity": "ready" if ready else "degraded",
            "agent": {
                "agent_state": "ready" if ready else "starting",
                "active": {"deployment_id": state["active"], "backend": "python_pytorch",
                           "serving_url": "http://127.0.0.1:18080/infer"},
            },
        },
    }


def fake_tensorplate(args):
    state = load_state()
    if denial_active(state) and os.environ.get("TP_FAKE_SANDBOXED") != "1":
        print("fake tensorplate: unsandboxed CLI call while the services run under the offline profile",
              file=sys.stderr)
        return 97
    if label_of(AGENT) not in state["labels"]:
        print("error: agent unreachable", file=sys.stderr)
        return 3
    command = args[0]
    if command == "status":
        if "agent-not-ready-after" in MODES and not denial_active(state) and state.get("offline_done"):
            return 3
        print(json.dumps(status_document(state)))
        # A good document with a failing exit, once the fresh deploy is in.
        if "status-exit-nonzero-after-deploy" in MODES and state.get("offline_done") and denial_active(state):
            return 1
        return 0
    if command == "deploy":
        if "deploy-fails" in MODES:
            print("error: deploy failed", file=sys.stderr)
            return 4
        deployment = args[args.index("--deployment-id") + 1]
        if "deploy-keeps-smoke" not in MODES:
            state["active"] = deployment
        state["offline_done"] = True
        save_state(state)
        phase = "failed" if "deploy-phase-failed" in MODES else "active"
        print(json.dumps({"command": "deploy", "status": "ok",
                          "payload": {"phase": phase, "deployment_id": deployment}}))
        return 0
    if command == "infer":
        if "infer-blocks" in MODES:
            marker("infer-blocked")
            time.sleep(60)
        if "infer-blocks-short" in MODES:
            marker("infer-blocked")
            time.sleep(2)
        with open(args[args.index("--input") + 1], encoding="utf-8") as handle:
            request = json.load(handle)
        sent = request["inputs"][0]
        tensor = dict(sent["tensor"], byte_offset=0, byte_size=16)
        if "infer-no-echo" in MODES:
            tensor["shape"] = [4]
        outputs = [{"name": "echo_probe", "tensor": tensor, "payload_b64": sent["payload_b64"]}]
        with open(args[args.index("--output-file") + 1], "w", encoding="utf-8") as handle:
            json.dump({"outputs": outputs}, handle)
        # An echoed response with a failing exit.
        return 1 if "infer-exit-nonzero" in MODES else 0
    if command == "doctor":
        if "crash-during-doctor" in MODES:
            job = state["labels"][label_of(AGENT)]
            job["runs"] += 1
            job["pid"] = new_process(state, ["agent-restarted"], True, 1, label_of(AGENT))
            save_state(state)
        if "rebootstrap-during-doctor" in MODES:
            path = state["labels"][label_of(AGENT)]["file"]
            unload_job(state, label_of(AGENT))
            load_job(state, AGENT, path)
            save_state(state)
        if "profile-changed" in MODES:
            profile = os.path.join(ROOT, "work", "offline-denial", "network-denied.sb")
            os.chmod(profile, 0o644)
            with open(profile, "a", encoding="utf-8") as handle:
                handle.write("(allow network*)")
        failing = 1 if "doctor-failing" in MODES else 0
        findings = [{"id": name, "status": "ok", "severity": "info", "message": "ok"}
                    for name in ("platform_profile", "agent_reachable", "agent_service_state",
                                 "observability_service_state")]
        if "--skip-agent" in args:
            findings[1].update(status="skipped", message="agent probe skipped via --skip-agent")
        findings.append({"id": "platform_row", "status": "ok", "severity": "info",
                         "message": "resolves to support row `macos26-m1pro-16gb`"})
        print(json.dumps({"command": "doctor", "status": "ok",
                          "payload": {"failing": failing, "total": len(findings), "findings": findings}}))
        return 10 if failing or "doctor-exit-nonzero" in MODES else 0
    return 64


def fake_python(args):
    state = load_state()
    if denial_active(state) and os.environ.get("TP_FAKE_SANDBOXED") != "1":
        print("fake python: unsandboxed MPS probe while the services run under the offline profile",
              file=sys.stderr)
        return 97
    sys.stdin.read()
    available = "mps-unavailable" not in MODES
    print(json.dumps({"accelerator_runtime_built": True, "accelerator_runtime_available": available}))
    return 0 if available else 1


def main():
    name = os.path.basename(sys.argv[0])
    args = sys.argv[1:]
    # Die from SIGINT like a real command, so bash acts on the signal.
    if signal.getsignal(signal.SIGINT) is signal.default_int_handler:
        signal.signal(signal.SIGINT, signal.SIG_DFL)
    record(name, args)
    handlers = {
        "brew": fake_brew, "launchctl": fake_launchctl, "lsof": fake_lsof, "pgrep": fake_pgrep,
        "ps": fake_ps, "sandbox-exec": fake_sandbox_exec, "tensorplate": fake_tensorplate,
        "python": fake_python, "sleep": lambda _: 0,
    }
    return handlers[name](args)


if __name__ == "__main__":
    sys.exit(main())
