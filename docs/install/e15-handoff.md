# V01-E14 → V01-E15 handoff

E14 closes out the v0.1.0 packaging and first-run install scope. This
document is the explicit handoff to the V01-E15 hardware-validation
gate.

## Artifacts E14 produces

| Artifact | Path | Owner |
| --- | --- | --- |
| Native package skeleton | `packaging/debian/` | F01 |
| Shared maintainer-script helpers | `packaging/scripts/` | F02 / F07 |
| Default install configs | `packaging/conf/*.json` | F04 |
| Backend descriptor | `packaging/backend-metadata/python_pytorch.json` | F05 |
| systemd units | `packaging/debian/tensorplate-{agent,observability}.service` | F03 |
| Packaging verification suite | `test/packaging/` | F08 |
| Operator docs | `docs/install/` | F02 / F03 / F05 / F07 / F08 |
| Doctor finding catalog | `docs/cli/doctor.md` | F06 |
| Single source of truth for paths | `protocol/rust/src/install_paths.rs` | F02 |

## What V01-E15 should run

1. **Clean-install runbook** — [`docs/install/clean-install-runbook.md`](./clean-install-runbook.md).
   Captures the canonical Jetson Orin Nano 8GB Super procedure plus
   the log-collection commands feeding the E15 validation report.
2. **Packaging verification suite** — `test/packaging/run.sh` on the
   target after `apt install`. Combined with the host-CI suite (which
   already passes), this gives full coverage of the F01-F07 contract.
3. **`tensorplate doctor` on a fresh install**, then post-deploy.
   Expected finding profiles documented in the runbook.
4. **Service start/stop probe**:
   ```bash
   sudo systemctl enable --now tensorplate-agent
   sudo systemctl status tensorplate-agent
   sudo systemctl restart tensorplate-agent
   sudo systemctl stop tensorplate-agent
   ```
   Each transition must complete within `TimeoutStartSec` / `TimeoutStopSec`
   as documented in `docs/install/services.md`.
5. **Bundle deploy** through the agent for the V01-E13 vision and
   SmolVLA fixtures. The agent's backend-probe gate (F05) must fire
   before staging when the Python/PyTorch backend is not installed.

## Package versions and runtime expectations

- Source / runtime version: `0.1.0~dev0` (`packaging/VERSION`,
  `protocol/rust/src/lib.rs::PROTOCOL_VERSION = "0.1"`,
  `CARGO_PKG_VERSION = "0.1.0-dev"`).
- Bundle format version: `BUNDLE_FORMAT_VERSION = "0.1"`.
- Schema version on every config / event: `0.1`.
- E15 may bump the runtime version when interfaces freeze. The
  packaging skeleton uses `packaging/VERSION` as the single source so
  updating it in one place propagates to `debian/changelog`.

## Hardware assumptions documented for E15

- Jetson Orin Nano 8GB Super or Orin NX 16GB.
- JetPack 6.x with the L4T 36.x BSP. The packaging tree does not
  assume any specific minor JetPack version; the
  V01-E15 validation will pin the exact JetPack build used.
- TensorRT, CUDA, and (optionally) LibTorch installed by the JetPack
  baseline. Doctor reports presence; functional validation lives in
  E15.
- Python 3.10+ for the `python_pytorch` backend. PyTorch wheel choice
  is platform-specific (Jetson aarch64 vs. CPU) — the descriptor's
  `python.interpreter` field pins which interpreter the sidecar
  uses; doctor probes that interpreter.

## Known risks

- **No publication path.** V01-E14 does not publish packages to APT,
  GitHub Releases, or any signed channel. E15 may need to set up a
  trusted local apt repository (or ship `.deb` files alongside the
  install runbook) to run the procedure on a network-isolated Jetson.
- **No live `dpkg-buildpackage` run on host CI.** The skeleton lints
  cleanly via `test/packaging/run.sh`, but the first end-to-end
  `dpkg-buildpackage` build happens on V01-E15's target host. Expect
  minor toolchain-specific fixes (e.g. `Depends:` shlib resolution).
- **PyTorch wheel selection is operator policy.** The package does
  not pull a wheel; the V01-E15 runbook annotates the exact wheel
  used during validation.
- **systemd hardening profile is intentionally conservative.** Sites
  running adjacent tooling that requires `MemoryDenyWriteExecute=false`
  (the default) or specific syscall whitelists may need drop-in
  overrides; E15 should document any drop-ins required for the
  SmolVLA sidecar.

## E14 has NOT validated

- Live install on the target. The hardware-in-the-loop run is the E15
  gate.
- TensorRT / LibTorch / CUDA functional execution.
- ROS 2 health stub native runtime (the `mock` runtime ships
  enabled-off in `observability.json`).
- Container-based install (deliberately out of scope for v0.1.0).
- APT repo signing / release engineering.

## Sign-off

E15 owners can sign off the E14 deliverable when:

1. `test/packaging/run.sh` is green on the target host.
2. `tensorplate doctor` reports a green profile on the clean Jetson
   install (no `fail` findings; `python_pytorch_*` may be `missing`
   if SmolVLA is not in scope for the run).
3. `systemctl enable --now tensorplate-agent` brings the agent to
   `ready` and the serving worker to a steady supervisor state.
4. A vision-on-TensorRT bundle deploys, serves, and is rollback-able
   per the V01-E15 vision validation procedure.
