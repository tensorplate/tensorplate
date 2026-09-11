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
| `unsupported-a100-pcie-40gb.txt` | `NVIDIA A100-PCIE-40GB` | The canonical near miss: same family **and** same capacity as the A100 40GB row, differing only in form factor. It replaced `unsupported-a100-80gb.txt` when the A100 80GB became a Preview row, and it is the stronger near miss of the two. GCP's A100s are all SXM4, so this stays off-matrix. The spelling follows NVIDIA's form-factor naming and is not recorded; that is immaterial here, because the property under test is that a card no row names is refused rather than matched to its nearest row. |
| `ubuntu2404-x86-a100-80g-a2ug1.txt`, `...-a2ug8.txt` | `NVIDIA A100-SXM4-80GB` | Follows the SXM4 naming of the committed A100 40GB row. **Not recorded.** The 81920 MiB framebuffer is carried over from the earlier transcription and is also unverified. The 8-device file repeats the line with distinct synthetic UUIDs. |
| `ubuntu2404-x86-h100-80g-a3hg1.txt`, `...-a3hg8.txt` | `NVIDIA H100 80GB HBM3` | Name and 81559 MiB framebuffer taken from observed `nvidia-smi` output on another provider's H100 host ([thundergolfer, "Why does an NVIDIA H100 80GB card offer 85.52 GB?"](https://thundergolfer.com/blog/nvidia-gpu-memory-capacity)). **Not recorded on GCP.** Consistent with GCP documenting `a3-highgpu-*` as H100 SXM; `a3-megagpu-8g` uses a different accelerator type (`nvidia-h100-mega-80gb`) and may report a different string, so it has no row. |

| Multi-GPU fixtures at 2, 4 and 8 devices for A100 40GB (`a2hg2/4/8`), A100 80GB (`a2ug2/4`), H100 (`a3hg2/4`) and L4 (`g2s24/48/96`) | as the 1-GPU fixture of each card | Each repeats its card's line once per device, with distinct synthetic UUIDs. The counts and shapes come from GCP's own machine-type API (`gcloud compute machine-types list`, us-central1), not from documentation, which truncates before its tables. The SKU and framebuffer carry the same provenance as the 1-GPU fixture they were derived from -- **none is recorded**. |

### One row per card and count, not per shape

A row is needed for each distinct (SKU, device count). A second shape at
an existing count needs none: it fails the row's machine-type check, so it
resolves as outside the row's validated environment and is admitted
against it when the row is supported. That covers `a3-edgegpu-8g` (H100,
eight devices, same accelerator type as `a3-highgpu-8g`) and the four
1-GPU L4 shapes besides `g2-standard-8`.

Three GCP shapes have no row, deliberately:

- `a2-megagpu-16g` -- sixteen A100 40GB. A count no other family offers,
  and outside the 1/2/4/8 set requested.
- `a3-megagpu-8g` -- a different accelerator type (`nvidia-h100-mega-80gb`)
  that may report a different product name; a row with a guessed string
  would never match.
- `g4-standard-96/192/384` -- RTX PRO 6000 at 2, 4 and 8. The G4 row is
  held as planned.

### The `NVIDIA ` prefix is driver-dependent

NVIDIA's HGX A100 software guide shows `nvidia-smi` printing
`A100-SXM4-40GB` -- **without** the `NVIDIA ` prefix -- from an older
driver. Current drivers print it: the recorded L4 captures (driver 580 and
595) read `NVIDIA L4`, and the H100 output above reads `NVIDIA H100 80GB
HBM3`. Every committed NVIDIA row uses the prefixed form.

Detection matches the product name verbatim with no normalisation, so a
host on an older driver would report a SKU no row names. That is true of
every NVIDIA row, not something these rows introduce -- but it means the
first `tensorplate doctor --record` on each of these machines is what
confirms the string, and its `accelerator_facts` finding will show exactly
what was reported if it does not match.
| `unsupported-rtx-a6000.txt` | `NVIDIA RTX A6000` | Named as explicitly out of matrix by the epic's non-goals. |
| `unsupported-rtx-6000-ada.txt` | `NVIDIA RTX 6000 Ada Generation` | Named as explicitly out of matrix by the epic's non-goals. |
| `mig-enabled-a100-40g.txt` | `NVIDIA A100-SXM4-40GB` | The A100 row's card with `mig.mode.current` set to `Enabled`. The row is Planned; the partitioning refusal is checked before support level, so this fixture still exercises it. |
| `multi-gpu-three-l4.txt` | `NVIDIA L4` | The L4 row's own **recorded** line, repeated for three devices with synthetic UUIDs. Not a recording: no multi-GPU host has been observed. What it exercises is the device count. **Three, deliberately**: GCP offers one, two, four, eight and sixteen of a card but never three, so no real shape can ever earn a row at this count. It was `multi-gpu-two-l4.txt` until an L4 row claimed two GPUs (`g2-standard-24`) and made the old example supported -- an unclaimed-count example has to be a count nothing will claim, or the next row falsifies every test that uses it. |

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
