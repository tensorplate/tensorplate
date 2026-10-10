# Bundle Compatibility and Agent Deploy Integration

**Supported bundle formats:** 0.1 and 0.2
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
    └── require format_version exactly 0.1 or 0.2; validate manifest semantics
        (model class, IO names, blocks, the format 0.2 rules, ...)
    └── verify artifact digests (streaming sha256)
    └── verify optional manifest_digest
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
violation onto its typed `AgentError`. A deploy then judges the manifest
against the target's own facts, described under
[rules judged against the target](#rules-judged-against-the-target).
Callers that want every violation
(CLI deploy/doctor rendering) use `parse_and_check` instead — it returns
the full `CompatibilityResult` without short-circuiting.
`AgentConfig.backend_capabilities` carries backend capability flags,
supported precision profiles, and accepted artifact kinds into that
`DeviceContext`.

---

## Compatibility checks

`SUPPORTED_BUNDLE_FORMAT_VERSIONS` is the exact allowlist `{0.1, 0.2}`.
Unknown minors such as `0.3`, unknown majors and alternate spellings are
refused before payload decoding or artifact access. The generic typed
manifest validator and deployment-descriptor reader enforce the same
allowlist. `BUNDLE_FORMAT_VERSION` remains `"0.1"`; format 0.2 is explicit
opt-in, not a change to the shared protocol/schema version.

The format 0.2 speech fixtures require `min_runtime_version: "0.3.0"`.
Pinned compatibility and coordinator tests reject runtime 0.2.1 and accept
0.3.0 and 0.3.1 with a mock worker. The released-agent check in
[`test/packaging`](../../test/packaging/README.md) executes the actual
v0.2.1 Debian agent and requires the runtime-floor error before staging
for both speech tasks. Lowering only that floor to 0.2.1 reaches the later
backend-readiness refusal. This control proves the refusal reason, not
speech execution or hardware qualification.

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
2. `verified` — `bundle::verify_before_staging()` returns `Ok`. The parser,
   the compatibility checks and the rules judged against the target passed.
3. `staged` — bundle copied into `<staging_dir>/<bundle_id>/`.
4. `capacity_checked` — second-pass memory check against the running worker.
5. `prepared` / `warmed` / `promoted` / `active` — worker control plane.

If it returns `Err`, the transaction transitions to `failed` with
the typed error and never modifies the active deployment.

The serving worker receives the verified candidate through the agent's
worker control plane. The portable `DeploymentDescriptor` is a separate
protocol value object; it has not replaced that load message. The worker does not
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

## Format 0.2 manifest rules

Format 0.2 opts into manifest-local checks before artifact staging or worker
contact. Format 0.1 retains its previous class/block and optional-field behavior.
The parser exposes a typed `BundleRuleCode` through
`ParseError::ManifestSemantics`. The agent preserves it in
`AgentError::BundleManifest` and the existing `ErrorRecord.context` string;
the outer wire code remains `config_invalid` and the error is non-recoverable.
The diagnostic message names the field and, for reserved classes, the unavailable
posture. No new shared wire error enum is required.

| Rule code | Refusal |
| --- | --- |
| `bundle_r1_model_block` | Missing, mismatched or multiple active class blocks. |
| `bundle_r2_reserved_class` | Format 0.2 `language`, `embedding` or `custom` deployment. |
| `bundle_r3_streaming_state` | Streaming speech lacks chunking or a nonzero `per_session_state_bytes` line across its declared memory domains. |
| `bundle_r7_budget_line` | An aggregate or domain budget contains a noncanonical line name. |
| `bundle_r10_caller_execution` | A caller-owned stage carries fields beyond `stage`, `ownership`, `observable` and `interface`. |
| `bundle_r11_class_payload` | A VLA block declares a reserved serving mode. The supported tensor payload, including fractional control frequency, remains valid without that selector. |
| `bundle_r12_required_field` | An applicable format 0.2 field is missing, or the speech runtime minimum is below 0.3.0 or is not three numeric components. |
| `bundle_r12_ambiguous_selector` | A runner-selected profile carries legacy `profile_id`, `backend_profile` or `default_backend`, or a manifest carries a competing top-level `serving_mode`. Present-null selectors also reject. |
| `bundle_r12_precision_conflict` | Speech precision and compute type disagree. |
| `bundle_r12_explicit_precision` | Speech uses `auto`, including an omitted precision hint. |
| `bundle_r8_base_reference` | `base_model_ref` and `variant_identity` are not declared together, or the base reference names the declaring bundle's own name and version. From the lineage check: no known base has that name, version and manifest digest. |
| `bundle_r8_variant_identity` | From the lineage check only: another bundle already declares this variant id and revision on the base, or the id with another kind. |
| `bundle_r8_variant_support_level` | From the lineage check only: the variant asks for more support than its base holds, or declares no `support_level`. |
| `bundle_r8_reserved_variant` | The manifest declares a variant. Every variant kind is reserved. The refusal ends manifest validation: the manifest's other rules and its warmup artifact references are judged first, and the form of its artifact digests is checked, but a variant's artifact digests are not verified against the files and a declared `manifest_digest` is not compared. |

The parser has no facts about other bundles, so a variant declaration that
reaches it and is otherwise valid always ends in `bundle_r8_reserved_variant`.
The two codes marked "from the lineage check only", and the unknown-base case
of `bundle_r8_base_reference`, come from `BundleProfile::check_lineage`, which
takes the known bases from its caller. The agent is that caller at deploy.
See [variant lineage](manifest.md#variant-lineage).

### Rules judged against the target

Some rules compare a manifest with facts only the target holds. The agent
judges them in `verify_before_staging`, the one validation path of a deploy
and of a deploy replayed at startup, before anything is staged. Each refusal
is non-recoverable and carries its code in `ErrorRecord.context`. The error
code says which side is at fault: `config_invalid` where the manifest is
wrong for any target or contradicts itself, `unsupported` where it is a
valid request this installation cannot serve. Format 0.1 bundles are not
judged by them.

| Rule code | Error code | Refusal | Facts |
| --- | --- | --- | --- |
| `bundle_r8_base_reference`, `bundle_r8_variant_support_level` | `config_invalid` | A variant's base is not a bundle this agent is serving on a row it holds, or the variant asks for more support than that row gives. | The active deployment and the serving members of the resident set, by name, version and bundle digest; the support level of the row the machine was admitted on. |
| `bundle_r9_hardware_row` | `config_invalid` | A `hardware_compatibility` id is not a row of the installed registry. | The registry's rows, whatever their level. |
| `bundle_r9_support_claim` | `config_invalid` | The manifest asks for `production` and names a row that is not Production. | Each named row's own support level. |
| `bundle_r6_runner_selector` | `config_invalid` | The backend and runner selectors contradict each other: the manifest names a runner profile under a backend that has no descriptor (`tensorrt`, `libtorch`, `vitis_ai`, `mock`) and so declares none. | The backend the hint names. |
| `bundle_r6_compute_type` | `unsupported` | The installed runner profile does not list the manifest's `compute_type`. The deployment descriptor refuses the same fact with the same error code. | The profile's `compute_types` in the probed backend descriptor. |

A runner profile that is not installed, or whose interpreter cannot run, is
the same rule's refusal and keeps the reason it already had:
`unsupported` with the platform reason `missing_backend_package` or
`accelerator_runtime_unavailable` in `context` (see
[backend registry](../architecture/backend-registry.md)). Parsing a bundle
never requires its runner to be installed.

The lineage check runs first, on the profile decoded from the manifest text,
because the parser refuses every variant as reserved. A relational failure
returns its own code; a declaration that passes goes on to the parser and is
refused as `bundle_r8_reserved_variant`. A manifest the agent cannot read or
decode at that point is left to the parser, which reports why. A machine
holds a row when it was admitted on it and the row's evidence covers it: a
machine admitted on technical prerequisites, or an agent with no registry,
knows no base. `bundle_r8_variant_identity` cannot come from a deploy yet:
no variant deploys, so none is recorded on a base.

The row and claim checks need a loaded registry. A production agent that
has none refuses every deploy before this point; an embedder that builds a
coordinator without one skips them, as it skips platform admission.
A `production` request on rows that are all Production passes these checks.
That is not a grant: whether the row's evidence covers this bundle's
artifacts and configuration is not resolved yet, and the level a deployed
bundle is known at is always the row's, never the one its manifest asks for.

Not judged yet: quotas that several members claim from the same free
capacity, a warmup declaration required by the target row's benchmark
profile, and the resolution of a Production request to evidence.

All format 0.2 manifests require `support_level`, `hardware_compatibility` and
exactly the class block selected by `model_class`. Speech additionally requires
`runner_profile`, `compute_type`, `pipeline_stages`, both budget declarations,
`max_concurrent_sessions`, `degraded_profile` and a minimum runtime of at least
0.3.0. A Production declaration also requires the aggregate budget and explicit
`capability_requirements`. These are declarations, not evidence of support or
sufficient admission capacity.
Warmup remains conditional on the target benchmark profile.

Speech precision pairs are `fp32`/`float32`, `fp16`/`float16`,
`bfloat16`/`bfloat16`, and `int8` with `int8`, `int8_float32`,
`int8_float16` or `int8_bfloat16`. The mixed int8 compute types retain int8
weight precision. No precision hint represents `int16`; it cannot satisfy this
speech contract. The installed runner's actual supported compute types are a
separate capability check. Explicit precision is required even for qualification;
an experimental support declaration does not permit a silent fallback.

The deployment descriptor shares field decoding, including budget-line,
caller-stage and chunking checks. Its own validation also requires speech
class/contract agreement, a deployable class, nonzero streaming state, explicit
matching precision and the execution fields it carries. Manifest-only fields
(class blocks other than speech, requested support, hardware row declarations,
aggregate budget, capability declarations, legacy selectors and minimum-runtime declarations) are checked
before descriptor derivation and are not reconstructed from the descriptor.
The descriptor omits disabled degradation rather than carrying manifest `null`.

The fixture pairs under `test/models/bundles/v0_2/` are synthetic. The parser
replays every pair, and the agent deploys every pair against a mock worker on
an agent given the committed platform rows, two installed runner profiles and
the lineage base bundle. They do not exercise a real runner installation,
registry evidence, per-domain admission or real speech execution. The lineage
fixtures are also judged against the base facts in `lineage_known_bases.json`,
which is test input and not a published format.
