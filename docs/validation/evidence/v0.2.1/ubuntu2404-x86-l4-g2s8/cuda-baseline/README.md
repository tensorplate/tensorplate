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
retained privately. The repacked deb's revision suffix and the inference
`request_id` are shown with an internal label removed; the repacked deb's
sha256 is the file as measured.

## What each file proves

| File | What it holds |
| --- | --- |
| `systemd-effective-properties.txt` | The agent unit's hardening as `systemctl show` reports it, not as the unit file spells it — 11 of the unit file's 18 hardening directives — plus the PATH a unit with no `Environment=PATH` actually gets |
| `nvidia-smi-reachability.txt` | `nvidia-smi` resolved and answered as uid `tensorplate` inside the agent's mount namespace under that PATH alone; and what the observability unit's private `/dev` does instead |
| `device-nodes.txt` | `/dev/nvidiactl`, `/dev/nvidia0` and `/dev/nvidia-uvm` present in the agent's namespace and opened as uid `tensorplate` |
| `cuda-under-agent-hardening.json` | PyTorch reporting CUDA available, and a 512×512 matmul returning the arithmetically correct sum, as uid `tensorplate` in a transient unit given the hardening that readback covers, not all of the unit's (see below) |
| `package-closure.txt` | No source tree on the host, each TensorPlate file the published packages install owned by its package, and `dpkg --verify` over them, taken from the published set alone before the backend package was repacked (see below) |
| `cuda-deploy.json` | The deploy reaching `active` inside the agent unit itself and an inference round-tripping through the worker, with the GPU memory the sidecar held while it was live and the live process tree; its status excerpt reads `degraded` for the reason below |
| `cuda-artifact-provenance.txt` | Exactly which artifact carried the CUDA runner, and the patch that distinguishes it from the published one |
| `tensorrt-refusal.json` | A TensorRT bundle refused at admission |
| `accelerator-recording.txt` | The `doctor --record` SKU line against the recordings already in the tree, and the stack this run measured |
| `kernel-driver-stack-experiment.txt` | Why that measured stack is not written into the row's `kernel_driver_stack.components` |
| `doctor-findings.json` | Every doctor finding, before and after the backend package and PyTorch were installed |
| `agent-journal.txt` | The agent's own journal lines for the run |
| `reboot-and-boot-binding.txt` | The login path and the guest agent returning after a reboot with the appliance's autostart disabled, and the machine-type record still carrying the pre-reboot boot id |

`cuda-deploy.json`'s status excerpt reads `degraded` with the deployment
active and ready because its quarantine list is not empty, and this CLI
reports any quarantine entry as degraded. The entry is an earlier attempt at
the same deployment id, refused with `backend descriptor not installed`: the
backend package was installed while the agent was running, and the agent
probes backends only at startup. `agent-journal.txt` shows the first start
with `DescriptorMissing` and the restart with `Runnable`; the deployment that
reached `active` came after that restart.

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
  The patch is the payload as the deploy ran it. Since the run, the probe
  reports the NVIDIA driver's version as the accelerator runtime version,
  where this payload reported the version of CUDA the PyTorch wheel was built
  against, and the module's docstring changed; no file here records a probe's
  version output.
- **The transient unit covered part of the agent unit's hardening.** The
  readback records 11 of the unit file's 18 hardening directives, and the
  transient unit was given those; `ProtectKernelTunables`,
  `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`,
  `RestrictAddressFamilies`, `RestrictRealtime` and `RestrictSUIDSGID` were
  not read back, and the transient unit's own properties were not captured.
  `cuda-under-agent-hardening.json` speaks for those eleven only. The check
  made under the unit's whole sandbox is the deploy in `cuda-deploy.json`:
  its sidecar ran as a descendant of the agent unit's own process, and
  `cuda_fixture` loads only after a checked CUDA matmul.
- **The closure is of the published set alone, not of everything the
  deploy ran.** It was taken before the backend package was repacked, so its
  `dpkg --verify` compares the published packages' installed files with the
  checksums those packages shipped, and says nothing about the repacked
  backend package installed afterwards; that package's provenance is the
  pair of digests and the patch in `cuda-artifact-provenance.txt`. No
  package listing from the closure's moment is in this bundle, and
  `dpkg --verify` reads the same either way; that the closure preceded the
  repack is the operator's record of the run. PyTorch is owned by no package
  at all: it was installed with pip into the system interpreter.
- **`/dev/nvidia-uvm` was present from boot on this image**, so the failure
  this gate watches for — the node's lazy creation blocked under
  `NoNewPrivileges` — could not arise here. That is a property of this
  image, not a clearance for one that creates the node on first use.
- **No model was loaded and no speech runner ran.** The kernel here is a
  matmul chosen because it goes through cuBLAS, which is the library the
  speech runners reach the GPU through.
- **A `pgrep -f` count is not a process count** on these hosts: the pattern
  matches the invoking command's own line. The process facts recorded here
  come from `ps` and from `systemctl show -p MainPID`.
