# CUDA baseline for the `ubuntu2404-x86-l4-g2s8` row

The lifecycle evidence beside this directory was recorded with the
device-neutral `fixture` backend profile, which executes no CUDA kernel —
the harness says so in its own header. This directory is the separate
question that lifecycle run cannot answer: **does the hardened service
sandbox let the accelerator be used at all?**

Recorded 2026-09-27 on a disposable `g2-standard-8` with one NVIDIA L4,
booted from `common-cu129-ubuntu-2404-nvidia-580`
(`deeplearning-platform-release`), against the published `v0.2.1` release.
Identifiers are the evidence README's synthetic forms; the raw run is
retained privately.

## What each file proves

| File | What it holds |
| --- | --- |
| `systemd-effective-properties.txt` | The hardening as `systemctl show` reports it, not as the unit file spells it, plus the PATH a unit with no `Environment=PATH` actually gets |
| `nvidia-smi-reachability.txt` | `nvidia-smi` resolved and answered as uid `tensorplate` inside the agent's mount namespace under that PATH alone; and what the observability unit's private `/dev` does instead |
| `device-nodes.txt` | `/dev/nvidiactl`, `/dev/nvidia0` and `/dev/nvidia-uvm` present in the agent's namespace and opened as uid `tensorplate` |
| `cuda-under-agent-hardening.json` | PyTorch reporting CUDA available, and a 512×512 matmul returning the arithmetically correct sum, as uid `tensorplate` in a transient unit carrying every one of the agent unit's hardening properties |
| `package-closure.txt` | Every artifact the deploy needs owned by an installed package, no source tree on the host, and `dpkg --verify` finding no installed file that differs from its package |
| `cuda-deploy.json` | The deploy reaching `active` and an inference round-tripping through the worker, with the GPU memory the sidecar held while it was live and the live process tree |
| `cuda-artifact-provenance.txt` | Exactly which artifact carried the CUDA runner, and the patch that distinguishes it from the published one |
| `tensorrt-refusal.json` | A TensorRT bundle refused at admission |
| `accelerator-recording.txt` | The `doctor --record` SKU line against the recordings already in the tree, and the stack this run measured |
| `kernel-driver-stack-experiment.txt` | Why that measured stack is not written into the row's `kernel_driver_stack.components` |
| `doctor-findings.json` | Every doctor finding, before and after the backend package and PyTorch were installed |
| `agent-journal.txt` | The agent's own journal lines for the run |
| `reboot-and-boot-binding.txt` | The login path and the guest agent returning after a reboot with the appliance's autostart disabled, and the machine-type record still carrying the pre-reboot boot id |

## What it does not prove

- **Not a lifecycle run.** Install, upgrade, rollback and the denied-egress
  stage are the files beside this directory, from a different run.
- **The CUDA deploy did not run on a published artifact.** The published
  packages register `fixture`, `mps_fixture` and `smolvla`; the first two
  never reach CUDA on Linux and the third needs a VLA model. The deploy
  used the published backend package with the `cuda_fixture` payload added
  and its revision suffixed, which
  `cuda-artifact-provenance.txt` records in full, including the
  patch. Every other package installed is the published artifact unmodified.
  The closure evidence above is from the published set alone.
- **`/dev/nvidia-uvm` was present from boot on this image**, so the failure
  this gate watches for — the node's lazy creation blocked under
  `NoNewPrivileges` — could not arise here. That is a property of this
  image, not a general clearance for a host without `nvidia-persistenced`.
- **No model was loaded and no speech runner ran.** The kernel here is a
  matmul chosen because it goes through cuBLAS, which is the library the
  speech runners reach the GPU through.
- **A `pgrep -f` count is not a process count** on these hosts: the pattern
  matches the invoking command's own line. The process facts recorded here
  come from `ps` and from `systemctl show -p MainPID`.
