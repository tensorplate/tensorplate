# TensorPlate Clean-Room Release Smoke

This procedure validates the release as an external user. It starts from
GitHub Release assets and a clean device state. Local source-tree binaries,
local package build directories, and unpublished fixtures invalidate the
evidence. Set `TP_VERSION` to the release being validated; the examples
default to `0.1.0`.

## Release Variables

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
export TP_DEBIAN_VERSION="${TP_VERSION}-1"
export TP_ARCH=arm64
export TP_REPO=tensorplate/tensorplate
export TP_RELEASE_URL="https://github.com/${TP_REPO}/releases/download/${TP_TAG}"
export TP_MANIFEST="tensorplate-${TP_TAG}-artifacts.json"
export TP_EVIDENCE_DIR="dist/release/${TP_TAG}"
```

## Target

| Field | Required value |
| --- | --- |
| Hardware | Jetson Orin Nano 8GB Super |
| OS | JetPack 6.x with L4T 36.x |
| Packages | GitHub Release `.deb` assets for `TP_TAG` |
| Optional backend | `tensorplate-backend-python-pytorch` when validating a `python_pytorch` bundle |

## Evidence Record

Create `${TP_EVIDENCE_DIR}/clean-room.md` or attach an external archive
with these fields:

```text
Release: vX.Y.Z
Release decision: TODO
GitHub Release URL: TODO
Device model: TODO
Hostname: TODO
Ubuntu release: TODO
Kernel: TODO
L4T / JetPack: TODO
CUDA: TODO
TensorRT: TODO
Python: TODO
PyTorch: TODO or not-installed
Package versions: TODO
Artifact manifest: TODO
SHA256SUMS: TODO
Bundle source: TODO
Doctor before services: TODO
Doctor after services: TODO
Deploy result: TODO
Inference result: TODO
Status/logs/metrics result: TODO
Rollback result: TODO or not-applicable
Uninstall/reinstall result: TODO or not-run
Redactions applied: secrets, credentials, raw images, tensor payloads, unbounded logs
```

Allowed final release decisions are `pass`, `conditional-pass`, or
`block`. Publication requires `pass` or signed `conditional-pass`.

## Native Jetson Helper

For repeatable Jetson validation from a source checkout, use
[`tools/validation/jetson-clean-room.sh`](../../tools/validation/jetson-clean-room.sh).
The helper intentionally validates on the Jetson host rather than in a VM
or container, because TensorRT/CUDA runtime behavior is release-critical
and sandboxed package smoke is not equivalent to host GPU validation.

Run against a published release:

```bash
tools/validation/jetson-clean-room.sh run \
  --version "${TP_VERSION}" \
  --with-python-backend \
  --confirm RESET-TENSORPLATE
```

Run against a previously downloaded artifact directory:

```bash
tools/validation/jetson-clean-room.sh run \
  --assets-dir "/tmp/tensorplate-${TP_TAG}-assets" \
  --with-python-backend \
  --confirm RESET-TENSORPLATE
```

The confirmation token is required because `run` and `reset` stop
TensorPlate services, purge TensorPlate Debian packages, and remove only
TensorPlate-owned state under `/etc/tensorplate`, `/var/lib/tensorplate`,
`/var/log/tensorplate`, and `/run/tensorplate`. The helper writes
`clean-room.md` plus a bounded evidence archive under its work directory.
Use `--allow-unsigned` only for build-only artifact validation; do not use
it for public release signoff.

## Clean State

Start from a device without TensorPlate packages installed:

```bash
sudo systemctl stop tensorplate-agent tensorplate-observability || true
sudo apt purge -y tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate-common tensorplate-backend-python-pytorch || true
sudo rm -rf /etc/tensorplate /var/lib/tensorplate /var/log/tensorplate /run/tensorplate
```

Do not remove unrelated system packages, JetPack components, CUDA,
TensorRT, or the operator's Python/PyTorch runtime unless the validation
plan explicitly says so.

## Download And Verify Release Assets

Download the **complete** release asset set into a clean working directory.
`sha256sum -c SHA256SUMS` checks every file listed in `SHA256SUMS` — all
`.deb` packages (including `tensorplate-apt-source`, the `tensorplate`
metapackage, and the complete `amd64` runtime set), `install.sh`, the SDK
wheel + sdist, and
the manifest — so a partial download (for example the runtime subset in
[`docs/install/external-install.md`](../install/external-install.md), which
covers the trust model and signature steps) false-fails verification:

```bash
gh release download "${TP_TAG}" --repo "${TP_REPO}" --dir . \
  --pattern '*.deb' \
  --pattern 'install.sh' \
  --pattern 'tensorplate_python-*.whl' \
  --pattern 'tensorplate_python-*.tar.gz' \
  --pattern "${TP_MANIFEST}" \
  --pattern 'SHA256SUMS' \
  --pattern 'SHA256SUMS.cosign.bundle'
```

Record (every file listed in `SHA256SUMS` must be present, or `sha256sum -c`
fails):

```bash
sha256sum -c SHA256SUMS | tee "/tmp/tensorplate-${TP_TAG}-checksums.txt"
cp "${TP_MANIFEST}" "/tmp/tensorplate-${TP_TAG}-artifacts.json"
```

Any checksum mismatch blocks the release.

## Install And Doctor

Install core packages from the downloaded assets:

```bash
# tensorplate-common is Architecture: all; the rest are per-architecture
# and ${TP_ARCH} selects which set this host installs.
sudo apt install "./tensorplate-common_${TP_DEBIAN_VERSION}_all.deb" \
  "./tensorplate-agent_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-serving_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-observability_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-cli_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"

tensorplate doctor --output json | tee "/tmp/tensorplate-${TP_TAG}-doctor-before.json"
```

Install findings must not include `fail`. Service-state warnings before
startup are acceptable if documented.

## Start Services And Verify Local Defaults

```bash
sudo systemctl enable --now tensorplate-agent
sudo systemctl enable --now tensorplate-observability
tensorplate doctor --output json | tee "/tmp/tensorplate-${TP_TAG}-doctor-after.json"
test -S /run/tensorplate/agent.sock
```

Record service state:

```bash
systemctl status tensorplate-agent --no-pager > "/tmp/tensorplate-${TP_TAG}-agent-status.txt"
systemctl status tensorplate-observability --no-pager > "/tmp/tensorplate-${TP_TAG}-observability-status.txt"
```

Failed service start, failed doctor checks, or non-local endpoint drift
blocks publication unless explicitly signed as a conditional pass.

## Deploy, Infer, Status, Logs, And Metrics

The release does not ship a model bundle. Generate the TensorRT identity
bundle on the target with the sanctioned generator — the same one the
automated clean-room driver (`tools/validation/jetson-clean-room.sh`) uses —
and do not use `test/models/` from a development checkout:

```bash
tools/validation/create_trt_identity_bundle.sh ./tensorplate-trt-identity-bundle
```

```bash
tensorplate deploy ./tensorplate-trt-identity-bundle --deployment-id release-clean-room
tensorplate infer \
  --input ./tensorplate-trt-identity-bundle/sample_infer.json \
  --output-file "/tmp/tensorplate-${TP_TAG}-infer-response.json" \
  --output json | tee "/tmp/tensorplate-${TP_TAG}-infer.json"
tensorplate status --output json | tee "/tmp/tensorplate-${TP_TAG}-status.json"
tensorplate logs --component agent --tail 100 > "/tmp/tensorplate-${TP_TAG}-agent.log"
tensorplate logs --component observability --tail 100 > "/tmp/tensorplate-${TP_TAG}-observability.log"
```

Expected result:

- Deploy succeeds.
- Inference succeeds.
- Status reports active deployment health `ready`.
- Logs contain no panic-level failures.
- Metrics or status counters show successful request completion.

## Optional Python/PyTorch Validation

If the release quickstart includes a `python_pytorch` bundle, validate the
optional package:

```bash
# tensorplate-backend-python-pytorch is Architecture: all.
sudo apt install "./tensorplate-backend-python-pytorch_${TP_DEBIAN_VERSION}_all.deb"
tensorplate doctor --output json | tee "/tmp/tensorplate-${TP_TAG}-python-doctor.json"
sudo systemctl restart tensorplate-agent
```

Doctor must report `python_pytorch_backend = ok` and
`python_pytorch_runtime = ok` before deploying the bundle.

## SDK Wheel Verification

The `tensorplate-python` wheel + sdist are signed release assets covered by
`SHA256SUMS`. From the verified download, confirm the wheel installs cleanly
and reports the release version:

```bash
python3 -m venv /tmp/tp-sdk-clean-room && . /tmp/tp-sdk-clean-room/bin/activate
pip install "./tensorplate_python-${TP_VERSION}-py3-none-any.whl[vision]"
python -c "import tensorplate; assert tensorplate.__version__ == '${TP_VERSION}', tensorplate.__version__"
python -c "from tensorplate import ServingClient, VisionClient, YOLO26_E2E_DETECTIONS"
deactivate
```

The import must resolve `__version__ == ${TP_VERSION}` and the public symbols
(including the v0.1.4 `YOLO26_E2E_DETECTIONS` contract). The SDK's optional
binary `/infer` transport is exercised end-to-end in
[`sdk-e2e-validation.md`](./sdk-e2e-validation.md); record that evidence link
here.

## Rollback And Uninstall Evidence

When a previous deployment exists:

```bash
tensorplate rollback --deployment-id <previous-deployment-id>
tensorplate status --output json | tee "/tmp/tensorplate-${TP_TAG}-rollback-status.json"
```

Record uninstall or reinstall behavior where feasible:

```bash
sudo apt remove -y tensorplate-agent tensorplate-serving tensorplate-observability tensorplate-cli tensorplate-backend-python-pytorch
sudo apt install "./tensorplate-agent_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-serving_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-observability_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb" \
  "./tensorplate-cli_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
tensorplate doctor --output json | tee "/tmp/tensorplate-${TP_TAG}-reinstall-doctor.json"
```

## Redaction

Before attaching evidence, redact:

- SSH credentials, tokens, and private URLs.
- Raw image payloads.
- Full tensor payloads.
- User data from bundles.
- Unbounded journals.

Keep bounded command transcripts, package versions, release URLs, artifact
checksums, service states, doctor summaries, status summaries, and failure
reason records.
