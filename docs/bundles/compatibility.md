# Bundle Compatibility and Agent Deploy Integration

**Status:** v0.1.0 (bundle format)
**Code:** [`protocol/rust/src/bundle.rs`](../../protocol/rust/src/bundle.rs) (shared evaluator), [`agent/src/bundle.rs`](../../agent/src/bundle.rs) (agent integration).
**Deploy transaction:** [`docs/architecture/agent.md`](../architecture/agent.md), [`docs/architecture/worker-supervision.md`](../architecture/worker-supervision.md).

A bundle is **valid** when the parser accepts the manifest, every
artifact digest matches, and the optional manifest self-digest matches.
A bundle is **compatible** when its declared runtime range, hardware
profile, backend hint, capability requirements, precision profile, and
memory estimate are satisfied by the local device.

bundle format collapses the two-phase verifier into a single shared path
inside `tensorplate_protocol::bundle`. The agent's `bundle::verify()`
function is now a thin wrapper around `parse_bundle()` +
`evaluate_compatibility()`; the migration removed the duplicate
validation surface that previously lived inside `agent/src/bundle.rs`.

---

## Validation pipeline

```text
parse_bundle(bundle_path)
    └── load manifest -> typed ParseError on missing/malformed/unsafe paths
    └── verify artifact digests (streaming sha256)
    └── verify optional manifest_digest
    └── validate manifest semantics (model class, IO names, blocks, ...)
    └── BundleDescriptor                            ← value object

evaluate_compatibility(descriptor, device_context)
    └── runtime version range
    └── target hardware family + memory
    └── backend availability
    └── backend capability flags
    └── declared precision vs backend supported_precision
    └── declared artifact kind vs backend supported_artifact_kinds
    └── CompatibilityResult { ok, violations[] }
```

The agent calls `parse_bundle` once, builds a `DeviceContext` from
`AgentConfig`, runs `evaluate_compatibility`, and projects the first
violation onto its typed `AgentError`. Callers that want every violation
(CLI deploy/doctor rendering) use `parse_and_check` instead — it returns
the full `CompatibilityResult` without short-circuiting.
`AgentConfig.backend_capabilities` carries backend capability flags,
supported precision profiles, and accepted artifact kinds into that
`DeviceContext`.

---

## Compatibility checks

| Check                          | Failure code                                                           |
| ------------------------------ | ---------------------------------------------------------------------- |
| Runtime version range          | `unsupported_runtime`                                                   |
| Device family                  | `unsupported_hardware`                                                  |
| Minimum / estimate memory      | `insufficient_memory`                                                   |
| Backend availability           | `unavailable_backend`                                                   |
| Backend capability gap         | `unsupported_capability`                                                |
| Backend precision support      | `unsupported_precision`                                                 |
| Backend / artifact kind        | `backend_artifact_mismatch`                                             |

The `code` field on each [`CompatibilityViolation`](../../protocol/rust/src/bundle.rs)
is a short stable slug; the agent's `AgentError` taxonomy maps each
violation onto the corresponding typed variant the CLI and transaction
log already understand.

---

## Where compatibility is enforced

The agent's deploy transaction (V01-E08) runs the verifier **before
staging**. Phases:

1. `received` — deploy request accepted, correlation ID assigned.
2. `verified` — `bundle::verify()` returns `Ok`. Parser + compat both passed.
3. `staged` — bundle copied into `<staging_dir>/<bundle_id>/`.
4. `capacity_checked` — second-pass memory check against the running worker.
5. `prepared` / `warmed` / `promoted` / `active` — worker control plane.

If `verify()` returns `Err`, the transaction transitions to `failed` with
the typed error and never modifies the active deployment.

The serving worker receives a *validated deployment descriptor* through
the agent's worker control plane (V01-E07/V01-E08). The worker does not
re-parse unsafe bundle paths or recompute digests; runtime adapters may
still validate declared backend/capability against the live SDK as
defense in depth.

---

## CLI rendering

`tensorplate deploy <bundle>` and `tensorplate doctor` consume the
typed `ResponseError` returned by the agent control API. Each error
class maps to a stable [exit code](../cli/) and a CLI-rendered hint;
nothing in the CLI parses log lines to detect failure reasons.

`parse_and_check` is the entry point for richer rendering (e.g.,
showing every failing check rather than the first). The CLI can pass
the full `CompatibilityResult` straight through to the JSON output
format without translating it.

---

## What stays in the runtime

The C++ runtime continues to validate declared backend/capability at
load boundaries. This is intentional defense in depth: a successful
deploy verify implies "the agent believes this should run on this
device". The adapter still owns the final SDK-level acceptance and may
raise `ErrorCode::Unsupported` if a TensorRT/LibTorch/Python sidecar
contract changes between bundle authoring and deploy time.

The runtime never selects a backend heuristically and never falls back
at inference time. The agent's verify step has already chosen one
backend; if it cannot run, the inference returns `Unsupported` rather
than trying another adapter.
