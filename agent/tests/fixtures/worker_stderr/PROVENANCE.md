# Serving worker stderr fixtures

What `tensorplate-serving` writes to stderr when the Python sidecar refuses
a load, replayed by `stub-worker.sh` for the agent's deploy tests.

`kokoro_undeclared_voice.stderr` is **recorded**: the stderr of a
`tensorplate-serving` built from this tree (RelWithDebInfo, the dev
container's clang 15), started with the serving config the agent renders
for a candidate, against a copy of
`test/models/bundles/v0_1/tts_kokoro_candidate/` whose runner entry selects
a voice it does not declare. The `kokoro` runner profile refuses the entry
with `unsupported` before importing any model library, so the recording
needs only CPython. The worker exited 65. Nothing was removed or replaced:
`ts_ns` is a steady-clock reading and the sidecar wrote nothing of its own.

`oom_at_load.stderr` is **transcribed** from that recording with the code
and message the same profile raises when the model build runs out of
memory (`oom_error`, "the Kokoro model could not be loaded"). An
out-of-memory load needs an accelerator and is not recorded here.
