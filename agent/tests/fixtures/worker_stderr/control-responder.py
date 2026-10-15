#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Stands in for a serving worker's control thread, invoked with the config path.

Answers ledger polls with an empty ledger and the agent's fence, activate and
retire by the worker's admission rules; exits 70 on another member or
operation. Each request and its answer are appended, before the answer is
sent, to `<endpoint>-<generation>.control.jsonl` beside the config. Asked to
stop, it leaves `<endpoint>.terminated` beside the config.
"""
import json
import os
import signal
import socket
import sys

config_path = sys.argv[1]
with open(config_path, encoding="utf-8") as config:
    rendered = json.load(config)
deployment = rendered["deployment"]
member = {"deployment_id": deployment["endpoint"], "generation": deployment["generation"]}
sessions = 1 if rendered.get("streaming", {}).get("enabled") is True else 0
admission = "open"
record_path = os.path.join(
    os.path.dirname(config_path),
    "{deployment_id}-{generation}.control.jsonl".format(**member),
)


def stop(*_):
    marker = os.path.join(os.path.dirname(config_path), member["deployment_id"] + ".terminated")
    open(marker, "w", encoding="utf-8").close()
    os._exit(0)


signal.signal(signal.SIGTERM, stop)
channel = socket.socket(fileno=0).makefile("rwb", buffering=0)
for line in channel:
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
    elif op == "retire":
        admission = "retiring"
    else:
        sys.exit(70)
    with open(record_path, "a", encoding="utf-8") as record:
        record.write(json.dumps({"request": request, "answer": answer}) + "\n")
    channel.write(json.dumps(answer, separators=(",", ":")).encode() + b"\n")
