# Accelerator detection fixtures

One `nvidia-smi` answer per file, in the exact shape
`platform/src/accelerator.rs` asks for. The provenance below distinguishes
recorded answers from transcribed and synthetic fixtures.

```bash
nvidia-smi --query-gpu=name,memory.total,driver_version,uuid,mig.mode.current \
  --format=csv,noheader,nounits
```

Accelerator matching uses the product name, compared verbatim, together
with the device count and partitioning state. Memory readings bound the
per-device capability; they are not a match key. Driver versions and UUIDs
are retained as evidence fields.

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
| Multi-GPU fixtures at 2, 4 and 8 devices for A100 40GB (`a2hg2/4/8`), A100 80GB (`a2ug2/4`), H100 (`a3hg2/4`) and L4 (`g2s24/48/96`) | as the 1-GPU fixture of each card | Each repeats its card's line once per device, with distinct synthetic UUIDs. The counts and shapes come from GCP's machine-type API (`gcloud compute machine-types list`, us-central1). The SKU and framebuffer retain the provenance of the single-device source; **none of these multi-GPU answers is a recording**. |
| `unsupported-rtx-a6000.txt` | `NVIDIA RTX A6000` | Named as explicitly out of matrix by the epic's non-goals. |
| `unsupported-rtx-6000-ada.txt` | `NVIDIA RTX 6000 Ada Generation` | Named as explicitly out of matrix by the epic's non-goals. |
| `mig-enabled-a100-40g.txt` | `NVIDIA A100-SXM4-40GB` | The A100 row's transcribed answer with `mig.mode.current` set to `Enabled`. The row is Preview; this fixture exercises the partitioning refusal independently of support level. |
| `ubuntu2404-x86-a100-40g-a2mg16.txt` | `NVIDIA A100-SXM4-40GB` | The A100 40GB fixture's line repeated for sixteen devices with synthetic UUIDs. Same accelerator type as the `a2-highgpu` shapes per GCP's machine-type API; carries that fixture's provenance, so it is **not recorded**. |
| `ubuntu2404-x86-rtxpro6000se-g4s96.txt`, `...-g4s192.txt`, `...-g4s384.txt` | `NVIDIA RTX PRO 6000 Blackwell Server Edition` | The G4 fixture's line repeated for two, four and eight devices, varying only the UUID. The G4 half of the first-run recording was never captured, so these are **not recorded** either. |
| `multi-gpu-three-l4.txt` | `NVIDIA L4` | The L4 row's recorded line, repeated for three devices with synthetic UUIDs. **Not a recording.** Three devices exercise an unclaimed count in the committed registry. This replaces the two-device refusal fixture because `g2-standard-24` now has a row for two L4s. |

UUIDs in these fixtures are synthetic. Driver versions are plausible for
the generation and are not asserted on. Transcribed framebuffer sizes are
unverified; the L4 derivatives retain the recorded single-device reading.

### Derived multi-GPU host fixtures

The paired host fixtures
`test/platform/host_identity/ubuntu2404-x86-l4-g2s24.json`,
`ubuntu2404-x86-l4-g2s48.json`, and `ubuntu2404-x86-l4-g2s96.json` are
**spec_authored derivatives**, not recordings from those shapes. They copy
the recorded `g2-standard-8` sources and change only the machine-type source
and the fixture's row and expected machine identity. The copied CPU,
memory, and PCI data remain from the single-device host; they do not
establish the hardware inventory of the larger shapes. Paired with the
synthetic accelerator answers above, these cases test matching and do not
provide hardware-validation evidence.

The same holds for `ubuntu2404-x86-a100-40g-a2mg16.json` (derived from the
A100 40GB `a2-highgpu-1g` fixture) and
`ubuntu2404-x86-rtxpro6000se-g4s96.json`, `...-g4s192.json` and
`...-g4s384.json` (derived from the G4 `g4-standard-48` fixture, and so
AMD EPYC, as G4 hosts are). All are `spec_authored`, and each note names
the fixture it was derived from.

Which host fixtures may claim `recorded` is a closed list,
`RECORDED_HOST_FIXTURES` in `platform/tests/host_identity.rs`. The L4
derivatives above first shipped claiming to be recordings -- a copy of a
recording inherits its provenance and its note along with everything else
-- and nothing failed. A new real capture belongs on that list; a derived
fixture never does.

### One row per card and count, not per shape

A row is needed for each distinct (SKU, device count). A second shape at
an existing count can resolve as outside that row's validated environment
instead of needing another row. Admission still applies the row's
prerequisites and execution constraints. A catalog entry alone does not
add multi-device execution: bundle requests for more than one device
remain refused.

Every GPU shape GCP offers for these cards is covered, one way or the
other. `a2-megagpu-16g` has a row: it is the same accelerator type as the
`a2-highgpu` shapes, so the only new fact is the count. G4 has rows at two,
four and eight (`g4-standard-96/192/384`), at Preview, beside the
single-device Production row -- which they do not change.

One shape has no row, deliberately: `a3-megagpu-8g`. GCP gives it a
different accelerator name (`nvidia-h100-mega-80gb`) but documents all
three A3 variants -- High, Mega and Edge -- as the same H100 SXM, and
`nvidia-smi` reports the product name the GPU itself gives, not GCP's
label. If it reports `NVIDIA H100 80GB HBM3`, it is already admitted
against the `a3-highgpu-8g` row as outside-validated, exactly as
`a3-edgegpu-8g` is -- `registry_query.rs` pins both. A row with a guessed
name would add nothing if the guess were right, and would sit unmatched in
the public matrix if it were wrong. That is conditional on the reported
name, and the first recording of that shape settles it.

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

### These must be replaced with recorded output

The remaining first-run captures replace the rest. **A mismatch between a
transcribed name here and the recorded one corrects the row and this
fixture — it is not an evidence exception.** Until then, a green test over
these files proves the parser and the matching path, not that the strings
are what the fleet reports.

- The A100 pair (`ubuntu2404-x86-a100-40g-a2hg1.txt`,
  `mig-enabled-a100-40g.txt`) remains transcribed while the row is Preview;
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

The transcribed A100 40GB fixture uses its nominal 40 GiB, but that value
has not been confirmed by a recording. The recorded L4 difference is
enough to show why memory cannot be an exact match dimension: it would
make the card miss its own row. Memory instead bounds the per-device
capability after the identity matches.
