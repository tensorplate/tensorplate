# Changelog

All notable changes to TensorPlate will be documented in this file.

This project follows the spirit of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic versioning once public releases begin.

## [Unreleased]

### Added

- `TP_ENABLE_TSAN` builds the C++ runtime, the serving worker and the
  tests with ThreadSanitizer. ThreadSanitizer cannot share a build with
  AddressSanitizer, so configure refuses `TP_ENABLE_TSAN` together with
  `TP_ENABLE_SANITIZERS`, and the `cmake.sanitizer_options` T1 test holds
  that refusal and checks each option alone is accepted. The C++ workflow
  gains a `tsan` leg that runs the T1, T2 and T3 labels; it is not yet a
  required check. `test/README.md` records what is not instrumented.
  (V030-E04-F03-T02)

### Fixed

- `HttpServer::stop()` closed the listening socket while the accept
  thread could still be polling it, a data race ThreadSanitizer reports
  in 25 of the 47 T2 tests. The socket is now closed only after that
  thread exits, so its descriptor cannot be reused under a live poll, and
  the thread accepts no connection once `stop()` has begun.
  (V030-E04-F03-T02)
### Changed

- Every version surface moves to `0.3.1`, the first release of the 0.3
  line: `packaging/VERSION`, the CMake project version, the Cargo
  workspace and path-dependency versions with `Cargo.lock`, the vcpkg
  `version-string`, the Python SDK version, the installer's default
  release and a `0.3.1-1` head stanza in `packaging/debian/changelog`.
  `develop` carries the next first-release version directly, as it has
  since the `0.2.1` bump, with no `-dev` suffix. The `python_pytorch`
  backend descriptor now admits the 0.3 line:
  `tensorplate_runtime_range.max_exclusive` rises from `0.3.0` to
  `0.4.0`, and `test/packaging/verify_descriptor.sh` targets release
  line `0.3`, which it requires the declared range to admit. That check
  also refuses a target line behind `packaging/VERSION`, which would
  otherwise pass unnoticed once the upper bound has moved, and it now
  runs each of its refusals against fixed inputs on every invocation so
  a broken check fails the script. This is a runtime-version bump only:
  the protocol and schema versions stay `0.1`, the bundle format version
  stays `0.1`, no bundle compatibility floor moves, and no dated
  `[0.3.1]` section or release notes file is opened here.
  (V030-E01-F01-T04)

- The agent no longer falls back to `state.json.bak` when `state.json` is
  refused for an unsupported state version (a newer state file supersedes an
  older backup, so the agent exits with `CorruptState` instead of starting
  on the stale backup) or cannot be read at all (an I/O error now stops the
  agent). A damaged, empty or missing `state.json` still falls back as
  before. On Linux, both directory syncs of a `0.2` state write must now
  succeed. A state write that fails at or after the rename that commits it
  (the one that makes it what the next start reads) returns the new
  `StateIndeterminate` error, and the store then refuses every later write
  until the agent restarts, while status reports the agent `failed` with a
  `last_error` saying so; a write that fails before that rename leaves the
  state and the store as they were. (V030-E03-F01-T03)
### Added

- Three error codes are appended to the shared error taxonomy:
  `cancelled` (the caller asked for the operation to stop),
  `unavailable` (a backend process or worker the operation needs is
  unavailable, was reset or is shutting down) and `resource_exhausted`
  (a bounded quota, credit or capacity limit was reached). They are C++
  `Error::Code` values 9, 10 and 11; no existing value moves. The Rust
  `ErrorCode`, the Python SDK's `ErrorCode`, the Python sidecar's `ERR_*`
  constants, `error.json` and all 17 schema copies of the code enum carry
  them. The router answers them with HTTP 499 (Client Closed Request),
  503 and 429, and serving metrics count `resource_exhausted` as an
  overload rejection and the other two as failed requests. Nothing emits
  them yet: the sidecar still reports a cancelled request as `timeout`.
  A serving worker that predates them reports an unknown sidecar code as
  `internal`, and an SDK that predates them reports an unknown code as
  `ErrorCode.INTERNAL`. (V030-E04-F01-T01)

- Five failure reasons describe how a streaming session ends, with a new
  `session` failure category: `input_credit_exceeded` and
  `slow_consumer` (category `session`, code `resource_exhausted`, not
  retryable), `backend_reset` (category `sidecar`), and
  `deployment_retired` and `worker_shutdown` (category `supervision`), all
  three with code `unavailable` and retryable. Observability derives no
  reason from the three new codes alone; a `serving_failed` state change
  whose last error code has no reason is reported as `internal`, as one
  with no code already was. (V030-E04-F01-T01)

- The shared enums are checked across languages. A Rust test holds every
  schema copy of the error-code enum, the `failure_reason.json` enums and
  the reason table in `docs/observability/failure-reasons.md` to the Rust
  taxonomy; a C++ test holds the `Error::Code` names to `error.json`; each
  Python package holds its constants to `error.json`; and the C++ name
  mapping, the router's status mapping and the metrics mapping are
  switches over every code, so under the CI warning flags a new code fails
  the build until it is named and mapped. `tensorplate infer` now parses
  a typed failure's code through the protocol enum instead of its own
  copy of the list. `protocol.md` documents the narrow exception that lets these enums
  grow under protocol `0.1`. (V030-E04-F01-T01)

- The agent's durable state file gains state version `0.2`. It records the
  resident set (the deployments kept loaded together, each member at a
  deployment generation with its bundle, descriptor and configuration
  digests, session quota, admission mode, serving or quarantined state and
  a retained previous generation, plus the committed endpoint map) and a
  `next_generation` counter that is never removed or lowered, so the agent
  never hands out the same generation twice from one state file. The state
  file now has its own version track:
  `protocol/schemas/agent_state.json` accepts state versions `0.1` and
  `0.2`, decoded by the new `tensorplate_protocol::decode_agent_state`,
  while `PROTOCOL_VERSION` and `SCHEMA_VERSION` stay `0.1` for every other
  payload. The agent stamps the oldest state version whose readers decode
  the file without loss: `0.2` once it has allocated a generation, `0.1`
  otherwise. No agent code path allocates a generation yet, so deployed
  agents keep writing the same `0.1` files as before. A `0.2` state is written
  `state.json.bak` first and `state.json` last, so an agent through 0.2.x
  never falls back to a `0.1` backup beside a `0.2` primary; it refuses the
  `0.2` file with its existing `CorruptState` error and exit status 3.
  Every write is refused before anything reaches disk unless it decodes back
  unchanged, keeps the generation counter, the resident set's identity and
  its revision moving forward, and adds no generation to the set below the
  newest one it already names. New `ResidentSet`, `ResidentMember`,
  `RetainedGeneration`, `EndpointEntry`, `AdmissionMode`, `MemberState` and
  `MemberQuota` types in `tensorplate-protocol`, with fixtures
  `agent_state_0_1_legacy.json` (recorded from the 0.2.1 state writer),
  `agent_state_0_2_restore_step.json` and
  `agent_state_0_2_two_member_set.json`. The release driver's schema version
  check admits a list of versions only on documents with their own version
  track, and only at the schema's root. (V030-E03-F01-T03)
- `tools/validation/check-public-hygiene.sh` scans what a push or a pull
  request publishes for values a public repository must never carry, and
  a second job in `.github/workflows/evidence-publication.yml` runs it on
  every pull request, and again when the title or body is edited. It reads
  the change as committed. That covers every changed file as each commit
  in the range wrote it, merges included, so a value one commit adds and
  a later one removes is still found. It also covers each changed file's
  name, every commit message, author and committer, the branch checked
  out, and the pull request's title, body and branch name. The job reads
  those three from the event payload file: never from an expression in a
  shell line, where they would be code, and never through a step's
  environment, which the log prints. Every file under
  `docs/validation/evidence/` and `test/platform/` also goes through
  `check-evidence-publication.sh`, unchanged, name and content.
  Everything is checked against a narrow source policy:
  - private keys, a kubeconfig's embedded key, and documented token
    prefixes (GitHub, Google, AWS, PyPI, Anthropic, Hugging Face, NVIDIA
    NGC, and HTTP Bearer or Basic credentials); a keyed credential is
    found with its value on a later line than its key too, as
    pretty-printed JSON and YAML block scalars write it, and past a YAML
    anchor, tag or comment
  - Google service accounts, and `projects/<number>` paths and keyed
    project numbers of six or more digits
  - home directories in POSIX, Windows, encoded and JSON-escaped forms,
    other than the synthetic operator's and the GitHub runner's
  - device UUIDs outside the all-zero namespace
  - IP addresses outside the loopback, unspecified, "this network" and
    documentation ranges, the metadata address and macOS's `fe80::1`,
    judged by value in any spelling: an IPv4-mapped address as its IPv4
    address, any other address with a dotted quad as IPv6
  - references to private planning material, as shapes only

  Lines are scanned as written, with escapes and terminal control
  sequences blanked, and with escapes decoded. A binary file is a finding
  whose printable text is still checked for the long shapes, and a
  submodule is a finding. The evidence scanner's host, journal, UUID,
  serial, e-mail, MAC, machine-id, cloud-project and planning classes
  stay out of the source policy, because negative tests, synthetic
  identities and the scanners' own patterns carry them. Five such files
  the evidence scanner rejects are the policy's positive fixtures. Like
  the evidence scanner, the new scanner never prints a matched value,
  masks a path either tier flags, and exits 0, 1 or 2. `--local` adds the
  machine's user name, host name and gcloud project as literals for the
  pre-push run, and is no verdict if gcloud does not answer.
  `--literals FILE` adds the operator's private literal file.
  A path that is not plain and relative, such as a tree entry named `..`,
  is no verdict. `tools/validation/public-hygiene-allowlist.txt` is the
  only override. Each entry is an exact path, a class, a count and a
  reason, and accepts up to that many findings of that class in that
  file, counting every occurrence. The count bounds how many values are
  accepted, not which ones. An entry never covers commit or pull request
  text, a credential, a finding only the evidence scanner makes, or any
  file under an evidence path, in any letter case: evidence is
  sanitized, not excepted, and the evidence scanner reports one finding
  per line, class and length, so a value only it decodes could not be
  counted against an entry. Values seen in different scan variants of a
  line count separately. `--tree` fails when a count is wrong. Its 12
  entries are the baseline's:
  - the closed legacy set of transcribed accelerator UUIDs in
    `platform/tests/accelerator_fixtures.rs`
  - private-range and public addresses in tests
  - synthetic home directories in harness verifiers
  - the README banner image

  The six transcribed accelerator fixtures whose device UUIDs predated
  the all-zero namespace now use it, so no evidence file needs an
  allowlist entry. Two test comments that cited private ledger labels are
  reworded.
  `test/validation/public_hygiene_test.sh` drives the scanner in
  throwaway repositories. Every alternative of every shape is generated
  at run time, from one context fixture per shape under
  `test/validation/fixtures/public_hygiene/`, so no committed file
  carries one. Each fixture's context must pass with a synthetic word in
  the value's place. The test also asserts that the repository tree
  passes with every count exact. Rule 10 of
  `docs/validation/fixture-and-evidence-rules.md`, `CONTRIBUTING.md` and
  `docs/contributing/local-validation.md` make the scan the step before
  every push. (V030-E06-F02-T01)
- The backend descriptor schema (`protocol/schemas/backend_descriptor.json`)
  gains an optional `runner_profiles` list. Each entry names an installed
  runner profile, the absolute interpreter and environment root its sidecar
  runs under, optional library directories for the sidecar's
  shared-library search path, the packages that install it, and the compute
  types it can load, from a closed list with no `auto`. The Rust reader
  also refuses what the schema cannot state: a repeated profile id, a path
  with a `.` or `..` segment, an interpreter or library directory outside
  the environment root, and a blank package name. A platform row's `backend_packages` entry
  gains optional per-runner-profile package lists, with the same schema and
  decoder agreement. Both changes are additive inside schema 0.1, and no
  version constant moves. No shipped descriptor or committed platform row
  declares a runner profile yet, and nothing reads either list yet. New
  fixture tests require the schema and the Rust decoder to accept the
  committed descriptors and to refuse every malformed variant the schema
  can express, and pin the schema's acceptance of each case only the
  decoder refuses. Checking the shipped
  `python_pytorch` descriptor this way showed that its `$schema` key had
  never been allowed by its own schema, so the schema now declares it.
  (V030-E01-F01-T04)

## [0.2.1] - 2026-09-23

### Added

- The two Ubuntu cloud rows, `ubuntu2404-x86-l4-g2s8` and
  `ubuntu2404-x86-h100-80g-a3hg1`, carry recorded lifecycle evidence for
  0.2.1, and their provenance is now `recorded`.
  `docs/validation/evidence/v0.2.1/` holds a passing
  `tools/validation/ubuntu-l4-cloud-lifecycle.sh` run for each, from the
  `v0.2.1-rc.3` tag commit on disposable GCP instances: an NVIDIA L4 on
  `g2-standard-8` running the stock Ubuntu 24.04 image, and a single
  non-partitioned H100 80GB HBM3 on `a3-highgpu-1g` running the Deep
  Learning VM image, taken as Spot because no on-demand capacity for that
  shape existed. Both report `outcome: pass` with all eight canonical
  stages present, passing, and carrying no skip reason. Both name
  `subject.tested_version` `0.2.1` and the same
  `subject.artifact_digest`, the SHA-256 of `v0.2.1-rc.3`'s signed
  `SHA256SUMS`, which each `artifact-digest.txt` names by that released
  file name. Both ran with `--baseline-assets-dir` against the published
  `v0.2.1-rc.2` package set, so upgrade and rollback are real: rc.2 to
  rc.3 and back to rc.2. That is the only amd64 upgrade path this release
  can record, because 0.1.x published no amd64 runtime packages. With
  these two, `check-evidence-bundles.sh --version 0.2.1` reports all four
  Production rows complete, and the release notes' validation status says
  so.

  Each bundle is the report, the eight stage logs it cites, and the
  digest. Fourteen of the twenty files are byte-identical to the
  recording. Two identifier classes are replaced with the evidence
  README's synthetic forms, both in `install.log`, `upgrade.log` and
  `rollback.log`: the operator's home directory, in each run, and a
  cloud project in a resource path, in the H100 run only. That project id
  belongs to a vendor's public package registry that the image's apt
  sources name, not to this run, but a reader cannot tell those apart. Ubuntu's regional
  apt mirror host stays as recorded: it names the region an image booted
  in, never an instance. The scanner read `needrestart`'s systemd template
  unit names as email addresses; it now exempts a token only where
  systemd's own command line puts one: on a line that is that command,
  after one of the verbs the scanner knows, in argument position. Stock
  output needs no editing, while an address elsewhere on that line,
  prose that merely mentions the tool, and a lowercase word standing in
  for a verb all remain findings. The unit type cannot carry the
  exemption by itself, because `target` is a delegated top-level domain.
  `evidence_publication_test.sh` covers each of those, and every rule
  the exemption rests on fails the suite when removed. The raw runs are retained privately and are not in this
  repository.

  `docs/validation/cloud-row-runbooks.md` said the current harness had
  not been run on hardware, so the crash-loop stage and the stronger
  journal and post-restart checks were still pending, and it left the
  Deep Learning VM image an open question about its interpreter. Both
  paragraphs now describe what ran, and the evidence README's apt clause
  now states one rule for the whole source list rather than a scope that
  excluded three of the sources these logs keep.

  Two test fixtures built a Planned row out of the committed L4 row and
  inherited its provenance, so both broke when that row became
  `recorded` — a Planned row must be `spec_authored`. Each now sets
  provenance explicitly, alongside the evidence and model-class fields it
  already normalized. The ignored tag-gate test
  `production_claims_rest_on_recorded_evidence` passes now that every
  Production row is recorded; its doc comment and its `#[ignore]` reason
  both said it was expected to fail, and both now say why it stays
  ignored anyway.

- The MacBook Pro M1 Pro row, `macos26-m1pro-16gb`, carries recorded
  lifecycle evidence for 0.2.1, and its provenance is now `recorded`.
  `docs/validation/evidence/v0.2.1/macos26-m1pro-16gb/` holds a passing
  `tools/validation/macos-homebrew-lifecycle.sh` run against the
  `v0.2.1-rc.2` formulae on the in-lab machine (macOS 26.6.2, Homebrew
  7.0.6): all 21 harness stages passed, and
  `tools/validation/lifecycle-report-from-stages.sh` converts them into the
  canonical eight under the mapping the runbook documents, all passing. Its
  `subject.artifact_digest` is the SHA-256 of the source archive all six
  formulae pin to, which is that build-from-source channel's only immutable
  artifact. The logs are the recorded ones with three identifier classes
  replaced by their synthetic forms: the operator's home directory, the
  per-user temporary-directory id in the sandboxed launchd paths, and the
  per-boot launchd session id in an inherited `SSH_AUTH_SOCK`; the
  evidence README's table now names the synthetic form for the last two,
  which are macOS directory ids no scanner pattern recognizes. Homebrew's
  untrusted-tap warning is removed from the two install logs rather than
  rewritten: it lists every other tap the machine carries and the formulae
  installed from them, which a run that touches none of them has no reason
  to publish. The README now says so, and the retained raw run keeps it.
  It is the second Production row `check-evidence-bundles.sh` reports
  complete; the two Ubuntu cloud rows still block a final tag.

  The macOS runbook named the 2026-08-17 record as current and described
  status-logs, offline-profile and offline-runtime as awaiting a hardware
  run; it now points at this bundle and keeps the older records as
  historical evidence. `physical-row-runbooks.md` said the same of those
  two macOS stages, and still called the Jetson harness's hardware run
  deferred after that row was recorded. Both now describe what ran.

- The Jetson Orin Nano row, `jetson-orin-nano-8gb-jp62`, carries recorded
  lifecycle evidence for 0.2.1, and its provenance is now `recorded`.
  `docs/validation/evidence/v0.2.1/jetson-orin-nano-8gb-jp62/` holds a
  passing `tools/validation/jetson-lifecycle.sh` run against the
  `v0.2.1-rc.2` artifacts on the in-lab device (JetPack 6.2, L4T r36.5.0):
  all eight stages, including the upgrade from `v0.1.5` and the rollback
  to it. Its `subject.artifact_digest` is the SHA-256 of that candidate's
  signed `SHA256SUMS`. The logs are the recorded ones with two identifiers
  replaced by their synthetic forms: the operator's home directory and the
  storage id in the device's apt source URL. It is the first Production
  row `check-evidence-bundles.sh` reports complete; the other three still
  block a final tag. The evidence README's scan step now covers a host or
  account name that is also ordinary evidence text, which a literal file
  cannot list bare: its identifying forms are listed instead, and every
  remaining occurrence is read before `git add`. Stage logs under
  `docs/validation/evidence/` are no longer caught by the repository's
  `*.log` ignore rule, which left a plain `git add` committing a bundle
  without the logs its report cites.

- The macOS Homebrew lifecycle harness has a status-logs stage, so the
  M1 Pro runbook maps all eight canonical lifecycle stages. After deploy
  smoke it requires `tensorplate status` to still report the deployment
  as ready, both launchd stderr logs to have gained output since the
  services started, and `tensorplate logs` to read the packaged structured
  event log and return an observability event from the current run. The
  deploy-smoke stage no longer runs `tensorplate logs --component agent`,
  which returned no entries because the agent writes no structured events.
- Lifecycle evidence is scanned for identifiers before it can be
  published. `tools/validation/check-evidence-publication.sh` fails
  closed on host names in journal, `hostnamectl` and `uname` output,
  machine and boot ids, home paths, device and other UUIDs, IP and MAC
  addresses, email addresses, cloud project paths, `.internal` and
  `.local` names, serials, credentials, planning identifiers, journal
  fields beyond a service's own, and symlinks, archives or other files
  that are not text. It reads raw lines, lines with terminal styling and
  backslash escapes taken out, and the strings and journalctl byte-array
  values of every JSON object or array in a file, including
  concatenated pretty-printed records and records quoted after other
  text. Repeated JSON members and decoded field/value associations are
  retained for inspection, and complete relative paths are checked before
  they can appear in diagnostics. Authorization headers and journal text
  metadata are checked too; four-part versions are exempt only in
  recognized package forms. Long physical log lines use bounded JSON
  decode work.
  Operators add their own host, account, project and instance names
  from a literal file kept outside the repository. Findings name a file,
  a line and a class but never the matched value. Exit 0 is publishable,
  1 is findings and 2 is no verdict. A new "evidence publication scan"
  workflow runs its tests and scans `docs/validation/evidence` on every
  pull request and on pushes to main, develop and release branches. The
  evidence README now lists the self-describing synthetic value for each
  identifier, and the runbooks require scanning sanitized evidence before
  its first commit.

- A lifecycle validation harness for the Ubuntu 24.04 x86_64 cloud rows,
  run by hand on a VM the operator starts themselves. It provisions no
  cloud resources. Five canonical stages are always exercised -- install,
  deploy-smoke, status-logs, restart and crash-loop. Upgrade and rollback
  run after them when a published, signed predecessor release is supplied
  with `--baseline-assets-dir`, and are skipped with a reason naming that
  option otherwise. Offline stays skipped with its reason recorded, deferred
  until cloud platform detection can work without GCE metadata access.
  Upgrade runs the candidate's installer over a baseline install that is
  serving a deployment, and requires the exact candidate package set,
  services restarted by the installer alone, an operator conffile edit
  kept, doctor green and the deployment re-warmed. Rollback follows the
  documented procedure: state set aside as `state.bak`, every TensorPlate
  package except `tensorplate-apt-source` removed, and the baseline
  installed fresh with the edit kept, the set-aside state not loaded, and
  a working deploy. The baseline is always installed with its signature
  verified, its digest is filed separately, and the report's artifact
  digest stays the candidate's. Preflight checks the baseline tag's public
  GitHub Release and matches the local checksum manifest to its published
  asset before any installation or removal. Drafts, unpublished tags and
  mismatched assets are refused even with `--allow-unsigned`.
  Crash-loop breaks the agent's config and requires systemd to retry and
  then give up on the agent, and the deployment to recover once the
  config is restored.
  Configuration restoration is also attempted on interruption; a failed
  restoration retains the backup and fails the run.
  Its deploy-smoke bundle selects a device-neutral fixture profile and
  executes no accelerator kernel.

- The Ubuntu x86_64 cloud rows now run the offline stage, so a run with
  `--baseline-assets-dir` exercises all eight canonical lifecycle stages.
  Both services, and each of `status`, `doctor`, a fresh deploy and an
  inference, run under a per-unit denial that allows only `127.0.0.1/32`
  and `::1/128` -- deliberately not systemd's `localhost` shorthand,
  which expands to `127.0.0.0/8` and would admit the systemd-resolved
  stub at `127.0.0.53` and the DNS namespace behind it. Each CLI call
  runs in its own denied transient unit. The denial is a runtime drop-in
  under `/run/systemd/system`, never `/etc`, and is removed on every exit
  path including `SIGINT`, `SIGTERM` and `SIGHUP`, with the removal read
  back from systemd rather than assumed. The cleanup is best-effort on
  every step: a removal that fails for one unit does not stop the other
  unit's, nor the reload, restart and readback, and a cleanup that still
  fails prints the drop-in paths and the command that removes them. A
  drop-in an earlier run left behind is refused in preflight, before
  anything is installed, as well as by the stage.
  Enforcement is established by probes run under the denial, each
  against a control run first with nothing denied: one in a denied
  transient unit, and one inside each service's own control group,
  because systemd attaches the filter to each unit on a best-effort basis
  and a filtered transient unit says nothing about a service whose own
  attach failed -- and one inside the transient unit of each CLI call, for
  the same reason: identical properties on every unit are configuration,
  not that unit's filter. The module's `run-denied` sends the datagrams
  from inside the call's unit, files them as
  `offline-cli-probe-<call>.json`, and execs the call in the same process
  only if they classify as enforced; otherwise it exits 71 and the call
  is never made. The harness's verifier fails a run in which only the
  doctor, deploy or infer unit's filter did not attach, and requires that
  call never to have run. Joining a service's control group takes root;
  the helper refuses any group that is not exactly that unit's, reads the
  move back, and drops to the operator's ids before it sends anything.
  Every operation in a control must have completed -- each datagram
  sent, the child process run to a clean exit, each TCP connect
  answered -- and a control that was refused, timed out, or whose child
  exited non-zero or never ran fails the stage by name: it sent nothing,
  so it cannot show that the later refusal was the denial's doing. The
  helper files a control only if it could be a baseline, so such a control
  fails the stage as it is taken, before either service is denied, and
  `run-denied` refuses one with exit status 1, naming the control rather
  than the unit, before it sends anything. The GCE metadata service must
  answer the controls outright, over TCP and as a datagram, since the
  stage's claim is that the denial is what made that service unreachable.
  A datagram under the denial must be refused with `EPERM`, which the
  kernel returns from `sendto()`. A TCP connect cannot be:
  `tcp_connect()` passes on only `ECONNREFUSED` from a transmit, so a
  connect whose SYN the filter dropped times out. It is accepted as
  silenced only where its control was answered, and filed apart from the
  refusals.
  The control also decides what each operation can prove: one the host
  could not perform with nothing denied cannot be refused by the denial
  either, so it is named in the result as an operation this host cannot
  send rather than reported as a denial that failed to bite -- except a
  loopback destination, which every host routes. That is the case for
  every global IPv6 destination on an IPv4-only VM, which is the Compute
  Engine default: the cgroup egress filter runs after the route lookup,
  so the answer is `ENETUNREACH` with and without the drop-in.
  Reading the properties back is not enough on its own -- `IPAddressDeny=`
  is silently inert where the BPF filter cannot be installed, and
  `systemctl show` answers for a dead or nonexistent unit with empty
  values, so every readback requires a loaded, active unit with an
  invocation id first. `systemctl show` also answers with the unit's
  *loaded* configuration, which counts a drop-in from `daemon-reload`
  onwards whether or not anything restarted under it, so each readback
  compares the invocation id against the one the unit carried before the
  policy changed -- on the way in and on the way out. systemd prints the
  prefix lists from a hash set whose order changes with each PID 1
  start, so the readback compares them as sets and files them in one
  canonical order.
  `offline-runtime.json` derives every verdict it states: the enforcement
  verdict by classifying every probe the document carries against its
  own control, each CLI verdict from the result file that check filed
  only after it passed, and the allow list from what systemd reported,
  which must be exactly the two host addresses. It refuses a restore that
  read back fewer removals than there were denied units, and a persistent
  drop-in found on either side of the stage.
  Doctor must still resolve the row with nothing failing, and the
  identity must come from the boot-bound machine-type record rather than
  from a live metadata answer: the agent's
  `source=recorded_gce_metadata record=not_applicable` line and doctor's
  `host_os` finding. Because that record is bound to the kernel boot,
  offline cold boot is not supported, and the runbook says so: after a
  reboot the agent must start once with metadata reachable before offline
  detection works. The stage runs before upgrade, whose clean baseline
  install deletes the record; install and upgrade stay online, and their
  doctor runs must show the machine type read live from GCE metadata.
  The mechanism lives in `tools/validation/linux_offline_runtime.py`,
  named for the mechanism rather than the row, with its own tests in
  `test/packaging/verify_linux_offline_runtime.py`. The drop-in, the
  policy readback, the probe and the classification carry no row in them;
  what is row-specific is supplied as options, so another systemd harness
  adopts the file unchanged rather than editing it. A row with no
  metadata service passes `--metadata-address none` and
  `--metadata-operation absent`, which then requires both metadata
  operations to be absent from every document rather than letting a
  missing one read as one that passed; a row whose agent and doctor say
  something else passes its own expected tokens, and the doctor check
  files the phrases it required rather than a machine-type source it did
  not establish. Every default is the Compute Engine row's.
  Every subcommand of the module runs behind one boundary, because what
  it prints to stderr lands in the stage log and in a failing report's
  `detail`: a failure it did not anticipate is reported as
  `error: <subcommand> failed unexpectedly: <exception type>` with exit
  status 70, never with a traceback or the exception's message, either of
  which quotes the checkout's path, and a file it cannot read is named by
  its base name and errno text. The deploy-smoke bundle check names an
  unreadable bundle file by its place in the bundle for the same reason,
  and the bundle is copied from inside itself, so `cp` names a file it
  cannot copy by a relative path, and a bundle it can no longer enter is
  refused without naming it. The harness's verifier runs a crashing probe
  from a copy of the harness under a `home/<name>` directory, as CI's own
  checkout is, and bundles from under one with a missing model, an
  unreadable manifest, an unreadable file only the copy reads, and a
  directory locked just before the copy, and requires every such run's
  evidence to pass the publication scanner with no traceback or path in
  it.

- A native lifecycle validation harness for the Jetson Orin Nano row,
  `tools/validation/jetson-lifecycle.sh`, which writes the canonical
  lifecycle report itself rather than through the clean-room step
  adapter and stage converter. That chain reported the weakest of the
  mapped steps that were present, so a stage whose decisive step never
  ran could read as a pass, and its status-logs mapping could never pass
  on a packaged install. Five stages are exercised -- install,
  deploy-smoke, status-logs, restart and crash-loop -- and upgrade,
  rollback and offline are skipped with their reasons recorded as
  follow-up work. The run installs a candidate downloaded by tag through
  the release's own installer with signature verification, binds the tag
  to the tested version and to the manifest's release tag, purges every
  TensorPlate package except the apt channel bootstrap, and checks each
  runtime package's installed version against its candidate package.
  Before anything is purged it refuses an operator session outside the
  `tensorplate` group and any assets, evidence or bundle directory that
  the run itself would delete. Package-inventory query errors fail the
  install stage before state is cleared rather than counting as an empty
  inventory. The harness pins CLI calls to the installed agent's local
  socket with a private temporary configuration and checks that inference
  uses the active deployment endpoint, so saved operator profiles cannot
  redirect validation to another appliance.
  Deploy-smoke is a TensorRT identity engine round trip with no Python
  backend installed; it makes no accuracy, throughput or compute claim.
  The Jetson runbook now uses this harness, and its prerequisites no
  longer claim the device carries no build toolchain. The clean-room
  harness is unchanged and remains the release clean-room smoke.

- The Jetson lifecycle harness has upgrade and rollback stages, run with
  `--baseline-tag` and `--baseline-assets-dir` against the last
  published arm64 runtime set and skipped with that reason when no
  baseline is given (V021-E05-F01-T02). The baseline is pinned to
  v0.1.5: the report names only the candidate, so nothing a gate reads
  could tell an earlier candidate of the same release from the path this
  row validates. Preflight also refuses a candidate that is not newer
  than it, a baseline whose runtime packages are not each strictly older
  than the candidate set's by `dpkg --compare-versions`, one whose
  `.deb` files `dpkg-deb` cannot read, one whose manifest names another
  tag, and one whose manifest records it as an unreleased local
  snapshot. The package comparison is what makes the path installable:
  the tag is release metadata, and an older tag over newer `.deb`
  versions would leave the upgrade stage's candidate install a downgrade
  that the `apt-get -y` inside `install.sh` refuses, on a device the run
  has already rebuilt twice. The versions compared are read from each
  `.deb`'s own control field with `dpkg-deb` rather than from the
  manifest, whose `version` the release driver parses out of the file
  name and never reads from the package. `upgrade-path.json` records
  them, so the evidence says why the path was admitted. The snapshot
  check reads fields the set's own manifest declares and is a snapshot
  filter rather than a proof of publication; binding a baseline to its
  public release the way `tools/validation/check-baseline-publication.py`
  does for the Ubuntu cloud rows would make this preflight depend on
  reaching GitHub from the device and is left as follow-up work. Both
  sets are always installed through their own installer with the
  signature verified, with no option to skip that. Both releases'
  `install.sh` read `TP_INSTALL_*` variables that switch verification
  off or point it elsewhere, so preflight refuses a run whose
  environment sets any of them; the upgrade's and the rollback's
  installs also run behind a prefix that drops any `TP_INSTALL_*`
  variable the sudo policy still passes, must print the installer's own
  "signature verified" line, filed per install, and refuse a set whose
  `SHA256SUMS` no longer hashes to the digest preflight recorded. The
  five existing stages keep their bodies, including the install stage's
  own installer call. Upgrade clears the candidate, installs the
  baseline, requires its services up, deploys and round-trips the
  identity engine on it, applies an operator conffile edit, then
  installs the candidate over the running baseline and requires the
  services back, the candidate's package versions, new service main
  pids, the operator's edited conffile, a green doctor resolving this
  row, and the baseline's deployment still serving with no deploy of the
  harness's own. Rollback follows the documented procedure: it refuses to
  replace an existing `state.bak`, stops both services, moves durable
  state aside, removes every installed `tensorplate*` package except the
  apt channel bootstrap -- `tensorplate-common` included, without which
  the older set would be a downgrade `apt-get -y` refuses -- and reads
  dpkg's own listing unfiltered before and after. Each of the four
  packages that ship a file under `/etc` must be in `config-files`
  state, so a package the listing leaves out or reports `not-installed`
  is caught as purged; every other package must be `not-installed` or
  `config-files`, so a half-configured one is caught as left behind; and
  the apt channel bootstrap must be as the removal found it, which on a
  device set up by `install.sh` alone means absent. It then installs the
  baseline fresh and requires its services up, the baseline versions,
  the operator's edit and the set-aside state to be intact, the older
  agent to report no active or previous deployment, and a fresh
  deployment to serve. The set-aside state is held to its contents, not
  to its pathname, and to the whole directory rather than to one name in
  it: with the services stopped and before the move, the harness lists
  `state/` and digests every file in it, then does the same to
  `state.bak/` after the baseline install and requires the two to match
  name for name and digest for digest, naming the first file that
  changed, went missing or was added. A device keeps more there than the
  agent's `state.json` -- the agent also refreshes `state.json.bak`, the
  copy it falls back to when the primary fails to decode, and the
  observability unit writes `observability-snapshot.json` beside them --
  so a removal or an install that emptied, truncated or rewrote any of
  them in place fails the stage by name, and a check on one pathname
  would have credited the rollback with preserving state it never read.
  The checks that follow deliberately do not load those files -- they
  exist to show the older agent did not -- so nothing else could catch
  it. Doctor on the baseline is recorded, not asserted.
  The harness drops `PYTHONOPTIMIZE`, which would otherwise turn its
  Python `assert` checks into passes. A run that ends while the device
  serves nothing -- in the upgrade from clearing the candidate to a
  completed baseline install, or in the rollback from stopping the
  services to a completed baseline install -- files
  `stranded-device.txt` beside the stage logs and prints it. The report
  reads dpkg when it is written rather than repeating a listing taken
  before an installer that can fail after installing every package,
  says the conffiles are kept only when the removal's listing showed
  it, and says whether the baseline installer ran, where durable state
  is, and how to recover, including how to return to the candidate when
  the rollback stopped before removing anything. Where it says the state
  was set aside, it also says what is behind that pathname, because it is
  written in exactly the window where a baseline install that failed may
  already have destroyed it and the stage's own check runs only after
  that install returns: whether the set-aside copy still matches the
  digests taken before the move, no longer matches them and should be
  treated as damaged, or could not be read back. That read is
  best-effort, so the report never fails on it. A terminal that has
  gone away does not stop the lifecycle report from being written. The
  report attests the candidate's `SHA256SUMS` digest; the baseline's is
  filed separately with the upgrade path. `docs/install/lifecycle.md` no
  longer says only the cloud harness runs the full rollback procedure.

- The Jetson lifecycle harness has an offline stage, so a run with a
  baseline set now exercises all eight canonical stages and the Jetson
  row can report `pass` rather than `incomplete`. It is built on
  `tools/validation/linux_offline_runtime.py`, the mechanism the Ubuntu
  cloud rows already use, so both rows run the same rule rather than two
  copies of it. Both services, and every TensorPlate CLI call the stage
  makes, run under a per-unit denial of all IP traffic but `127.0.0.1/32`
  and `::1/128` -- never systemd's `localhost` shorthand, which expands
  to `127.0.0.0/8` and so admits the `systemd-resolved` stub and the DNS
  namespace behind it. The denial is a runtime drop-in under
  `/run/systemd/system`; nothing is written under `/etc`, every exit path
  including INT, TERM and HUP removes it, and the removal is read back
  from systemd rather than assumed.
  Configuring a denial is not enforcing one -- `IPAddressDeny=` is
  silently inert wherever systemd cannot install its BPF filter, and
  `systemctl show` answers for a dead unit with empty values -- so the
  stage probes, and does so where systemd attaches the filter: in a
  denied transient unit, inside each service's own control group, and
  inside each CLI call's own transient unit before that call, which is
  made only if its own probe classified as enforced. Every probe is
  classified against a control taken the same way with nothing denied,
  and a control whose operations did not complete is refused as it is
  taken, before anything on the device changes. The readback compares
  each unit's invocation id from before the restart, so a policy that
  reached the loaded configuration but no running instance fails.
  This row has no metadata service, so the probes omit the metadata
  operations and every classification requires them to be absent from
  both documents rather than letting an operation that quietly vanished
  read as one that passed. There is no boot-bound machine-type record on
  a Jetson and the stage asserts nothing about one: the cloud row's
  identity claim is about a Compute Engine mechanism this device has none
  of, and restating it here would certify a fallback path that never
  runs. What this row requires instead is that the denial changed nothing
  about how it resolves -- doctor still reporting nothing failing,
  resolving `jetson-orin-nano-8gb-jp62` exactly, still naming the L4T
  release read from `/etc/nv_tegra_release` in `host_os` and never a
  machine type from GCE metadata in either spelling, and the agent's
  start-up identity line saying it established no machine type and
  recorded none, which is what it says online too.
  While denied, the appliance has to keep working: status answers over
  the agent socket and still reports the deployment the agent re-warmed,
  a fresh bundle deploys as `<deployment-id>-offline` and the agent then
  reports that deployment as the active one -- the deploy reply is the
  CLI's account of its own request, and only a status read afterwards is
  the agent's -- and the TensorRT identity engine returns its input
  unchanged. The stage files `offline-runtime.json`, which re-derives its
  enforcement verdict from the probes and controls it carries rather than
  restating one, and which carries every one of those CLI verdicts, so a
  stage that stopped making one of the checks cannot still file a
  certificate that claims it.
  Every `journalctl` capture the Jetson harness takes is now projected to
  the service's own fields where it is taken -- the message, its priority
  and identifier, the unit and invocation, the pid and the timestamp --
  which is exactly the set
  `tools/validation/check-evidence-publication.sh` admits. The host name,
  machine and boot ids, cursor and command line systemd attaches to every
  record never reach the evidence directory and no raw copy is kept, so
  the offline stage's `offline-agent-journal.txt` and the
  `agent-journal.txt`, `observability-journal.txt` and
  `crash-loop-journal.txt` that predate it are publishable as written
  rather than by hand. This is the Ubuntu cloud harness's
  `project_journal_records`, so both rows keep the same set.
  The stage runs between crash-loop and upgrade, and the position is
  load-bearing in both directions: it is about the candidate install the
  stages above exercise and the deployment they left serving, and
  upgrade's clearing step purges that install and deletes
  `/var/lib/tensorplate`. Preflight now also refuses a device without
  `systemd-run`, a harness copied out of the checkout without the offline
  module beside it, a denial drop-in an earlier run left behind (per
  unit, including a dangling symlink, and changing nothing), and a
  `--deployment-id` that is outside the allowed charset, is one of the
  reserved path segments `.` and `..`, or is long enough that any id the
  run derives from it would pass the CLI's 128-byte limit. All four are
  checked -- the id as given and the `-offline`, `-baseline` and
  `-rollback` forms -- because the two 9-byte suffixes would otherwise
  fail in the last two stages of a run, after the upgrade's clearing step
  has already purged the candidate.
  Every transient unit the stage starts runs as the operator, with the
  operator's groups, so what the denial is applied to is the call this
  operator makes online rather than one made as root.

- The release installer supports Ubuntu 24.04 on x86_64 as a runtime
  platform alongside JetPack 6.x / L4T 36.x on arm64. Each architecture
  is validated against its own platform: an x86_64 host is no longer
  asked for Jetson L4T metadata, and an arm64 host still is. Hardware
  validation stays advisory and now warns when an x86_64 host has no
  NVIDIA driver. The APT channel still serves `jammy` only, so x86_64
  hosts install from release assets.

- Lifecycle validation reports record which artifact a run installed, as
  `subject.artifact_digest`. Each harness writes an `artifact-digest.txt`
  beside its stage logs while the run is happening: the Jetson clean room
  hashes the release's verified `SHA256SUMS`, and the macOS lifecycle
  records the source archive its formulae are pinned to. Both report
  producers take the value from that evidence rather than from the
  operator, accept only bare lowercase sha256 hex, and omit the field
  when no harness recorded one.

- Preview rows cover A100 40GB, A100 80GB, and H100 hosts with one, two,
  four, or eight GPUs, A100 40GB with sixteen, plus L4 and RTX PRO 6000
  hosts with two, four, or eight GPUs -- every GPU shape GCP offers for
  these cards. The existing single-GPU A100 40GB row moves from Planned to
  Preview. These rows carry no new hardware validation evidence;
  multi-device bundle requests remain refused.

- The platform support matrix displays device counts for multi-GPU rows.

- Doctor reports accelerator SKU and device count in an `accelerator_facts`
  finding, including mixed-SKU and partitioned sets, separately from the
  platform support verdict.

- Supervision status reports requested and running accelerator pins in the
  agent protocol and CLI, retaining the old worker's pin until it exits.

- Worker supervision accepts an optional device pin at launch through
  `DesiredWorker`. A pin sets `CUDA_VISIBLE_DEVICES` after the environment
  allowlist; unpinned workers retain the allowlist behavior. Deployment
  allocation remains single-device and does not assign pins yet.

- Bundle manifests can declare an optional `accelerator_requirements`
  block with `device_count` and a `replicas` or `device_set` mode. Omission
  retains single-device behavior without a format-version change. Zero
  devices are invalid; requests above one device are refused with the
  requested count and mode until multi-device execution is supported.

- Platform capabilities carry the admitted device count alongside a
  per-device memory ceiling.

- Support rows can declare an accelerator `device_count`, defaulting to
  one. Rows with different counts can coexist and match their respective
  homogeneous device sets.

- Platform detection on a Compute Engine instance no longer needs the
  GCE metadata service at every start. On each start where the service
  answers, `tensorplate-agent` records the machine type in
  `/var/lib/tensorplate/state/machine-type.json` (`0640`, removed by
  purge), together with the kernel boot ID, logical CPU count, `MemTotal`
  and NVIDIA display device ids it was answered on. When the service cannot be
  reached (the connection or the request fails, or nothing at all comes
  back within the budget), detection in the agent and in `doctor` uses
  that record, but only in the same kernel boot while all hardware facts
  still match exactly. Every OS reboot requires one online agent start;
  offline cold boot is not supported. Schema version 2 requires the boot ID
  and refuses older records until an online start refreshes them. If
  there is no record, the record is not a regular file, is oversized or
  unusable, or a fact changed, detection fails with an error naming the
  reason. It never reports the instance without a machine type, because
  the shape-scoped cloud rows would then admit it as unvalidated. A
  service that sends anything else still fails detection and the record
  never overrides it: an error status, a closed or reset connection, an
  incomplete or unparseable response, or a 200 whose body is not
  `projects/<project>/machineTypes/<machine-type>`. Such an answer is
  never recorded. Response framing must be valid and complete: a timeout
  cannot terminate a body without Content-Length, and malformed lengths,
  duplicate lengths, transfer encoding, and non-HTTP status lines are refused.
  Record I/O pins parent directories and opens without following symlinks,
  so an agent-owned path cannot redirect an elevated doctor capture.
  The record lives in a directory only root and the
  `tensorplate` group can read, so offline `doctor` run by anyone else
  reports it as unreadable, with a hint to re-run as root or as a group
  member. The agent logs `platform identity: machine_type=...
  source=gce_metadata|recorded_gce_metadata|none
  record=written|unchanged|not_applicable|not_recorded (...)|failed (...)`
  on every start, and a failed detection as
  `platform detection failed: ...`. `doctor` shows the source after the
  machine type in `host_os`. `doctor --record` captures a missing record
  or a readable regular record within the size limit whose JSON or facts
  cannot establish identity, with a note. Unreadable, oversized, non-regular,
  and symlinked record paths still stop recording before fixture creation.
  The cloud lifecycle harness's offline stage rests on this record; see
  the entry above for what it establishes and what the boot binding costs.
  The tests use the recorded L4 `g2-standard-8` host fixture and
  synthetic cases. The H100 row has no recorded host fixture yet, so it
  is not exercised with recorded facts.

### Changed

- The Ubuntu x86_64 cloud lifecycle harness projects its journal captures
  where it takes them. `journalctl --output=json` is read into a scratch
  directory, and only `MESSAGE`, `PRIORITY`, `SYSLOG_IDENTIFIER`, `UNIT`,
  `_PID`, `_SYSTEMD_UNIT`, `_SYSTEMD_INVOCATION_ID` and
  `__REALTIME_TIMESTAMP` are written to `agent-journal.txt`,
  `observability-journal.txt` and `crash-loop-journal.txt`. The host
  metadata systemd attaches to every entry -- `_HOSTNAME`, `_MACHINE_ID`,
  `_BOOT_ID`, `__CURSOR`, `_CMDLINE` and the rest -- is never recorded,
  so a real run's journal evidence passes the publication scanner with
  nothing edited by hand, and the projected capture is the record that is
  retained: no raw copy of it outlives the capture. The scratch directory
  is removed whatever the verdict on the capture was, and by the
  harness's exit handler when a signal interrupts the capture; only an
  uncatchable kill or a machine failure can leave it in `$TMPDIR`. A line
  that is not a JSON record, including journalctl's own
  `-- No entries --`, is refused rather than copied through, and fails
  the stage that captured it. The unit and invocation assertions the
  status-logs and crash-loop stages make are unchanged and now read the
  projected records. `packages.txt` is recorded with the same
  `dpkg-query` the harness already used elsewhere rather than with
  `dpkg -l`, whose output carries each package's description and the
  planning identifiers those quote.

  The harness's own verifier checks that all three captures carry the
  allowed fields and no others, that they still carry the fields the
  stage assertions read, that a run which fails after the capture still
  files a projected one, and that the install listing is the query's
  three fields a line. Its stub journalctl emits the field set journald
  attaches to a service's output and to systemd's own records, plus one
  field whose name is new on every run, so only a projection that keeps
  the service's fields -- not one that drops the host fields it knows --
  passes. A non-record line after valid records must fail the stage for
  either kind of capture, with the refused line named in the stage log,
  because the projection is now the only reader of the raw capture. No
  run may leave a raw capture in the harness's scratch space: not a
  passing one, not one whose capture was refused, and not one signalled
  mid-capture.
  `tools/validation/check-evidence-publication.sh --patterns-only` must
  admit the evidence of every stubbed run, passing or failing, with or
  without a baseline, and must refuse a stubbed run's evidence with a
  single capture replaced by the unprojected output it was projected
  from, for that file's journal fields and nothing else. A passing scan
  on its own would certify a harness that had stopped projecting. The
  field set the harness keeps, the verifier asserts and the scanner
  admits is compared across all three files, so no two of them can drift
  together.

- The packaging verification suite runs in two groups. `test/packaging/run.sh core` holds the packaging, installer and descriptor checks; `run.sh harness` holds the lifecycle harness verifiers, which CI now runs as their own job with a 45-minute budget instead of inside the 15-minute packaging job. The release artifact build runs only the core group, since the harness verifiers check validation tooling rather than the artifacts. With no argument `run.sh` still runs everything, and a verifier in neither group fails the suite. The harness job also installs the release tooling's Python requirements, so its stubbed reports are validated against the lifecycle schema rather than skipping that check.

- The macOS Homebrew lifecycle harness's offline stage now runs the
  installed services with the network denied, not just doctor and an MPS
  check. Both launchd services run under a `sandbox-exec` profile that
  allows only loopback on the worker's serving and candidate ports and
  unix sockets other than mDNSResponder, loaded from derived plists so
  launchd still supervises them. Under that profile the agent must
  recover the deploy-smoke deployment on the exact M1 Pro row with
  validated evidence, and a fresh deploy, inference, doctor with its
  agent probe and the MPS probe must pass. A probe inside the sandbox
  must be refused every other destination, including other hosts on the
  serving ports and loopback on other ports, and binds on other ports,
  with `EPERM`, while the same operations outside it are not refused.
  Every process in the service trees must read back as sandboxed with
  the network denied; every internet socket a TensorPlate process holds,
  other than one never bound, must be on loopback; and neither service
  may restart during the stage. The stage then puts both services back
  under their normal launchd jobs and checks that nothing sandboxed
  remains. Accepted gaps, documented in the runbook: `fe80::1` through
  another interface on the two serving ports, a wildcard listener on
  those ports (which the loopback-only socket check refuses), and brokers
  reachable over unix sockets or XPC. A new `offline-profile` preflight
  stage checks the profile semantics against `sandbox-exec` before any
  Homebrew change and is not mapped to a canonical stage.

- The macOS Homebrew lifecycle harness restores normal launchd
  supervision and the agent config on every exit, ignoring INT, TERM and
  HUP only while it does. INT, TERM and HUP now exit with 130, 143 and
  129, so an interrupted stage records a fail row where TERM and HUP used
  to leave none. Cleanup's output goes to `cleanup.log` in the evidence
  directory rather than the failed stage's log, with its messages copied
  to the terminal, so a closed terminal or a stopped `| tee` no longer
  stops the restore. A cleanup that cannot copy the agent config back
  keeps the copy and fails the run.

- The single-GPU H100 row (`ubuntu2404-x86-h100-80g-a3hg1`) is now a
  Production row, and the RTX PRO 6000 Blackwell Server Edition row
  (`ubuntu2404-x86-rtxpro6000se-g4s48`) is now Preview. The Production
  set for this release is the Jetson Orin Nano, the MacBook Pro M1 Pro,
  and the single-GPU L4 and H100 cloud rows. Every multi-GPU row stays
  Preview: supported, and not yet validated. The RTX PRO 6000 row's
  model-class claims move to Preview with it, and its evidence location
  is removed, since a Preview row with no validation run has no evidence
  to point at. The H100 row claims no model classes; it is a Production
  target whose platform validation is still pending. Its provenance
  remains `spec_authored`, and the release gate requires recorded evidence
  before it can qualify for release.

- `build-release-artifacts.sh --snapshot --arch amd64` configures the
  x86_64 serving worker with the release's compiler, `-gdwarf-4` and
  backend selection, without the TensorRT and DWARF environment
  workarounds. They come from `tools/release/amd64-build-profile.sh`,
  which the release workflow's amd64 job and the Ubuntu x86_64 CPU-only
  smoke also read. vcpkg detection is unchanged, so `VCPKG_ROOT`,
  `VCPKG_INSTALLATION_ROOT` or `TP_CMAKE_TOOLCHAIN_FILE` on the build
  host still adds a toolchain file the release does not use. On amd64 the
  builder refuses `TP_ENABLE_TENSORRT`, `TP_REQUIRE_TENSORRT_SDK`,
  `TP_ENABLE_LIBTORCH` and `TP_ENABLE_PYTHON_PYTORCH_SIDECAR` overrides, a
  missing clang++, and a build directory that recorded another compiler,
  before anything is compiled. The arm64 build is unchanged.
  On every architecture `--manifest` and `--checksums` are optional,
  defaulting to `tensorplate-<tag>-artifacts.json` and `SHA256SUMS` in
  the artifacts directory: the manifest name a URL install fetches, in
  the directory `install.sh --local-artifacts` reads. Any other name or
  directory is refused before the build.

### Fixed

- `prepare` now folds `[Unreleased]` into the dated section for the version
  being prepared, rather than only opening that section the first time it
  runs. A tag is cut from a trunk commit, so every entry under
  `[Unreleased]` at that commit ships in that tag; once the heading
  existed the step did nothing, so this release's own entries -- nine of
  them, merged after the heading was opened -- stayed under `[Unreleased]`
  above the `0.2.1` section and shipped filed as unreleased. Nothing caught
  it: a `prepare` that changes nothing is exactly how
  `release_metadata_pending`, and so `cut`, reports the metadata final.

  The fold is a pure move. Entries go to the top of the same-named
  subsection, newest first; a subsection the release section has no block
  for is opened at its top; entry text, including indented continuation
  lines and fenced blocks, is carried over byte for byte; and
  `[Unreleased]` is left present and empty, so a second run changes
  nothing. A heading-shaped line inside a fence opened at column 0 is
  quoted text rather than a section, so an entry that shows a changelog's
  own shape moves intact instead of being filed into the fence. A fence
  ends only at a run of its own character at least as long as the one that
  opened it, so a ``` line quoted inside a ```` block does not end it. A shape
  the step does not recognise -- no `[Unreleased]`, a repeated heading, a
  version section that is not the one directly below `[Unreleased]`,
  content ahead of a section's first subsection, or a fence left open --
  is refused rather than guessed at. `cut` now refuses to tag until a
  preparation pull request commits the fold, which is what should happen
  whenever the trunk moved since the last one. Folding this release's own
  entries is that pull request's job, not this change's.

- The macOS Homebrew lifecycle harness names each service's launchd job the
  way the installed Homebrew does. Homebrew 6.0.22 changed the label and
  keg plist name of a formula's service from `homebrew.mxcl.<formula>` to
  `sh.brew.<formula>`, and the harness hardcoded the old form in every
  launchd check. The `v0.2.1-rc.2` run on the M1 Pro, under Homebrew 7.0.6,
  therefore failed at `launchd-start`: `brew services start` loaded
  `sh.brew.tensorplate-agent` and the harness looked for
  `homebrew.mxcl.tensorplate-agent`. No stage after it had ever run under
  the new naming.

  The harness now reads each service's label from the plist Homebrew
  generated in the formula's keg. That one value names the keg plist, the
  `~/Library/LaunchAgents` copy `brew services start` installs and the job
  it loads, and launchd-start, launchd-restart, launchd-crash-loop and
  offline-runtime all use it; the offline stage derives its sandboxed plist
  from that keg plist and runs it under the same label. A keg holding a
  plist of neither form, or of both, fails the stage. Cleanup and the
  uninstall stage may run with the keg gone and check both forms, since a
  job loaded by an earlier Homebrew keeps its label. The uninstall stage
  now also requires that no job under either label is still loaded, which
  its transcript entry already claimed. The runbook's manual `launchctl
  bootout` steps say how to find the label instead of naming the old one.

- A completed macOS lifecycle run no longer leaves the host without the
  formula trust its next preflight requires. `brew uninstall` drops a tap
  formula's trust entry unless the whole tap is trusted, and the harness
  uninstalls the candidate graph, so every run that reached the clean
  install ended with only `tensorplate/tap/tensorplate` trusted, re-added by
  the baseline install, while the harness untrusted only entries it had
  added itself. It now records the six formulae's per-formula trust before
  the tap-trust stage and puts exactly that set back before the clean
  install, before the upgrade and on exit: it re-trusts what an uninstall
  removed, untrusts what an install added, adds nothing else and logs each
  change. The clean install needs this as well: a graph left
  installed by an interrupted run lost its trust when the stage removed it,
  and Homebrew refuses to load an untrusted dependency.

- The macOS lifecycle harness's tap-trust stage refuses a tap on a custom
  remote, such as one tapped from a local clone, before anything changes.
  Homebrew keys that tap's formula trust by the remote rather than by
  `tensorplate/tap`, so the trust restore would re-trust entries it never
  reads back: the clean install would fail with the baseline already
  removed, and the run would leave remote-keyed entries in place of the
  operator's. The runbook no longer tells the operator to untrust the
  component formulae after a run, which undid the restore, and says to
  re-grant that trust when a run is killed, or its cleanup interrupted,
  before the restore.

- A release candidate's signed `SHA256SUMS` and manifest now name the files
  the release contains. GitHub rewrites `~` to `.` in an uploaded asset's
  name, so `v0.2.1-rc.1` served `tensorplate-agent_0.2.1.rc.1-1_arm64.deb`
  while both lists named `tensorplate-agent_0.2.1~rc.1-1_arm64.deb`, which
  was a 404. `install.sh` failed on the first package it downloaded, the
  Jetson lifecycle harness refused the set, and `sha256sum -c SHA256SUMS`
  over the published release reported thirteen listed files it could not
  read. Final releases carry no tilde and were never affected.

  `build-release-artifacts.sh` now stages every package under the name GitHub
  serves before anything records it, and manifest generation and
  verification refuse any asset name that still holds a `~`. Only the file
  name changes. Each package's control Version stays `0.2.1~rc.1-1`, and the
  manifest records that version, not the published spelling, which sorts
  above `0.2.1`. Because the published name can no longer tell `.` from
  `~`, manifest generation reads each package's control Version with
  `dpkg-deb`, records that, and refuses a package whose control Version is
  not the one its name stands for; it now needs `dpkg-deb` to run. The
  Ubuntu cloud lifecycle harness ordered the upgrade path on the manifest's
  version. It now reads each package's control Version with `dpkg-deb`, as
  the Jetson harness already did, and both harnesses name a listed package
  that is missing from the set instead of reporting it as unreadable.

  The release workflow now reads back the asset names a new GitHub Release
  serves and fails unless they are exactly the names `SHA256SUMS` lists,
  plus `SHA256SUMS` and its signature bundle. That checks GitHub's actual
  behaviour rather than the one rewrite the build models, and for a final
  release it runs while the release is still a draft.

  `v0.2.1-rc.1`'s release and tag are no longer on GitHub. The next
  candidate is the first one built this way, and the first that can serve
  as the amd64 upgrade and rollback baseline.

- `jetson-runner-control.sh status` reports the grant the runner actually
  has. It probed whether the runner account could `sudo apt-get`, which the
  previous change deliberately removed from the allowance, so a correctly
  provisioned runner printed `runner_can_sudo_apt_get: no` -- the right state
  wearing the shape of a failure -- and nothing reported on the wrapper that
  replaced it. Status now runs a `probe` mode through the wrapper, which is
  reachable only if the sudoers grant permits that path, and says plainly
  whether the wrapper is installed at all. An absent wrapper names the
  remedy, because the way to get one is to re-run `off` and `on` from a
  checkout new enough to contain it -- which is exactly the mistake that
  cost two release-workflow runs.

- The self-hosted Jetson release job no longer elevates binaries the runner
  is not allowed to run. Two separate steps did: the apt drop-in was written
  with `sudo tee`, and `tools/ci/apt-get.sh` ran `sudo timeout ... apt-get`
  and `sudo dpkg --configure -a`. The runner's allowance names `apt-get` and
  `install` only, so each asked for a password nobody can type; the first
  failed the initial `v0.2.1-rc.1` build and the second would have failed the
  step after it.

  The drop-in now goes through `install`. The time bound moves into
  `/usr/local/sbin/tensorplate-apt`, a root-owned wrapper that
  `jetson-runner-control.sh` installs alongside the sudoers file, and the
  allowance names the wrapper instead of `apt-get` -- narrower than before,
  since the wrapper accepts only `update`, `install` and a `configure-pending`
  mode. The bound has to be apt's direct parent, which is why it could not
  simply move outside `sudo`; granting `timeout` would have worked and handed
  the runner account a root shell via `sudo timeout 1 /bin/sh`.

  Re-run `jetson-runner-control.sh off` then `on` to install the wrapper.

- The release workflow's self-hosted Jetson job writes its apt drop-in with a
  binary the runner is actually allowed to run. `jetson-runner-control.sh`
  installs a deliberately narrow sudoers allowance -- the runner account gets
  `NOPASSWD` on `apt-get` and `install`, and nothing else -- while the step
  used `sudo tee`, the spelling every GitHub-hosted copy of it uses. On the
  Jetson that asks for a password nobody can type, and the job died with
  `sudo: a password is required` before any package was built. The step had
  been written for hosted runners and had never run on this one, so it failed
  the first `v0.2.1-rc.1` build. It now writes a temporary file and installs
  it with `install`, rather than widening a grant whose whole purpose is to
  stay small.

  `test/release/test_build_configuration.py` now derives the granted binaries
  from the control script's `NOPASSWD` line and fails if the self-hosted job
  elevates anything outside them. The grant and the workflow live in separate
  files and nothing bound them, which is why a step could be added to that job
  without anyone noticing it could not run there.

- The release workflow's amd64 package-closure assertion no longer fails on a
  package that is correct. `dpkg-deb -c "$deb" | grep -q PATH` exits at the
  first match, closing the pipe while dpkg-deb's tar still has entries to
  write; tar dies on SIGPIPE, and under `set -o pipefail` that becomes the
  pipeline's status even though the match succeeded. The step then reported
  `tensorplate-serving amd64 package does not ship
  /usr/lib/tensorplate/tensorplate-serving` and printed, directly beneath it,
  a listing containing that file. It failed the `v0.2.1-rc.1` build, where the
  package was correct and every downstream publish job was skipped.

  Both copies of the check now match against a here-string, which has no
  producer process to signal. Capturing the listing into a variable is not
  sufficient on its own: piping that variable back into `grep -q` recreates
  the identical race with the shell as the producer, which is worth stating
  because it is the obvious fix and it does not work.
  `test/packaging/verify_arch_package_set.sh` carried the same pipeline and is
  fixed with it; it had not fired only because its stub-built archive is small
  enough that tar finishes writing before grep exits.

- The macOS offline-runtime stage no longer credits an unsandboxed
  control whose network operation never completed. The control is what
  distinguishes "the sandbox refused this" from "this never ran", and
  `classify` rejected only three outcomes -- a missing name, `EPERM`, and
  a socket that could not be pinned -- so everything else counted as a
  valid control: a connect that waited out its timeout, a child that
  exited without reaching its send or never started at all, an exception
  `attempt` recorded by name, a routing answer for a loopback address
  `lo0` does route. The probe's `EPERM` was then read as proof of denial
  against a baseline that had proved nothing, which turns "we could not
  tell" into "denial proved" in published evidence.

  Controls are now checked against what each operation completing looks
  like rather than against a list of the failures someone thought of,
  which is how the Linux module already reads its own controls, under the
  same two names: `control_not_refused:<operation>` for a refusal by
  something else on the host, `control_completed:<operation>` for an
  operation that did not happen. On macOS the sandbox decides on the
  destination address before the route lookup, so a control carried as
  far as a routing answer got past where the sandbox would have refused
  it, and three completions are recognized per operation: a send, bind or
  listen that returned; the loopback connect the far end refused; and,
  for the destinations pinned to `lo0`, `ENETUNREACH` or `EHOSTUNREACH`.
  Each denied operation is placed in one of those sets by name, spelled
  out one at a time rather than matched against the destination in the
  name: a substring rule reads a new operation to a destination already
  listed -- the likely addition -- straight into the loosest set on its
  own. A denial added later that nothing places falls to the strictest
  set, so it has to be placed deliberately instead of inheriting an
  excuse from a neighbour.

  The control is also judged as it is taken, before the profile is
  applied to anything, so a host that cannot provide a baseline now fails
  the stage before its launchd services are moved under the sandbox
  rather than after -- again matching the Linux module. `classify` still
  repeats the check and takes no control on trust.

  `verify_macos_offline_runtime.sh` pins each operation's completion set
  against a table written out in the test, requires the recorded probes
  to bear that placement out, and drives every denied operation through
  every outcome that is not one of its completions, asserting the exact
  failure each names. A stage case runs the harness with a control child
  that exits without sending and requires the stage to fail at the
  control, before anything is denied.

- The macOS offline-runtime evidence no longer reports readback controls
  that were not measured. Before reading whether a process is sandboxed,
  the stage proves on the host that the readback tells a sandboxed
  process from an unsandboxed one and rejects a process that has exited,
  reaped or not. Those four proofs raised on failure but returned four
  hardcoded `true` literals, and `offline-runtime.json` published the
  literals, so the artifact reported four measurements whatever the
  checks had done -- and one of the four could be deleted outright with
  the verifier staying green, because nothing exercised it. Each flag is
  now the result of the check that proves it and starts false, anything
  still unproved fails the stage by name before it can reach the
  evidence, and `verify_macos_offline_runtime.sh` drives each of the four
  to failure on its own. The verifier also checks all four as the
  published `offline-runtime.json` carries them (and, on macOS,
  `offline-profile.json`), not only as the helper returns them: the
  artifact could otherwise drop the flags with every check still green.

- The Ubuntu x86_64 cloud lifecycle harness no longer credits its
  rollback stage with preserving operator state on the strength of a
  pathname. `tools/validation/ubuntu-l4-cloud-lifecycle.sh` asserted the
  set-aside state with `test -f state.bak/state.json`, which a file
  truncated to zero bytes, emptied, or rewritten with different content
  during the removal or the baseline install satisfies just as well as
  the original — so the stage recorded the documented rollback procedure
  as preserving durable state it had never read, which is the one thing
  that stage exists to prove. With the services stopped and before the
  move, the harness now lists `state/` and digests every file in it, and
  after the baseline install does the same to `state.bak/` and requires
  the two to match name for name and digest for digest, naming the first
  file that changed, went missing or was added. The claim is about the
  whole directory, not one name in it: a host keeps more there than the
  agent's `state.json` — the agent also refreshes `state.json.bak`, the
  copy it falls back to when the primary fails to decode, and the
  observability unit writes `observability-snapshot.json` beside them —
  so destroying any of them in place now fails the stage by name. A state
  directory with no `state.json` is refused where the manifest is taken,
  before anything is moved or removed, and a `sha256sum` whose leading
  field is not sha256 hex is refused where it is read rather than carried
  forward, so two unreadable files can never compare equal. The checks
  that follow deliberately do not load the set-aside files — they exist
  to show the older agent did not — so nothing else could have caught
  this. The Jetson harness was fixed the same way and by the same method,
  so the two now make the same claim rather than diverging.
  `verify_ubuntu_l4_cloud_lifecycle.sh` drives the new guard against a
  stubbed appliance whose durable state holds all three files: thirteen
  rollback regressions — emptied, truncated and rewritten copies, a
  deleted recovery copy and snapshot, an added file, an emptied
  directory, a directory that is gone outright, a missing `state.json`
  and a non-hex digest — each fail the run at the named step, and a
  failing privileged digest read is injected as well. The passing run
  pins that the listing and the digests are taken after the stop and
  before the move, that the saved copy is read back file by file after
  the baseline install, and that no existence check on `state.bak` is
  made at all. The runbook's rollback section states the same claim.

  Review of that change added three things. The two harnesses now hold
  the guard as the same five functions, and `verify_lifecycle_state_guard.sh`
  binds them: each body must be byte-identical in
  `ubuntu-l4-cloud-lifecycle.sh` and `jetson-lifecycle.sh`, and every
  harness variable those bodies read — derived from the bodies, not
  listed — must be bound to the same value in both. Nothing else read
  both files, so a tightening applied to one would have left the other's
  Production-row evidence weaker while reading as if it had not. The
  comparison that reports a destroyed file now splits a manifest line at
  its last space rather than its first, so a state file whose name holds
  a space is named in full: the verdict was already right, but the
  failure an operator acts on named a file that does not exist and
  printed a fragment of the name where a digest belongs. And the
  refusal of a set-aside directory that is there and holds nothing is
  held to naming the directory: the emptied-directory case now also
  requires that no individual file is named, because admitting the empty
  manifest would still fail the stage while sending the operator to
  recover one file out of three.

  A further review closed three gaps in that. The half of the
  comparison that reports an ADDED file splits its lines on its own, and
  no case reached it: the spaced-name case changes a file, so reverting
  only that loop to a first-space split left every verifier green while
  the failure named a file called `extra` for one called `extra file.json`.
  `rollback-adds-spaced-file` now plants the latter in the set-aside copy
  and requires the whole name. The binding had blind spots of its own.
  It counted only the canonical `name() {` line, so a one-line
  redefinition after the checked copy -- the one bash actually runs --
  passed; and it derived only upper-case names read as `$NAME` or
  `${NAME}`, `${NAME%`, `${NAME#`, `${NAME:`, so both bodies reading
  `${NAME-}` or `${#NAME}` of a variable only one harness defines passed
  too. It now counts every spelling bash accepts, derives names in either
  case through every expansion form, and requires each to be bound on
  exactly one line in each harness, in any spelling -- a caller's
  shadowing `local` included. It then plants each of those drifts into
  copies of the two harnesses and requires itself to refuse every one for
  its own reason, so deleting any one rule fails its own run.

- A Compute Engine instance whose metadata service is not answering yet
  when `tensorplate-agent` starts no longer refuses deploys for the whole
  boot. Platform detection ran exactly once per start, so a single
  unreachable metadata query settled a detection failure that the agent
  then held until somebody restarted it by hand — on a host that was
  otherwise healthy and would have detected correctly a second later. The
  observation is now retried up to six times, with the delay doubling from
  500ms, and the verdict is settled from whichever attempt answers. A
  twenty-second budget bounds the sleep schedule: no sleep is started that
  would end past it, and one that would is shortened to whatever is left.
  It bounds the schedule rather than total elapsed time — the attempt that
  runs after the last permitted sleep is not itself timed — so an episode
  ends at up to the budget plus one attempt's work, and the exhaustion line
  reports the elapsed figure rather than assuming it.

- Both shipped units raise `StartLimitIntervalSec` from 60 to 300 seconds,
  so a unit that fails repeatedly still settles into `failed` instead of
  restarting forever. `StartLimitBurst` rate-limits unit STARTS, so it only
  bites when five of them land inside the window. A start that pays the new
  detection retry and then fails after admission — an unopenable state
  store, an already-bound socket — takes that budget plus `RestartSec` per
  cycle, so at most three such starts fitted in a sixty-second window and
  the burst never tripped. Five worst-case cycles are about 150 seconds,
  which 300 holds with margin. A fast-failing unit is unaffected: five
  starts at a five-second cycle still land inside twenty-odd seconds and
  still stop at the same wall clock, so this only adds the slow-cycle case
  the old window missed. `verify_systemd_units.sh` now derives the worst
  cycle from `DETECTION_RETRY_BUDGET` and `RestartSec` and fails when the
  window cannot contain `StartLimitBurst` of them, because the two numbers
  live in different files and nothing previously linked them — raising the
  retry budget alone would have reintroduced the defect silently.

  The retry is gated on which step failed, not on what the error says.
  Both the sources step and the identify step raise the same
  `IdentityUnestablished` variant and mean different things: from the
  identify step it means this is a Compute Engine instance whose metadata
  service could not be reached and which has no usable record for this
  boot, which is the recoverable case; from the sources step it means the
  machine-type record is a directory, a symlinked path, or oversized,
  which is a tamper signal, is deterministic, and stays terminal. Retrying
  the second would let a later live answer overwrite the record without
  the signal ever being reported, so the two are told apart structurally
  rather than by matching on prose. Every other failure — an unreadable
  source, an uninterpretable one, or a metadata endpoint that answered
  with something that was not a machine type — still settles on the first
  attempt.

  The retry sits inside the observation, before the accelerator probe and
  the package database, so a failed attempt re-reads the host sources and
  re-asks the metadata service but never re-runs `nvidia-smi` or
  `dpkg-query`. It also settles before the matched row's memory ceiling is
  applied, which is what keeps `device_memory_bytes` written exactly once
  through the existing path: a verdict that turned supported after that
  point would leave both memory gates disabled, and a regression test and
  a comment at `apply_memory_limit` now pin that ordering.

  Four new journal lines, and only when the retry does something:
  `platform detection retry:` per failed attempt with the elapsed time,
  the next delay and the error, then exactly one of three lines closing
  the episode. `platform detection recovered:` names the attempt that
  answered, how many failed, how long it took and the FIRST error, so a
  late success does not make the earlier failure invisible. `platform
  detection exhausted:` names both bounds when no attempt answered.
  `platform detection stopped:` names the attempt count, the budget and
  the error when a failure that is not retryable arrives after the retry
  has already begun — a metadata service that comes up answering badly
  mid-window ends the episode with its budget unspent, and an operator
  reading the journal has to be able to see that rather than infer it.
  Exhaustion is byte-identical to the previous behaviour — the existing
  `platform detection failed:` line, a rejection carrying no frozen
  reason, and an agent that keeps listening. A host that answers on the
  first attempt, and every host that is not a Compute Engine instance,
  writes none of these lines and pays no delay: the journal is unchanged.

  The twenty-second budget is a first estimate. The gap between
  `network.target` and the first metadata answer has not been measured on
  either production cloud row; the recovered, exhausted and stopped lines
  report exactly that gap, so the fleet's own journals are what should
  correct it. Whoever raises it should raise `install.sh`'s
  `TP_INSTALL_SERVICE_READY_TIMEOUT_SECONDS` with it: that wait defaults
  to 30 seconds and the budget is spent before the agent socket exists, so
  a budget pushed toward 30 turns an install that would have completed
  degraded into a hard failure. The systemd unit is deliberately unchanged — there is one agent
  unit, so ordering it behind `network-online.target` would be paid by
  Jetson, bare-metal and air-gapped installs that have no metadata service
  at all, and shipping both changes at once would make neither effect
  separable in the evidence. The accelerator-probe branch is not retried
  either: a driver module still loading is the same shape of bug and is
  not fixed here.

- `doctor`'s `cuda_runtime` finding reports what is true on the host it
  runs on. It checked seven fixed system-toolkit paths and said "CUDA not
  detected; vision-on-TensorRT validation will skip" for everything else,
  which on an x86_64 GPU host was wrong twice: the driver could be loaded
  and the card identified, and the amd64 serving worker is built without
  the TensorRT adapter, so there was no TensorRT validation to skip. The
  NVIDIA driver and a system CUDA toolkit are now separate facts, probed
  under separate path lists and each named by the path it was found
  under — `libcuda` is the driver's own library and no longer counts as a
  toolkit, and a versioned `libcudart.so.<soname>` does count, so a host
  carrying only the CUDA runtime package is no longer told it carries
  nothing. Each list falls back to a scan of its own library directories
  for a versioned soname, so a driver is still found where the exact
  names are absent: `/proc/driver/nvidia/version` comes from `nvidia.ko`,
  which L4T does not load, and a Jetson shipping `libcuda.so.1.1` without
  a `libcuda.so.1` would otherwise have read as driverless and been
  warned about a device that is fine. In the exact lists and in the scan
  alike, a candidate counts only when it resolves to an existing regular
  file: ldconfig's soname links outlive the files they point at, so an
  incomplete upgrade or a removed package leaves `libcudart.so.12`
  pointing at nothing, and accepting a directory entry on its name — or
  a directory standing where a library is looked for — would report the
  CUDA runtime as present on a host whose worker cannot load it, which
  is the broken dependency this finding exists to name. A rejected
  candidate is named in the message, which now reads ``no system CUDA
  toolkit at the known paths (`<path>` is a name with no library behind
  it)``, so a host part-way through an upgrade no longer reads exactly
  like one that never carried the library: the verdict is the same on
  both and the work is not. The driver's clause says the same of a dead
  driver name and keeps the driverless hint, because a name with no
  library behind it does not establish that the driver package is
  installed. The path reported is the name the probe looked under, never
  the versioned target a link resolves to. The third probe behind this
  finding — which of the CUDA-consuming components are installed — takes
  the same rule: the python_pytorch sidecar counts when a descriptor
  *file* is at its packaged path, parseable or not, and a directory or a
  dead link standing there is no longer read as an installed sidecar.
  On x86_64 the rule runs the other
  way: `/proc/driver/nvidia/version` is what establishes a loaded driver,
  and the driver's user-mode `libcuda.so.1` is not accepted in its place
  — that package installs with no working kernel module, which is the
  inference `packaging/scripts/install.sh` already refuses in a comment.
  Found alone, the library is reported as its own state, naming it and
  the absent kernel interface, with the verdict a driverless host gets
  and a hint pointing at the module, `nvidia-smi`, a pending reboot and
  Secure Boot. The verdict is taken against the installed build, read
  from the CLI's own build target because the CLI and the worker ship in
  one per-architecture artifact set: the arm64 worker links the TensorRT
  adapter, so no toolkit there is a `warning` with an install hint, while
  the amd64 worker has no such adapter and its python_pytorch sidecar
  brings its own CUDA runtime in the PyTorch wheel, so no system toolkit
  there is `ok`. The driver governs the status on its own: `libcuda` is
  the driver package's library and every CUDA consumer loads through it,
  so a host with no driver is never `ok` however many toolkit files it
  carries — `missing` on amd64 and `warning` on the TensorRT-linked
  build, each naming the absent driver. The message states only what is
  installed here: wherever the verdict rests on the python_pytorch
  sidecar it names that sidecar as present or absent rather than assuming
  it, and where neither the serving worker nor that sidecar is installed
  — a `--cli-only` install — the finding is `skipped`, because nothing on
  the host consumes a CUDA runtime. The sidecar's package only
  `Recommends` the serving one, so it can be installed alone; it is then
  the host's only CUDA consumer, and the absent worker's TensorRT adapter
  is not asked for on its behalf. What its own wheel needs is asked for
  instead, which is an architecture question rather than a build one: the
  x86_64 PyPI CUDA wheel carries its runtime libraries, so driver and no
  toolkit is `ok`, while NVIDIA's aarch64 wheel links the JetPack CUDA
  runtime — the Jetson install guide has the operator `apt install` it
  and says `import torch` fails on a missing CUDA shared library without
  it — so on the arm64 artifact set the same host is a `warning` pointing
  at that apt list. A platform whose build has no CUDA path at all is
  `skipped` too. The finding still never fails `doctor`, so no install
  and no lifecycle harness changes outcome. `cuda_runtime` is the one
  finding id whose severity range has widened since v0.1.0; the exception
  is recorded against the contract note in
  `cli/src/commands/doctor/finding.rs`.

- `doctor`'s `tensorrt_runtime` and `libtorch_runtime` findings hold
  each library name to the rule `cuda_runtime` holds its candidates to:
  a directory or a dead link standing at `libnvinfer.so`,
  `NvInferVersion.h` or `/usr/lib/libtorch.so` was reported as the
  runtime being detected and is now reported as absent.
  `/usr/local/libtorch` and `/opt/libtorch` keep the weaker rule and are
  detected as directories, because those two names are the unpacked
  LibTorch distribution's own directory rather than a library. Neither
  finding is build-aware yet and neither fails `doctor`.

- The twenty-two `ubuntu2404-x86` GPU support rows no longer declare a
  `tensorrt` backend path. The amd64 `tensorplate-serving` build sets
  `TP_ENABLE_TENSORRT=OFF`, so the shipped binary contains no TensorRT
  adapter and `packaging/conf/agent.amd64.json` advertises only
  `python_pytorch`; the rows nonetheless named a `tensorrt` path satisfied
  by the `tensorplate-serving` package. That was a false published claim
  about every L4, H100, A100 and RTX PRO 6000 row first, and a live
  admission hole second. On a stock install the hole was not reachable:
  the shipped `/etc/tensorplate/agent.json` lists `python_pytorch` alone,
  and a TensorRT bundle is refused a step earlier, at compatibility
  evaluation, with `UnavailableBackend`. Deploy admission consults the row
  only after that check passes, so the row was the last gate exactly where
  an operator had edited that dpkg conffile to advertise `tensorrt` — and
  there the row found `tensorplate-serving` installed and admitted a bundle
  the worker can only refuse at engine lookup. That host is now refused with
  `MissingBackendPackage` naming the undeclared path, the architecture, the
  paths the row does declare, and that admission read those declarations
  rather than the installed package set — so an operator does not read the
  refusal as something to install. It asserts nothing about which backends
  the installed build compiled in: that is true of `tensorrt` here and is
  why the row is right to be silent, but a row records a package set, not a
  build, and a row withholding a path its build does contain is allowed.
  `docs/platform/support-reasons.md` now separates the two next steps
  `missing_backend_package` covers. The `arm64` Jetson rows keep `tensorrt`:
  that build compiles the adapter. `doctor` output is unchanged — it never
  reported a row's declared backend paths, and it still reports no
  installed build's compiled backends, which is part of why the claim went
  unnoticed.

- A support row may no longer declare a backend path that the agent config
  shipped for its package channel and CPU architecture does not list in
  `available_backends`. The mapping is the one
  `packaging/debian/tensorplate-agent.install` encodes, and it fails
  closed: a channel and architecture pair with no shipped config is an
  error to state rather than a row to skip.
  `packaging/conf/agent.amd64.json`, which only a dh-exec filter installs
  and which nothing previously parsed, is now read through the agent's own
  config validator.

- The CLI reads the `/etc/tensorplate/cli.json` its own package installs.
  Discovery was `--config`, then `$TENSORPLATE_CLI_CONFIG`, then built-in
  defaults, so on a Debian install the packaged conffile had no effect:
  `doctor` reported the built-in `/var/run/tensorplate/agent.sock` rather
  than the packaged `/run/tensorplate/agent.sock`, and an operator's edits
  to the file changed nothing. The packaged conffile is now the step
  before the built-in defaults. It is one fixed absolute path, not a
  search, and only root can write it on an installed host. The
  environment step still wins, so the Homebrew launcher continues to
  select the config under its own prefix.

  A packaged config that is present and unusable — malformed JSON, a
  document that fails validation, or bytes that are not text — fails every
  command that needs the configured profile, naming the file, instead of
  falling back to defaults that disagree with the install. `doctor` and
  `version` are exempt: they answer from the built-in defaults instead of
  aborting, because doctor is the command the docs name for diagnosing a
  broken install and `version` reads nothing from the config. Doctor
  reports the CLI's reason for refusing the file as a failing
  `config_files` finding and exits `10`, including for a file that is
  valid JSON with a recognized `schema_version` but fails the CLI's own
  validation, so it never calls such an install healthy. Reading the
  conffile at all is what made that failure mode reachable from `/etc`.

  A packaged config that exists but this caller may not read — the config
  directory is `root:tensorplate 0750` — leaves the defaults in place for
  every command and prints one line on stderr naming the file and the
  group. That is a property of the caller, not of the install.

  Both notes are human stderr, suppressed by `--quiet`. Under `--output
  json` stderr still carries the error envelope and nothing else, and the
  same text is carried by the envelope's new optional top-level `warnings`
  array, on the ok and the error path alike — so a scripted caller can
  tell an install running on the packaged profile from one running on the
  built-in defaults, which it could not do before.

  Config discovery has a test that fails when the wiring is wrong: which
  file `resolve()` looks for is now a named function a unit test asserts
  on, and reading the environment with `var_os` rather than `var` is
  pinned by driving the binary with a path that is not valid UTF-8. The
  suite stayed green against a CLI that read the wrong file, which is the
  blind spot that let this ship.

- `tensorplate logs` no longer points at a log file nothing writes. The
  Debian CLI config named `/var/log/tensorplate/tensorplate-agent.log`,
  which no component creates: both units log to the journal, the agent
  writes its diagnostics to stderr, and the only NDJSON writer, the
  observability service's retention sink, is off in the Debian config and
  carries only that service's own events in any case. The packaged config
  now names no log path, and a `logs` run with no NDJSON source exits 6
  (`unavailable`) carrying a hint that names the journal to read instead:
  `journalctl -u tensorplate-agent` for the agent and for the serving
  worker and backends it supervises, `journalctl -u
  tensorplate-observability` for the observability service, both when no
  component was given. It never returns an empty successful read that
  looks like "no such events". A configured `log_source.path` that does
  not exist gets the same answer, so an upgrade that keeps a locally
  modified conffile naming the old path stays actionable; a path that
  cannot be read, and a `--source` the operator named, remain IO errors
  that name the file. On macOS the same missing-path case gets its own
  hint: the Homebrew observability formula does write that file, so the
  answer there is that `brew services start tensorplate-observability` has
  not run yet, not that nothing writes it.

  The CPU-only smoke, which runs the real CLI against a real package
  install, now requires the documented answer instead of discarding the
  status with `|| true`: exit `6` whose output names `journalctl -u
  tensorplate-agent`, or exit `0` that returned entries or said why it
  returned none. It also asserts the evidence line from the issue itself —
  doctor's `agent_socket` finding must name the packaged
  `/run/tensorplate/agent.sock`, not the built-in `/var/run/...` — because
  the `logs` answer alone is identical whether or not the conffile was
  read. The cloud and Jetson lifecycle harnesses still record this exit
  status rather than requiring it.

- `tensorplate logs` says so when a read matched nothing. A source that
  opens and yields no entry is the other way the command could answer
  with silence, and it is the standing case for `--component agent`
  against an events file: the only NDJSON writer is the observability
  service's retention sink, whose event listener transport is
  `in_process`, so the agent and serving worker — separate processes —
  never appear in it. That covers the Homebrew install and any Linux site
  that turns retention on. The command now writes one line to stderr
  naming the source, the component filter, and every other filter that was
  applied.

  It adds where that component's own output is — the journal on Linux, the
  per-service `*.error.log` beside the packaged event log on Homebrew —
  only when nothing else can explain the empty result: not when `--level`,
  `--since-ms` or `--correlation-id` could have excluded every entry, not
  for a `--source` the operator named themselves, not when every line was
  malformed, and not for `--component observability`, which is the service
  that does write there. Claiming otherwise sends the operator to the
  wrong file.

  Human output only, suppressed by `--quiet`, so `--output json` callers
  still read `payload.entries` from a clean envelope.

- `tensorplate logs` no longer writes the malformed-line count straight to
  the process's stderr. The bare `eprintln!` emitted a plain line in
  `--output json` runs, where stderr is a single envelope document, and
  bypassed the writer the caller passes in, so no test could see it. It is
  a renderer note now, and JSON callers read the count from
  `payload.malformed`.

- `--quiet` suppresses informational stderr, as `--help` says it does.
  `Verbosity` was parsed and read nowhere. The usage line no longer claims
  `--verbose` expands anything, because nothing expands today.

- NVIDIA probe tests use immutable executable fixtures with isolated
  temporary output paths, avoiding intermittent Linux `Text file busy`
  failures when parallel tests launch a freshly written executable.

- The macOS offline-runtime stage waits for launchd to assign each
  sandboxed service its initial PID after Homebrew bootstraps the job.
  It reads each job at most 30 times, waiting one second between pending
  reads, so a slow first spawn can complete without failing immediately
  and a job that never starts still fails the stage.
  Both services must still finish on that initial PID after exactly one
  launchd run; a restart during the stage remains a failure.

- The package lifecycle and services documentation no longer claims dpkg
  restarts the services on upgrade. The package scripts stop both units
  and nothing in the packages starts them; `systemctl enable --now`, which
  the release installer runs, brings them back, and the v0.1.1 to v0.1.2
  APT upgrade steps now include it before doctor must be green. The
  rollback procedure, in the lifecycle and post-release documentation,
  removes every installed TensorPlate package except
  `tensorplate-apt-source`, including `tensorplate-common` and
  `tensorplate-backend-python-pytorch`, since a newer package left behind
  makes the older install a downgrade that `apt-get -y` refuses.

- The macOS status-logs check follows event retention across
  `events.ndjson` and `events.1`. File identities and byte offsets taken
  before service startup distinguish current-run output after rotation,
  including rotation between the CLI read and verification. A returned
  event that remains in either generation can still pass the check;
  the harness does not delete or truncate logs.

- The macOS Homebrew lifecycle harness enforces sixteen assertions that
  macOS `/bin/bash` 3.2 silently skipped: bash 3.2 does not apply errexit
  to a failing `[[ ]]` statement, so the launcher, binary and descriptor
  presence checks, config file modes, the `0600` agent socket, PID changes
  across a launchd restart, LaunchAgent removal on uninstall, and the
  rollback state marker could all fail and still record a passing stage.
  Each now fails its stage with a message naming the check. The
  packaging verifier rejects an assertion that ends in `[[ ]]`, `(( ))`
  or `!` without an explicit check, `set +e` outside the exit cleanup,
  and any `run_stage` call that is not a top-level statement, and shows
  that a failing stage body records no pass.

- The macOS Homebrew lifecycle harness's M1 exact-row stage parses the
  agent's platform admission line again. The line gained `posture` and
  `evidence` fields between the reason and the memory ceiling, and the
  stage's pattern stopped matching it, so the stage would have failed on
  any current agent with "no platform admission decision". The packaging
  verifier renders the line from the agent's own format string and
  requires the harness pattern to recover the row, reason and ceiling.

- The macOS Homebrew lifecycle harness's launchd crash-loop stage
  requires a config error written after it broke the agent config. It
  previously searched the whole append-only `agent.error.log`, so a
  config error left by any earlier run on the same Mac satisfied it
  even if the agent never re-read the broken config.

- The lifecycle report converter reports `fail` when a harness stage the
  runbook mapping does not name failed or recorded an invalid status.
  Unmapped skipped stages do not fail the report. It previously built the
  outcome from mapped stages alone, so once the macOS mapping named all eight
  canonical stages, a run whose last stage, the tap-restored check,
  failed after rollback converted to a `pass` report the release gate
  accepts.

- The release evidence gate captures checker exit codes under GitHub
  Actions' `bash -e` shell. Incomplete evidence permits candidate
  publication and build-only validation; final publication still requires
  complete evidence, and checker execution failures block every mode.
  Python failures, including malformed registry or schema JSON, are
  classified as internal faults instead of incomplete evidence.
- Cloud lifecycle validation requires journal entries from each service's
  current invocation, and verifies serving health and an inference echo
  after restarting. Empty or stale journal captures and a deployment
  recorded in status but unable to serve no longer pass those stages.

- The cloud validation runbook requires PyTorch in the packaged backend
  interpreter, `/usr/bin/python3`; selecting a virtualenv only through
  the sidecar environment variable does not satisfy preflight or doctor.

- Installer hardware validation verifies that the NVIDIA driver query
  succeeds when driver metadata is unavailable. An installed but unusable
  `nvidia-smi` now warns and fails under `--strict-hardware`.

- Clean macOS lifecycle installs remove the full TensorPlate formula
  graph so same-version components cannot retain an older source pin.
  Jetson and shared lifecycle retries clear the previous artifact digest
  before recording new evidence, preventing failed attempts from
  inheriting a digest from an earlier run.

- Both lifecycle report producers refuse a `TP_LIFECYCLE_SOURCE_REVISION`
  that is not a full 40-character git SHA, rather than writing it into a
  report that fails schema validation at the release gate.

- Doctor distinguishes an unavailable accelerator identity from absent
  hardware when NVIDIA PCI evidence is present, directing operators to
  the platform finding for the driver or runtime diagnosis.

- Human-readable status distinguishes a live unpinned worker from an
  absent worker and shows pending pin removal separately from stopping
  the deployment.

- Changing, adding, or removing a device pin for a running deployment
  stops the old worker before launching its replacement with the latest
  requested pin. Repeating an unchanged pin preserves the running worker.

- Multi-device memory ceilings use the smallest known device capacity,
  capped by the row budget, regardless of device order. Missing readings
  preserve known bounds; when no device reports capacity, the row budget
  remains the fallback. Device 0's exact evidence stays separate.

- Mixed accelerator SKUs are refused regardless of device order. Hosts
  with supported silicon but an unsupported count retain topology
  diagnostics outside the validated machine shape, and zero-count rows
  fail validation.

### Added

- The release workflow now blocks on evidence (V021-E05-F02-T02). A gate
  job runs the completeness check before anything is built, and every
  build and publish job sits behind it — a release cannot ship a
  Production claim whose evidence does not exist.

  It gates before the builds rather than after, because the arm64
  packages are built on the fleet's single Jetson and that build takes
  roughly ninety minutes. Failing afterwards would waste the one runner
  and tell nobody anything sooner.

  Reported on every run, enforced only when publishing a final release.
  `develop` may carry rows nobody has validated yet, and a build-only
  dispatch is how the pipeline itself gets exercised, so blocking that
  would leave the gate untestable except by attempting a real release. A
  release candidate is not blocked either: the evidence is collected on
  candidate artifacts, so a gate that refused candidates could never be
  satisfied. A checker that fails to reach a verdict is refused on every
  tag.

  A gate nothing depends on is an optional step wearing a gate's name,
  and that failure is silent: the workflow still runs green while
  shipping unevidenced claims. The release checks now walk the job graph
  and fail if any build or publish job can reach the artifacts without
  passing the gate first.

### Added

- A machine-checkable evidence-completeness check for Production rows
  (V021-E05-F02-T01). For each row claiming Production it verifies three
  things: the row's provenance is `recorded` rather than spec-authored,
  its declared evidence directory holds a lifecycle report for that row,
  and every one of the eight stages passed.

  The guard this replaces asserted that a row *declared* an evidence
  location whose string contained the row id. It passed happily for a
  directory nobody had created — which is how four Production rows
  reached a release branch with `provenance: spec_authored` and empty
  evidence directories. A check that cannot fail is not a check.

  Completeness is defined in terms of artifacts that already exist rather
  than a new manifest format, and the report is per row and names what is
  missing: "evidence incomplete" without a subject is not actionable. It
  reports every Production row as incomplete today, which is the true
  answer until the hardware runs happen.

  A report filed under a different row than the one it is evidence for is
  a failure rather than a pass, and a registry with no Production rows at
  all is an error rather than a vacuous success — both are ways a
  completeness check can look like it worked while checking nothing.

### Added

- Physical-row validation runbooks for the in-lab Jetson Orin Nano and the
  M1 Pro, and a converter that gives their harnesses the same lifecycle
  report the cloud rows will produce (V021-E05-F01-T03).

  Both harnesses predate the report and record their own stage names, and
  both run only on hardware CI cannot reach. Rewriting their stage calls
  would mean editing, untested, the code whose entire purpose is to be
  trustworthy on a machine nobody can reach — so the stage log they
  already write is converted instead. The mapping from harness stage to
  canonical stage is an assertion, and a wrong one lies in the gate's
  favour, so it is stated per run rather than guessed.

  A canonical stage with no mapped source is emitted as `skipped` with
  that stated, and a mapped stage absent from the log says it did not
  run: two different problems with two different fixes, where a shorter
  report would have shown neither. Timestamps are omitted for a stage
  that did not run rather than filled in, since inventing one makes an
  omission look like a very fast pass.

  The Jetson identity check confirms the BSP generation the row names
  (`L4T r36.x`) rather than a single revision — a device on r36.4.3 and
  one on r36.5.0 both satisfy the row, and a runbook insisting on one
  would refuse the other on identical hardware.

### Added

- A shared lifecycle-stage runner and a schema for what it writes
  (V021-E05-F01-T01). The eight stages — install, upgrade, deploy-smoke,
  status-logs, rollback, restart, crash-loop, offline — now produce one
  machine-readable report per row, with each stage's log attached whether
  it passed or failed.

  The macOS harness already had this shape and the Jetson one did not, so
  the two produced different evidence for the same stages and only one of
  them could be read by a machine. The release gate has to decide
  completeness by reading the report rather than reading prose, so both
  now emit the same one.

  The failure discipline is the part worth keeping from the original: a
  stage that fails must APPEAR in the report as a failure. Under `set -e`
  the runner never reaches its own bookkeeping, so the record is written
  from an EXIT trap — a harness that merely stopped writing would produce
  a short report indistinguishable from a short run. A skipped stage
  requires a reason for the same reason.

  The schema and the shell script name the same eight stages, and a test
  reads both and fails if they drift.

### Fixed

- `doctor` now reports service health on macOS (V021-E04-F02-T03).
  Service-state checks were gated on systemd, so on a Mac they reported
  "systemd not present" — which says nothing about whether the agent is
  running there, and that is the question an operator is asking. The
  check now asks whichever supervisor owns the service: `systemctl` on
  Linux, `brew services` on macOS, where the agent is a Homebrew-managed
  launchd job. A host without the supervisor is skipped rather than
  failed, since the CLI runs on machines that never installed the
  services.

  The listing is matched on the exact service name. `tensorplate-agent`
  is a prefix of nothing today, and relying on that staying true is how a
  future sibling service would silently answer for the agent. Both
  services are read from one `brew services list` snapshot. A failed
  supervisor query is reported as unavailable, with bounded diagnostics,
  rather than being mistaken for two services that are not installed.
  Homebrew's CLI config and platform registry also count as install
  footprints, so these checks run for a component install even when the
  optional Python backend is absent.

### Deferred

- Live thermal, power, throttle, and GPU-utilization collectors, together
  with their hardware evidence samples, remain part of the tracked
  hardware-validation work. This change lands the row-owned policy,
  fail-closed snapshot resolver, admission gate, and status projection;
  it does not manufacture live outcomes on machines where no collector is
  wired. Startup memory facts are the exception because they already come
  from the platform report used for row resolution.

### Added

- Gate-semantic handling for the platform signals a row declares
  (V021-E04-F02-T02), and with it the only producer of
  `telemetry_degraded`. A row states, per signal, whether it gates
  behaviour, is reported for context, or is absent here. When a caller
  supplies a collector snapshot, the resolver applies that declaration
  to every stable signal name; an omitted applicable result becomes an
  explicit unavailable outcome rather than silently passing.

  The three postures are genuinely different and every pair is plausible
  to collapse. A thermal sensor that fails on a Jetson is a machine that
  cannot be trusted to throttle itself, and it degrades deployment. The
  same failure on a datacenter row is a missing number on a chassis whose
  cooling is somebody else's problem: it is recorded, and it does not
  block — refusing there would make a context signal load-bearing by the
  back door, which is the row's decision and not this code's. A power
  reading macOS does not expose without privileges was never going to be
  there, so it is not asked for and cannot fail.

  An absent signal carries the row's own free-text explanation rather
  than a typed platform reason. Those say why a *platform* is
  unsupported; a sensor an OS does not expose is not a support claim
  about the machine.

### Changed

- Platform telemetry has an additive agent-status projection, also
  surfaced by `tensorplate status` so hardware validation can record the
  same JSON as evidence. It is not placed on the metric stream: the frozen
  metric event cannot express these signals — `MetricUnit` has no celsius
  or watts and the allowed label keys carry no row identity. The decision
  is recorded rather than assumed, and is reversible.

  The optional, output-only status field remains on the local control
  protocol's pre-1.0 `0.1` contract, matching the existing supervision and
  serving-URL additions: old serde readers ignore it and new readers
  default its absence. The constrained exception is now explicit in the
  protocol/versioning documentation; request fields and semantic changes
  still require a version bump.

### Added

- Per-row memory telemetry reports what a machine has alongside its row's
  nominal capacity, in the row's own memory model (V021-E04-F02-T01). Host memory
  is now captured as an exact fact (`/proc/meminfo` on Linux,
  `hw.memsize` on macOS), and the telemetry pairs it with the accelerator
  figure the row-appropriate source reports.

  The distinction it exists to carry is unified versus discrete. A
  discrete GPU has two pools and the framebuffer is the one a model loads
  into; a unified-memory platform has ONE pool the host and accelerator
  both draw from. Reporting a Jetson's 8 GiB as if it were a discrete
  framebuffer would tell an operator they have all of it for a model
  while the OS is living in it. The telemetry says which model applies,
  so a caller cannot sum two halves of one pool.

  Nominal row capacity is not a minimum usable-memory threshold. Driver
  and firmware reservations make canonical L4 and Jetson observations
  smaller than their marketed capacities, so the observed value is a
  usable ceiling bounded by the row's nominal claim. An unreadable figure
  remains absent rather than becoming a fabricated shortfall. A future
  load-bearing minimum must be a separately validated row value, not a
  comparison between these unlike quantities.

### Added

- `doctor` now explains which model classes the matched platform row
  serves, and at what level (V021-E04-F01-T03). The Jetson row shows
  `chunked_policy` at Production, the G4 row shows all three VLA shape
  rows, and the deploy-smoke rows show Preview — read from each row's
  `model_class_rows` registry pointers rather than a list kept in the CLI,
  so a row that gains or loses a model class changes the output without
  code.

  A row that claims none says so plainly rather than rendering an empty
  list: a Planned row carries no model-class claims and the registry
  refuses to let it, which is an honest row rather than a broken one. The
  finding is skipped when no row matched, since `platform_row` already
  carries the reason and two lines for one fact is worse than one.

### Added

- An unsupported-combination matrix asserts, in one table, that each way a
  machine can miss the support matrix reports the dimension it is actually
  off-matrix in (V021-E04-F01-T04). Twelve cases, each naming a specific
  typed reason rather than merely failing, plus a control that a canonical
  M-series chip still resolves — a matrix that refused everything would
  otherwise look complete.

  Writing it corrected an expectation rather than the code. An L4 on
  Ubuntu 22.04 reports `unsupported_accelerator_sku`, not
  `unsupported_os_version`, and that is right: 22.04 is a supported OS
  with a Preview CPU row, so telling that operator their OS is unsupported
  would send them to reinstall a platform that is fine. What no row covers
  is a GPU on it.

  The coverage half is the point. Two of the ten reasons reached this
  release with no producer at all, and a table is what makes that visible:
  the matrix now derives its covered set from its own cases and fails if
  any reason has none.

### Added

- The typed platform-reason vocabulary is frozen for v0.2.1 and documented
  in `docs/platform/support-reasons.md` (V021-E04-F01-T02): ten values,
  their wire spellings, and the condition that triggers each.

- Backend probe failures now carry a typed reason instead of prose alone.
  `accelerator_runtime_unavailable` gets its first producer on the Rust
  side, and the boundary it exists to keep is now enforced by a single
  classification: an absent descriptor is `missing_backend_package`
  (install something), and every other probe failure is a runtime that is
  installed and unusable. Collapsing them tells an operator whose PyTorch
  cannot reach its accelerator to reinstall a package they already have.
  The reason reaches the wire through the same error-record context the
  admission rejections already use.

- The reason vocabulary is now checked as a cross-language contract. The
  Python sidecar emits its own reason strings — an unavailable MPS runtime
  is the one that exists today — and a test reads the sidecar source and
  asserts every constant is a spelling this enum owns. A rename on either
  side that the other does not follow would otherwise surface as an
  unrecognized string on a machine rather than as a failing test.

### Added

- `doctor` now names the support row a machine **is**, not only the rows it
  could be (V021-E04-F01-T01). The new `platform_row` finding resolves the
  detected host *and* accelerator against the registry — the answer
  `platform_profile`'s hint has always deferred to with "accelerator
  identity is needed to name one". `doctor` observes the accelerator the
  way the agent's startup does (host sources for Apple and Jetson, the
  `nvidia-smi` probe for a discrete card), so it cannot resolve a different
  row than deploy admission will.

  The two findings stay separate on purpose: an operator whose accelerator
  probe fails still gets the host-level answer, and the pair says which
  half of the identity was the problem. A Planned row resolves as
  `unsupported` naming the row and `row_planned_not_validated` — the
  A100 40GB row reads that way today.

  A GPU host whose driver is missing or broken is refused here rather
  than resolved: it reports no accelerator, so resolving on host identity
  alone would land it on a CPU-only row and tell an operator their broken
  machine is supported. The PCI bus distinguishes the two without a
  driver, checked in the same order deploy admission checks it, so the two
  cannot disagree about that machine.

  This is also where an exact-equality miss becomes visible. A near-miss OS
  version or an off-matrix accelerator SKU resolves to no row with the
  typed reason naming the dimension that missed, where the host-level
  profile alone would have reported a clean match on a host whose card is
  wrong.

### Changed

- The L4 row's fixtures are now recorded, not transcribed
  (V021-E02-F02-T04). Captured by `tensorplate doctor --record` on a
  disposable GCP `g2-standard-8` booted from stock `ubuntu-2404-lts-amd64`
  with the driver installed the documented way (`ubuntu-drivers install
  --gpgpu`, branch 595). The recorded SKU, memory figure, and `[N/A]` MIG
  spelling agreed with the transcription byte-for-byte, so the row needed
  no correction — the fixture now proves it rather than assumes it. The
  invented driver version is corrected from observation; the observed GPU
  UUID is replaced with a clearly synthetic value in the public fixture. A
  second recording from the Ubuntu 24.04 Deep Learning VM image (driver
  580.173.02) agreed on every silicon fact and is committed as the row's
  second covered boot path (`dlvm-ubuntu2404-l4-g2s8` host and accelerator
  fixtures), harness-asserted like the lab Jetson's extra recording.

### Added

- `tensorplate doctor --record <dir>` captures this machine's raw platform
  sources as private evidence in fixture-compatible shapes
  (V021-E02-F02-T04). The command warns that raw output can contain live
  cloud identifiers, device UUIDs, or serials and must not be committed
  directly. A reviewed, sanitized publication copy uses the exact shapes
  consumed by `test/platform/host_identity/` and
  `test/platform/accelerator/`, with `provenance: recorded` — so first-run
  recording sessions on real hardware become one command per box instead
  of a hand-assembled capture without putting raw identifiers in git.

  Record-first, deliberately: raw text is written even when detection
  cannot interpret it, because the machines worth recording are exactly the
  ones detection cannot interpret yet — a multi-GPU host, an unknown SKU, a
  new OS image. Interpretation failures become notes in the output, never
  aborts. When the machine resolves to a row, the files are named for it
  and the observed SKU is compared byte-for-byte against the row's declared
  one, replacing the manual `od -c` discipline the hardware runbook
  prescribes; a mismatch is called out as a row correction, never an
  evidence exception. The accelerator identity is derived the way
  production derives it — the device tree names a Jetson's GPU when
  `nvidia-smi` has nothing to say.

### Changed

- The A100 40GB row (`ubuntu2404-x86-a100-40g-a2hg1`) is now **Planned**,
  not Production (V021-E02-F02-T04). GCP refused the project's A100 quota on
  2026-08-23, so the row cannot earn evidence this release, and a Production
  claim on a match key nobody has observed is exactly what the pre-tag gate
  exists to refuse. The row keeps its identity, fixtures and partitioning
  checks, and drops the `chunked_policy` Preview pointer, since a Planned
  row claims no model-class posture. Quota has been re-requested; promotion
  back is one field plus evidence.

  The MIG fail-closed tests stay on the A100 fixture: partitioning is
  checked before support level, so a MIG-enabled A100 is still refused with
  `mig_mode_enabled` and an unpartitioned one is refused as Planned — which
  is the control the check needs. The Production datacenter exemplar in the
  admission and server-reach tests moves to the L4 row. Live MIG observation
  moves to the RTX PRO 6000 Blackwell Server Edition, which supports up to
  four MIG instances and whose row already records `partitioning:
  unsupported`.

### Fixed

- A Jetson row now matches the module it names across the JetPack 6.2 line
  rather than one L4T revision (V021-E02-F02-T04). A row recorded
  `L4T r36.4.x` and matching is exact string equality, so an Orin Nano on
  r36.5 matched no row at all and deploy admission refused it — while the
  same board on r36.4 was supported. Which revision NVIDIA happens to be
  shipping is not a property of the hardware anyone bought.

  Rows now name the BSP generation (`L4T r36.x`) and the JetPack feature
  release (`6.2`), the same reduction macOS already made in recording `26`
  for a machine reporting `26.5.2`. An Orin Nano on JetPack 6.2, 6.2.1 or
  6.2.3 resolves to the same row. The exact values are not discarded:
  `ExactHostFacts` carries `r36.4.3` or `r36.5.0` for evidence, which needs
  the precision matching deliberately drops.

  Detection still refuses to guess. An L4T generation it has not been told
  about — r38, say — produces no JetPack version and matches nothing,
  rather than borrowing a release it was never validated against.

  The L4T-to-JetPack fallback, which the in-lab BSP-flashed device needs
  because it carries no `nvidia-jetpack` package to read a version from,
  now enumerates exactly the revisions NVIDIA's archive names as JetPack
  6.2.x: 6.2 is L4T 36.4.3, 6.2.1 is 36.4.4, 6.2.2 is 36.5.0, and 6.2.3 is
  36.5.2. All four resolve to the `6.2` feature release and therefore to
  one row, which is the point of the generalisation.

  It is keyed on the full revision rather than the r36.4/r36.5 line
  because those lines are not wholly 6.2 — base L4T 36.4 is JetPack 6.1.
  Answering for the line would have handed a 6.1 board the version that
  admits it to a 6.2 **Production** row, on evidence that never covered it
  and with nothing downstream able to tell the difference, since the board
  is otherwise identical. A revision the archive does not name yields no
  JetPack version and matches nothing. Only the fallback is enumerated: a
  device carrying the metapackage reads its own version, so a future 6.2.x
  on a new revision still resolves for every normally-flashed device.

  The package version is also parsed correctly for that release. The build
  suffix is `-` on 6.2 (`6.2-b77`) and `+` on 6.2.3 (`6.2.3+b81`); only the
  first was handled, so a device with the metapackage installed carried its
  suffix into the comparison and matched nothing.

  Startup admission agrees with that match. The row also recorded an `l4t`
  entry in `kernel_driver_stack`, which `PlatformAdmission` compares
  against the stack the agent observes — and `observe_platform` reports no
  stack components, so the row was resolved and then rejected with
  `MissingDriverRuntime` on the very device it describes. The entry
  duplicated `image_identity`, which already carries the L4T line and is
  what matching keys on, at a granularity the generalisation had moved; it
  is dropped rather than restated, and a regression drives the recorded lab
  fixture through detection into admission with the empty stack a real
  startup supplies.

- The agent config schema now describes the config the agent actually
  accepts. `config/schemas/agent.json` declared a nested `control` object
  holding `transport`, `socket_path`, `tcp_bind_host` and `tcp_bind_port`,
  while the runtime reads all four at the top level — and with
  `additionalProperties: false`, that meant **the shipped
  `packaging/conf/agent.json` did not validate against its own schema**.
  `docs/architecture/agent.md` names that schema as the wire format for the
  config, so an operator writing one from it produced a file the agent
  would not accept, and an operator copying the shipped file failed
  validation against the documented shape.

  Corrected toward the runtime rather than the other way round: no config
  has ever carried a `control` object, on any host or in any commit, so the
  schema described a shape that never existed. Moving the runtime instead
  would have invalidated every config file already installed. `runtime_version`,
  which the agent fills in at load, is now declared too.

  **Operators upgrading should check their config first.** The runtime now
  refuses unknown fields, which the schema had claimed all along with
  `additionalProperties: false`. A hand-edited `/etc/tensorplate/agent.json`
  carrying a stray or misspelled key started before this change and will not
  start after it. The upgrade preflight checks `schema_version` only, so it
  does not catch this before the package swap, so `tensorplate doctor` gained
  an `agent_config_valid` check that validates the installed config against
  the schema and lists every problem at once. Run it before upgrading.
  Refusing is the point: the alternative is what this replaces. Before this, `worker: {"mod":
  "process"}` — one character off `mode` — parsed cleanly and left
  `mode = Mock`, so an operator asking for the real serving binary got the
  in-process mock and served nothing real, with no error at any point.

  The two also agree on values, not only on which fields exist. The runtime
  took any string for `supported_precision` and `supported_artifact_kinds`
  and any name in `available_backends`, while the schema constrained all
  three — so a config no validator would pass could run in production. These
  are refused at load now too, with the same upgrade caveat. Those
  are now checked against the protocol's own `PrecisionHint` and
  `ArtifactKind`, rather than a second copy of the allowed values.

  The schema also now states what the runtime actually requires. It
  accepted `{"schema_version": "0.1"}`, which the agent then refused for
  want of `state_dir` and `staging_dir` — so a config written from the
  published schema could fail at startup. `state_dir` and `staging_dir` are
  required, `socket_path` is required whenever the transport is (or
  defaults to) a Unix socket, and the three path fields must be absolute,
  as the agent has always insisted. An audit of every runtime-validation
  branch closed the rest: whitespace-only backend names, non-loopback hosts
  and relative paths in the process worker and the supervisor, and a
  relative event-sink socket.

  Constraints that apply to one branch are now written as conditionals,
  because the runtime reads them that way. A relative `socket_path` under
  TCP, or a wide `tcp_bind_host` under a Unix socket, is a value nothing
  consults — the schema rejected such configs while the agent started them
  happily, which is the same disagreement pointing the other way.

  Two rules stay runtime-only and are documented as such: a process
  worker's two ports must differ, and the backoff maximum must not fall
  below the initial delay. Both compare one value against another, which
  JSON Schema draft-07 cannot express, so a test asserts the runtime
  refuses them and flags them for promotion if that ever changes.

  Guarded by a bidirectional contract test rather than a field-by-field
  one. Every field a fully-populated config serializes must be declared by
  the schema, and every property the schema declares must be one the
  runtime accepts — the second direction is what catches an invented field
  like `control`, which a one-way check would have passed. Both shipped
  configs, Debian and Homebrew, are validated with the real JSON Schema
  compiler rather than by comparing top-level key names — a nested
  property, a wrong type, a bad enum value or an out-of-range number all
  pass a membership check while failing the schema. Restoring the previous
  schema fails three of these tests.

- The APT channel lifecycle rehearsal no longer hangs for its whole budget
  when a package mirror stalls. Bounding every apt call in the workflows
  left the calls made *inside* the rehearsal script unbounded, and the
  sources list there still carries the Ubuntu archives — so `apt-get
  update` reaches real mirrors and can stall on one that accepts the
  connection and then trickles bytes, which no `Acquire` timeout catches.
  Observed as a 30-minute cancellation on the amd64 job while the arm64
  job ran the same script in under two minutes.

  Not every call is treated the same way, because retrying is not always
  the right answer:

  - The three network-facing calls are bounded and retried.
  - The stock-state negative — which asserts that a host with no channel
    knowledge *cannot* resolve the package — is bounded but never retried.
    A retry would repeat a failure the test asserts, and the retry path's
    `dpkg --configure -a` would mutate the very stock host under test.
  - `tensorplate-ready-check --online` is bounded but never retried:
    whether `apt-get update` succeeds against the configured sources is
    what it reports, so retrying until it works would hide the flakiness
    it exists to surface. A stall is now distinguished from a failure in
    its output rather than reported as the same thing.
  - The two `apt-get remove` calls are left alone. They are local dpkg
    work with no mirror to stall on, and retrying a partially applied
    removal risks more than the stall it would prevent.

  The shared wrapper also stops assuming it is not root. The rehearsal
  runs as root, and a container with no `sudo` installed is a normal place
  to run it, so hard-coding `sudo` would have made the wrapper unusable
  exactly where the unbounded calls were. It also emits plain text instead
  of GitHub Actions annotation syntax when not running in CI.

### Changed

- Deploy admission on a server or bare-metal machine no row's evidence
  covers now gates on technical prerequisites instead of refusing
  (V021-E02-F03-T03). Bare metal, AWS and Azure hosts carrying silicon a
  row already describes were told `outside_validated_environment` and
  refused, which restricted deployment to three exact GCP shapes.

  Which machines this applies to is not a new policy. It is read off the
  matched row's `gate_semantics`, which every row already declares: a row
  gating thermal, power or throttle as `load_bearing` is one whose cooling
  belongs to the operator, and validation recorded in someone else's
  enclosure says nothing about this one — every Jetson row and the physical
  workstation row are unchanged and still require evidence that covers the
  machine. A row recording those as context is a managed machine, and there
  the missing evidence is about the chassis rather than about whether the
  hardware can serve.

  Such a machine runs, and is reported as `unvalidated` at startup with the
  posture that admitted it and where that came from. It is bounded by the
  same memory ceiling a validated match gets: "not validated" must not read
  as "not bounded", or the unvalidated host would be bounded less than the
  validated one. An operator who wants a uniformly validated fleet can pin
  `admission_posture` to `validated_row_required`, which closes this path
  everywhere; the row's floor can only be raised, never lowered.

  A Planned or Experimental row is never admitted on this path. An exact
  match on one is refused, and a machine reaching the prerequisite path is
  further from the row than an exact match — so admitting it there would
  have made such a row deployable only where its own evidence covers even
  less.

  Prerequisites are checked for PRESENCE, not for the exact versions a row
  records. Those versions are what the evidence run happened to have, not a
  minimum — refusing a machine for carrying a newer driver would refuse
  most of the fleet this path exists to admit. A machine matching a row
  exactly is unaffected and still compared against the recorded versions.

- A GPU host whose driver is missing or broken is now refused instead of
  deploying as a CPU box (V021-E02-F03-T03). `nvidia-smi` needs a working
  driver to answer, so a broken driver and no card at all produced the same
  silence, and the host resolved to a CPU-only row and served on the CPU
  without saying so. The PCI bus distinguishes them and answers without a
  driver. Refused on every posture: this is not a question of how strictly
  the machine is judged, since serving CPU work on a machine bought for its
  accelerator is a silent downgrade under any of them.

  This covers the probe FAILING as well as returning nothing. The usual
  broken driver is an installed `nvidia-smi` exiting non-zero, which is an
  error rather than an absent accelerator, and an error previously replaced
  the host report before the PCI evidence could be read — so the operator
  got an untyped detection failure where the machine could have been told
  its driver is broken.

  Two conditions have to hold before the driver is blamed, and both exist
  to avoid sending an operator after a driver that is working. The probe
  must have been unable to READ the tool — a probe that ran and returned an
  answer this release cannot interpret, such as more than one GPU or an
  unknown partitioning state, means the driver answered fine and the
  topology is what is unsupported. And an NVIDIA function must be on the
  PCI bus, since nothing there evidences no driver problem at all. Anything
  else stays an untyped detection failure, which still refuses the deploy.

  Note the consequence for a deliberately driverless GPU host — a card
  unbound for passthrough, say, on a box meant to serve CPU work. That host
  deployed before and is refused now, and there is no override for it in
  this release. Fixing the silent downgrade was the requirement; an opt-out
  is a follow-up, and `admission_posture` is the natural place for it.

### Fixed

- CI jobs no longer hang for their entire budget when a package mirror
  stalls. A stalled mirror is not a failing one: apt holds an
  open-but-idle connection and waits, so a job burns its whole timeout
  without ever producing an error — and retries do nothing, because there
  is no failure to retry. A response timeout turns the stall into an error
  that retries can then recover.

  apt's own `Acquire` timeouts are set too, but they are not sufficient on
  their own: they fire when a connection goes **idle**, and a mirror
  trickling bytes never trips them. That was observed directly — a job
  stalled for ten minutes with the drop-in accepted and active. So every
  apt invocation is additionally bounded in wall clock, which does not
  depend on why apt is slow.

  The bound converts a stall into a failed step, which is re-runnable;
  it does not retry automatically. That is a deliberate stopping point
  rather than the ideal: three attempts at an automatic-retry wrapper
  introduced bugs of their own, and a change to every workflow is not the
  place to be clever.

  The C++ workflow also gains per-job timeouts. It was the only workflow
  with none, so its jobs inherited the six-hour default: one stalled apt
  step ran 68 minutes before anyone looked, where the same stall in a
  workflow with budgets failed in 15.

### Added

- Admission posture: how strictly a machine is judged before a deploy is
  admitted is now an explicit, reported value rather than one policy applied
  everywhere.

  The strictness floor is **derived from each row's own gate semantics**,
  not stored. A row that acts on thermal, power or throttle as
  `load_bearing` is one whose cooling belongs to the operator — a Jetson in
  a product, a workstation under a desk — and its evidence does not transfer
  to a chassis nobody characterised. A row that merely reports those signals
  is a managed machine. Row authors already made that judgement, per row;
  this reads their decision rather than adding one, which is why the same
  accelerator lands differently in two chassis: the RTX PRO 6000 Workstation
  row gates on temperature and its Server Edition sibling does not.

  An operator may set a posture, and it can only make admission stricter:
  the value in force is the maximum of theirs and the row's floor, so an
  edge row's requirement is a property of the hardware rather than a default
  that can be switched off. An unrecognised posture is a startup error, not
  a silent fallback — a value a future release adds must not be quietly
  ignored by an older agent running at a strictness nobody chose. The
  option is declared in `config/schemas/agent.json`, which sets
  `additionalProperties: false` — without that, schema-aware validators
  would reject the override while the agent accepted it.

  **Nothing consults the posture yet and behaviour is unchanged.** It is
  reported, with its provenance, so an operator can see which strictness
  they are running at and whether it came from the row or from their own
  configuration. That is what lets them pin it, and what keeps a future
  change to the default from being a silent behaviour change on upgrade.

- The PCI bus is now read, so an accelerator that is physically present can
  be told from one that is absent. `nvidia-smi` needs a working driver to
  answer, which makes a card with a missing or broken driver
  indistinguishable from no card at all — and such a host resolves to the
  CPU-only row and deploys as though it had no accelerator, silently.

  Nothing consumes the reading yet and matching is unchanged: it is
  recorded alongside the other exact facts, which matching never reads.
  Distinguishing the two cases needs a decision about what to do with the
  answer, and that decision belongs with the admission work rather than
  with the observation.

  Vendor and device class are both checked. A discrete card commonly
  presents an HDMI audio function on the same board under the same vendor,
  so a vendor-only match would report two accelerators where the machine
  has one.

  A machine with no PCI bus at all — a Mac, a Jetson — reports absence,
  which is a signal rather than a failure, exactly as every other source
  here. A bus that exists and cannot be read is an error, and so is a
  device attribute that exists and cannot be read: collapsing either would
  report unreadable sysfs as a machine with no devices, which is the same
  wrong answer this reading exists to prevent. Only an attribute that has
  genuinely vanished — hot-unplug between listing and reading — is skipped.

### Fixed

- A supported discrete NVIDIA GPU whose framebuffer size cannot be read now
  retains the validated platform row's memory budget as its deploy ceiling.
  Missing framebuffer evidence no longer becomes a zero-byte limit that
  rejects every bundle with a positive memory estimate, while the row budget
  continues to prevent admission from becoming unbounded. (V021-E02-F02-T01)

- Homebrew now confines the agent control surface to the installing user: the
  runtime directory is `0700` and the Unix-domain socket is `0600`. Native
  Linux packages retain their dedicated `tensorplate`-group authorization
  boundary and `0660` socket. The architecture and lifecycle checks now pin
  both package-specific trust models. (V021-E03-F01-T03)

- A detection failure no longer leaves a machine ungated. Both platform
  lanes wired deploy admission independently, and they disagreed on this
  point: one recorded a failure to read the hardware as a rejection, the
  other treated it as "no verdict" and let deployment proceed with no
  platform gate at all — on exactly the hardware nobody has characterised.

  The rejection is the right answer, and it does not require the false
  claim the other design was avoiding. A rejection whose reason is absent
  says "detection failed", not "your platform is unsupported"; only the
  gate closes. Settling the verdict now returns a verdict rather than an
  option, so there is no path on which the agent runs ungated.

- Per-deployment platform admission now checks the matched row's backend
  package requirements after bundle verification and before staging. Package
  inventory comes from Homebrew formulas on macOS and the Debian package
  database on Linux, preserving the same fail-closed gate on both lanes.

- Deployment identifiers are now constrained to one bounded,
  filesystem-safe path segment at protocol, agent, and CLI boundaries before
  the agent creates transaction state or derives a staging path. Local and
  device-routed deploys share the same policy. Import pruning continues to
  recognize longer filesystem-safe names created by older clients, so stale
  legacy imports remain reclaimable.

### Added

- The macOS lifecycle deploy gate now uses a verified bundle artifact to
  select an MPS-backed sidecar fixture whose load performs and synchronizes a
  real PyTorch tensor operation on the MPS device. The gate rejects any other
  backend profile, requires an active deployment with ready CLI state and a
  serving endpoint whose health names the expected deployment, uses a unique
  deployment identifier per rehearsal, checks supervisor health when
  configured, and emits an allowlisted per-stage transcript without
  operator paths or environment values. macOS install guidance now states
  the exact supported row and removes the entire six-formula graph before
  untapping. (V021-E03-F01-T04, V021-E03-F02-T03)

- A guarded Apple M1 Pro Homebrew lifecycle harness now records redacted,
  stage-by-stage evidence for clean install, packaged-only deploy smoke,
  launchd restart and crash-loop behavior, network-denied runtime checks,
  CLI-only formula upgrade continuity, rollback, and uninstall. The
  accompanying runbook defines immutable RC/commit source pins and recovery
  steps while preserving the operator's prior CLI-only installation. The
  packaged CLI also supplies the formula-managed backend module path so
  doctor probes the same Python/PyTorch runtime as the agent. The current-head
  gate also captures the packaged agent's admission decision and requires the
  M1 Pro exact Production row to win over the M-series Preview fallback while
  preserving both rows' 16 GiB ceiling.
  (V021-E03-F01-T04)

- Homebrew now links the installed platform registry and Python/PyTorch
  backend descriptor into its shared prefix, points the descriptor at the
  formula-managed PyTorch interpreter, and passes the corresponding discovery
  and Python paths through the agent's launchd service. The agent, CLI, and
  observability process resolve absolute descriptor and registry directory
  overrides while retaining the native-package defaults. Python 3.14 joins
  backend CI and descriptor support to match the current Homebrew PyTorch
  runtime. (V021-E03-F01-T01, V021-E03-F02-T03)

- The Python/PyTorch sidecar now probes the configured Apple accelerator
  runtime before SmolVLA model dependencies or weights are loaded. Load and
  health responses publish a vendor-neutral runtime capability record with
  framework and operating-system runtime versions, build state, and current
  availability. An unavailable runtime rejects the load with the typed
  `accelerator_runtime_unavailable` platform reason, while the shared
  `tp::Error::Code` remains `unsupported`. The backend descriptor now
  declares the supported Apple device target. (V021-E03-F02-T03)

- Apple silicon detection now reads the exact chip identity and unified-memory
  size from `sysctl`, resolves exact rows before a narrowly recognized Apple
  M-series Preview compatibility row, and publishes a vendor-neutral
  `PlatformCapability`. Its
  `max_resident_model_memory` is the lesser of detected memory and the row
  budget. Agent admission applies that ceiling before bundle capacity checks
  and rejects Planned rows, unsupported chips, unsupported macOS versions, or
  failed detection before staging and model load. The M1 Pro 16 GB exact row
  remains the current evidence-backed hardware-validation target; M2 Pro, M3
  Max, and M4 Pro fixtures exercise family compatibility without asserting
  per-SKU hardware validation, and a non-M-series Apple fixture proves the
  boundary fails closed. Family rows are constrained to Preview,
  `spec_authored`, and evidence-free so validation evidence can attach only to
  exact hardware rows.
  (V021-E03-F02-T01, V021-E03-F02-T02)

- Homebrew now installs prefix-rendered agent, CLI, and observability configs,
  secures their config/state/runtime/log paths during post-install, connects
  the packaged CLI to the local agent UDS, and routes launchd output plus
  structured diagnostics to documented log files. The post-install checks
  reject symlinked managed paths and fail with the affected path when a
  required mode cannot be enforced. (V021-E03-F01-T03)

- Homebrew installs launchd service definitions for the agent and
  observability processes. Both start when loaded, restart after unsuccessful
  exits with launchd throttling, and remain independent; the serving worker
  deliberately has no launchd job because the agent remains its sole process
  owner. (V021-E03-F01-T02)

- Homebrew packaging templates now cover the complete macOS appliance:
  agent, serving worker, CLI, observability, and the Python/PyTorch backend,
  with `tensorplate` retained as the meta-formula so existing installs have a
  continuous upgrade name. Release automation renders the entire graph from
  one tagged source archive and checksum and submits it as one tap change,
  preventing component versions from drifting. The CLI component explicitly
  accepts the command path owned by the former CLI-only formula; the tap
  migration will exercise that handoff before release. Service definitions,
  macOS paths, and hardware smoke remain owned by the following macOS
  lifecycle changes. (V021-E03-F01-T01)
- A release-prep check that a Production support claim rests on a recorded
  run. The existing guard asserts only that a Production row *declares* an
  evidence location containing its row id — not that the directory exists,
  nor that anything was ever recorded. Every committed Production row
  satisfies it while resting on a SKU string transcribed from a datasheet.

  The new check is ignored by default and fails when run, which is its
  point: it is a tag prerequisite in the release runbook rather than a
  PR-blocking test, because a row may legitimately sit at Production with
  spec-authored values while its evidence run is scheduled. What must not
  happen is shipping one. The failure names each offending row.

  Either resolution is honest: record the evidence, or downgrade the row
  until it exists. Downgrading is cheap — a Preview row is still a supported
  combination and still deploys, so it changes the published claim rather
  than what runs.

### Fixed

- A change to platform detection now triggers the job that tests platform
  detection. The APT lifecycle workflow is path-filtered to packaging and
  release paths, but the Ubuntu CPU-only smoke it runs is the only job that
  resolves a support row by **live detection against real installed
  binaries** — and `platform/**` was not in the filter. So the one check
  that would catch a detection regression on a real install could not be
  triggered by a detection change, and fired only when a pull request
  happened to touch packaging as well.


- A Jetson is no longer refused every deploy. `nvidia-smi` is the only
  accelerator probe and JetPack does not ship it, so detection reported no
  accelerator — which was read as the affirmative fact "this machine has no
  accelerator" and mismatched every row that declares one. Every Jetson
  resolved to no row at all, and deploy admission refused it before the
  bundle was opened, on hardware that had been working.

  A Jetson's accelerator is part of the SoC: no separate device exists to
  enumerate, so the absence of the vendor tool is not evidence of absent
  hardware. Its identity is instead derived from what the board reports
  about itself — `/proc/device-tree/model` names the module, and `MemTotal`
  separates two modules of one family that report the same model string.

  The derived SKU is then compared verbatim, exactly as a discrete card's
  is. That is what makes the derivation safe in the direction that matters:
  a SKU derived wrongly matches no row and the machine is refused, rather
  than being handed a row belonging to a different board. An Orin NX 8GB
  produces `Jetson Orin NX 8GB`, which no row names, and is reported
  unsupported.

  A board this cannot name is refused, not left ungated. It yields an
  identity carrying what the board actually reported, which no row names,
  so the machine gets `unsupported_accelerator_sku` — the same answer an
  off-matrix discrete card gets. Erroring instead would be a fail-open: the
  agent reads a probe error as "hardware unreadable, admission disabled", so
  an unrecognized Jetson would go from refused to not gated at all, on
  exactly the hardware the gate exists for.

  Deriving an identity for a Jetson cannot fail at all, and that is the
  whole safety argument rather than a judgement about which inputs are bad
  enough to error on. The agent reads any probe error as "hardware
  unreadable, admission disabled", so any error on this path would take a
  Jetson from refused to not gated. A genuinely unreadable file cannot
  reach this code in any case: the probe maps one to an `Unreadable` error
  and propagates it before these sources are assembled, so an absent source
  here is a signal rather than a failure. (V021-E02-F02-T02)

### Added

- A deploy is now refused before anything is staged when the machine
  itself cannot honour it. A partitioned accelerator is the case this
  exists for: the card is supported and the host is supported, and serving
  it anyway would serve at a capacity the matched row's evidence was never
  collected at. `mig_mode_enabled` is reported, and the same card
  unpartitioned is admitted — the control that keeps the check from being a
  blanket refusal.

  Requirements come from the matched row, never from a constant in the
  agent. Driver and runtime components are compared against what the row
  records, and the packages a backend path needs are the ones that row
  declares for that path. Rows record no components until their first
  evidence run, so the stack check is deliberately silent today rather than
  inventing a requirement — an empty list means "not yet recorded", not
  "nothing required".

  A backend path a row never declared is refused rather than waved through.
  An absent package set is not a row with nothing to require; it is a row
  that never claimed to serve that path, which is exactly the deploy that
  has no evidence behind it.

  The two halves are asked at different times because they have different
  inputs: whether this machine matches a row at all is settled once at
  startup, while which packages matter depends on the backend the bundle
  names. Detection failing is **not** a rejection — an agent that cannot
  read its own hardware has no basis to refuse a deploy, and turning "I
  could not look" into "your platform is unsupported" is the collapse this
  codebase refuses everywhere else.

  Two rejections deliberately carry no typed reason, for the same cause.
  A machine whose machine shape no row's evidence covers, and a machine
  matching an Experimental row, have no value in the frozen vocabulary.
  The nearest candidates each name a dimension that is fine: an operator
  told their OS version is unsupported, when their OS is correct and their
  chassis is not, reinstalls the wrong thing — and one told a row is
  "awaiting validation" waits for an evidence run that is never coming,
  because an Experimental integration is not awaiting one.

  Where a reason does apply it reaches the caller, not just the log line.
  The typed reason is projected into the error record's context, which the
  CLI already renders and the durable store already keeps, so
  `missing_driver_runtime` and `missing_backend_package` are readable by a
  machine rather than only legible in prose.
  (V021-E02-F02-T02, V021-E02-F02-T03)

- Discrete NVIDIA accelerators are now detected by exact SKU, so a card
  either resolves to the row whose evidence was collected on it or is
  reported unsupported — never to the nearest thing. An A100 80GB is one
  capacity away from a supported A100 and must not inherit its claim.

  Read from `nvidia-smi` rather than NVML. NVML means linking a vendor SDK
  into a crate the agent, CLI, and observability all depend on, and the
  value needed is a string the tool already prints; the command boundary
  is also what keeps vendor types out of the public contract. The product
  name is carried through **verbatim**, because a row records exactly what
  the tool prints and any normalization would be a second spelling of the
  same fact for the two to drift apart on.

  Memory is recorded but never matched on. A row records nominal capacity
  and the tool reports the usable framebuffer, and for an L4 those are
  different numbers — matching on it would make a supported card miss its
  own row. The reported framebuffer, driver version, device UUID, and MIG
  mode are kept alongside identity for evidence recording, the same split
  host detection already uses.

  Absence and failure stay distinct, as everywhere else in detection. No
  `nvidia-smi` means no discrete accelerator, which is how the CPU-only
  rows are told apart from the GPU ones. A tool that is present and fails
  is an error: a host whose driver will not load is a broken GPU machine,
  and reporting it as a machine without a GPU would resolve it to a
  CPU-only row and tell an operator with a broken driver that their
  platform is fine. More than one accelerator is refused rather than
  narrowed to the first, because every row this release claims is
  single-GPU.

  Fixtures cover every row naming an NVIDIA card, three off-matrix SKUs,
  and a partitioned device, and they are checked by resolving against the
  real registry rather than against hand-written expectations. **They are
  transcribed, not recorded** — no GPU in the validation fleet has been
  reached yet — and their provenance says so; the first G2 and A2 runs
  replace them, and a mismatch corrects the row. (V021-E02-F02-T01)

- The Ubuntu x86_64 CPU-only row now has a live smoke, and it is the only
  packaging check that installs real binaries and runs the real CLI. The
  others stub the runtime so they can rehearse packaging shape cheaply, which
  cannot tell you whether the installed appliance comes up. This one builds
  the runtime, installs the package set, starts the agent and observability,
  and then requires a **green** `tensorplate doctor` — absent CUDA and
  TensorRT are informational findings on this row, so a non-zero exit means
  something real.

  It resolves the row by **live detection** rather than from a recorded
  fixture: the runner is Ubuntu 22.04 on x86_64 with no accelerator, which is
  the row itself, and the row declares no machine type, which is how the
  schema spells "any instance". The smoke refuses to run anywhere else, since
  a green result on the wrong host would say nothing about the row. It also
  asserts the row is Preview in the *installed* registry and that nothing
  doctor prints describes this host as Production, and it records host facts,
  package versions, and the doctor output as evidence.
  (V021-E02-F01-T04)

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
  machine to exactly one support row. Exact accelerator rows take precedence
  over explicitly declared lower-priority family compatibility rows.
  `PlatformRegistry` loads the
  committed rows and roadmap targets and **fails closed**: one invalid
  document means no registry, because a half-loaded registry would report
  supported platforms as unsupported. Colliding entries — a duplicated row
  id, two rows matching the same platform identity at the same priority, or
  a roadmap target shadowing a row id — are rejected at load rather than resolved by
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
  defines one platform row (OS with exact version and image
  identity, kernel/driver stack, CPU architecture and vendor, accelerator
  identity with exact matching by default and an explicit,
  lower-priority family policy, required memory size, memory profile reference,
  and partition posture, backend package sets per channel, model-class row
  pointers, per-signal gate semantics, support level, provenance, validation
  environment, and evidence location), and the deliberately smaller
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
  (five Production, three Preview, four Planned) and all four roadmap
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
  decoder sees them. (V030-E03-F04-T03)

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
  mirror verdict-identical. (V030-E03-F04-T01, V030-E03-F04-T02)

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
