# Speech validation reference bundles

`manifest.json` is the example trust root installed with `tensorplate-cli`.
An operator may supply any manifest of the same schema. These candidate-only
bundles preserve the entry layouts and declarations of the synthetic speech
fixtures, with real upstream files and their recorded SHA-256 digests. They
use the existing unary candidate path; provisioning grants no speech support
or performance claim.

The eight model files were downloaded from the following immutable revisions
on 2026-09-30, then hashed from their exact bytes. The provisioning manifest
records every size, SHA-256 and URL. Model bytes are fetched by the operator;
they are not redistributed inside the CLI package.

| Reference | Repository and pinned revision | Files |
| --- | --- | --- |
| Whisper large-v3-turbo CT2 FP16 | [mobiuslabsgmbh/faster-whisper-large-v3-turbo](https://huggingface.co/mobiuslabsgmbh/faster-whisper-large-v3-turbo/tree/0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf), `0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf` | `config.json`, `model.bin`, `preprocessor_config.json`, `tokenizer.json`, `vocabulary.json` |
| Kokoro-82M FP32, en-US/af_heart | [hexgrad/Kokoro-82M](https://huggingface.co/hexgrad/Kokoro-82M/tree/f3ff3571791e39611d31c381e3a41a3af07b4987), `f3ff3571791e39611d31c381e3a41a3af07b4987` | `config.json`, `kokoro-v1_0.pth`, `voices/af_heart.pt` |

The [converted Whisper model card](https://huggingface.co/mobiuslabsgmbh/faster-whisper-large-v3-turbo/blob/0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf/README.md)
was also downloaded (1,445 bytes, SHA-256
`b3068692728faed23580cce5cd569fc47ff76c690c032b2641ffd5554ea64d8f`).
It identifies `openai/whisper-large-v3-turbo` as the conversion input and
records this command:

```sh
ct2-transformers-converter --model openai/whisper-large-v3-turbo \
  --output_dir faster-whisper-large-v3-turbo \
  --copy_files tokenizer.json preprocessor_config.json --quantization float16
```

The publisher does not record the input revision or converter version. Those
facts are unavailable, not inferred from the converted repository revision.
This is a candidate provenance limit for the artifact freeze review. No
conversion runs on the appliance. The card declares MIT; the pinned Kokoro
card declares Apache-2.0. Artifact and runtime redistribution/licensing reviews
and actual L4 qualification remain required before a release support claim.

Neither entry references VAD or G2P files. The VAD is the copy in the pinned
faster-whisper wheel; Misaki lexicons and spaCy assets are wheelhouse data.
Their identities belong to the runtime dependency lock and qualification
record. `tokenizer.json` is explicitly provisioned so the Whisper loader need
not retrieve a fallback tokenizer.
