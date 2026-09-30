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
