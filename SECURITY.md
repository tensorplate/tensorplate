# Security Policy

## Reporting a Vulnerability

Please do not report security vulnerabilities through public GitHub issues.

Until a dedicated security contact is published, report vulnerabilities privately to the repository owner. Include enough detail to reproduce and assess the issue:

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
[`docs/release/v0.1.0-post-release.md`](docs/release/v0.1.0-post-release.md).
