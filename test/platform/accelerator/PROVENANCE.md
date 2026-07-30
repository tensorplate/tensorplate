# Accelerator detection fixtures

One recorded `nvidia-smi` answer per file, in the exact shape
`platform/src/accelerator.rs` asks for:

```bash
nvidia-smi --query-gpu=name,memory.total,driver_version,uuid,mig.mode.current \
  --format=csv,noheader,nounits
```

The **product name is the only field a support row matches on**, and it is
compared verbatim. Everything else is recorded for evidence and telemetry.

## Every fixture here is transcribed, not recorded

No GPU in this project's validation fleet has been reached yet: the GCP
project, quota, and CI auth are greenfield, and the in-lab Jetson has no
discrete NVIDIA card. So none of these files came off a machine we own.

| Fixture | Product name | Source of the name |
| --- | --- | --- |
| `ubuntu2404-x86-l4-g2s8.txt` | `NVIDIA L4` | NVIDIA L4 product documentation; the name has no memory suffix. |
| `ubuntu2404-x86-a100-40g-a2hg1.txt` | `NVIDIA A100-SXM4-40GB` | NVIDIA A100 documentation; SXM4 boards encode form factor and capacity in the name. |
| `ubuntu2404-x86-rtxpro6000se-g4s48.txt` | `NVIDIA RTX PRO 6000 Blackwell Server Edition` | NVIDIA RTX PRO 6000 Blackwell product naming. |
| `unsupported-a100-80gb.txt` | `NVIDIA A100-SXM4-80GB` | Same family as the supported A100, one capacity away. |
| `unsupported-rtx-a6000.txt` | `NVIDIA RTX A6000` | Named as explicitly out of matrix by the epic's non-goals. |
| `unsupported-rtx-6000-ada.txt` | `NVIDIA RTX 6000 Ada Generation` | Named as explicitly out of matrix by the epic's non-goals. |
| `mig-enabled-a100-40g.txt` | `NVIDIA A100-SXM4-40GB` | The supported A100 with `mig.mode.current` set to `Enabled`. |

UUIDs are synthetic. Driver versions are plausible for the generation and are
not asserted on. Framebuffer sizes are approximately what each card reports
and are deliberately **not** the row's nominal capacity — see below.

### These must be replaced with recorded output

The first `g2-standard-8` and `a2-highgpu-1g` runs capture real
`nvidia-smi` output and commit it as row facts. **A mismatch between a
transcribed name here and the recorded one corrects the row and this
fixture — it is not an evidence exception.** Until then, a green test over
these files proves the parser and the matching path, not that the strings
are what the fleet reports.

The RTX PRO 6000 Server Edition fixture stays transcribed for longer: that
row carries evidence from the previous release cycle rather than a run of
this pipeline.

## Why the framebuffer is not the row's capacity

An L4's row records 24 GiB (`25769803776` bytes). The card reports roughly
`23034` MiB, because the row records nominal capacity and the tool reports
the usable framebuffer. They are different numbers for every card here.

That is why memory is recorded but never matched on. Making it a match
dimension would make every supported card miss its own row, and the
mismatch is not a rounding error that a tolerance would paper over.
