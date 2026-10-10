# Format 0.2 bundle fixtures

Schema fixtures for manifests that opt into `format_version: "0.2"`: the
general profile fields and the speech contract described in
[`docs/bundles/manifest.md`](../../../../docs/bundles/manifest.md#format-02).
They run on host CI with no model download, like the
[format 0.1 fixtures](../v0_1/README.md).

## Valid fixtures

| Path | Class | Runner profile | Notes |
| --- | --- | --- | --- |
| `speech_stt_streaming/` | `speech` | `faster_whisper` | Streaming STT: 16 kHz PCM plus the 8 kHz μ-law decode path, `en` and `ar`, 20–320 ms frames, 30 s utterances, `guest_ram` and `device_vram` budgets. |
| `speech_tts_streaming/` | `speech` | `kokoro` | Streaming TTS: mono 24 kHz PCM16 output, `en-US`, voice `af_heart`, segment and synthesis limits, a text warmup fixture. |

Rejections are covered by
[`protocol/rust/tests/bundle_profile_fixtures.rs`](../../../../protocol/rust/tests/bundle_profile_fixtures.rs):
each case changes one declaration of these two manifests and asserts that the
schema and the parser reach the same verdict.

## What is synthetic

Everything in these directories is a TensorPlate-authored placeholder under
the repository's Apache-2.0 license:

- `entry.json` is a placeholder runner entry. The parser verifies its digest
  and does not read its contents.
- The memory budgets are illustrative declarations in round binary units,
  not measurements. A real bundle's budgets come from qualifying it on its
  row.
- `algorithm_profile.digest`, `quality_profile_digest` and
  `benchmark_profile_digest` are the SHA-256 of the placeholder line
  `tensorplate fixture placeholder: <stt|tts> <algorithm|quality|benchmark> profile`
  followed by a newline. They name no real profile document.

Regenerate artifact digests with `tensorplate-bundle-tool` as described in
the [format 0.1 README](../v0_1/README.md#regenerating-digests).

## Manifest-local rule pairs

The `valid_r*/` and `invalid_r*/` directories each carry a complete manifest,
a placeholder artifact and `expected.json`. A null `rule` means acceptance;
a string names the exact typed refusal. `bundle_conformance.rs` replays every
pair; the agent's
[`bundle_cross_field_rules.rs`](../../../../agent/tests/bundle_cross_field_rules.rs)
deploys every pair and checks each refusal before staging, worker contact and
active-deployment changes. These are authored contract
fixtures derived from the STT schema fixture, not recordings or measurements.
The vision and VLA examples exercise supported payload shapes. The existing
format 0.1 fixtures remain unchanged.

## Variant lineage fixtures

`valid_r8_base_bundle/` is a TTS bundle with no lineage declaration; the
other `*_r8_*` directories are copies of it that declare themselves its
variants. Every one of them is refused by the parser, which is what `rule`
records: `bundle_r8_reserved_variant` for an otherwise valid declaration of
each of the three kinds, and `bundle_r8_base_reference` for the bundle that
names itself as its base.

Where `expected.json` also carries `lineage_rule`, the declaration is judged
by `BundleProfile::check_lineage` against `lineage_known_bases.json`: null
means the declaration is consistent with the known base, a string names the
refusal.

| Directory | `lineage_rule` |
| --- | --- |
| `invalid_r8_reserved_speaker_embedding/`, `invalid_r8_reserved_adapter/`, `invalid_r8_reserved_full_checkpoint/` | null |
| `invalid_r8_unresolved_base/` | `bundle_r8_base_reference`: the digest is not the base's |
| `invalid_r8_identity_conflict/` | `bundle_r8_variant_identity`: the id and revision are already declared on the base |
| `invalid_r8_support_escalation/` | `bundle_r8_variant_support_level`: asks for `preview` on an `experimental` base |
| `invalid_r8_support_above_row/` | `bundle_r8_variant_support_level`: asks for `production` on an `experimental` base |

`lineage_known_bases.json` is input to these tests only. It names a base by
its fixture directory, and the test reads that bundle's name, version and
manifest digest from the parser, so `base_model_ref.manifest_digest` in the
fixtures is the real digest of `valid_r8_base_bundle/manifest.json`. Changing
that manifest changes the digest every variant fixture must carry.

## Rules judged against the target

Three rules compare a manifest with facts only a deployment target holds, so
the parser accepts both twins of their pairs (`rule` is null) and the agent
refuses one. Where a deploy's verdict differs from the parser's,
`expected.json` carries `deploy_code`: the code the refusal returns in its
error context. `bundle_cross_field_rules.rs` deploys every directory here on
an agent it builds with these facts, against a mock worker:

- the platform rows committed under `config/platform/`, on a machine admitted
  on the Preview row `ubuntu2404-x86-l4-g2s24`;
- a `python_pytorch` probe report with `faster_whisper` (`float16`, `float32`)
  and `kokoro` (`float32`) installed and runnable;
- `tensorrt` listed as an available backend;
- `valid_r8_base_bundle/` deployed and active.

| Directory | `deploy_code` |
| --- | --- |
| `valid_r6_runner_profile/` | none: the profile is installed and lists `float16` |
| `invalid_r6_compute_type/` | `bundle_r6_compute_type`: `int8_float16`, which the installed `faster_whisper` does not list |
| `invalid_r6_runner_selector/` | `bundle_r6_runner_selector`: a runner profile under `tensorrt`, which declares none |
| `invalid_r6_runner_not_installed/` | `missing_backend_package`: the platform reason an absent profile already had |
| `invalid_r8_unresolved_base/` | `bundle_r8_base_reference`: the digest is not the deployed base's |
| `invalid_r8_support_above_row/` | `bundle_r8_variant_support_level`: asks for `production` where the machine holds a Preview row |
| `valid_r9_hardware_rows/` | none: both rows exist and are Production |
| `invalid_r9_unknown_row/` | `bundle_r9_hardware_row`: `ubuntu2404-x86-l4-unlisted` is not a row |
| `invalid_r9_production_claim/` | `bundle_r9_support_claim`: asks for `production` and names a Preview row |

The other variant fixtures deploy to the parser's `rule`: on this agent their
base is known and they ask for no more than the row gives, so they are
refused as reserved. `invalid_r8_identity_conflict/` is among them, because
an agent records no variant on a base while none can deploy.

`valid_r9_hardware_rows/` asks for `production` on Production rows and
deploys. That shows the claim check reads the rows; it is not a Production
claim for any bundle. The fixtures name real row ids, and the test asserts
the level of each row it relies on, so a change to those rows fails there.
