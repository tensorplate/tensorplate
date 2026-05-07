# T3 Adapter contract tests

Real backend adapters exercised through the `ModelLoader*` / `ExecutionSession`
interface. Validates lifecycle, readiness, shape mismatch, bad path, unload,
and `BufferRef` lifetime invariants.

Runs nightly and on release branches. Must skip cleanly (not fail) when the
required backend SDK is not available on the runner.
