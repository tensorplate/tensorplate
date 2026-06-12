# Security Policy

## Reporting a Vulnerability

Please do not report security vulnerabilities through public GitHub issues.
Use GitHub private vulnerability reporting for this repository when it is
enabled. If that path is unavailable, email `security@tensorplate.com`.
Include enough detail to reproduce and assess the issue:

- Affected component or package path.
- Branch, commit, release, or tag.
- Hardware target, operating system, and relevant runtime versions.
- Steps to reproduce.
- Impact assessment.
- Any known workaround or mitigation.

If you are unsure whether something is security-sensitive, report it privately first.

## Supported Versions

TensorPlate v0.1.0 is the first supported release line once the
`v0.1.0` GitHub Release is published. Until that tag and release page are
public, the branch is in release-candidate preparation.

| Version | Supported |
| --- | --- |
| `v0.1.x` | Supported for the published GitHub Release assets on the validated Jetson Orin Nano 8GB Super / JetPack 6.x floor. |
| Unreleased `develop` branch | Best effort |
| Older development snapshots | No |

Support applies to the release artifacts named in the GitHub Release
manifest. It does not imply support for Kria, Vitis AI execution, hosted
fleet control, container-only install, public network exposure of local
endpoints, or third-party PyTorch wheels.

## Release Integrity and Authenticity

Every published release attaches, alongside the `.deb` packages and
`install.sh`:

- `SHA256SUMS` — checksums for the manifest and all package assets.
- `SHA256SUMS.cosign.bundle` — a keyless [cosign](https://docs.sigstore.dev/cosign/installation)
  signature over `SHA256SUMS` (a self-contained Sigstore bundle: signature,
  Fulcio certificate, and Rekor transparency-log inclusion proof).
- SLSA build-provenance attestations for every `.deb`, `install.sh`, the
  manifest, and `SHA256SUMS`, generated with
  [`actions/attest-build-provenance`](https://github.com/actions/attest-build-provenance).

The signature and provenance bind the artifacts to the TensorPlate release
workflow (`.github/workflows/release.yml`) running on a `vX.Y.Z[-rc.N]` tag,
via the GitHub Actions OIDC identity. This provides authenticity, not just
integrity: a tampered or re-hosted `SHA256SUMS` cannot be re-signed without
that workflow identity. The release `install.sh` verifies the cosign
signature by default before trusting any checksum. If `cosign` is not
already installed, the installer downloads a pinned Linux `arm64`/`amd64`
cosign binary to its temporary work directory, verifies the pinned SHA256
for that binary, and runs it from there. Signature verification still fails
closed unless `--allow-unsigned` is passed.

Verify a release manually:

```bash
# 1) Authenticity: verify the signature over the checksum manifest.
cosign verify-blob \
  --bundle SHA256SUMS.cosign.bundle \
  --certificate-identity-regexp '^https://github.com/tensorplate/tensorplate/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS

# 2) Integrity: verify each asset against the now-trusted manifest.
sha256sum -c SHA256SUMS   # macOS: shasum -a 256 -c SHA256SUMS

# 3) Provenance: confirm an asset was built by the release workflow.
gh attestation verify tensorplate-agent_0.1.0-1_arm64.deb \
  --repo tensorplate/tensorplate
```

A dependency SBOM (SPDX/CycloneDX) attested with `actions/attest-sbom` is on
the roadmap; until then, provenance attestations capture build materials and
the manifest records package versions and digests.

Hosts installed through the APT channel (`packages.tensorplate.com`) get
this verification automatically: repository metadata (`InRelease`) is
signed with the TensorPlate archive key and validated by APT against the
keyring shipped by `tensorplate-apt-source` (Deb822 `Signed-By`;
`apt-key` is never used), and the repository is generated exclusively
from cosign-verified release assets. Trust model and validation
checklist: [`docs/release/apt-repository.md`](docs/release/apt-repository.md).

## Security-Sensitive Areas

Treat these areas as security-sensitive:

- Device agent deployment, rollback, authentication, and control-plane communication.
- Serving-worker local control IPC.
- Observability heartbeat, fatal-error reporting, and safe-state signaling.
- Config parsing and schema validation.
- Protocol schemas and generated bindings.
- Model loading and backend adapter boundaries.
- File paths, device paths, and hardware resource ownership.

## Secure Development Expectations

- Validate config and protocol inputs at boundaries.
- Keep deployment-specific behavior in config, not hidden branches.
- Use typed errors and structured logs without leaking secrets.
- Avoid raw ownership transfer across C++/Rust or adapter boundaries.
- Do not pass vendor SDK handles or raw hardware resource pointers through public value objects.
- Add regression tests for security fixes when practical.

## Disclosure Process

Security fixes should avoid public exploit detail until a fix or
mitigation is available. Public disclosure notes should include affected
versions, impact, mitigation, upgrade or rollback guidance, and the fixed
release tag. Patch releases follow the hotfix process in
[`docs/release/post-release.md`](docs/release/post-release.md).
