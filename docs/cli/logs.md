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
