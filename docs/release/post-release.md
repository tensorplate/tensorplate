# TensorPlate post-release support and hotfix procedure

This procedure is reusable across release lines. Each GitHub Release must
state its supported artifacts, hardware floor, OS/runtime assumptions,
known limitations, and security posture.

## Support Posture

Support applies only to artifacts published under immutable final release
tags and named in that release's artifact manifest.

For the v0.1.x line, support currently applies to:

- Jetson Orin Nano 8GB Super hardware floor.
- JetPack 6.x with L4T 36.x.
- `arm64` Debian packages attached to the final GitHub Release.
- From v0.1.2: the stable APT channel at
  `https://packages.tensorplate.com/apt` (`jammy/main`), the
  `tensorplate-apt-source` bootstrap package, the `tensorplate` runtime
  metapackage (`arm64`), the `tensorplate-cli` AMD64 workstation
  package, and the macOS Apple Silicon CLI via the
  `tensorplate/homebrew-tap` Homebrew tap (CLI-only).
- Core services: `tensorplate-agent`, `tensorplate-serving`,
  `tensorplate-observability`, and `tensorplate-cli`.
- Optional package: `tensorplate-backend-python-pytorch`, with PyTorch
  installed separately by the operator.

Best-effort support may apply to related Orin hardware when the same
JetPack and package set are used. Kria, Vitis AI execution, hosted fleet
management, container-only install, and public network endpoints are not
supported by v0.1.x unless a later release explicitly says otherwise.

## Hotfix Policy

Use a hotfix only for a targeted defect in released artifacts. Do not use
hotfix branches for unrelated feature work.

| Item | Policy |
| --- | --- |
| Branch | `hotfix/vX.Y.Z`, branched from and **merged into the trunk before tagging**. The release workflow rejects a tag that is not an ancestor of `develop`, so an unmerged hotfix branch cannot be released from. |
| Tag | `vX.Y.Z`, annotated, created on the trunk after the hotfix merges. |
| Changelog | Add a dated `X.Y.Z` section. |
| Version metadata | Runtime/package patch version becomes `X.Y.Z`; protocol/schema/bundle versions move only if the fix changes those surfaces. |
| Validation | Run required CI plus the smallest release validation slice that proves the fix. |
| Artifacts | Rebuild packages from the trunk commit carrying the fix, generate a new manifest and checksums, and publish under the new tag. |

Hotfixes preserve frozen public contracts for the release line unless a
security fix requires a documented exception.

## Artifact Deprecation And Yanking

Never replace a published final-release asset under the same tag.

For severe defects:

1. Publish a GitHub Release notice that marks the affected asset as
   deprecated.
2. Add an advisory or known-issue note to the release notes.
3. Open a hotfix issue and branch.
4. Publish fixed artifacts under a new patch tag.
5. Keep the original manifest and checksums available for auditability.

If an asset must be removed for legal or security reasons, record the
reason in the release evidence and publish a replacement under a new tag.

## User Rollback Guidance

Rollback should preserve user state unless the operator explicitly purges
packages. Follow the lifecycle policy in
[`docs/install/lifecycle.md`](../install/lifecycle.md).

Package rollback procedure. The installed packages must be REMOVED first:
the upgrade preflight refuses a downgrade on the version comparison alone,
so installing the older packages over the newer ones is rejected even with
`--allow-downgrades`. `remove` keeps `/etc/tensorplate` and
`/var/lib/tensorplate`, so operator config and durable state survive.

Follow the commands under "Downgrade and rollback" in
[`docs/install/lifecycle.md`](../install/lifecycle.md): stop both services,
move durable state aside to `state.bak`, copy the Compute Engine
machine-type record back out of it when there is one, remove every installed
TensorPlate package except `tensorplate-apt-source` — including
`tensorplate-common` and `tensorplate-backend-python-pytorch` — and install
the previous release fresh. That section also covers what happens to the set-aside state.

Deployment rollback uses the CLI path from
[`docs/cli/deploy-rollback.md`](../cli/deploy-rollback.md):

```bash
tensorplate rollback --deployment-id <previous-deployment-id>
tensorplate status
```

Do not bypass package ownership by manually editing files under
`/usr/lib/tensorplate` or `/usr/share/tensorplate`.

## Security Advisory Procedure

Security-sensitive reports follow `SECURITY.md`.

1. Keep exploit details private until a fix or mitigation is available.
2. Identify affected versions and packages.
3. Prepare a targeted fix and changelog entry.
4. Run relevant CI and validation slices.
5. Publish an advisory with impact, affected versions, mitigation,
   upgrade or rollback guidance, and fixed tag.

## Post-Release Monitoring Checklist

- New install failures.
- `tensorplate doctor` finding regressions.
- Service start or endpoint bind failures.
- Checksum or artifact download problems.
- Security reports.
- Docs corrections.
- Conditional-pass follow-up issues.
- Requests for unsupported hardware or model classes that should feed the
  next planned release line, not hotfix scope.
