# Changelog

All notable changes to TensorPlate will be documented in this file.

This project follows the spirit of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic versioning once public releases begin.

## [Unreleased]

### Added

- The systemd supervision contract and the package rollback procedure now
  have tests that run them rather than read them. A new packaging check
  drives the shipped units against a real systemd: the agent reaches active
  and writes to the documented log path, a `SIGKILL` is recovered, a crash
  **loop** is given up on instead of retried forever, a clean stop is not
  treated as a failure, observability survives the agent stopping, and no
  serving unit exists. The unit-text check additionally asserts there are no
  architecture-specific unit files and that both units agree on the
  supervision directives, so the parity claim is verified rather than
  assumed — the units were already single-source, what was missing was
  evidence.

  **The documented rollback could not work.** `upgrade-preflight.sh` refuses
  a downgrade on the version comparison alone and never inspects durable
  state, so the published instruction to move state aside and then
  `apt install --allow-downgrades` was refused all the same. Rolling back
  means removing the runtime set — which keeps `/etc/tensorplate` and
  everything under `/var/lib/tensorplate` — and installing the older version
  as a fresh install. Both documents now say that, and the lifecycle
  rehearsal proves the guard stays armed with and without state present
  before exercising the procedure that works.

  `install-paths.sh` now verifies the layout it just applied instead of
  trusting it. Its ownership changes are deliberately tolerant, because the
  system group is not guaranteed to exist for every path when they run — so
  a failed `chgrp` used to leave the agent unable to read its own state and
  report nothing. A half-applied layout is now a dpkg configure failure that
  names the directory, the observed value, and the command that fixes it.

  The agent config becomes per-architecture. It declares `device_family` and
  the backends the build actually contains, and the agent reads both into its
  deploy compatibility check, so one shared file would make x86_64 reject
  bundles that correctly target it while advertising a TensorRT backend that
  build does not contain. Both variants install to the same conffile path, so
  existing hosts see a byte-identical file and dpkg does not prompt.

  The channel lifecycle rehearsal now runs on x86_64 as well as arm64, which
  became possible only once the runtime metapackage stopped being arm64-only.
  (V021-E02-F01-T02, V021-E02-F01-T03)

- Releases now publish a complete Ubuntu x86_64 runtime package set —
  agent, serving worker, observability service, operator CLI, and the
  `tensorplate` metapackage — alongside the Jetson arm64 set, instead of
  an amd64 operator CLI on its own. The optional Python/PyTorch backend
  and the other architecture-independent packages are shared between
  both, so an x86_64 host installs the same appliance a Jetson does.

  The packages are built on the **oldest Ubuntu LTS they must run on**,
  not the newest they target. A shared-library floor is set by the build
  host, so building on 24.04 would have raised it to a glibc no 22.04
  machine has and quietly excluded a platform row that claims support.

  The release manifest keeps describing one primary target and gains no
  second target block. It never needed one: every artifact already
  carries its own architecture, and that is what the installer selects
  on — the same shape the desktop CLI has used all along.

  What changed is the rule about *which* second-architecture packages may
  appear. It was "the CLI, and nothing else"; it is now an explicit set,
  and verification asserts that set by package **and** architecture.
  Verification's package check was previously name-only, so it could not
  tell a complete second architecture from a half-published one — the
  arm64 sibling of any missing amd64 package satisfied it, and the sole
  architecture-specific assertion covered the CLI. Collection had no
  notion of the other x86_64 packages at all: it required the amd64 CLI
  and failed without it, but an amd64 agent, serving worker, or
  observability build staged beside it was dropped without a word, and a
  release would have gone out green with them simply absent.

  The x86_64 serving worker ships without the TensorRT adapter. A hosted
  runner has no CUDA/TensorRT SDK, and building the adapter without one
  yields a backend that registers, passes deploy admission, and only
  then fails at engine load. Leaving it out means a TensorRT deploy
  fails at lookup instead — earlier, and for the real reason. The
  `python_pytorch` sidecar path is unconditional and unaffected.

  The `tensorplate` metapackage becomes `Architecture: any` rather than
  arm64-only, so `apt install tensorplate` installs the full runtime on
  either architecture. It stays architecture-qualified rather than
  becoming `all` because its strict `= ${binary:Version}` relations bind
  runtime binaries built in the same run, which are per-architecture.
  (V021-E02-F01-T01)

- The Python/PyTorch backend descriptor's declared runtime range now
  admits the 0.2 line, and a packaging check asserts the range brackets
  the version the tree builds. The upper bound is inert in shipped code
  today — only the lower bound is compared — but the descriptor schema
  publicly promises rejection outside the range, and the descriptor is
  not among the files the release driver rewrites on a version bump, so
  a stale bound had nothing to catch it. The check asserts the range
  admits the release line rather than only the current version, because
  a bound one minor behind still brackets the version that precedes it.
  (V021-E02-F01-T01)

- `tensorplate doctor` gains a host section, and it reports what the
  machine says about itself rather than what the binary was compiled for.
  The old probe read `std::env::consts::ARCH`, which names the *build
  target*: an `amd64` CLI on an arm64 host reported the wrong
  architecture and an operator had no way to tell. Detection now goes
  through `tensorplate-platform`, the same code the agent uses, so there
  is one answer on a device instead of two. `host_facts` reports detected
  architecture and vendor, `host_os` reports OS identity with the exact
  version, build, and L4T release alongside it for evidence, and a new
  `platform_profile` finding reports which support rows the host could
  be. A host whose sources cannot be read is reported as undetected, not
  as unsupported — those need different fixes.

  Profile selection is deliberately a **set**: rows sharing an OS and CPU
  profile differ only by accelerator, so naming one would assert a match
  nobody has established; narrowing to a single row needs accelerator
  identity. A host matching nothing returns a typed reason rather than an
  empty list the caller has to interpret, and that reason never blames
  the accelerator, because at host level nothing has looked at one.

  Matching also stops treating a row with no machine type as a wildcard. A
  row validated on physical hardware makes no claim about a cloud
  instance — its evidence was recorded in a chassis whose thermals,
  firmware, and power delivery are the operator's, none of which transfer
  to a hypervisor — so a host reporting a cloud machine shape no longer
  sees physical rows offered as candidates. Rows that are deliberately
  chassis-independent declare `cloud_instance` with no machine type, which
  is how the schema now documents "any instance", and they are
  unaffected. `kind` is load-bearing for matching as a result, and the row
  schema says so: changing it changes which machines a row matches.

  A host whose hardware this release validates but whose machine shape no
  row covers is reported as exactly that, rather than borrowing a frozen
  reason that would tell its operator something untrue about their OS.
  (V021-E01-F02-T03)

- The release-notes support matrix is generated from the platform registry
  instead of written by hand. `docs/release/support-matrix.md` is projected
  from the committed rows and guarded by a golden test, so the platforms a
  release claims are the same rows `doctor` matches against and deploy
  admission enforces — a sentence in a release note can no longer promise
  something the software will not honour. The projection never invents a
  claim the rows do not make: Planned rows are listed as planned and
  nothing more, roadmap targets render in a separate non-support appendix
  that no count includes, and Experimental rows get their own section
  outside the supported set. Experimental has no rows in this release but
  is a frozen schema value, so its rendering is goldened against a
  synthetic row now rather than appearing unreviewed in a release note the
  day a row first uses it. Output is ordered by row id so a support-level
  change is a one-line diff rather than a reshuffle.
  (V021-E01-F01-T04)

- Host identity detection: CPU architecture and vendor, and OS identity,
  produced in the exact spelling a support row is written in. Detection is
  a pure function of recorded source content — `/etc/os-release`,
  `/proc/cpuinfo`, `/etc/nv_tegra_release`, `sw_vers`, the device tree —
  so every committed row has a fixture proving its host identity is
  detectable, with no hardware in the room. A row whose identity no probe
  can produce is unmatchable on the very machine it describes, and nothing
  about the row alone reveals that; the fixtures are therefore checked
  against the registry's own comparison rather than against restated
  expectations. Detection **normalizes to row granularity**, because a row
  records the OS the project committed to validating and a machine reports
  more than that: Linux says `aarch64` where a row says `arm64`, macOS
  says `26.5.2` where a row says `26`, and Jetson reports L4T `r36.4.3`
  where a row names the `r36.4.x` line. The precision is not discarded —
  the exact version, build string, L4T patch, and device model come back
  alongside the identity, because evidence recording needs what matching
  deliberately ignores. An unrecognized architecture or vendor is reported
  verbatim rather than as a detection failure, so an off-matrix machine is
  called unsupported instead of undetectable — including on arm64 Linux,
  where `/proc/cpuinfo` carries no `vendor_id` at all. A Jetson takes its
  JetPack version from the `nvidia-jetpack` package where that is
  installed and from its L4T line where it is not, so a device flashed
  from the base BSP or running in an `l4t` container still matches the row
  that describes it; an L4T line this release does not know is left
  unmapped rather than guessed into a version that would match a row the
  device was never validated against. On Compute Engine the
  machine type is read from the metadata service, but only after the host
  is recognized as an instance from firmware — a physical workstation
  reports no machine type, which is what its row declares, and never pays
  a network timeout to establish that. The metadata read is framed by
  `Content-Length` and bounded by an overall deadline, so a complete
  answer is used the moment it arrives rather than waiting for the peer to
  close, a truncated one is refused rather than becoming half a machine
  type, and a peer that trickles bytes cannot hold a service start open by
  resetting a per-read timer.
  (V021-E01-F02-T01, V021-E01-F02-T02)

- The agent, `tensorplate doctor`, and the observability service now
  answer platform questions from one registry instead of each carrying
  its own. `tensorplate-common` installs the support rows and roadmap
  targets to `/usr/share/tensorplate/platform`, and all three consumers
  resolve them from that one `PLATFORM_REGISTRY_DIR` constant through the
  same loader and the same fail-closed policy — the services via
  `PlatformRegistry::load_installed()`, and `doctor` via the prefix-aware
  form every install probe uses, which resolves to the same path — so no
  consumer can form a second opinion about what is supported. The consumer crates depend on
  `tensorplate-platform` and never on each other, which is asserted
  rather than assumed. `doctor` reports a new `platform_registry` finding
  that separates the states an operator can actually be in: no registry
  installed (`missing`, and not a failure, so a clean dev host still
  passes), a registry that loads (with its row, supported-combination,
  and roadmap-target counts), a registry that is installed but not
  readable by the calling account (`warning` — it ships group-readable,
  and being outside the group says nothing about the rows inside, so
  `doctor` neither calls it invalid nor fails on a device whose only
  fault is which account is running the command), and a registry that is
  readable but unusable (`fail`, naming the offending document where one
  document is at fault). An empty registry directory reports as broken
  rather than as a clean pass with zero rows, since a registry with no
  rows answers "unsupported" for every machine.
  The agent and the observability service load the registry once at
  startup and log what they loaded; neither treats a registry it could
  not load as an empty one, because "no basis to judge" and "nothing is
  supported" are different answers. Loading is best-effort in both
  services: they come up and let `doctor` report the problem rather than
  refusing to start over package data they only read.
  (V021-E01-F01-T03)

- Platform registry loading and the query API that resolves a detected
  machine to exactly one support row. `PlatformRegistry` loads the
  committed rows and roadmap targets and **fails closed**: one invalid
  document means no registry, because a half-loaded registry would report
  supported platforms as unsupported. Colliding entries — a duplicated row
  id, two rows matching the same platform identity, or a roadmap target
  shadowing a row id — are rejected at load rather than resolved by
  picking a winner at query time. Roadmap targets load into a separate
  catalog that matching never consults, so a target can never be read as
  support. `resolve()` returns a typed `RowMatch`: supported, matched a
  Planned row (`row_planned_not_validated`), or unsupported with a reason
  drawn from the *nearest* row — the one the machine fails in the fewest
  dimensions — so a machine one CPU vendor away from a row is told about
  the vendor rather than about its accelerator. A partitioned accelerator
  is rejected outright. Matching also honours machine shape: rows carry an
  exact `machine_type` where their evidence is scoped to one — the cloud
  rows, whose machine type a metadata probe reports; physical rows are
  identified by their exact accelerator SKU, and the accelerator-less
  utility rows make a deliberately chassis-independent claim — and a
  machine whose hardware matches a shape-scoped row but whose shape is
  outside that row's validated environment resolves to
  `OutsideValidatedEnvironment` rather than inheriting the claim, because
  evidence does not transfer across machine shapes. That outcome names the
  row only when exactly one row's hardware matches; where several differ
  only by shape, naming one would be arbitrary. Experimental rows get
  their own non-deployable state rather than borrowing the Planned reason.
  Detected CPU architecture and vendor are open values, so a host reporting
  something no row names is reported as unsupported rather than as
  undetectable. `candidates()` narrows on host identity alone and deliberately
  returns a set, since several rows share an OS and CPU and differ only by
  accelerator. `HostProbe` and `AcceleratorProbe` define the detection
  seam so OS-specific probes stay out of the matching logic.
  (V021-E01-F01-T03)

- Platform support row registry: the schema-first artifact every later
  platform feature keys off. `config/schemas/platform_support_row.json`
  defines one exact platform row (OS with exact version and image
  identity, kernel/driver stack, CPU architecture and vendor, accelerator
  SKU with its required memory size, memory profile reference and
  partition posture, backend
  package sets per channel, model-class row pointers, per-signal gate
  semantics, support level, provenance, validation environment, and
  evidence location), and the deliberately smaller
  `config/schemas/roadmap_target.json` describes future targets that are
  not exact enough to be rows — a target has no row id, support level,
  model-class rows, gate semantics, or evidence, is never matched against
  a detected platform, and never counts as a supported combination.
  Cross-field rules are enforced identically by the schema and the
  decoder: `not_applicable` signals must state why, Production rows must
  declare where evidence is filed, Planned rows carry no evidence and no
  model-class claims, accelerator-less rows cannot report GPU
  utilization, free-text fields must carry a non-whitespace character
  (consumer-enforced rather than a schema `pattern`, because regex engines
  disagree about which code points count as whitespace), and a row with an
  accelerator names exactly one CPU vendor
  while an accelerator-less utility row states the vendor set it covers
  explicitly — vendor support is decided by registry membership, never an
  out-of-band allowlist.
- The committed registry under `config/platform/`: all twelve v0.2.1 rows
  (five Production, two Preview, five Planned) and all four roadmap
  targets, including the deploy-smoke rows' Preview model-class pointers
  so diagnosis renders posture from the registry rather than a hardcoded
  list. Every row is currently marked `spec_authored` — no v0.2.1
  evidence run has happened yet — and each is re-verified against recorded
  output when its hardware first runs; Planned rows are additionally
  required to stay `spec_authored`. Production rows declare where their
  evidence will be filed; the release gate, not the schema, checks that a
  bundle is actually there.
- New `tensorplate-platform` workspace crate owning platform identity:
  row and roadmap-target value objects whose only constructor is the
  validating `from_json` (neither type implements `Deserialize`, so no
  loader can produce an unvalidated or inexactly-decoded record), and the
  ten-value typed `PlatformReason`
  vocabulary (`unsupported_accelerator_sku`, `unsupported_os_version`,
  `unsupported_cpu_arch`, `unsupported_cpu_vendor`, `mig_mode_enabled`,
  `missing_backend_package`, `missing_driver_runtime`,
  `accelerator_runtime_unavailable`, `telemetry_degraded`,
  `row_planned_not_validated`) that diagnosis and admission will emit.
  (V021-E01-F01-T01, V021-E01-F01-T02)

- Platform memory profile records and consolidated telemetry field names.
  The new `config/schemas/platform_memory_profile.json` defines the two
  property-named profiles (`unified_memory`: one shared budget pool;
  `discrete_gpu`: separate guest-RAM and device-VRAM domains) with their
  measurement sources, headroom computation, copy-pressure posture, and
  v0.2 platform instances; the per-profile domain sets are frozen and
  enforced by both the schema's conditionals and the Rust decoder. The
  thirteen consolidated platform memory telemetry field spellings
  (configured/projected budget, per-domain observed peak and headroom,
  pressure transitions, cache high-water, ledger state, observed output
  queue, sidecar RSS, engine pool utilization/preemptions, GPU memory) are
  defined once here for platform registry and telemetry consumers to
  reference. `tensorplate-protocol` gains the mirror
  (`PlatformMemoryProfile` with canonical record constructors,
  `PLATFORM_MEMORY_TELEMETRY_FIELD_NAMES`) on its own config-schema
  version track with failures mapping to `config_invalid`, plus committed
  canonical record fixtures pinned to the constructors. Validation is
  unavoidable: a custom `Deserialize` impl routes every decoding path —
  including generic serde loaders — through the version gate and
  frozen-semantics checks, decoded records are read-only, enum-valued
  fields and object-shaped values decode only from their string and object
  forms respectively (serde's derived sequence and externally-tagged map
  forms are rejected, matching the schema's `type` constraints), and
  instance identifiers must be unique and lowercase-hyphenated. Decoding
  from JSON text additionally rejects duplicate object keys; loaders that
  pre-parse into `serde_json::Value` collapse duplicates before any
  decoder sees them. (V023-E03-F04-T03)

- Canonical memory budget line-item vocabulary. The new
  `config/schemas/memory_budget_breakdown.json` defines the eleven
  `memory_budget_breakdown_bytes` lines shared by every model class:
  undeclared lines default to zero, unknown line names are rejected
  fail-closed, and line values are integers in [0, 2^53) — the
  exactly-representable IEEE-754 range, so declared byte counts can never
  be silently rounded by a JSON parser (integral-valued numbers accepted;
  non-numeric, negative, fractional, or out-of-range values are typed
  decode errors). Each number token's exact decimal lexeme is validated
  before parsing, so high-precision tokens that would round to integers
  under IEEE-754 (e.g. 1.0000000000000001) are rejected rather than
  silently truncated.
  The schema versions on its own config-schema track
  (`MEMORY_BUDGET_SCHEMA_VERSION`), independent of the cross-process
  protocol version, and validation failures map to `config_invalid`.
  `tensorplate-protocol` gains the Rust mirror (`MemoryBudgetBreakdown`,
  `MemoryBudgetDeclaration::from_json`, `MemoryBudgetError`,
  `MEMORY_BUDGET_LINE_NAMES`) plus committed per-class mapping fixtures for
  VLA, speech STT, speech TTS, vision, and language readiness; the speech
  fixtures declare non-zero `per_session_state_bytes` as the foundation for
  streaming-session ledger admission. A Draft-07 validator conformance test
  (dev-only `jsonschema` dependency) keeps the schema document and the Rust
  mirror verdict-identical. (V023-E03-F04-T01, V023-E03-F04-T02)

### Changed

- `tensorplate-protocol` gains shared config-schema helpers used by both
  the memory-pathway mirrors and the new platform registry:
  `json_numbers` (exact decimal lexeme validation and canonicalization for
  byte-valued fields, so a declared size can never be silently rounded by
  a JSON parser) and `serde_shape` (object-form and string-form pinning,
  so a decoder is never shape-weaker than its schema, plus canonical
  identifier checks). The memory budget and platform memory profile
  modules now use these instead of private copies. Behavior is unchanged
  except that byte-value error messages now say "byte-value domain" rather
  than "byte-line domain", and number tokens with leading zeros are now
  reported as the JSON grammar errors they are instead of being
  canonicalized into legal integers.

## [0.1.5] - 2026-07-20

### Changed

- The agent control socket now binds group-accessible (`0o660`, the documented
  `SOCKET_0660`) instead of owner-only (`0o600`). CLI/operator users reach the
  agent by membership in the `tensorplate` group — the MicroK8s-style
  "install → add to group → enroll" flow — so SSH device enrollment no longer
  requires a per-user sudoers rule in the common case. Socket access is confined
  to the non-root `tensorplate` user and the five control ops (deploy, status,
  rollback, health, version); this is the same privilege the `--run-as` sudoers
  path already granted, so `--run-as` becomes a fallback for sites that will not
  grant group membership. (V015-F01-T03)

### Added

- The packaged install now creates `/var/lib/tensorplate/bundles/import`
  (`1775`, sticky + group-writable, `tensorplate:tensorplate`) — added to
  `required_directories()` and the install scripts — so remote deploy staging
  works with no manual `mkdir`/`chown`/`chmod`. `device add`'s reachability
  preflight also now reports a pre-0.1.5 remote explicitly ("upgrade the device
  to >= 0.1.5") instead of a generic unreachable-agent hint. (V015-F01-T03)

- Remote deploy staging and import pruning for device-routed workflows. `deploy
  <bundle>` under `--device` now validates the local bundle, copies it to a
  staged import path on the device (`<import-dir>/<deployment-id>/`, via `rsync`
  with an `scp` fallback through an injectable copier), then runs the remote
  deploy transaction against that path with the original flags forwarded; a
  deployment id is generated when one is not supplied, and unsafe ids (path
  traversal) are rejected. A new `device prune <name> [--keep <n>]
  [--older-than <dur>]` reclaims staged import storage: it requires an explicit
  policy, always keeps the active deployment's import, and keeps any import that
  survives either policy (so a just-staged import is never reclaimed out from
  under an in-flight deploy). (V015-F01-T03)
- Device enrollment reachability, run-as execution, and metadata sync. `device
  add` now runs a reachability preflight by default — it executes
  `tensorplate --local status --output json` on the device over SSH and refuses
  to save the entry if that fails, with an actionable hint (SSH as an
  agent-capable user, configure `--run-as` with a non-interactive sudoers rule,
  or make the socket group-accessible); `--no-verify` skips it for
  offline/pre-enrollment. Devices enrolled with `--run-as` now route through a
  structured, non-interactive `sudo -n -u <user> -- …` invocation (no shell
  string is interpolated), and `device add` vets the remote binary (absolute,
  root-owned, not group/other-writable) before trusting it under sudo. A new
  `device sync [<name>]` refreshes cached facts (remote CLI version, protocol
  version, last-seen time) and is non-destructive on failure. (V015-F01-T02)
- SSH remote command adapter: with a device selected (via `--device <name>` or
  a default set by `device use`), `status`, `rollback`, `logs`, `doctor`,
  `infer`, and `version` run against the device over plain OpenSSH. The adapter
  is orthogonal to the profile/transport layer — it shells out to `ssh` and
  re-invokes the remote `tensorplate` with a forced `--local` (built from
  structured, POSIX-quoted arguments so nothing is shell-injected), and never
  routes through a `ProfileMode`. Selection precedence is
  `--local` > `--device` > `--profile`/`--agent-url` > the registry default >
  local. `infer --input` is read locally and piped to the device over stdin and
  `infer --output-file` is written locally; `logs --source` and
  `status --observability-snapshot` are device-local. JSON output preserves the
  stable envelope and adds a top-level `device` object (added to
  `protocol/schemas/cli_output.json`); human output is forwarded verbatim. The
  CLI fails closed when SSH exits non-zero, remote output is malformed, or the
  remote protocol version is incompatible, mirroring the remote exit code. The
  CLI now also version-checks agent responses through the shared
  `decode_with_version_check`, so an incompatible agent surfaces a typed
  protocol error instead of a generic parse failure. `deploy` over `--device`
  and `--run-as` execution are deferred to later changes. (V015-F01-T02)
- Local SSH device registry and the `tensorplate device` command group
  (`add`, `list`, `use`, `remove`, `rename`) so any operator running the CLI
  (macOS or Ubuntu) can enroll and remember SSH-reachable devices over the
  network without a hosted service. The registry is an atomic JSON file
  (temp-file + rename) that resolves its own path
  (`$TENSORPLATE_DEVICE_REGISTRY`, else
  `$XDG_CONFIG_HOME/tensorplate/devices.json`, else
  `~/.config/tensorplate/devices.json`), independent of the CLI config. It
  stores only device access metadata (SSH target, optional port, optional
  run-as user, remote import dir) plus cached device facts, and never SSH keys,
  passwords, or agent secrets. Each entry records the remote import directory
  the deploy path needs, defaulting to `/var/lib/tensorplate/bundles/import`
  and overridable with `device add --import-dir`. `device add --use` sets the
  new device as the default; a plain `device add` sets the default only when
  none exists yet. The command group is local-only — it resolves no transport
  profile and opens no agent client, so it keeps working when the CLI config's
  default profile is a reserved/unsupported mode. `device` is added to the
  `cli_output` envelope `command` enum so `--output json` stays schema-valid. A
  missing registry means "no devices enrolled", not an error, so existing local
  and profile workflows are unchanged. Documented in `docs/cli/device.md` with
  the schema in `config/schemas/devices.json`. (V015-F01-T01)
## [0.1.4] - 2026-06-26

### Added

- Binary `/infer` transport for `tensorplate-serving` and the Python SDK: an
  optional, content-type-negotiated wire format
  (`application/vnd.tensorplate.infer.binary.v1`) that frames tensor payloads
  as raw bytes (8-byte magic, little-endian uint32 metadata length, metadata
  JSON, then concatenated payloads) instead of base64, cutting request encode
  cost and payload size for large vision inputs. The transport is opt-in and
  fully backward compatible: the JSON envelope (`schema_version` `0.1`) is
  unchanged, and a worker that does not accept the binary content-type replies
  `415` so `auto`-transport clients transparently fall back to JSON. The wire
  framing is documented in `protocol/schemas/serving_http_envelope.json`.
- `tensorplate-python` SDK `yolo26_e2e_detections` output contract
  (`YOLO26_E2E_DETECTIONS`): decodes the Ultralytics YOLO26 default
  one-to-one / end-to-end detection head — an NMS-free `[1, K, 6]` tensor — via
  `decode_detections` and `VisionClient.detect`, with `--contract` wiring added
  to the vision examples and the detection benchmark.

## [0.1.3] - 2026-06-19

### Added

- `tensorplate-python` Python SDK package skeleton (import package
  `tensorplate`): PEP 621 packaging metadata, `src/` layout, `py.typed`
  marker, and the placeholder public surface (`ServingClient`,
  `VisionClient`, `Detection`, and the `TensorPlateError` base exception)
  with import smoke tests and CI coverage. The serving client, vision
  detection helpers, examples, and published distribution land in later
  v0.1.3 changes. (V013-F01-T01)
- `ServingClient` for the v0.1 serving HTTP envelope: schema-valid
  `InferRequest` marshalling with base64 tensor payloads, success/failure
  parsing into typed results and exceptions, `schema_version` enforcement,
  a `GET /health` readiness snapshot, and tensor input/output value objects
  with optional numpy array access. Serving endpoint resolution matches the
  CLI precedence (explicit URL, CLI profile, read-only agent discovery,
  loopback default) with the same URL canonicalization. (V013-F01-T02,
  V013-F01-T03)
- Vision detection helpers (the `tensorplate-python[vision]` extra —
  numpy + Pillow): client-side image preprocessing (`preprocess`,
  `PreprocessConfig`, `LetterboxTransform`) that decodes path/bytes/ndarray
  images and letterboxes them into NCHW float32 input tensors while
  recording the transform for source-pixel box back-mapping; and
  YOLOv8-style postprocessing (`decode_detections`, `Detection`,
  class-aware NMS, the `yolo_v8_single_output` contract, and the
  `detections.*` semantic-tag constants). The core install stays
  dependency-free. (V013-F02-T01, V013-F02-T02)
- `VisionClient.detect`: one-call detection composing preprocessing,
  `ServingClient.infer`, and YOLO postprocessing — accepts path/bytes/
  ndarray input with configurable endpoint, input/output names, score and
  NMS thresholds, labels, and output contract. Selects the detection
  output explicitly, by single-output, or by a `detections.*`
  `semantic_tag`, and returns source-pixel `Detection`s. Synchronous in
  v0.1.3. (V013-F02-T03)
- Vision detection SDK examples (`examples/vision_detection_sdk/`):
  `yolo_detect.py` detects on an image via `VisionClient.detect`, and
  `camera_infer.py` is a user-space reference `camera -> SDK -> /infer`
  loop (per-frame; OpenCV capture is an optional, example-only
  dependency). Documented as samples — not supported in-runtime ingest,
  DeepStream, or streaming. (V013-F02-T04)
- SDK release packaging: the release workflow builds the
  `tensorplate-python` wheel + sdist at the release version, folds them into
  the cosign-signed `SHA256SUMS` + artifact manifest, and attaches them to
  the GitHub Release alongside the runtime/CLI assets. The SDK is published to
  PyPI via Trusted Publishing (OIDC, through a protected `pypi` environment,
  on final releases only); the same signed wheel + sdist remain attached to
  the GitHub Release for checksum/cosign-verified installs. A build +
  `twine check` + clean-environment install gate runs in CI. (V013-F03-T01)
- SDK documentation (`docs/sdk/`): a quickstart and API reference for
  `ServingClient`, `VisionClient`, `Detection`, the tensor value objects,
  and the typed error hierarchy; the detection workflow (preprocessing, the
  `yolo_v8_single_output` contract, the `detections.*` convention, and
  postprocessing); and endpoint-resolution semantics matching the CLI. The
  package README, the example README, and the top-level README link it, and
  `docs/release/notes/v0.1.3.md` headlines the SDK. (V013-F03-T02)
- SDK end-to-end release validation: a serving-worker compatibility test
  proving the SDK round-trips against the unchanged v0.1 (`schema_version`
  `0.1`) envelope shipped since v0.1.2, an opt-in real-worker e2e test
  (`TENSORPLATE_SERVING_WORKER_BIN`) that exercises the `tensorplate-serving`
  mock worker, and a release-signoff checklist
  (`docs/validation/sdk-e2e-validation.md`) covering clean install, fixture
  integration, the failure/transport/schema cases, v0.1.2 compatibility, and
  the deferred Jetson detector signoff. (V013-F03-T03)

## [0.1.2] - 2026-06-12

### Added

- First-party package-manager installation (the v0.1.2 distribution
  feature):
    - Stable signed APT repository at
      `https://packages.tensorplate.com/apt` (`jammy/main`, `arm64` and
      `amd64`), generated exclusively from checksum- and cosign-verified
      release assets and published automatically when a final release
      goes public. Repository metadata verifies against the keyring
      shipped on the host.
    - `tensorplate-apt-source` bootstrap package: one-time archive
      keyring + Deb822 source setup for the stable channel; installs no
      runtime components and never runs `apt update`.
    - `tensorplate` runtime metapackage (Jetson `arm64`), making
      `sudo apt update && sudo apt install tensorplate` the complete
      runtime install on TensorPlate-ready hosts.
    - `tensorplate-cli` built and published for Ubuntu AMD64
      workstations.
    - First-party Homebrew tap (`tensorplate/homebrew-tap`) for the
      macOS Apple Silicon CLI-only install.
    - TensorPlate-ready host validation
      (`tools/validation/tensorplate-ready-check.sh`), an image and
      provisioning runbook, the documented v0.1.1 → v0.1.2 upgrade flow,
      and a CI lifecycle rehearsal covering bootstrap, in-place upgrade,
      and future-version discovery.

### Changed

- Release branching moved to a single per-minor maintenance line
  (`release/0.1`): all v0.1.x patch tags are created there; per-version
  release branches are no longer created.
- GitHub Release assets and `install.sh` remain fully supported as the
  signed no-APT fallback install path; public install docs now lead with
  the APT channel.

## [0.1.1] - 2026-06-05

### Added

- Release workflow and external installability (release publication). v0.1.0 gains
  maintainer-facing release machinery and public install documentation,
  but the release is still cut only after this tooling PR merges.
    - `tools/release/tensorplate-release.sh` adds guarded release
      subcommands for preflight, metadata prepare dry-runs, artifact
      manifest/checksum generation, annotated tag creation, and draft
      GitHub Release publication. Mutating paths require clean worktree,
      explicit confirmation, and final release evidence.
    - Release documentation under `docs/release/` defines the runbook,
      version/changelog policy, branch and tag policy, final tag
      immutability, sign-off template, artifact manifest format,
      checksum verification, GitHub Release attachment procedure,
      post-release hotfix/deprecation policy, and draft release notes.
    - External-user docs under `docs/install/` now start from GitHub
      Release assets rather than local build-tree paths and cover package
      download, SHA256 verification, core install, optional
      `tensorplate-backend-python-pytorch`, service start, doctor,
      quickstart deploy/inference, status/log/metrics inspection,
      rollback, uninstall, and troubleshooting.
    - `packaging/scripts/install.sh` is now a release asset and the
      primary external install path. It downloads release artifacts from
      the manifest, self-checks against `SHA256SUMS`, verifies selected
      package assets, installs core packages through the idempotent
      `apt --reinstall` path, enables TensorPlate services, and gates
      completion on critical `tensorplate doctor` findings. The installer
      also supports `--cli-only` for desktop operator hosts when a
      matching `tensorplate-cli` package asset is published, plus
      `--local-artifacts` for checksum-verified build-only or source
      snapshot artifacts.
    - `packaging/scripts/build-install-from-source.sh` adds the
      unreleased branch path: clone or check out a branch such as
      `develop`, build `X.Y.Z~dev.YYYYMMDD.gitsha` snapshot packages,
      generate and verify a local manifest plus `SHA256SUMS`, then
      install through `install.sh --local-artifacts --allow-unsigned`.
      Snapshot manifests are explicitly labeled as unreleased
      local-source builds and are not GitHub Release evidence.
    - `tools/release/build-release-artifacts.sh` supports snapshot mode
      for native Jetson builds and x86-to-Jetson cross builds when a
      Jetson sysroot, cross compiler, and vcpkg chainload toolchain are
      provided.
    - The release workflow now supports manual build-only validation
      (`publish=false`) so maintainers can build the exact release asset
      bundle, download it from GitHub Actions, and smoke-test installer
      flows before creating a GitHub Release or public prerelease.
    - Supply-chain hardening for published releases: the publish path
      keyless-signs `SHA256SUMS` with cosign (attaching
      `SHA256SUMS.cosign.bundle`) and records SLSA build provenance with
      `actions/attest-build-provenance`. `install.sh` verifies the cosign
      signature against the release workflow identity before trusting any
      checksum, bootstraps a pinned transient Linux `arm64`/`amd64` cosign
      binary when `cosign` is absent, and still fails closed unless
      `--allow-unsigned` is passed. `release.yml` passes `workflow_dispatch` inputs through
      environment variables (no shell interpolation), scopes permissions to
      the job, pins all actions by commit SHA, and only signs/publishes when
      the workflow itself is running from the release tag ref.
    - Clean-room release smoke procedure under `docs/validation/` defines
      the post-merge clean-room evidence path from GitHub Release assets on the
      Jetson Orin Nano 8GB Super hardware floor.

- Packaging and first-run install (packaging). v0.1.0 becomes
  installable as a Jetson-class Linux appliance. None of the
  artifacts are published yet — packaging ships the inspectable
  skeleton, the verifier suite, and the release validation handoff.
    - Native Debian-style package split under `packaging/debian/`
      with binary packages for `tensorplate-common`,
      `tensorplate-agent`, `tensorplate-serving`,
      `tensorplate-observability`, `tensorplate-cli`, and the
      separately installable `tensorplate-backend-python-pytorch`.
      Core packages do not depend on the Python/PyTorch backend.
      PyTorch is intentionally not a Debian dependency.
    - On-device filesystem contract under
      `protocol/rust/src/install_paths.rs` (single source of truth)
      mirrored by `packaging/scripts/path-constants.sh` for
      maintainer scripts and tests:
      `/etc/tensorplate`, `/var/lib/tensorplate/{state,bundles/{staging,active,previous,quarantine},worker-configs}`,
      `/var/log/tensorplate`, `/run/tensorplate`, and
      `/usr/share/tensorplate/backends`.
    - Shared maintainer-script helpers
      (`create-users.sh`, `install-paths.sh`, `upgrade-preflight.sh`,
      `version-utils.sh`) shipped by the new
      `tensorplate-common` package so every other package can
      Pre-Depend on it for layout + ownership.
    - systemd units for `tensorplate-agent.service` and
      `tensorplate-observability.service` with hardened defaults
      (`User=tensorplate`, `ProtectSystem=strict` + scoped
      `ReadWritePaths`, `RuntimeDirectory=tensorplate`,
      `NoNewPrivileges`, `ProtectKernel*`, restricted address
      families, bounded restart). No `tensorplate-serving.service`:
      the agent supervises the worker (V01-E09 invariant
      encoded directly in the package layout).
    - Default config files installed under `/etc/tensorplate/` as
      dpkg conffiles. All endpoints default to loopback / Unix
      sockets. First-run state is the existing typed
      `SupervisionServingState::NoActiveDeployment`, not an error.
    - Backend descriptor surface
      (`protocol::backend_descriptor` + schema
      `protocol/schemas/backend_descriptor.json`) and a shared
      `protocol::backend_probe` that the agent and the CLI doctor
      both consume. Probes never execute user model code; they
      shell out only to `python3 -c 'import sys; ...'` /
      `python3 -c 'import torch; ...'` against the descriptor's
      pinned interpreter and return a typed `BackendProbeState`.
    - `tensorplate-agent` startup probes every backend listed in
      `available_backends`; the new
      `AgentError::BackendUnrunnable` is raised before staging
      when the bundle's `backend_hint` maps to a non-Runnable
      probe. SmolVLA / Python bundles fail at deploy time, never
      at first inference.
    - `tensorplate doctor` gains the packaging install diagnostics
      (stable finding IDs):
      `path_layout`, `config_files`, `agent_systemd_unit`,
      `observability_systemd_unit`, `serving_systemd_absent`,
      `serving_binary_installed`, `python_pytorch_backend`,
      `python_pytorch_runtime`, `cuda_runtime`. Doctor degrades
      to `missing` (not `fail`) when no install layout is
      detected so host CI on dev hosts stays green.
    - Lifecycle policy (packaging):
      reinstall preserves user state; upgrade preflight refuses
      unknown `schema_version`, unsafe `/var/lib/tensorplate`
      ownership, or a downgrade; `remove` keeps state, `purge`
      clears state but never deletes the `tensorplate` system
      user / group.
    - Packaging verification suite under `test/packaging/`
      (`verify_layout.sh`, `verify_debian_metadata.sh`,
      `verify_systemd_units.sh`, `verify_lifecycle_scripts.sh`,
      `verify_descriptor.sh`, `run.sh`). All five verifiers pass
      on the host CI without root.
    - Operator + handoff documentation under `docs/install/`:
      filesystem-layout, services, lifecycle,
      python-pytorch-backend, clean-install-runbook, and
      packaging-validation-handoff. Doctor finding catalog updated in
      `docs/cli/doctor.md`.

### Behavior changes

- `tensorplate_agent::config::AgentConfig` is unchanged in shape;
  the packaging backend probe is carried on the coordinator via the
  new `Coordinator::with_backend_probes` builder. Tests and
  embedders that did not call the builder see no behavior change.
- `tensorplate_agent::bundle::verify` is unchanged. The new
  `verify_with_probes(bundle_path, config, probes)` is the
  deploy-time entry point used by the coordinator; the legacy
  `verify` calls it with an empty map. Callers that hit
  `verify` directly retain the previous behavior.
- `tensorplate doctor` no longer emits the historical placeholder
  findings for `python_pytorch_backend` / `tensorrt_runtime` /
  `libtorch_runtime` from the runtime-environment probe; the
  install-probe module owns those checks now with real probing.
  Finding IDs are stable; the release validation harness's grep keys are
  unchanged.

- Model bundle format (bundle format). The v0.1.0 bundle authoring surface
  lands as the shared `tensorplate_protocol::bundle` module that the
  agent verifier, CLI, and fixture tooling consume. The same module
  owns the deployable artifact contract: envelope, manifest schema,
  named input/output schema, model-class blocks, runtime capability
  declarations, precision metadata, integrity verification, and
  compatibility evaluation.
    - `protocol/rust/src/bundle.rs` (bundle format) — the
      single shared parser entrypoint. `parse_bundle` reads a bundle
      directory, validates the manifest semantically, streams sha256
      digests over every artifact, verifies the optional canonical
      manifest digest, and emits a `BundleDescriptor` value object.
      `evaluate_compatibility` consumes the descriptor plus a
      `DeviceContext` and returns a typed `CompatibilityResult` whose
      violations cover runtime range, hardware family, memory,
      backend availability, capability gaps, precision support, and
      backend / artifact-kind cross-checks.
    - `protocol/schemas/bundle_manifest.json` (bundle format) — full v0.1.0 manifest authoring surface. Adds
      named `inputs[]` / `outputs[]` (vision is the n=1 case;
      SmolVLA uses named multi-input + named action chunk output),
      `model_blocks` for every class with consistency validation,
      the reserved `language` block (tokenizer reference / kind /
      revision_or_digest, context_length_tokens, and default/empty
      generation_config), `precision` with Jetson FP32/FP16/INT8
      profiles and Vitis AI quantization / calibration metadata, an
      extended `capability_requirements` (deterministic_latency,
      control_loop_integration, op_coverage_limits,
      memory_estimate_bytes), and optional `signature` / `provenance`
      / `sbom` fields. Unknown / typoed `backend_hint` values are
      rejected at parse time. v0.1.0 recognizes `tensorrt`,
      `libtorch`, `python_pytorch`; `vitis_ai` and `onnxruntime` are
      reserved schema slots.
    - Agent deploy integration (bundle format). `agent/src/bundle.rs`
      is now a thin wrapper around the shared parser + evaluator;
      the duplicate V01-E08 verifier was removed in favor of the
      shared path. `parse_and_check` exposes the full violation list
      to CLI deploy/doctor rendering; `verify` preserves the typed
      single-error short-circuit for the deploy transaction.
    - Example bundle fixtures (bundle format) under
      `test/models/bundles/v0_1/`: vision_tensorrt,
      smolvla_python_pytorch, language_reserved, and the synthetic
      Vitis-shaped fixture with a fake `.xmodel` and Vitis-style
      INT8 calibration metadata. Invalid variants cover corrupted
      artifact, unsafe path, missing artifact, duplicate IO names,
      and a language block on a non-language class.
    - `tools/bundle/` (`tensorplate-bundle-tool`) — deterministic
      fixture digest helper. Re-uses the shared canonicalization so
      stale digests fail conformance tests rather than silently
      drifting.
    - Bundle conformance suite (bundle format) at
      `protocol/rust/tests/bundle_conformance.rs` — 13 tests asserting
      parser, schema, integrity, compatibility, backend hint, precision,
      and model-class block behavior, including that the runtime does
      not attempt heuristic backend fallback when the declared backend
      is unavailable.
    - Documentation under `docs/bundles/` (layout, manifest,
      model_classes, backends, integrity, compatibility) and the bundle format
      schema review addendum in
      `docs/architecture/kria-vitis-ai-review.md`.

### Behavior changes

- `tensorplate_protocol::bundle_manifest::CapabilityRequirements`
  picks up new optional fields (`deterministic_latency`,
  `control_loop_integration`, `op_coverage_limits`,
  `memory_estimate_bytes`). All default to `false` / empty so existing
  bundles continue to parse without modification. The struct is no
  longer `Copy`; callers that previously passed it by value now pass
  by reference (one in-tree call site updated;
  `agent::check_capabilities` signature is unchanged at the public
  shape).
- `tensorplate_agent::config::BackendCapability` now carries the E13
  capability flags plus `supported_precision` and
  `supported_artifact_kinds` so deploy validation can reject unsupported
  precision profiles and backend/artifact mismatches before staging.
- `BundleManifest` and `BundleDescriptor` lose `Eq` because `f64`
  fields (VLA control frequency, vision normalization) participate in
  equality. `PartialEq` is still derived; tests that need stable
  comparison round-trip through JSON.
- The agent's `bundle::verify` is now a thin wrapper around the shared
  parser/evaluator; behavior is preserved across every existing
  V01-E08 test.

- Observability baseline (V01-E12). The v0.1 telemetry surface lands as
  a coordinated extension of `tensorplate-protocol` and
  `tensorplate-observability`. It makes a single device diagnosable
  without a hosted-platform connection: structured logs, correlation
  IDs, typed failure reasons, a bounded local metrics registry,
  control-loop jitter/frequency metrics, retention with non-blocking
  sinks, and an extended status projection that the V01-E11 CLI
  consumes through the same observability snapshot file.
    - `protocol/schemas/log_event.json` + `tensorplate_protocol::log_event`
      (V01-E12-F01) — shared structured log envelope with bounded
      component, level, and context. `LogContextValue` accepts
      strings/integers/floats/bools/null; the sanitiser drops NUL
      bytes, control bytes, oversize entries, and unknown context
      keys at insert time. Catalog and producer contract documented in
      `docs/observability/log-schema.md`.
    - `protocol/schemas/failure_reason.json` +
      `tensorplate_protocol::failure_reason` (V01-E12-F03) —
      operator-visible failure reason taxonomy mapping each reason to
      a stable category, severity hint, retry hint, and canonical
      `ErrorCode`. `FailureReasonRecord::validate_payload` rejects
      records that drift from the canonical mapping. Catalog in
      `docs/observability/failure-reasons.md`.
    - `tensorplate_protocol::correlation_id::CorrelationId`
      (V01-E12-F02) — bounded `[A-Za-z0-9_-]{1,64}` identifier shared
      across request, transaction, and correlation ids.
      `CorrelationId::from_seed` and `sanitise_or_generate` keep
      label/log cardinality bounded for externally-supplied values.
      Propagation policy in `docs/observability/correlation-ids.md`.
    - `protocol/schemas/metric_event.json` +
      `tensorplate_protocol::metric_event` (V01-E12-F04) — wire-format
      sample envelope for counters, gauges, and histograms. Names must
      start with `tp_`; labels are restricted to the bounded v0.1 set
      (`endpoint`, `model_class`, `model_name`, `backend`, `component`,
      `status`); units are explicit. Histogram samples use
      Prometheus-style cumulative bucket counts (length = bounds + 1).
    - `protocol/schemas/control_loop_metrics.json` +
      `tensorplate_protocol::control_loop_metrics` (V01-E12-F05) —
      rolling-window summary event for VLA validation. Formulas
      `target_period_ms = 1000 / control_frequency_hz`,
      `jitter_ms = abs(interval_ms - target_period_ms)`,
      `instant_frequency_hz = 1000 / interval_ms`, and
      `frequency_error_pct = abs(mean_frequency_hz -
      control_frequency_hz) / control_frequency_hz * 100` are pinned to
      the roadmap. Bounded label set `(endpoint, model_class,
      model_name, backend)`. Truncation in `ControlLoopLabels::new`
      keeps every label inside `MAX_CONTROL_LOOP_LABEL_BYTES = 64`.
    - `tensorplate_observability::metrics` (V01-E12-F04) — local
      metrics registry. `MetricsRegistry::register_counter` /
      `register_gauge` / `register_histogram` enforce the bounded label
      policy at registration time, return a typed `SeriesId`, and bump
      typed counters (`series_rejected_unknown_label`,
      `series_rejected_bounded_label`, `series_rejected_full`) on
      rejection. The exporter ships `noop`, `in_memory`, `file`
      (JSON-lines append), and `stdout` sinks. `take_snapshot` returns
      wire-format `MetricEvent` payloads so an HTTP scrape consumer can
      stream them. Canonical Jetson Orin Nano latency buckets exposed
      via `default_latency_buckets_ms`.
    - `tensorplate_observability::control_loop` (V01-E12-F05) — rolling
      60s control-loop aggregator with deterministic
      `FakeClock`-driven percentiles, mean frequency, frequency
      standard deviation, frequency error percent, and
      missed-deadline rate. Invalid intervals (zero/negative) bump a
      bounded counter; the rolling-window eviction is bounded by
      `MAX_CONTROL_LOOP_SAMPLES = 4096`. Default grace window is 25%
      of `target_period_ms`.
    - `tensorplate_observability::retention` (V01-E12-F06) — bounded
      diagnostics retention with `drop_oldest` (default) or
      `drop_incoming` policies, file rotation at a configurable
      threshold (`1 MiB` default; the file is renamed to `<file>.1`
      before further writes), and bounded counters surfaced through
      the status projection. Shutdown flush is bounded.
    - `tensorplate_observability::log_emitter` (V01-E12-F01) — bounded,
      non-blocking emitter wrapper that stamps every event with a
      monotonic timestamp, runs the bounded-context sanitiser, and
      forwards into `DiagnosticsRetention`. `emit_failure` carries the
      canonical `FailureReason -> ErrorCode` mapping; `emit_with`
      exposes a builder callback for bounded context.
    - `tensorplate_observability::snapshot` extended (V01-E12-F07) —
      `StatusSnapshot` now carries `diagnostics_sink`,
      `metrics_export`, `control_loop`, `last_correlation_id`, and
      `last_failure_reason` fields. `SnapshotWriter::update_v12` and
      `update_last_failure` keep V01-E10 callers untouched while
      letting V01-E11 / release validation consumers read the new fields. Schema
      mirror at `protocol/schemas/observability_status.json` (extended
      with `diagnostics_sink`, `metrics_export`, `control_loop`,
      `last_correlation_id`, `last_failure_reason`); empty fields skip
      serialisation so older parsers continue to round-trip.
    - `tensorplate_observability::Service` now owns the V01-E12
      retention store, structured log emitter, metrics registry, and
      optional control-loop aggregator from production configuration.
      Every tick logs accepted health inputs and state transitions,
      updates the V01-E12 status projection, and exports configured
      local metrics / diagnostics sinks without introducing hosted
      platform dependencies.
    - Documentation: `docs/observability/README.md`,
      `log-schema.md`, `correlation-ids.md`, `failure-reasons.md`,
      `metrics.md`, `control-loop.md`, `retention.md`, and
      `status-projection.md`.
    - Integration tests
      (`observability/tests/observability_baseline_integration.rs`)
      cover failed-deploy correlation, failed-inference typed
      error+metric+log, file-sink export without platform
      connectivity, retention event storm with bounded drops, invalid
      metric label rejection, unknown log-schema-version rejection, no
      payload/secret leakage, stable control-loop formulas under a
      fake clock, and the V01-E12 snapshot projection. The full
      workspace test suite (`cargo test --workspace`) covers 410 tests
      including the 96 observability unit tests and the 152 protocol
      unit tests.

- CLI and device access profiles (V01-E11). `tensorplate-cli` is now a
  working single-device operator client wired against the V01-E08 agent
  control API. The crate ships a library + binary with the following
  modules:
    - `tensorplate_cli::args` (V01-E11-F01) — hand-rolled argv parser
      for global flags (`--config`, `--profile`, `--agent-url`,
      `--output`, `--timeout-ms`, `--no-color`, `--quiet`/`--verbose`)
      and per-subcommand flags. `tensorplate --help` and
      `tensorplate <cmd> --help` render stable text suitable for release validation
      validation logs.
    - `tensorplate_cli::config::CliConfig` (V01-E11-F01-T01) —
      versioned schema for the CLI config file mirrored at
      `config/schemas/cli.json`. Unknown schema versions are rejected
      with typed `config_invalid` errors. The default config seeds a
      `local` profile pointing at `/var/run/tensorplate/agent.sock`,
      so the CLI is operable without a config file on the device.
    - `tensorplate_cli::profile` (V01-E11-F02-T01) — profile resolver.
      v0.1.0 implements the `local` and explicit `url` modes;
      `ssh_tunnel`, `overlay`, and `relay` parse but fail with a
      typed `Unsupported` error at command execution.
    - `tensorplate_cli::client::{AgentClient, NetAgentClient,
      MockAgentClient}` (V01-E11-F02-T02) — Unix-domain-socket and
      loopback-TCP transport for the agent's newline-delimited JSON
      control API. The client decodes typed agent errors and maps
      them to CLI errors without losing the protocol error code; a
      `MockAgentClient` is provided for tests.
    - `tensorplate_cli::error::{CliError, ExitCode}` (V01-E11-F01-T02)
      — typed error taxonomy with stable documented exit codes
      (`0` success, `2` usage, `3` agent_error, `4` transport, `5`
      busy, `6` unavailable, `10` doctor_findings, `11`
      inference_failed). The mapping lives in `docs/cli/exit-codes.md`.
    - `tensorplate_cli::output::Renderer` (V01-E11-F01-T02) — shared
      human + JSON renderer. JSON envelopes follow
      `protocol/schemas/cli_output.json` so release validation can
      grep on stable field names.
    - `tensorplate doctor` (V01-E11-F03) — read-only validation pass
      with a stable finding taxonomy
      (`cli_version`, `profile_mode`, `agent_socket`,
      `agent_reachable`, `agent_status_shape`, `agent_state`,
      `active_deployment`, `worker_state`, `worker_crash_loop`,
      `host_facts`, `host_os`, `python_pytorch_backend`,
      `tensorrt_runtime`, `libtorch_runtime`, `ros2_health_stub`).
      Each finding carries `status` (`ok` / `fail` / `missing` /
      `unsupported` / `skipped` / `warning`), `severity`, message,
      and hint. Exit code `10` when any check fails.
    - `tensorplate deploy <bundle>` (V01-E11-F04-T01) — validates the
      local bundle path (directory + `manifest.json`), submits the
      canonicalised path through the agent deploy transaction API,
      and either returns the transaction id (with `--no-wait`) or
      polls until `active` / `failed` / `rolled_back` (`--wait`,
      default). Transaction phases render through stable labels
      (`received`, `verified`, `staged`, `capacity_checked`,
      `prepared`, `warmed`, `promoted`, `active`, `failed`,
      `rolled_back`).
    - `tensorplate rollback` (V01-E11-F04-T02) — calls the agent
      rollback API; surfaces typed `unavailable` (exit `6`) when
      there is no previous active deployment.
    - `tensorplate status` (V01-E11-F05) — projects the agent's
      `AgentStatus` and supervision summary plus an optional
      observability snapshot (`--observability-snapshot <path>`,
      mirroring V01-E10). Severity ordering
      (`ready < degraded < no_heartbeat < crash_loop < failed`) is
      stable for release validation grep assertions.
    - `tensorplate infer` (V01-E11-F06) — convenience inference call
      against the v0.1.0 serving HTTP envelope. Endpoint resolution
      order: `--serving-url` flag, profile `serving_url`,
      agent-discovered loopback default. Typed failures from the
      serving worker map to exit `11` with a backend-specific
      message.
    - `tensorplate logs` (V01-E11-F07) — bounded NDJSON reader over
      the configured `log_source.path`. Supports `--component`,
      `--level` (ordered), `--correlation-id`, `--since-ms`,
      `--tail` (capped at 10,000), and `--follow` against a single
      file. Remote profiles return typed `unavailable` until the
      V01-E12 agent log API lands.
    - Output schema mirror: `protocol/schemas/cli_output.json`.
    - Docs: `docs/cli/README.md`, `doctor.md`, `deploy-rollback.md`,
      `status.md`, `infer.md`, `logs.md`, `profiles.md`,
      `exit-codes.md`.
    - Integration tests (V01-E11-F08): `cli_local_profile.rs`,
      `cli_infer_workflow.rs`, `cli_logs_and_remote.rs` exercise the
      binary against a stub UDS agent server, a stub serving HTTP
      worker, and a local NDJSON log fixture; assert stable exit
      codes and JSON envelopes.

- Observability service baseline (V01-E10). `tensorplate-observability`
  is now a working independent health monitor that runs without
  depending on the serving request path or the V01-E08 deploy
  transaction. The crate ships a library + binary with the following
  modules:
    - `observability::config::ObservabilityConfig` (V01-E10-F01) —
      validated schema covering listener transport, heartbeat policy
      (`expected_interval_ms`, `grace_ms`, `missed_threshold`,
      `recovery_heartbeats`), safe-state sink, snapshot writer, ROS 2
      health stub. Defaults are local-only; the ROS 2 publisher is
      disabled unless explicitly enabled. Schema mirrored at
      `config/schemas/observability.json`.
    - `observability::error::ObservabilityError` — typed errors mapped
      to stable `tensorplate_protocol::ErrorCode` values so consumers
      see the same codes as the rest of the runtime.
    - `observability::clock::{MonotonicClock, SystemMonotonicClock,
      FakeClock}` — monotonic clock abstraction with a fake-clock test
      hook so every freshness decision is deterministic.
    - `observability::listener::{EventListener, HealthInput,
      ListenerCounters}` (V01-E10-F02) — bounded local listener that
      ingests serving-worker `HealthEvent` heartbeats,
      `WorkerStatus` snapshots, and `SupervisionEvent` transitions,
      normalises them into one `HealthInput` type, and tracks
      accepted / dropped / malformed / duplicate /
      out-of-order / unknown-version counters. A bounded VecDeque
      drops the oldest event when full so a slow consumer never
      blocks the producer. `unix_socket` is reserved in v0.1.0 and
      fails startup with a typed config error rather than silently
      starting without a socket listener.
    - `observability::heartbeat::{HeartbeatEvaluator,
      HeartbeatHealth, SourceState}` (V01-E10-F03) — per-source
      heartbeat freshness using monotonic time. Missed beats
      increment a bounded counter; the source flips to `NoHeartbeat`
      after `missed_threshold`; recovery requires
      `recovery_heartbeats` consecutive fresh heartbeats and resets
      the counter. Wall-clock changes never influence freshness.
    - `observability::state::{Aggregator, ObservabilityState,
      SafeStateEvent, SafeStateReason}` (V01-E10-F04) — aggregator
      that combines heartbeat freshness, serving state, agent state,
      overload, and last-error code into one `ready` /
      `degraded` / `failed` / `no_heartbeat` state. Emits a
      `SafeStateEvent` on every transition AND, when configured, on
      every `safe_state.periodic_ms` tick the state is not `ready`.
      Precedence table: `no_heartbeat > failed > degraded > ready`.
    - `observability::sink::{SafeStateSink, InMemorySafeStateSink,
      FileSafeStateSink, NoopSafeStateSink, WireSafeStateEvent}`
      (V01-E10-F04) — bounded sinks. The in-memory ring drops oldest
      when full and tallies the bounded drop counter; the file sink
      appends JSON lines and tallies write failures; neither blocks
      heartbeat evaluation.
    - `observability::ros2::{Ros2HealthPublisher, MockHealthPublisher,
      DiagnosticArray, DiagnosticStatus, DiagnosticLevel,
      DiagnosticKeyValue, build_diagnostic_array}` (V01-E10-F05) —
      optional ROS 2 health topic stub. When enabled, the publisher
      emits `diagnostic_msgs/msg/DiagnosticArray` on
      `/tensorplate/health` (configurable) with one `DiagnosticStatus`
      named `tensorplate/runtime`. Level mapping
      `ready -> OK / degraded -> WARN / failed -> ERROR /
      no_heartbeat -> STALE`. Key-values include `agent_state`,
      `serving_state`, `observability_state`, `active_deployment`,
      `backend`, `missed_heartbeat_count`, `missed_deadline_rate`,
      `queue_depth`, `last_error_code`. The v0.1.0 stub ships a
      mock-backed implementation so it runs in CI without a ROS 2
      distribution; the native publisher is reserved for a
      post-v0.1.0 release.
    - `observability::snapshot::{SnapshotWriter, StatusSnapshot,
      SinkStatus, PublisherStatus, ListenerStatus,
      BoundedDiagnostics, RecentTransition, RecentError}`
      (V01-E10-F06) — versioned status snapshot that surfaces every
      v0.1.0 required field plus the diagnostics ring V01-E11 / release validation
      consume. File-backed snapshots use atomic-replace
      (`*.partial` -> rename) so readers never observe partial records.
    - `observability::service::Service` (V01-E10-F01) — composition
      root. `Service::tick(now)` drains the listener, advances the
      heartbeat evaluator, updates the aggregator, emits any
      safe-state events, refreshes the snapshot, and publishes the
      ROS 2 health topic when enabled. The binary main loop calls
      `tick` on the configured heartbeat cadence without synthesizing
      serving-worker heartbeats; internal self-heartbeats are only
      emitted when `primary_source=internal`. Tests drive the same
      pipeline through a `FakeClock`.
- Observability protocol schemas:
    - `protocol/schemas/health_event.json` — serving-worker health
      events now optionally carry sequence/source metadata, serving
      state, active deployment, backend, queue depth, and
      missed-deadline rate so observability status and ROS 2 key-values
      can be populated without a schema extension.
    - `protocol/schemas/safe_state_event.json` — discrete safe-state
      event payload with version-fixed schema; documents the v0.1.0
      state names, transition reasons, and bounded diagnostic
      context.
    - `protocol/schemas/observability_status.json` — versioned status
      snapshot schema consumed by the V01-E11 CLI (`tensorplate
      status`) and the release validation harness. Includes
      sink / publisher / listener counters and the bounded
      `diagnostics` ring.
- Observability integration / failure-injection tests
  (V01-E10-F07) at
  `observability/tests/observability_failure_injection.rs`. Coverage
  includes healthy heartbeat, missing heartbeat without agent input
  (proves independent detection), heartbeat recovery, explicit failed
  state, crash-loop supervision event, worker-exit supervision event,
  worker-not-ready supervision event, overload event with `Overload`
  reason, malformed payload counter, unknown schema version typed
  rejection, event storm bounded-drop behaviour, duplicate / out-of-
  order sequence counters, periodic safe-state emission until
  recovery, ROS 2 DiagnosticArray mapping (level + required
  key-values), disabled ROS 2 publisher, file-backed snapshot
  atomic-replace, bounded diagnostics ring, agent supervision event
  enrichment, and the V01-E10 "no agent input" acceptance criterion.
- Observability architecture doc at
  `docs/architecture/observability.md` covering the state model,
  precedence table, monotonic heartbeat semantics, safe-state event
  shape, ROS 2 health topic mapping, snapshot schema, and the
  independence-from-agent contract.

- Agent worker supervision (V01-E09). `tensorplate-agent` now owns the
  full lifecycle of the V01-E07 `tensorplate-serving` worker. A new
  `tensorplate_agent::supervision` module ships:
    - `supervision::config::SupervisorConfig` (V01-E09-F01) — validated
      schema covering binary path, args, environment allowlist, working
      directory, serving-config reference, loopback control endpoint,
      stdio mode, startup / graceful-stop / kill / status-poll
      timeouts, restart policy, and bounded supervision-event sink.
      Validation enforces absolute paths, loopback-only control host,
      and non-zero timeouts before durable state is touched. Schema
      mirrored under `config/schemas/agent.json` (`supervision` block).
    - `supervision::process::{WorkerProcess, SystemWorkerProcess,
      MockWorkerProcess}` (V01-E09-F01-T02) — narrow process trait with
      a production unix-only implementation plus a deterministic
      in-process mock used by tests; tracks PID, monotonic start
      instant, command digest, and `launch_sequence`; supports graceful
      stop, escalated force-terminate, and idempotent re-stops.
    - `supervision::readiness::{ReadinessProbe, HttpReadinessProbe,
      MockReadinessProbe}` (V01-E09-F02) — readiness watcher that
      separates process liveness from serving readiness, polls the
      worker's `/health` endpoint over loopback, surfaces `failed` /
      `degraded` / `ready` plus active deployment id, queue depth, and
      last-error code.
    - `supervision::policy::{BackoffScheduler, FailureClass,
      BackoffDecision}` (V01-E09-F03) — bounded exponential backoff
      with a rolling-window crash-loop detector. All timing uses
      monotonic `Instant`; stable ready uptime decays the rolling
      counter; the threshold transitions to a terminal `crash_loop`
      state instead of restarting indefinitely.
    - `supervision::state::{SupervisionPhase, SupervisionState,
      SupervisionStatus, SupervisionReconcileAction}` (V01-E09-F04) —
      agent-local supervision state plus a stable status projection
      consumed by V01-E10 observability and V01-E11 CLI. Phase names
      (`no_active_deployment`, `starting`, `running`, `ready`,
      `degraded`, `failed`, `stopping`, `stopped`, `awaiting_restart`,
      `crash_loop`) are wire-stable and mirrored in
      `tensorplate_protocol::supervision_event`. Startup
      reconciliation produces a typed action from durable desired
      state, actual worker state, and the last terminal phase.
    - `supervision::event::{SupervisionEventSink, RingEventSink,
      NoopEventSink, SupervisionEventPayload}` (V01-E09-F05) — bounded
      ring-buffer event sink for supervision transitions. The sink
      drops the oldest pending event when its queue is full, bumps a
      typed drop counter, and never blocks `tick`; a missing or absent
      observability consumer cannot stall supervision decisions.
    - `supervision::supervisor::{WorkerSupervisor, DesiredWorker,
      TickOutcome, SupervisionFault}` (V01-E09-F04 / F06 / F07) — the
      `tick(now)`-driven state machine that owns process lifecycle,
      readiness watching, backoff scheduling, graceful stop, force-kill
      escalation, and supervision-event emission. `tick` is idempotent
      and uses a monotonic clock injected via the `MonotonicClock`
      trait so tests drive backoff windows deterministically through
      `FakeClock`. The supervisor never promotes a candidate;
      promotion remains the V01-E08 coordinator's responsibility.
- Cross-process supervision event schema (V01-E09-F05-T01) at
  `protocol/schemas/supervision_event.json` and
  `protocol/rust/src/supervision_event.rs`. Event kinds:
  `worker_started`, `worker_ready`, `worker_exit`, `worker_not_ready`,
  `restart_scheduled`, `worker_degraded`, `worker_failed`,
  `crash_loop_entered`, `worker_stopping`, `worker_stopped`. Each event
  carries a per-process sequence, monotonic timestamp, agent / serving
  state names, active deployment, backend, restart count, optional
  next-restart delay, exit code / signal, after-ready flag, and a
  bounded diagnostic message (truncated at 512 UTF-8 bytes by
  producers). Schema is version-fixed at `0.1`; decoders reject unknown
  versions through the existing `decode_with_version_check` path.
- Coordinator-supervisor coordination (V01-E09-F06-T02).
  `Coordinator::with_supervisor(Arc<WorkerSupervisor>)` attaches a
  supervisor; the coordinator now installs the new active deployment as
  the supervisor's desired state on every successful promote and
  invokes `recover_after_operator_action` so a fresh deploy or rollback
  is the documented exit from `crash_loop` terminal state. The
  supervisor never mutates the durable state store; promotion remains
  the coordinator's sole responsibility.
- Supervision integration / failure-injection tests (V01-E09-F07) at
  `agent/tests/supervision_failure_injection.rs` and
  `agent/tests/supervision_coordination.rs`. Coverage includes launch
  -> ready, exit before ready, single backoff restart, repeated
  crash-loop, not-ready timeout, graceful stop with `worker_stopping`
  / `worker_stopped` events, ignored stop escalating to force kill,
  exit-after-ready flag propagation, absent observability consumer,
  bounded sink drop behavior, deploy + rollback promoting supervisor
  desired state, and crash-loop recovery through deploy.
- Supervision architecture doc at
  `docs/architecture/worker-supervision.md` covering the state
  machine, failure classes, restart policy, supervision events, and
  the V01-E08 coordinator integration contract.

- `tensorplate-agent` desired state, deploy transaction, bundle
  verification, rollback, and restart-recovery baseline (V01-E08).
  The Rust agent now owns a durable desired-state store
  (`protocol/schemas/agent_state.json`, `protocol/rust/src/agent_state.rs`)
  that persists active / previous-active / candidate deployment records,
  the in-flight transaction phase, the bounded quarantine list, and a
  bounded `last_error` slot through atomic `tmp + rename(2)` writes with
  a backup file refreshed after every successful primary commit. The
  state schema is value-fixed to `schema_version: "0.1"`; decoders
  reject unknown future versions with the typed `Unsupported` error.
- Local control API (V01-E08-F01) speaks newline-delimited JSON over a
  Unix domain socket by default (loopback TCP is an opt-in escape
  hatch). The wire envelope is documented in
  `protocol/schemas/agent_control.json` and mirrored as
  `tensorplate_protocol::ControlRequest` / `ControlResponse`. Supported
  operations: `deploy`, `status`, `rollback`, `health`, and `version`;
  responses carry typed `agent_status`, `deploy_status`, and
  `error.code` projections so the CLI (V01-E11) and observability
  service (V01-E10) see stable error codes. Concurrent conflicting
  mutating requests return `agent_busy`; rollback with no previous
  active returns `unavailable`; unknown schema versions return a typed
  `Unsupported` error before the request touches the state store.
- Bundle verifier (V01-E08-F03) at `agent/src/bundle.rs` plus the new
  `protocol/schemas/bundle_manifest.json` (`schema_version: 0.1`,
  `format_version: MAJOR.MINOR`, role-tagged artifacts, optional
  `manifest_digest`). Verification runs in a fixed order: bundle path
  exists -> manifest schema/version -> per-artifact sha256 -> optional
  self-digest -> format-version major -> runtime-compatibility window
  -> hardware family / memory envelope -> declared `backend_hint`
  availability -> `capability_requirements` vs configured
  `backend_capabilities`. No heuristic backend fallback: bundles that
  declare an unknown or unavailable backend are rejected with
  `Error::Code::Unsupported` before any worker interaction. Unsafe
  artifact paths (absolute, `..` segments) and duplicate artifact paths
  are typed errors. Verified bundles return a `VerifiedBundle` carrying
  the canonical manifest digest (sha256 of the manifest minus the
  `manifest_digest` field) used as the persisted `bundle_digest`.
- Deploy transaction coordinator (V01-E08-F04 / F05 / F06) at
  `agent/src/coordinator.rs` walks the durable state machine
  `received -> verified -> staged -> capacity_checked -> prepared ->
  warmed -> promoted -> active`, persisting each phase before the next
  begins. Phase classification at `agent/src/transaction.rs`
  separates replayable phases (`received`/`verified`/`staged`/
  `capacity_checked`) from worker-side phases (`prepared`/`warmed`/
  `promoted`). Staging copies the manifest plus every declared artifact
  into `<staging_dir>/<deployment_id>/` before the worker is contacted.
  Promotion is the only transition that rotates the active deployment;
  rollback uses the same prepare/warm/promote sequence on the
  previous-active record and refuses with `Unavailable` when no
  previous active exists or when the previous bundle's staged files
  are missing. Failed candidates record the last-successful phase and
  move into the bounded `quarantined` list with a typed error;
  active deployment is preserved across every candidate failure.
- Agent → serving-worker control surface (V01-E08-F05) at
  `protocol/schemas/worker_control.json` and `agent/src/worker.rs`.
  Worker IPC is modelled as the narrow `WorkerControl` trait
  (`prepare`, `warm`, `promote`, `unload`, `active_deployment_id`)
  so the coordinator depends on an interface, not on a concrete
  process. v0.1.0 ships the deterministic `MockWorkerControl` used by
  host CI and a process-backed `ProcessWorkerControl` selected with
  `worker.mode=process`; process mode renders a V01-E07 serving config,
  starts `tensorplate-serving`, polls `/health`, and promotes only
  warmed candidates. Prepare/warm operations are bounded by
  configurable timeouts (`worker.prepare_timeout_ms`,
  `worker.warm_timeout_ms`) and surface `Error::Code::Timeout` on
  expiry. `unload` of the previous active is best-effort and never
  undoes a successful promotion.
- Recovery planner (V01-E08-F07) at `agent/src/recovery.rs` computes a
  typed `RecoveryAction` from durable state plus (best-effort) the
  worker's actual active deployment id. Replayable phases recommend
  `resume_verify` / `resume_stage` / `resume_prepare`; worker-side
  phases recommend `quarantine_candidate`; promoted-but-not-finalized
  states return `finalize_promotion` only when the worker-reported
  active deployment matches the transaction target; agreement between
  desired and actual returns `no_op`; disagreement returns
  `operator_required`. Recovery is state-diff based and never replays
  commands solely because they appeared in the original request order.
- `tensorplate-agent` binary entrypoint (`agent/src/main.rs`) reads
  `--config <path>` or `--config-json <inline>`, validates the agent
  config against `config/schemas/agent.json`, opens the durable state
  store, applies startup recovery before binding the local control
  socket, and starts the local control API. v0.1.0 relies on systemd /
  supervisor to deliver SIGTERM; durable mutations are atomic so
  termination at any point leaves state consistent.
- Integration test suite at `agent/tests/` (V01-E08-F08): a shared
  `Harness` builder creates isolated `state_dir` + `staging_dir`
  directories per test and wires the coordinator to a configurable
  `MockWorkerControl`. Four test files cover the deploy happy path,
  failure injection (corrupt artifact, unsupported backend, capacity
  overflow, prepare failure, warm-not-ready), rollback and restart
  recovery, and the full UDS round-trip through the local control API.
- New protocol Rust modules `agent_control`, `agent_state`,
  `bundle_manifest`, and `worker_control` (each a serde mirror with
  validating constructors and `decode_with_version_check` semantic
  validation). Architecture documentation lives in
  `docs/architecture/agent.md`; the protocol schema index in
  `protocol/schemas/README.md` lists the new envelopes.
- `tensorplate-serving` worker and loopback HTTP data-plane endpoint
  (V01-E07). The composition root (`include/tensorplate/serving/
  worker.hpp`, `runtime/src/serving/worker.cpp`) wires
  `BufferManager`, the `BackendRegistry`-resolved `ExecutionSession`,
  `make_scheduler`, the request router, the async-policy store, the
  HTTP server, metrics, health, and the shutdown controller in a
  single deterministic order. The new in-tree HTTP/1.1 server
  (`runtime/src/http/http_server.cpp`) is loopback-only by default
  and enforces `max_body_bytes`, `max_header_bytes`,
  `request_timeout`, and a bounded accept-queue depth before any
  buffer-plane allocation happens. Public route contract:
  `POST /infer` (sync), `POST /policy/infer` + `GET /policy/result/
  <id>` + `POST /policy/cancel/<id>` (LeRobot PolicyServer-compatible
  async chunk pattern without a bridge), `GET /health`, and
  `GET /metrics`. Every response carries `x-correlation-id`.
- Serving worker config schema (V01-E07-F01). New file
  `config/schemas/serving_worker.json` documents the JSON config
  consumed by `tensorplate::ServingConfig::parse_json`. Validation
  rejects non-loopback bind without an explicit opt-in, zero-byte
  HTTP limits, missing model for non-mock deployments, and unknown
  schema versions with typed `Error::Code` values. `--config <path>`,
  `--config-json <inline>`, `--bind-host`, `--bind-port`, and
  `--mock` CLI flags are wired into `serving_worker/src/main.cpp`,
  along with SIGINT/SIGTERM graceful-shutdown signal handlers and
  the documented exit-code matrix (Ok / ConfigError / LoadError /
  ServeError / Internal).
- LeRobot-compatible async-policy state store (V01-E07-F04). The
  in-process `AsyncPolicyStore` (`include/tensorplate/serving/
  async_policy.hpp`) records the lifecycle of every accepted async
  request (`pending`, `in_flight`, `completed`, `cancelled`,
  `stale`, `failed`, `expired`), enforces `max_pending` /
  `max_completed` / `completed_ttl_ms` bounds, and runs the
  stale-sequence cancellation that fans out to
  `InferScheduler::cancel(StaleSequence)` when an incoming request
  carries `metadata.stale_after_sequence`. The wire route contract
  is documented in `protocol/schemas/serving_http_envelope.json`
  alongside the rest of the v0.1.0 envelopes.
- Serving pipeline (V01-E07-F05) connecting normalized requests to
  scheduler admission, dispatch, completion, and buffer release.
  The pipeline holds the scheduler and the session through their
  public interfaces only; success and failure paths both release
  request-owned buffers exactly once through `release_request_buffers`,
  and partial outputs after suppressed delivery are released via
  `release_partial_outputs`. The dispatcher thread drains the
  scheduler for async requests and an evictor thread enforces async-
  policy retention bounds; both stop cleanly during graceful
  shutdown.
- Health, metrics, and structured-log fan-out for the serving worker
  (V01-E07-F06). `HealthState` (`protocol/schemas/serving_health.json`)
  publishes `starting` / `ready` / `degraded` / `failed` /
  `stopping` / `draining` / `stopped` with HTTP status mapping that
  keeps `degraded` at 200 so agents read the discriminator field
  instead of flapping liveness probes. `ServingMetrics` is a
  thread-safe counter / histogram bag with four bounded labels
  (`endpoint`, `model_class`, `model_name`, `backend`), the
  Prometheus 0.0.4 text exposition body, and a JSON mirror
  documented in `protocol/schemas/serving_metrics.json`. Latency
  histograms use the v0.1.0 bucket layout
  (`0.5, 1, 2, 5, 10, 25, 50, 100, 250, 1000, 5000, +Inf` ms)
  shared by V01-E12. Correlation IDs are generated at ingress when
  the client does not supply one and echoed through metadata,
  responses, and `x-correlation-id` headers.
- Graceful shutdown controller for the serving worker
  (V01-E07-F07). `ShutdownController` walks `Running` ->
  `Stopping` -> `Draining` -> `Stopped`; the composition root stops
  the HTTP listener, runs `InferScheduler::shutdown`, waits up to
  the configured `drain_deadline_ms`, calls
  `AsyncPolicyStore::cancel_all`, and unloads the active session
  exactly once. The integration suite asserts
  `BufferManager::accounting().active_count == 0` after teardown.
- End-to-end serving worker integration tests (V01-E07-F08) at
  `test/integration/serving_e2e_test.cpp`. Fourteen T2 cases cover
  `/health`, `/metrics`, `/infer` happy-path, correlation-id
  propagation through `metadata`, malformed / oversized / duplicate-
  input rejection, the LeRobot-compatible async accept + result +
  cancel cycle, 404 / 405 routing, shutdown-during-flight buffer
  cleanup, and admission rejection while stopping. The
  `test/mocks/serving_http_client.hpp` helper drives the worker
  through real TCP loopback connections. Unit-level coverage at
  `test/unit/serving_{config,serialization,health_metrics,
  async_store}_test.cpp` (38 cases total) covers config validation,
  base64 round-trip, request-decoder error paths, health-state
  transitions, the rejection-code -> metric mapping, latency
  histogram bucketing, the Prometheus and JSON exporters, and the
  async-store stale-sequence + bounded-retention behavior. Tests
  exercise the public `ServingWorker` interface, run against the
  built-in mock session on host CI, and require no real backend.
- Serving worker architecture documentation
  (`docs/architecture/serving-worker.md`). Captures the composition
  root, HTTP framework selection rationale (loopback by default,
  request limits enforced before buffer allocation, graceful
  shutdown, testability, no third-party server dependency beyond
  `nlohmann::json` and POSIX sockets), route contract, typed-error
  -> HTTP-status mapping, LeRobot-compatible async semantics,
  health / metrics / correlation-id propagation, and the shutdown
  state machine. Indexed from `docs/architecture/README.md`.
- SmolVLA-style async chunk and stale-cancel scheduler fixtures
  (V01-E06-F07). New shared mocks at `test/mocks/vla_fixtures.hpp`
  (named multi-input payload `image_front` /`proprioception` /
  `instruction`, action-chunk identity, LeRobot
  `stale_after_sequence` marker, helper that filters queued
  envelopes by stale sequence, all backed by small fake buffers
  through a real `BufferManager`). New T2 coverage at
  `test/integration/scheduler_smolvla_test.cpp` (7 cases) covers
  overlapping chunk admission and arrival-order dispatch, queued
  stale-sequence cancellation with deterministic buffer release,
  in-flight stale cancellation observability through the
  `SchedulerEvent` (`cancellation_reason = stale_sequence`),
  deadline-margin admission rejection under load, queued expiry
  under overlapping requests, and a mixed admit/dispatch/complete
  /expire/cancel flow that asserts metrics counts and
  `BufferManager::accounting().active_count == 0` end-to-end.
  Tests run against a mock executor / `InferScheduler*` pointer
  and do not require SmolVLA weights.
- Scheduler memory and thermal pressure-aware admission
  (V01-E06-F06). New protocol schema at
  `protocol/schemas/scheduler_pressure_signal.json` documents the
  `PressureSignal` value object (`source`, `severity`,
  `timestamp_unix_nanos`, optional bounded `detail`) without any
  vendor SDK type. The scheduler records the most recent severity
  per source; `SchedulerConfig::pressure_reject_threshold` selects
  whether warning- or critical-level pressure rejects new admission
  with `Error::Code::OOMError` (incrementing
  `admission_rejected_pressure`) or runs in record-only mode.
  Queued and in-flight work is never killed solely by a pressure
  signal at v0.1.0 baseline. T1 coverage at
  `test/unit/scheduler_pressure_test.cpp` (10 cases) including the
  V01-E03 `BufferAccounting::pressure -> PressureSeverity` mapping
  used to bridge buffer-plane accounting into the scheduler.
- Scheduler metrics and event protocol schemas (V01-E06-F05) at
  `protocol/schemas/scheduler_metrics.json` and
  `protocol/schemas/scheduler_event.json`. The metrics snapshot
  documents queue depth / in-flight count / accepted / rejected
  (overload / deadline / pressure) / expired / cancelled / completed
  (success / failure) / pressure-event counters, plus wait-time
  aggregates (sum / samples / max) using monotonic
  steady-clock nanoseconds. The event schema documents the bounded
  event labels (`endpoint`, `backend_name`, `policy`,
  `error_code`, `completion_status`, `cancellation_reason`,
  `pressure_source`, `pressure_severity`, `wait_time_ns`,
  `timestamp_unix_nanos`) emitted on every state transition. T1
  coverage at `test/unit/scheduler_metrics_test.cpp` (10 cases)
  asserts counter increments per state-transition path, event
  ordering, bounded labels, and that a throwing event sink cannot
  break the scheduler critical path or counter accuracy.
- Scheduler completion, cancellation, and buffer-cleanup coverage
  (V01-E06-F04) at `test/unit/scheduler_cancellation_test.cpp`.
  `on_completion` removes in-flight accounting exactly once;
  duplicate completion and completion of an unknown id return
  typed `Error::Code::Internal` no-ops. `cancel` handles queued and
  in-flight requests by id: queued cancellation removes from the
  queue and releases input `BufferRef`s through the V01-E03 cleanup
  helpers; in-flight cancellation clears accounting and tombstones
  the id so a racing `on_completion` is a typed no-op. Double
  cancel and cancel-after-completion surface
  `Error::Code::NotReady`. `expire_due()` releases queued input
  buffers on stale-deadline removal. `shutdown()` drains every
  queued request (releasing buffers), tombstones every in-flight id,
  and flips subsequent admits to `Error::Code::NotReady`. SmolVLA-
  style async chunk requests (with `RequestMetadata::action_chunk_id`
  /`action_chunk_sequence`) and synchronous vision requests share
  the same cleanup path. 12 T1 cases.
- Deadline-aware admission and queued-expiry coverage (V01-E06-F03)
  at `test/unit/scheduler_deadline_test.cpp`. The `FifoScheduler`
  uses the injected `SchedulerClock` (monotonic only) for every
  deadline decision and rejects new admission with
  `Error::Code::Timeout` when a request is already past its deadline
  or when its estimated completion exceeds `deadline +
  deadline_margin`. The estimate accounts for current queue depth
  and in-flight count using the configured
  `default_service_estimate`; per-request `ServiceEstimate` overrides
  the default. `expire_due()` and `next()` both sweep stale queued
  requests, releasing input buffers through `release_request_buffers`
  when a `BufferManager` is wired into runtime hooks. 12 T1 cases
  cover boundary admission, monotonic-time isolation from wall
  clock, queue-depth-aware rejection, and deterministic
  buffer release on rejection. The shared `FakeSchedulerClock`
  default origin is now anchored to real `steady_clock` + 1 hour so
  deadlines composed against the fake clock also satisfy
  `InferRequest::create`'s validation gate.
- FIFO scheduler ordering and capacity coverage (V01-E06-F02) at
  `test/unit/scheduler_fifo_test.cpp`. The v0.1.0 default
  `FifoScheduler` (registered under the stable `fifo` policy key)
  preserves enqueue order among admitted requests, enforces
  `queue_capacity` with `Error::Code::OOMError`, gates dispatch on
  `in_flight_capacity`, increments in-flight on dispatch (not on
  enqueue), and exposes queue depth / in-flight count / wait-time
  high water through the `metrics()` snapshot without leaking the
  internal `std::deque` to callers. 13 T1 cases assert the dispatch
  order, capacity behavior, completion-frees-slot semantics, and
  that mock executor code only holds `InferScheduler*` (not
  `FifoScheduler*`).
- `InferScheduler` public interface (V01-E06-F01) at
  `include/tensorplate/scheduler/scheduler.hpp` plus the supporting
  envelope (`SchedulerRequest`), monotonic clock abstraction
  (`SchedulerClock` / `SystemSchedulerClock`), pressure value
  objects (`PressureSignal`, `PressureSource`, `PressureSeverity`),
  and the `SchedulerEvent` / `SchedulerEventSink` /
  `SchedulerMetrics` types. Includes the `InferSchedulerConcept`
  compile-time interface check. Strategy pattern is mediated by a
  `SchedulerPolicyRegistry` and the `make_scheduler` /
  `validate_scheduler_config` factory entry points in
  `include/tensorplate/scheduler/factory.hpp`. v0.1.0 registers the
  built-in `fifo` policy; unknown policies return
  `Error::Code::Unsupported`. New config schema at
  `config/schemas/scheduler.json`. Architecture doc at
  `docs/architecture/scheduler.md`. T1 coverage at
  `test/unit/scheduler_interface_test.cpp` plus shared mocks at
  `test/mocks/fake_scheduler_clock.hpp` and
  `test/mocks/scheduler_fixtures.hpp`.
- Kria / Vitis AI adapter design-review document at
  `docs/architecture/kria-vitis-ai-review.md` (V01-E05-F07). Maps a
  future Xilinx/AMD Kria adapter using Vitis AI and DPU execution
  against the published v0.1.0 contracts (`ExecutionSession` NVI,
  `BackendCapability`, `BackendRegistry`, `ModelSpec` and the
  `backend_hint` enum, `BufferRef` / `TensorView`, the bundle
  envelope, and the session event taxonomy). The review concludes
  that **no public interface change is required for v0.1.0 freeze**;
  the only work required for a future Vitis AI adapter is a new
  `runtime/src/adapters/vitis_ai/` directory, a bundle sibling block
  for Vitis-style calibration metadata (addable through the bundle format
  schema-evolution rules), a T1 unit-test set mirroring the
  TensorRT / LibTorch pattern, and Kria K26 / K24 HIL validation.
- Real-adapter conformance harness (V01-E05-F06-T01) at
  `test/contract/real_adapter_conformance_test.cpp` (T3). Reuses the
  V01-E04 `ExecutionSession` conformance suite from
  `test/contract/execution_session_conformance.hpp` and runs it
  against every adapter compiled into this build of `tp_runtime`. The
  `python_pytorch` adapter passes the full lifecycle suite on host CI
  via the `FixtureBackend`; the TensorRT and LibTorch variants run
  only when their SDKs are detected (HIL/release tier per
  V01-E05-F02 / F03). `test/CMakeLists.txt` now compiles
  `tp_test_contract` and labels its tests `T3`.
- Sidecar failure-injection tests (V01-E05-F06-T03) at
  `backends/python_pytorch/tests/test_failure_injection.py`. The
  `FixtureBackend` exposes `fail_load` / `fail_prime` / `fail_infer`
  hooks; the new tests cover typed `load_failed`, `inference_failed`,
  `config_invalid` (missing model_spec, malformed tensor entry), and
  the cancel-then-recordable-by-backend path. Combined with the
  V01-E05-F04 runner tests and C++ supervisor shutdown behavior, the
  host baseline covers typed runner errors, timeout/cancel message
  handling, malformed request rejection, and deterministic cleanup
  paths; heartbeat-driven liveness and externally killed sidecar
  recovery remain scheduler/supervision follow-up work.
- Golden-output fixture matrix and tolerance documentation at
  `test/models/GOLDEN_FIXTURES.md`. Defines what a golden fixture
  means for each adapter family, where each runs in the CI tier, how
  it is generated, what its expected output and tolerance are, and
  how the JSON comparison helper will work when the first real
  fixture lands. The fixture backend round-trip already covers exact
  bytewise correctness for `python_pytorch`; vision-TensorRT and
  LibTorch golden artifacts land in V01-E05-F02-T03 / F03-T03 /
  release validation.
- Python/PyTorch sidecar execution-backend adapter and supervisor
  (V01-E05-F05) under `runtime/src/adapters/python_pytorch/`,
  registered as `python_pytorch`. The adapter forks one Python sidecar
  subprocess per execution session (the V01-E05 closed decision), binds
  a Unix-domain socket under `TMPDIR`, accepts the child's connection,
  reads its `ready_event`, and translates `ExecutionSession::load /
  prime / infer / unload` into the sidecar IPC schema (V01-E05-F04).
  The sidecar wire protocol includes `infer_async`, but the C++ adapter
  keeps native async disabled until V01-E06 provides a real completion
  channel; `ExecutionSession::infer_async` therefore returns typed
  `Unsupported` without dispatching or allocating outputs. Capability
  record declares dynamic-shape support; async, generation, streaming,
  and KV-cache remain false.
- `SidecarProcess` (in `runtime/src/adapters/python_pytorch/`) owns the
  subprocess + socket pair, terminates the child with SIGTERM (or
  SIGKILL after a 500 ms grace period) on unload / error, and unlinks
  the socket path. `SidecarLauncher` is injectable so tests can run the
  Python runner via a non-default interpreter without touching the
  adapter code. The built-in factory honors
  `TP_PYTHON_PYTORCH_EXECUTABLE`, `TP_TEST_PYTHON_EXE`, then
  `TP_TEST_PYTHON` before falling back to `python3`; the default
  launcher does `fork()` +
  `execvp(python3, "-m", "tensorplate_pytorch_backend", "--socket",
  path)`.
- Input/output tensor marshaling: inputs are read out of `BufferManager`
  via `manager->view(buffer, tensor_view)`, packed into the sidecar
  payload region, and described in the JSON header's `tensors[]` array.
  Outputs are sliced back out of the response payload by
  `payload_offset / payload_length`, written into freshly allocated
  output `BufferRef`s, and surfaced through `NamedOutput`. The adapter
  refuses to construct a session without a `BufferManager` hook
  (`Error::Code::ConfigInvalid`).
- Timeout, cancellation, and health handling: per-operation deadlines
  use `std::chrono::steady_clock` clamped against `InferRequest`'s
  monotonic deadline; sidecar timeouts surface as
  `Error::Code::Timeout`; malformed response frames map to
  `Error::Code::InferenceFailed`. `prime` performs a real
  `health_check` round-trip and requires a ready health payload before
  publishing the session as ready. The adapter terminates the sidecar on
  unload, load failure, and transport failure so the OS does not retain
  a zombie. Adapter-owned heartbeat polling and `Cancel` dispatch are
  reserved for the scheduler/supervision wiring that owns async result
  delivery.
- `TP_ENABLE_PYTHON_PYTORCH_SIDECAR` is flipped on by default in
  `runtime/CMakeLists.txt`. When the flag is on the runtime links
  `nlohmann_json::nlohmann_json` (header-only) for JSON header
  encode/decode.
- T2 integration test in
  `test/integration/python_pytorch_adapter_test.cpp` exercises the full
  C++ ↔ Python lifecycle through the `FixtureBackend`: registration,
  capability publication, end-to-end echo (load → prime → infer →
  unload through real Unix-socket IPC), wrapper-level `infer_async`
  returning typed `Unsupported` without output allocation, and
  infer-before-prime returning `NotReady`. C++ CI now installs
  `backends/python_pytorch` before running T2/T3 so the round-trip is
  exercised instead of silently skipped.
- C++ CI now includes an adapter-shell job that builds with
  `TP_ENABLE_TENSORRT=ON` and `TP_ENABLE_LIBTORCH=ON` on a host without
  proprietary SDKs, then runs T1. This keeps the no-SDK registration and
  typed `Unsupported` paths compiling even when the default host matrix
  leaves hardware adapters disabled.
- Python/PyTorch sidecar IPC contract and Python backend runner
  (V01-E05-F04). The on-wire envelope is documented in
  `include/tensorplate/ipc/sidecar_codec.hpp`:
  `[u32 magic 'TPSC'][u32 wire_version][u32 header_len][u32 payload_len]
   [JSON header][raw tensor payload]`, all u32 fields big-endian, with
  generous-but-bounded maxima (1 MiB header, 256 MiB payload). The
  schema for the JSON header lives in
  `protocol/schemas/python_pytorch_ipc.json` and covers the seven
  request kinds (`load_model`, `prime`, `infer`, `infer_async`,
  `cancel`, `unload`, `health_check`) plus matching `*_response`
  kinds and the unsolicited `ready_event` / `error_event` /
  `metric_event` events. Successful `infer_async_response` headers carry
  `async_id`; successful `health_check_response` headers carry a bounded
  `health` payload (`ready`, `backend_factory`, `uptime_ns`,
  `last_error`). The Rust protocol mirror models these fields so schema
  fixtures do not drift from the Python runner.
- C++ codec helpers (`encode_frame`, `decode_frame`, `decode_frames`)
  that distinguish typed `Error::Code::NotReady` ("need more bytes")
  from `Error::Code::ConfigInvalid` ("malformed frame") so adapters can
  loop on streaming reads safely. Implemented in
  `runtime/src/ipc/sidecar_codec.cpp`; covered by
  `test/unit/sidecar_codec_test.cpp` for round-trips, partial-prefix
  / partial-body, bad magic, bad wire version, oversized header /
  payload, and multi-frame pipelines stopping at a partial frame.
- `include/tensorplate/ipc/unix_socket.hpp` plus
  `runtime/src/ipc/unix_socket.cpp`: minimal RAII `UnixSocket` wrapper
  around POSIX stream sockets with monotonic-deadline-aware
  `connect`, `bind_and_listen`, `accept`, `read_exact`, and
  `write_all`. Returns `Error::Code::Timeout` on deadline exhaustion
  and `Error::Code::ConfigInvalid` for paths exceeding `sun_path`.
  Covered by `test/integration/sidecar_socket_e2e_test.cpp` which
  forks a child and round-trips one frame end-to-end.
- Python backend runner under
  `backends/python_pytorch/src/tensorplate_pytorch_backend/`:
  `codec.py` mirrors the C++ wire format; `protocol.py` enumerates
  the schema constants; `backends/` ships the dependency-free
  `FixtureBackend` (echoes inputs as `echo_<name>` outputs) plus the
  `Backend` Protocol that the V01-E05-F05 TorchScript / SmolVLA
  backend will implement; `runner.py` runs a synchronous
  request/response loop with typed `BackendError` mapping to
  `*_response status: error` frames, and is wired as the
  `tensorplate-backend-python-pytorch` console script in
  `pyproject.toml`. Twenty-one pytest tests under
  `backends/python_pytorch/tests/` cover the codec round-trips, the
  lifecycle happy path through the fixture backend, infer-before-load
  (`not_ready`), unknown / bad-version (`unsupported`),
  cancel-then-infer (`timeout`), health-check, async-infer
  identification, and payload-window overflow (`shape_mismatch`).
- `nlohmann-json` added to `vcpkg.json` (header-only) so the V01-E05-F05
  C++ adapter can parse the JSON sidecar header without re-implementing
  a JSON decoder.
- LibTorch native execution backend adapter shell under
  `runtime/src/adapters/libtorch/` (registered as `libtorch`). Loads
  TorchScript (`torch::jit::load`) modules and is positioned as a
  reference / native-C++ backend, *not* a fallback for
  `python_pytorch` bundles. Capability record advertises
  FP32/FP16/BFloat16 precision and dynamic-shape support; async,
  generation, streaming, and KV-cache flags remain false. The adapter
  source compiles when `TP_ENABLE_LIBTORCH=ON`; when CMake also locates
  a LibTorch C++ distribution (`Torch_DIR` -> `find_package(Torch)`),
  it defines `TP_HAS_LIBTORCH_SDK=1` and the adapter loads the
  TorchScript module, maps row-major `BufferManager` inputs to CPU
  tensors, executes synchronous `forward`, and materializes Tensor or
  Tuple[Tensor, ...] outputs back into owned output buffers. Without
  the SDK the adapter still registers and
  surfaces typed `Error::Code::Unsupported` from `do_load` with an
  actionable rebuild hint. T1 unit tests in
  `test/unit/libtorch_adapter_test.cpp` cover registration, capability
  publication, the no-SDK `Unsupported` path, and explicit verification
  that `backend_hint: python_pytorch` does not silently redirect to
  LibTorch (V01-E05-F03-T01 / T02). Exported-graph fixture generation,
  SDK-enabled T3 evidence, and Jetson T4 conformance land in
  V01-E05-F03-T03 and V01-E05-F06.
- TensorRT execution backend adapter shell under
  `runtime/src/adapters/tensorrt/` (registered as `tensorrt`). The
  adapter publishes its `BackendCapability` (FP32/FP16/INT8, fixed-shape
  binding, sync execution only) and owns TensorRT and CUDA SDK handles
  privately through RAII wrappers (`TensorRTState`, `CudaStreamHandle`,
  `CudaDeviceBuffer`). The adapter compiles when `TP_ENABLE_TENSORRT=ON`;
  if the CMake configuration detects an installed TensorRT/CUDA SDK it
  defines `TP_HAS_TENSORRT_SDK=1` and the adapter deserializes the
  engine file and creates the runtime/engine/execution context. Without
  the SDK the adapter still registers and surfaces typed
  `Error::Code::Unsupported` from `do_load` with an actionable message
  so `tensorplate doctor` can enumerate it (V01-E05-F02-T01 / T02).
- T1 unit tests under `test/unit/tensorrt_adapter_test.cpp` covering
  registration under the stable key, capability-record consistency,
  backend_name on the constructed session, load-without-SDK returning
  `Unsupported`, and `validate_backend_hint` precision filtering.
  Vision golden conformance (T3) and Orin HIL validation (T4) land in
  V01-E05-F02-T03 / V01-E05-F06.
- `include/tensorplate/backend/capability.hpp` and
  `include/tensorplate/backend/registry.hpp` defining the vendor-neutral
  `tensorplate::BackendCapability` value object and the thread-safe
  `tensorplate::BackendRegistry` used by bundle validation and
  execution-session creation. Capability records publish backend name,
  optional profile id, supported precision list, shape-support tier,
  async / generation / streaming / KV-cache flags, op-coverage
  percentage, and memory estimate/limit. The registry rejects empty
  keys, null factories, and capability/name mismatches with
  `Error::Code::ConfigInvalid`; duplicate registration returns
  `Error::Code::Internal`; unknown backends surface as
  `Error::Code::Unsupported`. `validate_backend_hint` rejects unknown
  backends and declared precisions that the adapter does not advertise
  without falling back at inference time (V01-E05-F01).
- `include/tensorplate/backend/builtin.hpp` exposes
  `register_builtin_backends(BackendRegistry&)` so callers (the serving
  worker, conformance tests, doctor checks) can opt their registry into
  the adapter set compiled into `tp_runtime`. Adapter availability is
  driven by the new `TP_ENABLE_TENSORRT`, `TP_ENABLE_LIBTORCH`, and
  `TP_ENABLE_PYTHON_PYTORCH_SIDECAR` CMake options (all OFF by default
  for host CI; flipped on per adapter in V01-E05-F02 / F03 / F05).
- `protocol/schemas/backend_capability.json` mirrors `BackendCapability`
  so capability records can cross process boundaries without leaking
  adapter-specific types.
- `include/tensorplate/core/execution_session.hpp` defining the canonical
  public `tensorplate::ExecutionSession` lifecycle interface. The public
  method set is `load`, `prime`, `infer`, `infer_async`, `unload`,
  `is_ready`, and `backend_name`; lifecycle methods are non-virtual NVI
  wrappers and adapters override protected `do_*` implementation methods.
  No vendor SDK type appears in the header (V01-E04-F01-T02).
- `docs/architecture/execution-session.md` documenting the canonical
  `ExecutionSession` name decision (selected over the alternate
  `ModelLoader` spelling carried in the older implementation guidelines),
  the NVI pattern, the lifecycle state machine, the async method shape,
  the event taxonomy, and the non-GPU compatibility review notes
  (V01-E04-F01-T01).
- `tensorplate::SessionState` enum (`unloaded`, `loaded`, `ready`,
  `failed`) with `to_string` / `session_state_from_string` helpers, and
  `tensorplate::AsyncInferHandle` carrying `request_id` plus a
  session-scoped monotonically increasing `async_id`.
- `tensorplate::SessionEventKind` enum and `tensorplate::SessionEvent`
  record with `to_string` / `session_event_kind_from_string` helpers,
  plus the `tensorplate::SessionEventSink` interface used by the NVI
  wrapper to emit lifecycle and inference events.
- Session lifecycle state machine wiring the V01-E04-F01 public methods
  through the protected `do_*` adapter override points: `load`
  transitions `unloaded -> loaded` (or `unloaded -> failed`), `prime`
  transitions `loaded -> ready` (or `loaded -> loaded` on
  `ConfigInvalid`, otherwise `loaded -> failed`), `unload` returns any
  state to `unloaded` (or transitions to `failed` on adapter failure),
  and `infer` / `infer_async` surface `Error::Code::NotReady` before
  any adapter dispatch unless the session is `Ready`. The state
  machine is adapter-neutral and intentionally general enough for
  TensorRT engine setup, LibTorch model load, Python sidecar startup,
  and a future Vitis AI `.xmodel` / DPU lifecycle (V01-E04-F02).
- Shared mock `tensorplate::testing::MockSession` under `test/mocks/`
  that drops into `ExecutionSession*` and lets tests program adapter
  success/failure and inspect adapter dispatch counts and last-seen
  request/spec. Used by the V01-E04 lifecycle, validation, timing,
  async, event-emission, and conformance test suites.
- NVI readiness and validation gates in `ExecutionSession::infer` and
  `ExecutionSession::infer_async`: requests are rejected before any
  adapter dispatch when the session is not `Ready` (`NotReady`), when
  `request_id` / `endpoint` / `inputs` are empty (`ConfigInvalid`), on
  empty or duplicate input names (`ConfigInvalid`), on released or
  missing input buffers (`ConfigInvalid`), on tensor byte windows that
  do not fit inside their owning buffers (`ShapeMismatch`), and on
  already-expired monotonic deadlines (`Timeout`). The gates apply
  uniformly to sync and async paths so adapter `do_infer` /
  `do_infer_async` implementations cannot bypass them (V01-E04-F03).
- Monotonic latency stamping in `ExecutionSession::infer`: the wrapper
  measures `execution_latency` around the adapter `do_infer` call using
  `std::chrono::steady_clock` (no wall-clock dependency) and stamps it
  into the returned `InferResult` on both success and adapter-failure
  paths. Readiness and validation failures bypass the adapter entirely
  and surface as `Result::error` rather than a failure `InferResult`
  (V01-E04-F04-T01).
- Output validation in `ExecutionSession::infer`: empty outputs vectors,
  empty or duplicate output names, released output buffers, and tensor
  byte windows that overflow their buffers are all rejected before
  success is returned. When a `BufferManager` is supplied through
  adapter construction hooks, partial adapter-published outputs are
  released via `release_partial_outputs` so a failed `infer` does not
  leak buffer capacity (V01-E04-F04-T02).
- `ExecutionSession::infer_async` typed unsupported path: the default
  wrapper path returns `Error::Code::Unsupported` so v0.1.0 adapters
  without native async satisfy the public method shape without
  pretending to be async. Readiness (`NotReady`) and request validation
  errors (`ConfigInvalid`, `ShapeMismatch`, `Timeout`) are surfaced
  **before** the unsupported capability is considered, and the
  unsupported path allocates no output buffers and never dispatches to
  adapter execution. Native-async adapters opt in through the protected
  capability hook and override `do_infer_async` to return an
  `AsyncInferHandle` whose `async_id` is session-scoped and
  monotonically increasing through the `next_async_id()` helper
  (V01-E04-F05).
- Shared V01-E04 ExecutionSession conformance suite at
  `test/contract/execution_session_conformance.hpp`. A
  `tensorplate::testing::SessionFactory` closure plus a
  `ConformanceConfig` drives any `ExecutionSession*` adapter through
  backend-name identity, initial not-ready state, the load -> prime ->
  infer -> unload happy path, infer-before-prime, prime-before-load,
  bad model path, shape mismatch, infer_async (typed Unsupported or
  handle), unload-then-infer, and `BufferRef` lifetime invariants. Real backend
  adapters (TensorRT, LibTorch, Python/PyTorch sidecar, future Vitis
  AI) reuse the same suite without rewriting it. A T1 mock-conformance
  test in `test/unit/execution_session_conformance_test.cpp` runs the
  suite through `MockSession` so the suite is self-testing
  (V01-E04-F07-T01).
- `docs/architecture/non-gpu-lifecycle-review.md` recording the
  V01-E04-F07-T02 non-GPU lifecycle compatibility review and sign-off.
  The review walks `ExecutionSession`, `ModelSpec`, `BufferRef`,
  `TensorView`, and the event taxonomy against a future Kria/Vitis AI
  adapter (`.xmodel` discovery, DPU runner instantiation, fixed-shape
  binding, INT8 calibration metadata, adapter-owned memory copies) and
  confirms the V01-E04 interface is implementable without public
  interface revision before V01-E05 adapter work begins. A compile-time
  macro guard in the T1 interface test mechanically enforces that no
  CUDA / TensorRT / LibTorch / Vitis AI / XRT / DPU SDK type leaks into
  the public ExecutionSession header (V01-E04-F07-T02).
- Lifecycle and inference event emission from every public NVI wrapper.
  `load`, `prime`, `infer`, `infer_async`, and `unload` emit paired
  `*_start` / `*_end` events on success and `*_failed` (or
  `validation_failed` for pre-dispatch rejection, `unsupported_async`
  for the typed Unsupported async path) on failure. Each event carries
  bounded fields (`backend_name`, optional `model_id`, optional
  `request_id`, optional `Error::Code`, monotonic `duration`, and
  `state_after`) and no raw payload bytes. Emission is wrapped in a
  defensive `try { ... } catch (...) {}` so a throwing sink cannot
  corrupt session state; the wrapper continues the lifecycle path
  unchanged. Tests use the new `tensorplate::testing::RecordingEventSink`
  and `tensorplate::testing::ThrowingEventSink` shared mocks
  (V01-E04-F06).
- Developer-facing C++ example `tensorplate-example-buffer-plane` under
  `examples/buffer_plane/` that walks the V01-E03 buffer plane end to
  end: ingress copy → `BufferRef` + `TensorView` → `InferRequest` →
  mock policy → `InferResult` → cancellation cleanup → double-release
  diagnosis → pressure-event draining. Toggle with the
  `TP_BUILD_EXAMPLES` CMake option (defaults ON).
- T2 integration test `tp_test_integration` exercising the same loop
  under GoogleTest (`test/integration/buffer_plane_e2e_test.cpp`).
- Memory-pressure event shape and emission: `MemoryPressure` level
  (normal / warning / critical), `BufferPressureEvent` payload (pool
  name, previous + current level, capacity, in-use bytes, active count,
  high-water mark, allocation failures), and a bounded event ring drained
  through `BufferManager::drain_pressure_events`. The buffer manager
  records one event per threshold crossing without invoking callbacks or
  I/O on allocation/release paths. Mirrored on the wire in
  `protocol/schemas/buffer_pressure_event.json` and in the Rust
  `tensorplate-protocol` crate (V01-E03-F06).
- Session output helpers `allocate_output_buffer`, `build_named_output`,
  and `build_named_outputs`. The execution session allocates one owned
  buffer per output, pairs it with a validated `TensorView` byte window,
  and assembles `NamedOutput` value objects (including chunk-shaped VLA
  action outputs). Multi-output builds reject duplicate names and
  release any partial allocations on later failure (V01-E03-F05).
- Ingress copy helpers `copy_payload_into_buffer` and
  `build_named_inputs` that turn caller-owned byte payloads into
  buffer-plane-owned `BufferRef` storage with a single copy. Multi-input
  builds reject duplicate names, oversized payloads, and tensor-window
  metadata that does not fit the allocated buffer; partial allocations
  are released before an error is returned. Shared vision and
  SmolVLA-style payload fixtures live in
  `test/mocks/ingress_fixtures.hpp` and will be reused by the V01-E07
  HTTP router (V01-E03-F04).
- Buffer cleanup helpers `release_request_buffers`,
  `release_partial_outputs`, and the `RequestBufferGuard` RAII wrapper.
  Helpers release every unique buffer id at most once, never throw, avoid
  allocation on the successful cleanup path, preserve original request
  errors, and report release failures through a `CleanupReport`. Used by
  scheduler cancellation, deadline expiry, and execution-session error
  paths (V01-E03-F03).
- `BufferManager` v0.1.0 CPU buffer plane: capacity-bounded allocator with
  monotonic ids, aligned heap-backed storage, validated configuration,
  thread-safe allocate/release/data/view access, accounting snapshot
  (in-use bytes, active count, high-water mark, allocation/release failure
  counters), and derived `MemoryPressure` level. Storage is freed exactly
  once; double-release and stale-handle release return typed
  `Error::Code::Internal` (V01-E03-F01, V01-E03-F02).
- `docs/architecture/buffer-plane.md` describing the buffer-plane
  ownership model, copy/move semantics, cleanup-path contracts, and the
  scope boundaries that V01-E03 deliberately respects (V01-E03-F02).
- Top-level package skeleton for v0.1.0: `include/tensorplate/`, `runtime/`,
  `serving_worker/`, `agent/`, `cli/`, `observability/`, `protocol/schemas/`,
  `protocol/rust/`, `config/schemas/`, `test/`, `cmake/`, and
  `docs/architecture/` (V01-E01-F01).
- `docs/architecture/ownership.md` documenting per-package owners, allowed
  dependencies, and forbidden upward dependencies (V01-E01-F01-T02).
- Test tree layout for tiers T1 through T5 plus shared mocks and model
  fixtures, documented in `test/README.md` (V01-E01-F01-T03).
- Root CMake build with `tp_runtime` (alias `tp::runtime`) static library,
  `tp_serving_worker` binary (output `tensorplate-serving`), and CTest
  wiring with T1 label (V01-E01-F02-T01).
- vcpkg manifest (`vcpkg.json`) declaring the GoogleTest dependency and
  reserving feature flags for adapter SDKs; toolchain stubs
  `cmake/toolchains/x86_64-linux-gnu.cmake` and
  `cmake/toolchains/aarch64-jetson.cmake` (V01-E01-F02-T02).
- `cmake/features/warnings.cmake` and `cmake/features/sanitizers.cmake`
  helpers; `TP_ENABLE_SANITIZERS` and `TP_WARNINGS_AS_ERRORS` options;
  `tp_test_unit` GoogleTest target with smoke coverage (V01-E01-F02-T03).
- `.clang-format` and `.clang-tidy` baseline configurations.
- Cargo workspace at the repository root with members `tensorplate-agent`,
  `tensorplate-cli`, `tensorplate-observability`, and `tensorplate-protocol`,
  pinned `rust-toolchain.toml` (1.78.0), `rustfmt.toml` baseline, and
  workspace-wide rustc and clippy lints (V01-E01-F03-T01).
- Crate entrypoints with version banners and a baseline test in
  `tensorplate-protocol` proving workspace builds end to end without
  device hardware (V01-E01-F03-T02).
- Documented Rust quality commands (`cargo build`, `cargo test`,
  `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`) in `CONTRIBUTING.md` (V01-E01-F03-T03).
- `.github/workflows/cpp.yml` running the C++ build, T1 unit tests in a
  release and ASAN/UBSAN matrix, `clang-format --dry-run -Werror`, and
  `clang-tidy` against the exported compile commands (V01-E01-F04-T01).
- `.github/workflows/rust.yml` running `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo test --workspace` against the pinned toolchain (V01-E01-F04-T02).
- vcpkg and Cargo dependency caching, per-workflow concurrency, and a
  documented PR / nightly / release-branch status policy in
  `CONTRIBUTING.md` (V01-E01-F04-T03).
- `.devcontainer/Dockerfile` and `.devcontainer/devcontainer.json`
  delivering a reproducible Ubuntu 22.04 dev image with CMake, Ninja,
  Clang 15, clang-format/-tidy, vcpkg, and the pinned Rust toolchain;
  named volumes mount the vcpkg and cargo caches across rebuilds
  (V01-E01-F05-T01).
- `docs/contributing/jetson-cross-compile.md` documenting the supported
  cross-compile path: `cmake/toolchains/aarch64-jetson.cmake`,
  TP_JETSON_SYSROOT/CC/CXX inputs, and JetPack/TensorRT/CUDA system
  ownership (V01-E01-F05-T02).
- `docs/contributing/local-validation.md` enumerating the canonical
  CMake/CTest/clang-format/clang-tidy and Cargo commands that mirror CI
  (V01-E01-F05-T03).
- `include/tensorplate/version.hpp` (generated from `.hpp.in` by CMake's
  `configure_file`) exposing four independent version surfaces:
  `kRuntimeVersion`, `kProtocolVersion`, `kBundleFormatVersion`, and the
  per-component MAJOR/MINOR/PATCH constants (V01-E01-F06-T01).
- `tensorplate_protocol::PROTOCOL_VERSION_*` and
  `BUNDLE_FORMAT_VERSION_*` Rust constants mirroring the C++ surface
  (V01-E01-F06-T01).
- T1 unit tests (C++ `version_test.cpp`, Rust `tests` module in
  `protocol/rust`) verifying that composed version strings agree with
  their components on each side (V01-E01-F06-T01).
- `docs/architecture/versioning.md` documenting the runtime / protocol /
  schema / bundle-format surfaces, bump rules, and the planned
  compatibility-validation path (V01-E01-F06-T02).
- `CONTRIBUTING.md` "Release and Changelog Policy" section listing the
  changes that require a `CHANGELOG.md` entry plus a version bump
  (V01-E01-F06-T03).
- `backends/python_pytorch/` package skeleton for the out-of-process
  PyTorch backend per the V01-E01 scope expansion: PEP 621
  `pyproject.toml`, namespace package
  `tensorplate_pytorch_backend` with mirrored protocol/bundle-format
  version constants, `py.typed` marker, ruff/ruff-format/mypy/pytest
  configuration, and a smoke test suite (V01-E01-F01).
- `.github/workflows/python.yml` running `ruff check`,
  `ruff format --check`, `mypy src tests`, and `pytest -q` against
  Python 3.10 and 3.12 on Ubuntu 22.04 with pip caching.
- `docs/architecture/ownership.md` updated with the new package row,
  out-of-process IPC dependency arrow, and forbidden-dependency rule
  preventing the Python backend from linking against any C++ runtime
  module.
- `include/tensorplate/core/error.hpp` defining the `tensorplate::Error`
  value object and the stable `Error::Code` taxonomy
  (`ConfigInvalid`, `LoadFailed`, `NotReady`, `ShapeMismatch`,
  `Unsupported`, `OOMError`, `Timeout`, `InferenceFailed`, `Internal`)
  with snake_case `to_string` / `error_code_from_string` helpers
  (V01-E02-F01-T01).
- `include/tensorplate/core/result.hpp` providing
  `tensorplate::Result<T>` (and `Result<void>`) with std::expected-shaped
  semantics and a `tp` namespace alias for the planning-doc API surface
  (V01-E02-F01-T01).
- `protocol/schemas/error.json` (JSON Schema Draft 7) and Rust mirror
  `tensorplate_protocol::ProtocolError` / `ErrorCode`, plus
  `decode_with_version_check` and `DecodeError` enforcing typed
  rejection of unknown `schema_version` values (V01-E02-F01-T02).
- T1 unit tests for `Error`, `Result<T>`, the protocol round-trip, and
  unknown-schema-version rejection (V01-E02-F01-T03).
- `include/tensorplate/core/model_spec.hpp` defining the
  `tensorplate::ModelSpec` value object with `ModelClass`
  (`vision`, `speech`, `language`, `vla`, `embedding`, `custom`) and
  `PrecisionHint` (`auto`, `fp32`, `fp16`, `bfloat16`, `int8`, `int4`)
  taxonomies and a validating `create()` factory returning
  `Result<ModelSpec>` (V01-E02-F02-T01).
- `protocol/schemas/model_spec.json` and the Rust mirror
  `tensorplate_protocol::ModelSpec` with serde round-trip and
  `decode_with_version_check` support (V01-E02-F02-T02).
- T1 unit tests for `ModelSpec` validation (empty model_id,
  artifact_path, backend_hint, present-but-empty profile_id), enum
  string round-trip, equality, and Rust round-trip
  (V01-E02-F02-T03).
- `include/tensorplate/buffer/buffer_ref.hpp` defining the
  `tensorplate::BufferRef` opaque buffer-handle value object with the
  `BufferOwnership` (`Owned` / `Borrowed` / `Released`) state machine,
  documented copy/move contract, `kNullId` released sentinel, and
  `mark_released()` idempotent tombstone; the underlying allocator
  lands in V01-E03 (V01-E02-F05-T01).
- Documented copy/move/release semantics in the public header and
  through T1 unit tests, including the convention that holders needing
  unique-ptr-style invalidation must call `mark_released()` on the
  source explicitly (V01-E02-F05-T02).
- `protocol/schemas/buffer_ref.json` and Rust mirror
  `tensorplate_protocol::BufferRef` for protocol/test fixtures that
  compare buffer identity without transferring memory
  (V01-E02-F05-T03).
- `include/tensorplate/buffer/tensor_view.hpp` defining
  `tensorplate::TensorView` with `DType`
  (`float32`, `float16`, `bfloat16`, `int64`, `int32`, `int16`,
  `int8`, `uint8`, `bool`) and `Layout` (`row_major`, `col_major`)
  enums, locked dtype byte-width table, and a validating `create()`
  factory that auto-computes `byte_size` and rejects rank-0 / non-
  positive dims / size underflow / size-overflow with typed errors
  (V01-E02-F06-T01, T02).
- `protocol/schemas/tensor_view.json` and Rust mirror
  `tensorplate_protocol::TensorView` with serde round-trip,
  defaults compression for layout / byte_offset / byte_size, and
  matching `TensorViewError` taxonomy (V01-E02-F06-T03).
- T1 unit tests for dtype/layout name round-trip, locked byte-width
  table, valid construction, automatic byte_size, padding-allowed
  explicit byte_size, underflow rejection, empty/zero/negative shape
  rejection, SmolVLA-style chunk shape `[chunk_size, action_dim]`,
  byte_offset preservation, and equality.
- `include/tensorplate/core/infer_request.hpp` defining the
  `tensorplate::InferRequest` value object with a vector of named
  inputs, request metadata, and an optional monotonic
  `std::chrono::steady_clock::time_point` deadline. `NamedInput`
  binds a stable name to a `BufferRef` and a `TensorView`; the
  request supports single-input vision (n=1) and SmolVLA-class
  multi-input (image_front, image_wrist, state, instruction)
  through the same type. Validating `create()` and
  `create_with_relative_deadline()` factories return
  `Result<InferRequest>` (V01-E02-F03-T01).
- `tensorplate::RequestMetadata` carries explicit
  `correlation_id`, `action_chunk_id`, `action_chunk_sequence`, and
  `stale_after_sequence` fields preserving the LeRobot
  PolicyServer async-inference contract, plus a free-form
  string/string `extra` map for caller metadata
  (V01-E02-F03-T02).
- `protocol/schemas/infer_request.json` (JSON Schema Draft 7) with
  `$ref` references to `buffer_ref.json` and `tensor_view.json`,
  optional `metadata`, and a relative `deadline_ms` field that
  receivers convert to a monotonic absolute deadline by sampling
  their own steady clock.
- Rust mirror `tensorplate_protocol::InferRequest` with
  `RequestMetadata`, `NamedInput`, `InferRequestError`, and the
  same validation rules as the C++ factory.
- T1 unit tests for single-input and SmolVLA-style multi-input
  construction, LeRobot async metadata preservation, validation
  rejection (empty request_id / endpoint / inputs / input name and
  duplicate input names), no-deadline / future-deadline / past-
  deadline / clamped-to-zero behavior, the relative-deadline
  factory's negative-value rejection and monotonic conversion,
  equality, and the requirement that fixtures build without a
  buffer-pool or adapter (V01-E02-F03-T03).
- `include/tensorplate/core/infer_result.hpp` defining
  `tensorplate::InferResult` as a discriminated value carrying
  either a non-empty vector of `NamedOutput`s or a typed
  `tensorplate::Error`, plus optional `InferenceTiming`
  breakdowns (queue / execution / total latency in nanoseconds)
  populated by the V01-E04 ExecutionSession NVI wrapper. Chunk-
  shaped VLA action output is one pattern of `outputs` and does
  not require a VLA-specific result type. Success construction
  validates output naming the same way `InferRequest` validates
  inputs (V01-E02-F04-T01).
- `protocol/schemas/infer_result.json` (JSON Schema Draft 7) with
  $ref-composed `error.json` / `buffer_ref.json` / `tensor_view.json`
  fragments and an `allOf` constraint that enforces the
  status / outputs / error invariant on the wire. Rust mirror
  `tensorplate_protocol::InferResult` with `InferResultStatus`,
  `NamedOutput`, `InferenceTiming`, and `InferResultError`
  taxonomy (V01-E02-F04-T02).
- T1 unit tests covering success construction with chunk-shaped
  output, multi-named-output ordering, validation rejection
  (empty / duplicate / empty-name outputs), failure construction
  preserving the typed error code, ingress-time empty-request_id
  failures, safe-default accessors on wrong-state lookups,
  optional timing field preservation, equality, and explicit
  compatibility of every `Error::Code` with the result taxonomy
  (V01-E02-F04-T03).
- `docs/architecture/protocol.md` documenting the v0.1.0 protocol
  format selection (JSON Schema Draft 7), versioning policy
  (`schema_version` const-fixed, mandatory
  `decode_with_version_check`), hand-written-binding strategy, and
  the round-trip contract between Rust serde mirrors and the
  shared fixtures (V01-E02-F07-T01).
- `protocol/schemas/desired_state.json` (V01-E02-F07-T02),
  `protocol/schemas/worker_status.json` carrying the V01-E10 ROS 2
  health-publisher fields (`agent_state`, `serving_state`,
  `observability_state`, `active_deployment`, `backend`,
  `missed_heartbeat_count`, `missed_deadline_rate`, `queue_depth`,
  `last_error_code`) (V01-E02-F07-T03),
  `protocol/schemas/health_event.json` with the V01-E12-reserved
  control-loop telemetry block (jitter p50/p95/p99/max, mean
  frequency, frequency stddev, frequency-error percent, rolling
  window) and monotonic-only timestamps (V01-E02-F07-T04),
  `protocol/schemas/deploy_transaction.json` covering the
  received -> verified -> staged -> capacity_checked -> prepared ->
  warmed -> promoted -> active state machine plus terminal
  failed / rolled_back states with typed-and-recoverable failure
  metadata (V01-E02-F07-T05), and
  `protocol/schemas/python_pytorch_ipc.json` defining the JSON
  header for the Unix domain socket IPC (LoadModel / Prime /
  Infer / InferAsync / Cancel / Unload / HealthCheck plus
  ready / error / metric events) with raw tensor bytes carried
  after the header rather than JSON-encoded (V01-E02-F07-T06).
- Rust mirrors `tensorplate_protocol::DesiredState`,
  `WorkerStatus`, `HealthEvent`, `ControlLoopMetrics`,
  `DeployTransaction` / `DeployFailure` / `DeployState`, and
  `IpcMessage` with serde round-trip, validating constructors
  (e.g. `DesiredState::new` rejects malformed bundle digests,
  `WorkerStatus::new` rejects out-of-range
  `missed_deadline_rate`, `DeployTransaction::new` enforces the
  failure-metadata invariant, `IpcMessage::validate` enforces the
  JSON Schema `allOf` rules), and `decode_with_version_check`
  semantic validation plus acceptance/rejection tests.
- `protocol/rust/tests/round_trip.rs` integration suite with nine
  canonical JSON fixtures (vision and SmolVLA desired-state,
  ready / degraded worker-status, missed-deadline health event
  with full control-loop metrics, active and failed deploy
  transactions, sidecar load-model header, and a
  schema-version-rejection negative fixture) covering the Rust side
  of the fixture contract. C++ / Python binding round trips remain
  deferred until those bindings land in V01-E07 / V01-E05.
- `protocol/schemas/README.md` updated to reflect the realized
  schema set and conventions; per-schema ownership table.

### Changed

- `README.md` repository layout block now reflects the realized v0.1.0
  package skeleton and links to the ownership document.
- `tensorplate_protocol::decode_with_version_check` now rejects
  current-version payloads that deserialize structurally but violate
  constructor-level invariants, returning `DecodeError::InvalidPayload`
  mapped to `ErrorCode::ConfigInvalid`.
- `InferRequest` construction now rejects released / missing input
  buffers, present-but-empty metadata IDs, and already-expired
  deadlines.

### Deprecated

### Removed

### Fixed

- Serving HTTP routes no longer serialize behind long-running handlers
  ([#21](https://github.com/tensorplate/tensorplate/issues/21)). The route
  dispatcher held the route-table mutex across handler execution, so a
  slow `POST /infer` blocked every other route — `/health`, `/metrics`,
  and the async-policy `/policy/result` and `/policy/cancel` routes —
  turning a lookup lock into a global request-execution lock.
    - `HttpServer::Impl::dispatch()` now copies the matching
      `RouteHandler` out from under `routes_mutex` and invokes it after
      releasing the lock. The handler is copied (not referenced) because
      `add_route`/`add_prefix_route` can append to — and reallocate —
      the route vectors concurrently. Route matching, the 405-vs-404
      decision, and the 500-on-exception boundary are unchanged.
    - This relies on the existing `RouteHandler` contract that handlers
      are safe to call concurrently; no handler was depending on the
      mutex for mutual exclusion.

- Runtime socket write paths no longer depend on the embedding binary
  ignoring `SIGPIPE` process-wide
  ([#19](https://github.com/tensorplate/tensorplate/issues/19)). A peer that closed the
  connection before or during a write could raise `SIGPIPE` and
  terminate the host process before the code observed `EPIPE` and
  returned a typed error.
    - `tensorplate::http::HttpServer` and
      `tensorplate::ipc::UnixSocket` now suppress `SIGPIPE` locally:
      `MSG_NOSIGNAL` is passed on every `send()` where the platform
      provides it (Linux) and `SO_NOSIGPIPE` is set on each created or
      accepted socket where it exists (macOS/BSD). The shared policy
      lives in `runtime/src/net/socket_signal.hpp`.
    - The write-side poll waits now treat `POLLHUP`/`POLLERR` as a
      typed `LoadFailed` instead of reporting the descriptor ready, so a
      peer that hangs up mid-write yields an error rather than a busy
      retry loop. Read-side draining is unchanged.
    - `serving_worker` keeps its process-wide `SIGPIPE` ignore as
      defense-in-depth; it is no longer required for `tp_runtime`
      correctness.

### Security
