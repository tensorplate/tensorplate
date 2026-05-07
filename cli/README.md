# `cli/`

`tensorplate-cli` — the Rust operator command-line interface. Single-device
operator client for v0.1.0; targets exactly one reachable `tensorplate-agent`
endpoint at a time.

## Ownership

- **Layer:** management plane (operator client)
- **Language:** Rust
- **Cargo crate:** `tensorplate-cli` (binary)

## Scope (v0.1.0)

- `tensorplate doctor`
- `tensorplate deploy <bundle>`
- `tensorplate status`
- `tensorplate infer`
- `tensorplate logs`
- `tensorplate rollback`

## Dependency direction

```
cli/  ──(local control API)──>  agent/
```

The CLI never mutates the serving worker directly. All state-changing
operations route through the agent control API. Device access profiles
(`local`, `ssh-tunnel`, `overlay`, `relay`) are loaded from config; v0.1.0
implements `local` and explicit URL targeting first.

## Rules

- No cloud-backed auth, device registry, or fleet inventory in OSS v0.1.0.
- CLI talks only to one agent at a time.

Implementation lands in V01-E11.
