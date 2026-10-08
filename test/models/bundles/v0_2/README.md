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
pair; the agent's deploy failure test also checks every refusal before staging,
worker contact and active-deployment changes. These are authored contract
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

`lineage_known_bases.json` is input to these tests only. It names a base by
its fixture directory, and the test reads that bundle's name, version and
manifest digest from the parser, so `base_model_ref.manifest_digest` in the
fixtures is the real digest of `valid_r8_base_bundle/manifest.json`. Changing
that manifest changes the digest every variant fixture must carry.
