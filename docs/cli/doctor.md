# `tensorplate doctor`

Read-only validation pass over device, runtime, and agent state. The command
never mutates desired state, restarts workers, downloads packages, or modifies
config. It is safe to run from any operator session.

```
tensorplate doctor [--skip-agent] [--record <dir>] [--output <human|json>]
```

## What it checks

| Finding ID | What it asserts |
| --- | --- |
| `cli_version` | CLI + protocol version stamp. |
| `profile_mode` | Resolved profile mode is supported in v0.1.0. |
| `agent_socket` | Local profile's UDS path exists. |
| `agent_reachable` | Version request round-trips against the agent. |
| `agent_status_shape` | Agent returns a parseable `AgentStatus`. |
| `agent_state` | Agent self-reported state is ready/degraded/failed. |
| `active_deployment` | The agent has an active deployment promoted. |
| `worker_state` / `worker_crash_loop` | Supervision summary, crash-loop flag. |
| `host_facts` | Detected CPU architecture and vendor, read from the machine through `tensorplate-platform` — not from the binary's build target. `warning` when host identity could not be detected at all, with a hint that distinguishes a source that could not be *read* from one that was readable but not *interpretable*. A third hint covers a Compute Engine instance whose machine type could not be established: the metadata service could not be reached and `tensorplate-agent` has no usable record of an earlier answer, or the kernel boot ID, CPU count, `MemTotal` or NVIDIA devices it recorded no longer match. Detection never reports such an instance without a machine type, because a shape-scoped row would then admit it as unvalidated. The fix is one agent start in the current boot while the metadata service is reachable. Repeat after every OS reboot; offline cold boot is not supported by this record. When the record exists but `doctor` cannot read it, the hint says to re-run as root or as a member of the `tensorplate` group, which owns `/var/lib/tensorplate/state`. |
| `host_os` | Detected OS identity (name, version, and image identity where the platform has one), plus the cloud machine shape as ` on <machine-type>` where the host reports one — that shape decides whether shape-scoped rows can match. On Compute Engine the shape is followed by its source: ` (from GCE metadata)` for a live answer, or ` (recorded from GCE metadata by tensorplate-agent; metadata service unreachable; same kernel boot; CPU count, MemTotal and NVIDIA devices unchanged)` when the metadata service could not be reached and detection used the machine type the agent recorded from an earlier live answer. The exact version, build, and L4T release follow in brackets for evidence recording. |
| `accelerator_facts` | Detected accelerator SKU and device count, reported separately from the support verdict. Single-device and homogeneous multi-device hosts name their SKU and count; mixed-SKU sets identify the named SKU as the first device's only, and partitioned sets are marked `(partitioned)`. Always `info`, with `ok` for readable facts. When neither an accelerator nor NVIDIA PCI evidence is detected, it says `no accelerator detected`. If NVIDIA hardware is visible on PCI but its accelerator identity is unavailable, it is `skipped` and directs the operator to `platform_row`; it does not infer a SKU or device count from PCI functions. Host or accelerator probe failures also produce `skipped`. This finding appears on every host-detection outcome and does not change doctor's exit status. |
| `agent_config_valid` | Whether `/etc/tensorplate/agent.json` still satisfies the agent config schema. `skipped` when there is no agent config, which is normal for a CLI-only install; `unsupported` with every schema problem listed when it does not validate. Worth running BEFORE an upgrade: the agent refuses a config it cannot validate, including one carrying an unknown key, and the upgrade preflight checks only `schema_version`. The CLI validates against the schema rather than the agent's own loader because `cli/` may not depend on `agent/`; a contract test holds the two in agreement. |
| `platform_profile` | Which support rows the detected host could be. Deliberately a **set**: rows sharing an OS and CPU profile differ only by accelerator, so naming one would assert a match that has not been established. `unsupported` with a typed reason when the host matches no row; `unsupported` naming the machine shape when the hardware matches but no row's evidence covers the chassis it runs on — the hardware is validated, that shape is not, which does **not** by itself mean the machine will not run: where the matched row records its thermal, power and throttle signals as context rather than gates, the agent admits it on technical prerequisites and reports it as unvalidated at startup, while a row that gates on those signals requires evidence covering the machine. This finding is the host-level answer, taken before any accelerator is identified, so it cannot say which of the two applies; `skipped` when the registry could not be loaded (see `platform_registry` for why). Never `fail`: an off-matrix host is a reportable state, not a fault in `doctor`. |
| `platform_row` | The one support row the detected host **and accelerator** resolve to, or the typed reason they resolve to none. `platform_profile` answers from host identity alone and must report a *set*; this is the answer that set defers to, and it is where an exact-equality miss on either half becomes visible — a near-miss OS version or an off-matrix SKU resolves to no row with the reason naming which dimension missed. `unsupported` for a Planned or Experimental row (named, no validation evidence), for a machine shape no evidence covers, and for no match at all; `skipped` when the registry could not be loaded. Kept separate from `platform_profile` deliberately: an operator whose accelerator probe fails still gets the host-level answer, and the pair says which half of the identity was the problem. An unreadable accelerator probe plus independent NVIDIA PCI evidence is reported `unsupported` with `missing_driver_runtime`. Every returned accelerator row must parse: a malformed row, unusable product name, or unknown MIG state on any device remains a detection warning and never blames the driver. A readable multi-GPU host is `unsupported` with `unsupported_accelerator_topology` and a hint explaining the one-accelerator-per-host constraint; MIG enabled on any device takes precedence and reports `mig_mode_enabled`. |
| `model_class_rows` | Which model classes the matched row serves and at what support level, read from the row's `model_class_rows` registry pointers rather than a list kept in the CLI — a row that gains or loses a model class changes this without code. It consumes the same guarded resolution as `platform_row`, so a driverless GPU cannot resolve this dependent finding to a CPU row. `skipped` when no row matched or detection failed (the `platform_row` finding already carries why). A row that claims none says so plainly rather than rendering empty: a Planned row carries no model-class claims, and the registry refuses to let it. |
| `core_packages` | packaging. On the Debian package target, `tensorplate-common`, `-agent`, `-serving`, `-observability`, and `-cli` are installed and versioned. |
| `path_layout` | packaging. Every directory under `/etc/tensorplate`, `/var/lib/tensorplate`, `/var/log/tensorplate`, `/run/tensorplate`, `/usr/share/tensorplate/backends`, and `/usr/share/tensorplate/platform` is present, not world-writable, has the documented mode, and uses the expected owner/group on Linux. |
| `platform_registry` | packaging. The platform support registry at `/usr/share/tensorplate/platform` loads, and reports its row, supported-combination, and roadmap-target counts. `missing` when no registry is installed; `warning` when it is installed but this account cannot read it (it ships group-readable, so run as root or as a member of the `tensorplate` group — the registry itself is not inspected and is not claimed to be bad); **fails** when one is installed and readable but does not load — an invalid document, a collision between two rows, or no rows at all. The registry loads whole or not at all, so a partial load is never reported as a smaller registry. |
| `config_files` | packaging. Each `/etc/tensorplate/*.json` exists, has the documented file mode/ownership, and declares a recognized `schema_version`. |
| `config_endpoints` | packaging. Installed agent, serving-worker, and observability configs keep first-run endpoints on a Unix socket, loopback, or in-process transport. |
| `agent_systemd_unit` | packaging. `tensorplate-agent.service` is installed under a known systemd unit directory. |
| `agent_service_state` | packaging. Reports whether the agent is running, asked of the supervisor that owns it: `systemctl is-active` on Linux, `brew services list` on macOS, where the agent is a Homebrew-managed launchd job. Both Homebrew service findings share one validated listing; a failed query is `skipped`, not misreported as an absent service. A host without the supervisor is `skipped` rather than failed — the CLI runs on machines that never installed the services. |
| `observability_systemd_unit` | packaging. `tensorplate-observability.service` is installed. |
| `observability_service_state` | packaging. As `agent_service_state`, for the independent observability service. |
| `serving_systemd_absent` | packaging. **Fails** if `tensorplate-serving.service` is installed — the agent supervises the serving worker (V01-E09). |
| `serving_binary_installed` | packaging. `/usr/lib/tensorplate/tensorplate-serving` exists. |
| `python_pytorch_backend` | Packaging probe. The backend descriptor at `/usr/share/tensorplate/backends/python_pytorch/backend.json` is present and parses. |
| `python_pytorch_runtime` | packaging. The descriptor's Python interpreter exists, meets the declared minimum Python version, imports the declared backend module, and imports PyTorch at or above the declared minimum version. |
| `cuda_runtime` | packaging. Whether this host has the CUDA runtime the **installed serving build** needs, which is not the same question on every architecture. The NVIDIA driver and a system CUDA toolkit are reported as separate facts, named by the path each was found under, because different packages install them: `libcuda` is the driver's own user-mode library and is not a toolkit, and a versioned `libcudart.so.<soname>` counts, so a host carrying only the CUDA runtime package is not reported as carrying nothing. Each list also falls back to a scan of its own library directories for a versioned soname — `libcuda.so.<soname>` for the driver, `libcudart.so.<soname>` for the toolkit — because the exact names are not guaranteed: `/proc/driver/nvidia/version` comes from `nvidia.ko`, which L4T does not load, so a Jetson is recognized by its `libcuda` alone and a layout shipping only `libcuda.so.1.1` must not read as driverless. On x86_64 the opposite holds and the finding says so: `/proc/driver/nvidia/version` is what establishes a loaded driver there, because the driver's user-mode package installs `libcuda.so.1` with no working kernel module — a driver upgrade awaiting a reboot, Secure Boot refusing the module, or `libnvidia-compute-*` pulled onto a GPU-less VM. That library is probed separately and reported as its own state, `NVIDIA driver libraries at <path> but no /proc/driver/nvidia/version`, with the verdict a driverless host gets and a hint naming the kernel module; it is never reported as a driver present. `packaging/scripts/install.sh` refuses the same inference. The verdict is taken against the build: the arm64 (Jetson) worker is built with the TensorRT adapter and links the CUDA runtime, so a missing toolkit there is `warning` with the install hint — that adapter cannot load without it; the amd64 worker is built without the TensorRT adapter and reaches the accelerator through the python_pytorch sidecar, whose CUDA build of PyTorch ships its own runtime inside the wheel, so a missing system toolkit there is `ok` and says so rather than claiming CUDA was not detected. The NVIDIA driver governs the status on its own: `libcuda` comes from the driver package and every CUDA consumer loads through it, so a host with no driver — including one carrying only the x86_64 user-mode library — is never `ok` however many toolkit files it carries: on amd64 it is `missing` and on the TensorRT-linked build a `warning`, each naming what was and was not found as the fact that matters. The message states only what is installed here. Wherever the verdict rests on the python_pytorch sidecar — the amd64 build, whose accelerator path it is, and any host where it is the only CUDA consumer — the message names it as present or absent rather than assuming it (see `python_pytorch_backend`); on a Jetson with the serving worker installed the verdict rests on that worker's TensorRT adapter and the message is about the worker. Where neither the serving worker nor the python_pytorch sidecar is installed — a `--cli-only` install — the finding is `skipped`, because nothing on the host consumes a CUDA runtime. The sidecar on its own keeps the finding: its package only `Recommends` the serving one, so it can be installed without a worker, and it is then the host's only CUDA consumer. What the absent worker's build would have wanted is not asked for there, but the sidecar's own wheel is, and that is an architecture question rather than a build one: the x86_64 PyPI CUDA wheel carries its runtime libraries, so driver and no toolkit is `ok`, while NVIDIA's Jetson wheel links the JetPack CUDA runtime — [docs/install/python-pytorch-backend.md](../install/python-pytorch-backend.md) has the operator `apt install` it and says `import torch` fails on a missing CUDA shared library without it — so on the arm64 artifact set the same host is a `warning` pointing at that apt list. On a platform whose shipped build has no CUDA path at all (macOS, which serves on Metal) it is also `skipped`. Always `info` except the TensorRT-linked case above, and never `fail`: a host prerequisite is a reportable state, not an install fault. |
| `tensorrt_runtime` / `libtorch_runtime` | packaging. Best-effort artifact presence (paths only — actual validation happens in release validation). Neither is build-aware yet, so on a build with no TensorRT adapter (macOS and amd64) `tensorrt_runtime` still reports `TensorRT not detected; vision-on-TensorRT validation will skip` in the same report where `cuda_runtime` says this build has no TensorRT path — tracked with the amd64 adapter question in #204, not fixed here. |
| `ros2_health_stub` | Packaged observability config exposes the optional ROS 2 health-stub section; runtime publications remain visible in `tensorplate status`. |

Each finding has a stable `id`, `status` (`ok`, `fail`, `missing`, `unsupported`,
`skipped`, `warning`), `severity` (`info`, `warning`, `critical`), human
`message`, and optional `hint`. JSON output preserves all fields so release validation
scripts can grep on `id` strings.

## Exit codes

- `0` if no finding has `status: fail`.
- `10` if at least one finding is failing (see [exit-codes.md](./exit-codes.md)).

## Limitations

- CUDA / TensorRT / LibTorch checks assert *artifact presence* only. Functional
  validation happens in release validation. `cuda_runtime` interprets that
  presence against the installed build, but it still reads paths: it never
  queries the driver and never imports the sidecar's PyTorch, so it does not
  establish that PyTorch can reach the accelerator. Nothing in `doctor`
  does today — `python_pytorch_runtime` imports `torch` and reads its
  version, not `torch.cuda`.
- Which serving build is installed is read from this CLI's own build
  target, because the CLI and the serving worker are installed from the
  same per-architecture artifact set. What that build carries is read
  from the release build configuration, which is a default rather than a
  fixed value on the arm64 path: `tools/release/build-release-artifacts.sh`
  emits `${TP_ENABLE_TENSORRT:-ON}` there and refuses the environment
  override only on the amd64 path. So an arm64 artifact set built with
  `TP_ENABLE_TENSORRT=OFF`, like a worker rebuilt from source with
  different adapter flags, is not reflected here: `cuda_runtime` would
  ask for a CUDA runtime that worker does not link. `host_facts`
  answers the other question — what the machine is — and is read from the
  machine, never from the build target.
- The PyTorch runtime check shells out to the interpreter pinned in the
  backend descriptor (e.g. `/usr/bin/python3 -c 'import torch; ...'`). It
  never executes user model code; refused module names that fail an
  identifier-safety check fail as `python_pytorch_runtime = fail`.
- `--record <dir>` runs no checks at all: it captures this machine's raw
  platform sources — the same files and command output detection reads — as
  private evidence under `<dir>`. The JSON and accelerator text use the
  shapes consumed by `test/platform/host_identity/` and
  `test/platform/accelerator/`, but they are **not committable as-is**: the
  command warns that the raw files can contain live cloud identifiers,
  device UUIDs, or serials. Create and review a sanitized publication copy
  under the [fixture and evidence rules](../validation/fixture-and-evidence-rules.md)
  before its first commit; retain the unsanitized capture privately. For
  a machine-type record, replace the source boot ID and the embedded record
  boot ID consistently with a canonical synthetic UUID such as
  `00000000-0000-0000-0000-000000000001`. Preserve whether the two values
  match; a sanitized fixture exercises that comparison, not real boot
  continuity.
  Record-first: all raw accelerator rows are preserved, including on
  unsupported machines such as a multi-GPU host or one with an unknown
  SKU. A readable multi-GPU answer is interpreted and refused for its
  topology, not described as a detection failure. When a source answer
  cannot be interpreted, such as a malformed accelerator row, the failure
  becomes a note and the raw text is still recorded. A missing machine-type
  record, or a readable regular record within the size limit whose JSON or
  facts are unusable, also produces a note. An unreadable, oversized,
  non-regular, or symlinked record path aborts before fixture creation. When the
  machine resolves to a support row, the files are named for the row and the
  observed SKU is compared byte-for-byte against the row's declared one —
  a mismatch means the row gets corrected, never the recording. With a
  device configured, the command routes to the device and `<dir>` is a
  device-local path — the recording is written on the machine being
  recorded; fetch it from there.
- `--skip-agent` skips every agent-backed probe but still runs the install
  probes (packages, paths, configs, systemd units, service state, backend
  descriptor). Use it on a
  development host where no agent is running yet.
- The install probes degrade to `missing` (not `fail`) when no install layout
  is detected at all — that lets host CI on macOS / non-Linux dev hosts pass
  without a real package install. A *partial* install — some directories
  present but not all — still surfaces as `fail`.
