#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Stands in for a serving worker's control thread, invoked with the config path.

Answers each ledger poll on the socket inherited as fd 0 with an empty
ledger. Exits 70 on a request that names another member than the config it
was started with, or on any other operation. Asked to stop, it leaves
`<endpoint>.terminated` beside the config.
"""
import json
import os
import signal
import socket
import sys

config_path = sys.argv[1]
with open(config_path, encoding="utf-8") as config:
    deployment = json.load(config)["deployment"]
member = {"deployment_id": deployment["endpoint"], "generation": deployment["generation"]}


def stop(*_):
    marker = os.path.join(os.path.dirname(config_path), member["deployment_id"] + ".terminated")
    open(marker, "w", encoding="utf-8").close()
    os._exit(0)


signal.signal(signal.SIGTERM, stop)
channel = socket.socket(fileno=0).makefile("rwb", buffering=0)
for line in channel:
    request = json.loads(line)
    if request["member"] != member or request["op"] != "ledger_status":
        sys.exit(70)
    answer = {
        key: request[key]
        for key in ("schema_version", "transaction_id", "correlation_id", "op", "member")
    }
    answer["status"] = "ok"
    answer["ledger"] = {
        "reserved": 0,
        "active": 0,
        "closing": 0,
        "ceiling": 0,
        "admission_monotonic_ns": [],
    }
    channel.write(json.dumps(answer, separators=(",", ":")).encode() + b"\n")
