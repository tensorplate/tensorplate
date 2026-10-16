#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Stands in for a serving worker's control thread, invoked with the config path.

Answers ledger polls with an empty ledger and the agent's fence, activate and
retire by the worker's admission rules; exits 70 on another member or
operation. Each request and its answer are appended, before the answer is
sent, to `<endpoint>-<generation>.control.jsonl` beside the config.

The first retire closes admission for good and starts the drain, which ends
at once: no session is open. While `<endpoint>.draining` exists beside the
config, sessions are open, and the drain lasts until it is removed or the
window (the retire's `drain_timeout_ms`, at most 30000) ends; polls are
answered meanwhile, and a retire again is answered `ok` without starting the
drain again. Once the drain ends it leaves `<endpoint>-<generation>.retired`
beside the config and exits 0. An endpoint starting `deaf-` records each
retire with the answer `null` and never answers it.

Asked to stop, it leaves `<endpoint>.terminated` beside the config and exits
at once, during a drain as before one.
"""
import json
import os
import select
import signal
import socket
import sys
import time

MAX_DRAIN_MS = 30000

config_path = sys.argv[1]
config_dir = os.path.dirname(config_path)
with open(config_path, encoding="utf-8") as config:
    rendered = json.load(config)
deployment = rendered["deployment"]
member = {"deployment_id": deployment["endpoint"], "generation": deployment["generation"]}
sessions = 1 if rendered.get("streaming", {}).get("enabled") is True else 0
deaf = member["deployment_id"].startswith("deaf-")
admission = "open"
drain_ends = None
record_path = os.path.join(
    config_dir,
    "{deployment_id}-{generation}.control.jsonl".format(**member),
)
draining_path = os.path.join(config_dir, member["deployment_id"] + ".draining")


def stop(*_):
    marker = os.path.join(config_dir, member["deployment_id"] + ".terminated")
    open(marker, "w", encoding="utf-8").close()
    os._exit(0)


def drained():
    return not os.path.exists(draining_path) or time.monotonic() >= drain_ends


def retired():
    marker = os.path.join(config_dir, "{deployment_id}-{generation}.retired".format(**member))
    open(marker, "w", encoding="utf-8").close()
    os._exit(0)


def record(request, answer):
    with open(record_path, "a", encoding="utf-8") as recorded:
        recorded.write(json.dumps({"request": request, "answer": answer}) + "\n")


signal.signal(signal.SIGTERM, stop)
control = socket.socket(fileno=0)
channel = control.makefile("rwb", buffering=0)
while True:
    if drain_ends is not None:
        if drained():
            retired()
        if not select.select([control], [], [], 0.05)[0]:
            continue
    line = channel.readline()
    if not line:
        if drain_ends is None:
            break
        while not drained():
            time.sleep(0.05)
        retired()
    request = json.loads(line)
    op = request["op"]
    if request["member"] != member:
        sys.exit(70)
    answer = {
        key: request[key]
        for key in ("schema_version", "transaction_id", "correlation_id", "op", "member")
    }
    answer["status"] = "ok"
    if op == "ledger_status":
        answer["ledger"] = {
            "reserved": 0,
            "active": 0,
            "closing": 0,
            "ceiling": 0,
            "admission_monotonic_ns": [],
        }
    elif op == "admission_fence":
        if admission == "open":
            admission = "fenced"
    elif op == "activate" and admission == "retiring":
        answer["status"] = "error"
        answer["error"] = {
            "code": "unavailable",
            "message": "the member is retiring and admits nothing again",
            "context": "member_retiring",
        }
    elif op == "activate":
        admission = "open"
        answer["quota"] = {
            "session_count": sessions,
            "domain_bytes": {"guest_ram": 0} if sessions else {},
        }
    elif op == "retire" and deaf:
        admission = "retiring"
        record(request, None)
        continue
    elif op == "retire":
        if admission != "retiring":
            window_ms = min(request["drain_timeout_ms"], MAX_DRAIN_MS)
            drain_ends = time.monotonic() + window_ms / 1000
        admission = "retiring"
    else:
        sys.exit(70)
    record(request, answer)
    channel.write(json.dumps(answer, separators=(",", ":")).encode() + b"\n")
