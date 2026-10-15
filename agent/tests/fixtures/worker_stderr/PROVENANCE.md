# Serving worker stderr fixtures

What `tensorplate-serving` writes to stderr when it cannot start, because
the Python sidecar refuses a load or a listener cannot bind its port,
replayed by `stub-worker.sh` for the agent's deploy tests.

`kokoro_undeclared_voice.stderr` is **recorded**: the stderr of a
`tensorplate-serving` built from this tree (RelWithDebInfo, the dev
container's clang 15), started with the serving config the agent renders
for a candidate, against a copy of
`test/models/bundles/v0_1/tts_kokoro_candidate/` whose runner entry selects
a voice it does not declare. The `kokoro` runner profile refuses the entry
with `unsupported` before importing any model library, so the recording
needs only CPython. The worker exited 65. Nothing was removed or replaced:
`ts_ns` is a steady-clock reading and the sidecar wrote nothing of its own.
The last line is the worker's startup failure record, the one the agent
reads; `test/integration/python_pytorch_adapter_test.cpp` repeats the run
and fails when the worker's last line no longer equals it.

`oom_at_load.stderr` is **transcribed** from that recording, with both
lines' code and message replaced by those the same profile raises when the
model build runs out of memory (`oom_error`, "the Kokoro model could not be
loaded"). An out-of-memory load needs an accelerator and is not recorded
here.

`http_port_taken.stderr` is **recorded**: the stderr of a
`tensorplate-serving` built from this tree (RelWithDebInfo, the dev
container's clang 15), started with a serving config in the shape the agent
renders for a candidate (a mock session, generation 1, one end of a socket
pair as fd 0) on the HTTP port of another worker of the same user, started
by hand. The worker exited 66; the stand-in exits 65 for it, as for every
`fail-` fixture, since the agent answers from the record and does not read
the status when there is one. Nothing was removed or replaced: the port is
the one the other worker was given. `test/integration/worker_bind_test.cpp`
starts a worker on another worker's HTTP port, both by hand, and fails when
that worker's last line, with its own port, no longer equals the last line
here.

`control-responder.py` is not a recording. It is a stand-in, written by
hand, for a worker's control thread: it answers ledger polls in the shape of
`protocol/rust/tests/fixtures/worker_control_ledger_status.jsonl` with an
empty ledger, and `admission_fence`, `activate` and `retire` by the rules a
serving worker with a deployment generation keeps (see
`docs/architecture/serving-worker.md`, "Control channel"). `stub-worker.sh` runs it with `python3`, which these tests
need on `PATH`.

`pid-recording-worker.sh` is not a recording either. It writes its process
id beside the serving config and then runs `stub-worker.sh` in the same
process, so a test can signal the stand-in worker and see that it is gone.
