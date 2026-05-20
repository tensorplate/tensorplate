# `tensorplate infer`

Convenience inference workflow for operator-level validation. **Not** a
replacement for the serving worker's HTTP API or a future Python SDK.

```
tensorplate infer
  ( --input <path> | --stdin )
  [--serving-url <url>]              # http://host:port[/path]
  [--timeout-ms <n>]
  [--output-file <path>]             # also write the parsed response to this file
  [--output <human|json>]
```

## Endpoint resolution

Order of precedence:

1. `--serving-url <url>` flag.
2. `serving_url` field on the active profile.
3. Agent-discovered active deployment: the CLI asks the agent for status,
   confirms an active deployment exists, and uses the v0.1.0 default loopback
   serving endpoint (`http://127.0.0.1:18080/infer`).

Local profile (default) reaches loopback after a successful deploy. Explicit
`url` profiles require either a manual SSH tunnel to the serving endpoint or
a `serving_url` override.

## Input format

The CLI validates that the body is well-formed JSON before posting it. It does
not transform the payload — the body must already match the v0.1.0 serving
envelope (see [`protocol/schemas/serving_http_envelope.json`](../../protocol/schemas/serving_http_envelope.json)).
Tensor bytes are expected to be base64-encoded inside the named-input objects.

## Errors

| Exit | When |
| --- | --- |
| `2` | Input file missing, not JSON, or `--input`/`--stdin` mismatch. |
| `4` | Serving endpoint unreachable. |
| `6` | No active deployment and no override. |
| `11` | Serving worker returned a typed `failure` status. |

## Limitations

- No automatic retry, alternate backend selection, or shape-mismatch coercion.
- No streaming output rendering. v0.1.0 inference is sync request/response;
  the LeRobot async pattern is exposed via the serving worker's HTTP API but
  the CLI does not pretty-print accepted/result polling shapes.
- No tensor introspection. The CLI renders output names and shapes; payloads
  remain base64 inside the response.
