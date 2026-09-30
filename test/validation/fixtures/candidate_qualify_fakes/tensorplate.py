#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fake `tensorplate` CLI for the candidate qualify test.

Deploy, status, rollback and version answer in the real CLI's JSON envelopes and exit
codes. A failed deploy prints its ok envelope on stdout and the error envelope on stderr,
leaves the agent `degraded` with that failure as `last_error`, and the next successful
deploy or rollback clears it, as the real agent does. TP_FAKE_CLI_SKIP_VERIFY skips the
digest check, TP_FAKE_CLI_FAIL_CANDIDATE fails every deploy of that bundle name,
TP_FAKE_CLI_STALE_STATUS reports the previous deployment as active, and
TP_FAKE_CLI_WRONG_RESTORE reports a wrong restored id after rollback.
"""

import hashlib
import json
import os
import sys
from pathlib import Path

state_path = Path(os.environ["TP_FAKE_STATE_FILE"])
state = (
    json.loads(state_path.read_text())
    if state_path.exists()
    else {"active": None, "previous_active": None, "txn": 0, "last_error": None}
)


def save():
    state_path.write_text(json.dumps(state))


def envelope(command, status, payload=None, error=None):
    out = {"schema_version": "0.1", "command": command, "status": status}
    if payload is not None:
        out["payload"] = payload
    if error is not None:
        out["error"] = error
    return json.dumps(out, indent=2)


args = sys.argv[1:]
assert args[-2:] == ["--output", "json"], args
command, rest = args[0], args[1:-2]
if command == "version":
    print(envelope("version", "ok", {"cli": "0.3.1", "protocol": "0.1", "bundle_format": "0.1"}))
    sys.exit(0)
if command == "status":
    agent = {
        "available": True,
        "agent_state": "degraded" if state["last_error"] else "ready",
        "active": None,
        "previous_active": None,
        "last_error": state["last_error"],
    }
    for key in ("active", "previous_active"):
        d = state[key]
        if key == "active" and d and os.environ.get("TP_FAKE_CLI_STALE_STATUS") == "1":
            d = state["previous_active"] or d
        if d:
            agent[key] = {
                "deployment_id": d["deployment_id"],
                "bundle_digest": d["bundle_digest"],
                "bundle_name": d["bundle_name"],
                "bundle_version": "0.1.0",
                "backend": "python_pytorch",
                "model_class": "custom",
                "serving_url": os.environ["TP_FAKE_SERVING_URL"],
            }
    print(envelope("status", "ok", {"agent_response_status": "ok", "agent": agent}))
    sys.exit(0)
if command == "rollback":
    if not state["previous_active"]:
        sys.stderr.write(
            envelope(
                "rollback",
                "error",
                error={"code": "unavailable", "message": "no previous active deployment"},
            )
            + "\n"
        )
        sys.exit(6)
    state["txn"] += 1
    state["active"], state["previous_active"] = state["previous_active"], state["active"]
    state["last_error"] = None
    save()
    a = state["active"]
    restored = a["deployment_id"]
    if os.environ.get("TP_FAKE_CLI_WRONG_RESTORE") == "1":
        restored = "someone-else"
    print(
        envelope(
            "rollback",
            "ok",
            {
                "transaction_id": f"txn-{state['txn']}",
                "restored_deployment_id": restored,
                "restored_bundle_digest": a["bundle_digest"],
                "restored_backend": "python_pytorch",
            },
        )
    )
    sys.exit(0)
if command == "deploy":
    bundle = Path(rest[0])
    deployment_id = rest[rest.index("--deployment-id") + 1]
    state["txn"] += 1
    txn = f"txn-{state['txn']}"
    manifest = json.loads((bundle / "manifest.json").read_text())

    def fail(code, message):
        print(
            envelope(
                "deploy",
                "ok",
                {
                    "agent_response_status": "ok",
                    "transaction_id": txn,
                    "phase": "failed",
                    "deployment_id": deployment_id,
                    "failure": {"error_code": code, "message": message, "recoverable": False},
                },
            )
        )
        sys.stderr.write(
            envelope("deploy", "error", error={"code": code, "message": message}) + "\n"
        )
        state["last_error"] = {"code": code, "message": message, "context": None}
        save()
        sys.exit(3)

    entry = None
    for art in manifest["artifacts"]:
        data = (bundle / art["path"]).read_bytes()
        if (
            os.environ.get("TP_FAKE_CLI_SKIP_VERIFY") != "1"
            and "sha256:" + hashlib.sha256(data).hexdigest() != art["digest"]
        ):
            fail("load_failed", "bundle integrity: digest mismatch")
        if art["role"] == "model":
            entry = json.loads(data)
    if Path(os.environ["TP_FAKE_OOM_MARKER"]).exists():
        fail("oom_error", "the device is out of memory")
    if manifest["name"] == os.environ.get("TP_FAKE_CLI_FAIL_CANDIDATE"):
        fail("load_failed", "the sidecar did not become ready")
    profile = entry.get("backend_profile")
    if profile == "kokoro" and entry.get("voice") not in entry.get("voices", {}):
        fail("unsupported", "the selected language or voice is not declared")
    digest = "sha256:" + hashlib.sha256((bundle / "manifest.json").read_bytes()).hexdigest()
    state["previous_active"], state["active"] = (
        state["active"],
        {
            "deployment_id": deployment_id,
            "bundle_digest": digest,
            "bundle_name": manifest["name"],
            "entry": entry,
        },
    )
    state["last_error"] = None
    save()
    print(
        envelope(
            "deploy",
            "ok",
            {
                "agent_response_status": "ok",
                "transaction_id": txn,
                "phase": "active",
                "deployment_id": deployment_id,
                "bundle_digest": digest,
            },
        )
    )
    sys.exit(0)
sys.stderr.write("fake tensorplate: unknown command\n")
sys.exit(2)
