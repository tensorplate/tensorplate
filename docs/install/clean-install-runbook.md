# TensorPlate v0.1.0 clean-install runbook (Jetson Orin Nano 8GB Super)

V01-E14-F08 deliverable. This is the procedure V01-E15 runs on the
target hardware to validate the v0.1.0 architecture loop end-to-end.

## 0. Prerequisites

| Item | Required |
| --- | --- |
| Jetson Orin Nano 8GB Super (or Orin NX 16GB for production tier) | yes |
| JetPack 6.x with the L4T 36.x BSP | yes |
| Networked apt repository serving the tensorplate packages | yes (or a local file copy of the `.deb`s) |
| Operator account with sudo | yes |
| `/var/lib/tensorplate` not present (clean install) | recommended |

The target ships pre-flashed in V01-E15. Operators on other hardware
follow the same steps; doctor probes will surface hardware
mismatches.

## 1. Install the core packages

```bash
sudo apt update
sudo apt install \
  tensorplate-common \
  tensorplate-agent \
  tensorplate-serving \
  tensorplate-observability \
  tensorplate-cli
```

What this does (the F01-F07 contract):

- Creates the `tensorplate` system user and group
  (`tensorplate-common.postinst`).
- Lays down `/etc/tensorplate/`, `/var/lib/tensorplate/{state,bundles/*,worker-configs}/`,
  `/var/log/tensorplate/`, and `/usr/share/tensorplate/backends/`
  with the documented permissions.
- Installs the four config files at `/etc/tensorplate/*.json`.
- Installs the systemd units for `tensorplate-agent` and
  `tensorplate-observability` but does **not** start them.
- Installs the serving worker binary at
  `/usr/lib/tensorplate/tensorplate-serving`. No serving systemd
  unit is registered (V01-E09).

## 2. Run `tensorplate doctor`

```bash
tensorplate doctor
```

Expected findings on a clean install with no Python/PyTorch backend
yet:

- `path_layout = ok`
- `config_files = ok`
- `config_endpoints = ok`
- `core_packages = ok`
- `agent_systemd_unit = ok`
- `agent_service_state = warning` until the unit is enabled in step 3
- `observability_systemd_unit = ok`
- `observability_service_state = warning` until the unit is enabled in step 3
- `serving_systemd_absent = ok`
- `serving_binary_installed = ok`
- `python_pytorch_backend = missing` (informational; install in step 5
  if SmolVLA validation is in scope)
- `cuda_runtime = ok` on a Jetson with JetPack
- `tensorrt_runtime = ok` on a Jetson with TensorRT
- `agent_reachable = fail` until the unit is enabled in step 3

If any of the install findings are `fail` (not `missing`), see the
troubleshooting section below before continuing.

## 3. Enable the systemd units

```bash
sudo systemctl enable --now tensorplate-agent
sudo systemctl enable --now tensorplate-observability
```

Either order works — observability declares no dependency on the
agent (V01-E10 / F03).

Re-run `tensorplate doctor`; `agent_reachable` should now be `ok`,
`agent_state` should be `ready`, and both `*_service_state` findings
should be `ok`.

## 4. Deploy a vision bundle (V01-E15 happy path)

```bash
# An example detector bundle is staged in test/models/bundles/v01_e13/.
sudo cp -r /path/to/yolov8n_tensorrt /var/lib/tensorplate/bundles/staging/
sudo chown -R tensorplate:tensorplate /var/lib/tensorplate/bundles/staging/yolov8n_tensorrt

tensorplate deploy /var/lib/tensorplate/bundles/staging/yolov8n_tensorrt
tensorplate status
tensorplate infer --input /path/to/sample.json
```

## 5. (Optional) Install the Python/PyTorch backend for SmolVLA

```bash
sudo apt install tensorplate-backend-python-pytorch
# Then install PyTorch into the descriptor's interpreter; see
# docs/install/python-pytorch-backend.md.
```

Re-run `tensorplate doctor`. Both `python_pytorch_backend` and
`python_pytorch_runtime` should now be `ok`. Restart the agent after the
runtime finding turns green so deploy checks refresh the startup probe:

```bash
sudo systemctl restart tensorplate-agent
```

## 6. Collect logs for the E15 handoff

```bash
journalctl -u tensorplate-agent --no-pager > /tmp/agent.log
journalctl -u tensorplate-observability --no-pager > /tmp/observability.log
tensorplate doctor --output json > /tmp/doctor.json
tensorplate status --output json > /tmp/status.json

tar czf /tmp/tensorplate-e15-handoff.tar.gz \
  /tmp/agent.log /tmp/observability.log \
  /tmp/doctor.json /tmp/status.json
```

Attach the archive to the E15 validation report.

## Troubleshooting

| Symptom | Where to look |
| --- | --- |
| `tensorplate doctor` reports `path_layout = fail` | Run `sudo /usr/share/tensorplate/packaging/scripts/install-paths.sh` to recreate the layout. If `--reinstall tensorplate-common` does not fix it, file an issue with `tensorplate doctor --output json` attached. |
| `config_files = fail` | A config file is missing or has an unknown `schema_version`. Reinstall the owning package (`apt install --reinstall tensorplate-agent`) or restore from `/etc/tensorplate/<name>.json.dpkg-old`. |
| `serving_systemd_absent = fail` | An old or third-party unit named `tensorplate-serving.service` is installed. `sudo systemctl disable --now tensorplate-serving` then remove it. The agent owns the serving worker. |
| `agent_reachable = fail` after `systemctl enable --now` | `journalctl -u tensorplate-agent` for the typed startup error. Common causes: misconfigured `agent.json` (run `tensorplate doctor` for the validator output), `/run/tensorplate` group ownership mismatch (the postinst should handle this — file an issue if it doesn't). |
| `python_pytorch_backend = fail` | The descriptor parsed badly or PyTorch failed to import. See `docs/install/python-pytorch-backend.md`. |
| `tensorplate deploy` rejects a bundle with `BackendUnrunnable` | The bundle's `backend_hint` is declared but the agent's startup probe found it non-runnable. Doctor's `python_pytorch_runtime` / matching descriptor finding has the detail. |

## Known unsupported environments

- Non-Linux hosts. The doctor still runs and reports the install probes
  as `skipped`/`missing`, but no service can be enabled.
- Kria K26/K24 boards (post-v0.1.0 target). Doctor recognizes
  `target_hardware.device_family = "kria"` in a bundle and rejects it
  because there is no Vitis AI adapter in v0.1.0.
