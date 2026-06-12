# TensorPlate External Quickstart

This quickstart assumes the packages were installed through the APT path
in [`tensorplate-ready.md`](./tensorplate-ready.md) (or the
[`external-install.md`](./external-install.md) GitHub-assets fallback)
and that `tensorplate doctor` is green after service startup. Set
`TP_VERSION` to the installed release; the examples default to `0.1.0`.

## Release Variables

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
export TP_DEBIAN_VERSION="${TP_VERSION}-1"
export TP_ARCH=arm64
export TP_REPO=tensorplate/tensorplate
export TP_RELEASE_URL="https://github.com/${TP_REPO}/releases/download/${TP_TAG}"
export TP_SAMPLE_BUNDLE_ARCHIVE="tensorplate-trt-identity-bundle-${TP_TAG}-jetson-orin.tar.gz"
export TP_SAMPLE_BUNDLE_DIR="tensorplate-trt-identity-bundle"
```

## Check Status

```bash
tensorplate status
```

Expected summary:

- Agent is reachable.
- No active deployment is a valid first-run state.
- Observability status is available.
- Local endpoints remain local-only.

## Deploy A Released Sample Bundle

Use a bundle asset published with the GitHub Release or another
documented bundle that declares a v0.1 bundle format and a supported
backend. The clean-room release smoke should record the exact bundle URL.

Example using a release-attached TensorRT identity bundle:

```bash
mkdir -p "/tmp/tensorplate-${TP_TAG}-quickstart"
cd "/tmp/tensorplate-${TP_TAG}-quickstart"
curl -fL -O "${TP_RELEASE_URL}/${TP_SAMPLE_BUNDLE_ARCHIVE}"
tar xzf "${TP_SAMPLE_BUNDLE_ARCHIVE}"

tensorplate deploy "./${TP_SAMPLE_BUNDLE_DIR}" --deployment-id quickstart-trt-identity
```

If the final release does not publish a sample bundle asset, the release
notes must name the validated external bundle source before this
quickstart can be used as release evidence.

## Run Inference

```bash
tensorplate infer \
  --input "./${TP_SAMPLE_BUNDLE_DIR}/sample_infer.json" \
  --output-file ./quickstart-response.json \
  --output json
```

Expected summary:

- Command exits successfully.
- Response JSON contains the declared output tensor names.
- Request count increments in status and metrics.

## Inspect Status, Logs, And Metrics

```bash
tensorplate status --output json
tensorplate logs --component agent --tail 100
tensorplate logs --component observability --tail 100
```

The status output should show the active deployment, backend, request
counters, health state, and no failed requests after the sample inference.
Logs should not contain panic-level failures or unbounded payload dumps.

## Optional Python/PyTorch Path

Install `tensorplate-backend-python-pytorch` and the platform PyTorch
runtime before deploying a `python_pytorch` bundle:

```bash
sudo apt install "./tensorplate-backend-python-pytorch_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
tensorplate doctor
sudo systemctl restart tensorplate-agent
```

Deploy only a bundle whose `backend_hint` is `python_pytorch` and whose
runtime dependencies are present. Doctor must report the backend and
runtime as `ok` before the deploy is expected to pass.

## Roll Back

If a previous active deployment exists:

```bash
tensorplate rollback --deployment-id <previous-deployment-id>
tensorplate status
```

Expected summary:

- Active deployment changes back to the previous deployment.
- Health returns to `ready`.
- Logs contain the rollback transaction without service crashes.

## Known v0.1 Limitations

- No hosted platform connection is required or supported.
- No APT repository is published; install from release assets.
- Kria and Vitis AI execution are not supported.
- The Python/PyTorch package does not install PyTorch.
- Public network exposure of local endpoints is unsupported.
