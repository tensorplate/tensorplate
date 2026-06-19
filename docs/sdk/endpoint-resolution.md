# SDK endpoint resolution

`ServingClient` and `VisionClient` resolve the serving worker URL with the
**same precedence and URL canonicalization as `tensorplate infer`**, so the
SDK reaches the worker the CLI would. See
[`docs/cli/infer.md`](../cli/infer.md) for the CLI side.

## Precedence

Both clients take `serving_url=None` and resolve in this order:

1. **Explicit URL** — the `serving_url` argument (or `--serving-url` in the
   examples).
2. **CLI profile `serving_url`** — from the active profile in the CLI
   config.
3. **Agent-discovered active deployment** — a read-only `status` query to
   the local agent; if it reports an active deployment with a `serving_url`,
   that is used. Skipped when `discover=False`.
4. **Loopback default** — `http://127.0.0.1:18080`.

> **Deliberate deviation from the CLI:** agent discovery is *best-effort*.
> An unreachable or silent agent falls through to the loopback default
> rather than raising. Resolution only raises `EndpointResolutionError` for
> a malformed URL or a misconfigured CLI profile.

`resolve_serving_url(...)` is exported if you want the resolved
`ResolvedEndpoint` (its `url`, `host`, `port`, `path`, and `source`) without
constructing a client.

## URL canonicalization

`canonicalize_serving_url` matches the CLI exactly:

- Only `http://` is accepted — v0.1 serving is loopback HTTP. A
  non-`http://` URL raises `EndpointResolutionError`.
- A bare `http://host:port` gets the `/infer` path appended; a URL that
  already has a path keeps it.
- A missing port defaults to `80`; a non-numeric or out-of-range port
  raises.

```python
from tensorplate import canonicalize_serving_url

ep = canonicalize_serving_url("http://127.0.0.1:18080", "explicit")
ep.url     # "http://127.0.0.1:18080/infer"
ep.origin  # "http://127.0.0.1:18080"
```

## CLI config discovery

The CLI config is read from the `config_path` argument, else the
`TENSORPLATE_CLI_CONFIG` environment variable, else the built-in defaults —
there is **no arbitrary filesystem search**. The default `local` profile
discovers the agent over its Unix socket
(`/var/run/tensorplate/agent.sock`); a `url`-mode profile discovers over the
configured `agent_url` TCP address. Reserved profile modes
(`ssh_tunnel` / `overlay` / `relay`) are not used for discovery, mirroring
the CLI's "unsupported" stance.
