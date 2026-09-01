# Accelerator detection fixtures

One recorded `nvidia-smi` answer per file, in the exact shape
`platform/src/accelerator.rs` asks for:

```bash
nvidia-smi --query-gpu=name,memory.total,driver_version,uuid,mig.mode.current \
  --format=csv,noheader,nounits
```

The **product name is the only field a support row matches on**, and it is
compared verbatim. Everything else is recorded for evidence and telemetry.

## Provenance by fixture

`ubuntu2404-x86-l4-g2s8.txt` is **recorded**: captured 2026-08-30 by
`tensorplate doctor --record` on a disposable GCP `g2-standard-8` booted
from the stock `ubuntu-2404-lts-amd64` image with the driver installed via
`ubuntu-drivers install --gpgpu` (branch 595, `595.71.05`), device
`GPU-00000000-0000-0000-0000-000000000001`. The recorded line agreed with
the previously transcribed name, memory figure, and `[N/A]` MIG spelling
byte-for-byte. The recorded driver version replaces the transcribed value;
the observed UUID is sanitized in the published fixture.

`dlvm-ubuntu2404-l4-g2s8.txt` is also **recorded**: the same capture run
from the Ubuntu 24.04 Deep Learning VM image
(`common-cu129-ubuntu-2404-nvidia-580-v20260819`, driver 580.173.02),
committed as the row's second covered boot path alongside its host fixture
`test/platform/host_identity/dlvm-ubuntu2404-l4-g2s8.json` — the same
pattern as the lab Jetson's extra recording. The raw capture agreed with the
stock run on every silicon fact and differed only in driver and device.

**Published recordings are sanitized.** Device UUIDs are replaced with
clearly synthetic values and the GCP project identifier is redacted to
`projects/REDACTED` — matching reads neither, and the tests require only
that the fields exist. The unsanitized raw captures are retained privately
with the release evidence.

## Every other fixture here is transcribed, not recorded

None of the files below came off real hardware; each string's source is
listed so a future recording knows exactly what claim it replaces.

| Fixture | Product name | Source of the name |
| --- | --- | --- |
| `ubuntu2404-x86-a100-40g-a2hg1.txt` | `NVIDIA A100-SXM4-40GB` | NVIDIA A100 documentation; SXM4 boards encode form factor and capacity in the name. |
| `ubuntu2404-x86-rtxpro6000se-g4s48.txt` | `NVIDIA RTX PRO 6000 Blackwell Server Edition` | NVIDIA RTX PRO 6000 Blackwell product naming. |
| `ubuntu2404-x86-rtxpro6000we-physical.txt` | `NVIDIA RTX PRO 6000 Blackwell Workstation Edition` | NVIDIA RTX PRO 6000 Blackwell product naming. This row is **Planned** and has no recorded fixture, so this string has the weakest provenance of any here. |
| `unsupported-a100-80gb.txt` | `NVIDIA A100-SXM4-80GB` | Same family as the supported A100, one capacity away. |
| `unsupported-rtx-a6000.txt` | `NVIDIA RTX A6000` | Named as explicitly out of matrix by the epic's non-goals. |
| `unsupported-rtx-6000-ada.txt` | `NVIDIA RTX 6000 Ada Generation` | Named as explicitly out of matrix by the epic's non-goals. |
| `mig-enabled-a100-40g.txt` | `NVIDIA A100-SXM4-40GB` | The A100 row's card with `mig.mode.current` set to `Enabled`. The row is Planned; the partitioning refusal is checked before support level, so this fixture still exercises it. |

UUIDs are synthetic. Driver versions are plausible for the generation and are
not asserted on. Framebuffer sizes are approximately what each card reports,
which is not the same as the row's nominal capacity — see below.

### These must be replaced with recorded output

The remaining first-run captures replace the rest. **A mismatch between a
transcribed name here and the recorded one corrects the row and this
fixture — it is not an evidence exception.** Until then, a green test over
these files proves the parser and the matching path, not that the strings
are what the fleet reports.

- The A100 pair (`ubuntu2404-x86-a100-40g-a2hg1.txt`,
  `mig-enabled-a100-40g.txt`) remains transcribed while the row is Planned;
  both files must be regenerated from one recorded name in one session.
- The RTX PRO 6000 Server Edition row carries evidence from the previous
  release cycle rather than a recording from this pipeline. The Workstation
  Edition fixture remains unrecorded and is annotated unverifiable in
  release evidence.
- The `unsupported-*` fixtures stay transcribed by design: they name
  hardware the matrix refuses, and recording them would require the very
  machines the rows exclude.

## Why the framebuffer is not the row's capacity

An L4's row records 24 GiB (`25769803776` bytes). The card reports roughly
`23034` MiB, because the row records nominal capacity and the tool reports
the usable framebuffer.

They do **not** always differ: an A100 40GB reports exactly its nominal
40 GiB. That is the point — the two numbers *may* differ, and for at least
one supported card they do, which is enough to disqualify memory as a match
dimension. Matching on it would make that card miss its own row, and the
gap is far too large for a tolerance to paper over. An equality that happens
to hold for one card is not a property to build on.
