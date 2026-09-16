# `tensorplate logs`

Reads bounded NDJSON entries emitted by the agent, serving worker, and
observability service.

```
tensorplate logs
  [--component <name>]
  [--level <trace|debug|info|warn|error|critical>]
  [--since-ms <n>]
  [--tail <n>]                # default: log_source.tail_default in cli config
  [--follow]                  # tail -F semantics on a single file
  [--correlation-id <id>]
  [--source <path>]           # override cli config's log_source.path
  [--output <human|json>]
```

## Sources

- Path is taken from `--source`, otherwise `cli_config.log_source.path`.
- Path can be a single file (NDJSON) or a directory (the CLI reads files
  ending in `.ndjson`, `.log`, or `.jsonl`).
- v0.1.0 supports the **local** profile only. Remote profiles return exit
  code `6` (`unavailable`) with a hint to SSH to the device. V01-E12 will
  add an agent-side log API and unlock remote reads.

### Native Linux packages: no NDJSON source

The Debian packages configure **no** `log_source.path`, because nothing in
the product writes one there: `tensorplate-agent.service` and
`tensorplate-observability.service` set no `StandardOutput=`, the agent
writes its diagnostics to stderr, and the observability service's NDJSON
retention sink is off in the packaged config — and it only ever holds the
observability service's own events, since the v0.1 event listener is
in-process and the agent is a separate process.

So on a package install `tensorplate logs` exits `6` (`unavailable`) with a
hint naming the journal to read instead — `journalctl -u tensorplate-agent`
for the agent and for the serving worker and backends it supervises,
`journalctl -u tensorplate-observability` for the observability service. It
never returns an empty, successful read that looks like "no such events".

`--source <path>` still reads any NDJSON file, and setting
`log_source.path` in `/etc/tensorplate/cli.json` makes the command read
that file, for sites that configure `diagnostics_retention.file_path` in
`/etc/tensorplate/observability.json`.

A configured `log_source.path` that does not exist gets the same answer —
an upgrade that keeps a locally modified conffile can leave one naming the
log file earlier packages configured. A path that exists but cannot be
read, and a `--source` the operator named themselves, stay a plain IO
error (exit `1`) that names the file.

The Homebrew install is different: macOS has no journald, so its packaged
config enables retention and points `log_source.path` at
`${HOMEBREW_PREFIX}/var/log/tensorplate/events.ndjson`.

### A read that matched nothing

Whatever the source, a successful read that returned no entries writes one
line to stderr naming the source, the `--component` filter if one was
given, and where that component's own output is instead. In v0.1 the only
NDJSON writer is the observability service's retention sink: the event
listener transport is `in_process` (`unix_socket` is reserved and
rejected), and the agent and serving worker are separate processes, so
nothing they emit reaches it. `--component agent` against an events file
therefore matches nothing however long the file is — on Homebrew, and on
any Linux site that configures `diagnostics_retention.file_path` and points
`log_source.path` at it.

Like every other CLI note, the line is human output only: `--output json`
callers read `payload.entries` from the envelope on stdout.

## Filters

- `--component`: exact match on the entry's `component` field.
- `--level`: ordered comparison — passing `warn` keeps `warn`, `error`, and
  `critical`/`fatal` entries.
- `--correlation-id`: exact match on the entry's `correlation_id` field.
- `--since-ms`: keeps entries whose `monotonic_age_ms` is at most this many
  milliseconds old.
- `--tail`: hard upper bound of 10,000.

Malformed JSON lines are counted and skipped; the count is surfaced to
stderr so an operator can detect a log writer regression without the
command failing.

## Output

Human mode renders one row per entry:

```
  <timestamp>  WARN [agent] slow_path corr=cli-… message text
```

JSON mode emits the entries verbatim under `payload.entries[]` along with the
resolved source path and kind.
