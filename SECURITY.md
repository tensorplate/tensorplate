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

TensorPlate has not published a public release yet.

| Version | Supported |
| --- | --- |
| Unreleased main branch | Best effort |

This section will be updated once versioned releases begin.

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

Security fixes should avoid public exploit detail until a fix or mitigation is available. Public disclosure notes should include affected versions, impact, mitigation, and upgrade guidance once releases exist.
