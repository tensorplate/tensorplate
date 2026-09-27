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
3. Agent-discovered active deployment: the CLI asks the agent for status and
   uses `AgentStatus.active.serving_url` when the agent reports one. A
   resident set reports it only for a set of one serving member whose unary
   endpoint is `http://127.0.0.1:<port>` or `http://localhost:<port>` with
   no path. When status lists a `resident_set` but no `serving_url` (more
   than one member; an only member that is quarantined, has no unary
   endpoint, or has one of any other form; or no member), the CLI refuses
   (`unavailable`) rather than guess, and asks for `--serving-url` with the
   endpoint of the worker to query, which `tensorplate status` lists.
4. Only when status lists no resident set, names an active deployment and
   reports no `serving_url` (an older agent, or one with no serving process
   of its own running, such as mock worker mode or an exited worker), the
   CLI falls back to the
   v0.1.0 default loopback serving endpoint
   (`http://127.0.0.1:18080/infer`).

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
| `6` | No active deployment and no override, or a resident set with no single loopback serving URL. |
| `11` | Serving worker returned a typed `failure` status. |

## Limitations

- No automatic retry, alternate backend selection, or shape-mismatch coercion.
- No streaming output rendering. v0.1.0 inference is sync request/response;
  the LeRobot async pattern is exposed via the serving worker's HTTP API but
  the CLI does not pretty-print accepted/result polling shapes.
- No tensor introspection. The CLI renders output names and shapes; payloads
  remain base64 inside the response.
