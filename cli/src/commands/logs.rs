// SPDX-License-Identifier: Apache-2.0
//
// V01-E11-F07: `tensorplate logs` — bounded NDJSON log reader.
//
// v0.1.0 reads from a local file or directory of NDJSON entries
// produced by the agent, serving worker, and observability service.
// Each entry is one JSON object per line; the reader is bounded
// (default tail = 100) and never streams unbounded content.
//
// Remote profiles are explicitly *unsupported* for `logs` until the
// agent's log API lands in V01-E12. The CLI returns a typed
// `Unsupported` error rather than silently falling back.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use serde_json::{json, Value};

use crate::args::{LogsArgs, OutputMode};
use crate::config::{CliConfig, LogSourceConfig};
use crate::error::{CliError, CliResult};
use crate::output::Renderer;
use crate::profile::ResolvedProfile;

/// Hard ceiling for `--tail`: the CLI never returns more than this many
/// entries from one invocation. Protects constrained devices from a
/// pathologically large request.
const MAX_TAIL: u64 = 10_000;

/// Run the `logs` command.
///
/// # Errors
///
/// Returns:
/// - [`CliError::Unavailable`] when the resolved profile is not `local`, or
///   when no NDJSON log source is configured — carrying a hint that names
///   where this platform's components do write.
/// - [`CliError::Io`] when the source cannot be opened.
pub fn run<W: Write, E: Write>(
    renderer: &Renderer,
    profile: &ResolvedProfile,
    config: &CliConfig,
    args: &LogsArgs,
    out: &mut W,
    stderr: &mut E,
) -> CliResult<()> {
    if !matches!(profile.mode, crate::config::ProfileMode::Local) {
        return Err(CliError::Unavailable {
            message:
                "tensorplate logs reads local files only in v0.1.0; remote profiles are unsupported until the agent's log API lands"
                    .into(),
            hint: Some(
                "ssh to the device and run `tensorplate logs` there, or run with `--profile local`"
                    .into(),
            ),
        });
    }
    let source = resolve_source(&config.log_source, args)?;
    if args.follow {
        return follow_source(renderer, &source, args, stderr);
    }
    let tail = args
        .tail
        .unwrap_or(config.log_source.tail_default)
        .min(MAX_TAIL);
    let entries = read_bounded(&source, args, tail)?;
    let payload = json!({
        "source": source.display_path(),
        "kind": source.kind_label(),
        "entries": entries,
    });
    let human = render_human(&source, &entries);
    renderer.ok(out, "logs", &human, payload, None, None)
}

#[derive(Debug)]
struct LogSource {
    kind: LogKind,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
enum LogKind {
    File,
    Directory,
}

impl LogSource {
    fn display_path(&self) -> String {
        self.path.display().to_string()
    }
    fn kind_label(&self) -> &'static str {
        match self.kind {
            LogKind::File => "file",
            LogKind::Directory => "directory",
        }
    }
}

fn resolve_source(cfg: &LogSourceConfig, args: &LogsArgs) -> CliResult<LogSource> {
    if let Some(path) = args.source_override.as_deref() {
        return classify_path(path.to_path_buf());
    }
    let Some(p) = cfg.path.as_deref() else {
        // Not a usage mistake: on a native Linux install there is no
        // NDJSON file to name, because both services log to the journal.
        // Unavailable is the variant that carries a hint through both
        // renderers, which is what makes the answer actionable instead of
        // telling the operator to configure a path nothing writes.
        return Err(CliError::Unavailable {
            message:
                "tensorplate logs: no log_source.path is configured, and this install writes no NDJSON log file"
                    .into(),
            hint: Some(no_source_hint(args.component.as_deref())),
        });
    };
    // A configured path that is not there is the same operator question as
    // no path at all, and reaches an upgraded host that kept a conffile
    // naming the log file earlier packages configured. A path that exists
    // but cannot be read is a different problem and keeps the IO error
    // naming it, as does a `--source` the operator chose themselves.
    match std::fs::metadata(p) {
        Ok(meta) => classify_meta(p.to_path_buf(), &meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CliError::Unavailable {
            message: format!(
                "tensorplate logs: the configured log_source.path `{}` does not exist; \
                 no component on this install writes it",
                p.display()
            ),
            hint: Some(no_source_hint(args.component.as_deref())),
        }),
        Err(e) => Err(stat_error(p, &e)),
    }
}

/// Where the logs actually are when no NDJSON source is configured.
///
/// The native packages' units set no `StandardOutput=`, and the agent
/// writes diagnostics to stderr, so journald holds them. Name the unit
/// that carries the component the operator asked for: the serving worker
/// and its backends are children of the agent and share its journal.
///
/// Compiled on every target rather than only on Linux so the mapping from
/// component to unit is covered wherever the suite runs; only the Linux
/// build calls it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn journal_hint(component: Option<&str>) -> String {
    let units: &[&str] = match component {
        Some("observability") => &["tensorplate-observability"],
        Some("agent" | "serving_worker" | "runtime" | "adapter" | "python_pytorch_sidecar") => {
            &["tensorplate-agent"]
        }
        _ => &["tensorplate-agent", "tensorplate-observability"],
    };
    let commands = units
        .iter()
        .map(|unit| format!("`journalctl -u {unit}`"))
        .collect::<Vec<_>>()
        .join(" and ");
    format!(
        "this install's services log to the journal: run {commands}. \
         `tensorplate logs` reads NDJSON files only; pass `--source <path>` for one, \
         or set `log_source.path` in {} once a component writes it",
        crate::config::SYSTEM_CLI_CONFIG_PATH,
    )
}

#[cfg(target_os = "linux")]
fn no_source_hint(component: Option<&str>) -> String {
    journal_hint(component)
}

/// On macOS the packaged event log is named by the Homebrew launcher's
/// `TENSORPLATE_CLI_CONFIG`, so reaching this hint means the launcher was
/// bypassed or TensorPlate is not installed from Homebrew.
#[cfg(target_os = "macos")]
fn no_source_hint(_component: Option<&str>) -> String {
    "run the installed `tensorplate` launcher, which points the CLI at the packaged config, \
     or pass `--source <path>` to read an NDJSON file directly"
        .into()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn no_source_hint(_component: Option<&str>) -> String {
    "pass `--source <path>` to read an NDJSON file, or set `log_source.path` in the cli config"
        .into()
}

fn stat_error(path: &Path, error: &std::io::Error) -> CliError {
    CliError::Io(format!(
        "tensorplate logs: cannot stat `{}`: {error}",
        path.display()
    ))
}

fn classify_path(path: PathBuf) -> CliResult<LogSource> {
    let meta = std::fs::metadata(&path).map_err(|e| stat_error(&path, &e))?;
    classify_meta(path, &meta)
}

fn classify_meta(path: PathBuf, meta: &std::fs::Metadata) -> CliResult<LogSource> {
    if meta.is_file() {
        Ok(LogSource {
            kind: LogKind::File,
            path,
        })
    } else if meta.is_dir() {
        Ok(LogSource {
            kind: LogKind::Directory,
            path,
        })
    } else {
        Err(CliError::Config(format!(
            "tensorplate logs: `{}` is neither a file nor a directory",
            path.display()
        )))
    }
}

fn read_bounded(source: &LogSource, args: &LogsArgs, tail: u64) -> CliResult<Vec<Value>> {
    let files = collect_files(source);
    let mut entries = Vec::<Value>::with_capacity(tail as usize);
    let mut malformed = 0u64;
    for file in files {
        let f = File::open(&file).map_err(|e| {
            CliError::Io(format!(
                "tensorplate logs: cannot open `{}`: {e}",
                file.display()
            ))
        })?;
        let reader = BufReader::new(f);
        for line in reader.lines() {
            let Ok(raw) = line else { break };
            if raw.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&raw) {
                Ok(value) => {
                    if !matches_filters(&value, args) {
                        continue;
                    }
                    entries.push(value);
                }
                Err(_) => {
                    malformed += 1;
                }
            }
        }
    }
    if entries.len() > tail as usize {
        let skip = entries.len() - tail as usize;
        entries.drain(..skip);
    }
    if malformed > 0 {
        eprintln!(
            "tensorplate logs: skipped {malformed} malformed entries (bounded mode tolerates malformed lines)"
        );
    }
    Ok(entries)
}

fn collect_files(source: &LogSource) -> Vec<PathBuf> {
    match source.kind {
        LogKind::File => vec![source.path.clone()],
        LogKind::Directory => {
            let mut files: Vec<_> = std::fs::read_dir(&source.path)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| {
                    e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .is_some_and(|ext| matches!(ext, "ndjson" | "log" | "jsonl"))
                })
                .map(|e| e.path())
                .collect();
            files.sort();
            files
        }
    }
}

fn matches_filters(entry: &Value, args: &LogsArgs) -> bool {
    if let Some(comp) = args.component.as_deref() {
        let observed = entry
            .get("component")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if observed != comp {
            return false;
        }
    }
    if let Some(level) = args.level.as_deref() {
        let observed = entry
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !level_matches(observed, level) {
            return false;
        }
    }
    if let Some(corr) = args.correlation_id.as_deref() {
        let observed = entry
            .get("correlation_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if observed != corr {
            return false;
        }
    }
    if let Some(since_ms) = args.since_ms {
        let observed = entry
            .get("monotonic_age_ms")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        if observed > since_ms {
            return false;
        }
    }
    true
}

/// Level filtering uses an ordered comparison: passing `--level warn`
/// includes warn, error, and critical entries. Lowercase comparison so
/// JSON producers can use mixed case.
fn level_matches(observed: &str, requested: &str) -> bool {
    fn rank(level: &str) -> u8 {
        match level.to_ascii_lowercase().as_str() {
            "trace" => 0,
            "debug" => 1,
            "info" => 2,
            "warn" | "warning" => 3,
            "error" => 4,
            "critical" | "fatal" => 5,
            _ => 2,
        }
    }
    rank(observed) >= rank(requested)
}

fn render_human(source: &LogSource, entries: &[Value]) -> String {
    let mut out = format!(
        "logs: source={} kind={} entries={}\n",
        source.display_path(),
        source.kind_label(),
        entries.len(),
    );
    for e in entries {
        out.push_str(&render_entry(e));
        out.push('\n');
    }
    out
}

fn render_entry(value: &Value) -> String {
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let level = value.get("level").and_then(Value::as_str).unwrap_or("info");
    let component = value
        .get("component")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("event");
    let message = value.get("message").and_then(Value::as_str).unwrap_or("");
    let correlation = value
        .get("correlation_id")
        .and_then(Value::as_str)
        .map(|c| format!(" corr={c}"))
        .unwrap_or_default();
    format!(
        "  {ts} {:>5} [{component}] {event}{correlation} {message}",
        level.to_ascii_uppercase()
    )
}

fn follow_source<W: Write>(
    renderer: &Renderer,
    source: &LogSource,
    args: &LogsArgs,
    stderr: &mut W,
) -> CliResult<()> {
    // `--follow` is only meaningful for a single file in v0.1.0. Directory
    // follow requires per-file watches and is out of scope.
    let LogKind::File = source.kind else {
        return Err(CliError::Unavailable {
            message: "`--follow` is supported on a single file source in v0.1.0".into(),
            hint: Some("re-run without --follow against the directory or pick one file".into()),
        });
    };
    let mut file = File::open(&source.path).map_err(|e| {
        CliError::Io(format!(
            "tensorplate logs: cannot open `{}`: {e}",
            source.path.display()
        ))
    })?;
    file.seek(SeekFrom::End(0))
        .map_err(|e| CliError::Io(format!("{e}")))?;
    let mut buf = Vec::with_capacity(4096);
    let interval = Duration::from_millis(250);
    let _ = stderr.write_all(b"tensorplate logs: follow mode (Ctrl-C to stop)\n");
    loop {
        buf.clear();
        let bytes = (&mut file)
            .take(64 * 1024)
            .read_to_end(&mut buf)
            .map_err(|e| CliError::Io(format!("tensorplate logs: read failed: {e}")))?;
        if bytes == 0 {
            sleep(interval);
            continue;
        }
        for raw in buf.split(|b| *b == b'\n') {
            if raw.is_empty() {
                continue;
            }
            let Ok(text) = std::str::from_utf8(raw) else {
                continue;
            };
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                if matches_filters(&value, args) {
                    let line = match renderer.mode() {
                        OutputMode::Human => render_entry(&value),
                        OutputMode::Json => serde_json::to_string(&value).unwrap_or_default(),
                    };
                    let _ = writeln!(std::io::stdout(), "{line}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::default_trait_access,
        clippy::needless_pass_by_value,
        clippy::semicolon_if_nothing_returned,
        clippy::field_reassign_with_default,
        clippy::large_enum_variant,
        clippy::no_effect_underscore_binding,
        clippy::redundant_clone,
        clippy::redundant_closure_for_method_calls
    )]

    use super::*;
    use crate::args::OutputMode;
    use crate::config::{CliConfig, ProfileMode};
    use crate::profile::{ResolvedProfile, Transport};
    use std::time::Duration;
    use tempfile::TempDir;

    fn profile(mode: ProfileMode) -> ResolvedProfile {
        ResolvedProfile {
            name: "p".into(),
            mode,
            display_name: None,
            transport: Transport::UnixSocket {
                path: PathBuf::from("/tmp/agent.sock"),
            },
            serving_url: None,
            timeout: Duration::from_secs(5),
        }
    }

    fn make_cfg(td: &TempDir) -> (CliConfig, PathBuf) {
        let path = td.path().join("agent.ndjson");
        std::fs::write(
            &path,
            r#"{"timestamp":"t1","level":"info","component":"agent","event":"start","correlation_id":"c-1","message":"hello"}
{"timestamp":"t2","level":"warn","component":"agent","event":"slow","correlation_id":"c-1","message":"slow path","monotonic_age_ms":150}
{"timestamp":"t3","level":"error","component":"serving","event":"infer_failed","correlation_id":"c-2","message":"x","monotonic_age_ms":300}
not-a-json-line
"#,
        )
        .unwrap();
        let mut cfg = CliConfig::default().validate().unwrap();
        cfg.log_source.path = Some(path.clone());
        (cfg, path)
    }

    fn default_args() -> LogsArgs {
        LogsArgs {
            component: None,
            level: None,
            since_ms: None,
            tail: None,
            follow: false,
            correlation_id: None,
            source_override: None,
        }
    }

    #[test]
    fn logs_reads_bounded_default_tail() {
        let td = tempfile::tempdir().unwrap();
        let (cfg, _) = make_cfg(&td);
        let args = default_args();
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        let entries = parsed["payload"]["entries"].as_array().unwrap();
        // One malformed line dropped, three valid entries remain.
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn logs_filters_by_component() {
        let td = tempfile::tempdir().unwrap();
        let (cfg, _) = make_cfg(&td);
        let args = LogsArgs {
            component: Some("serving".into()),
            ..default_args()
        };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        let entries = parsed["payload"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["component"], "serving");
    }

    #[test]
    fn logs_level_filter_is_ordered() {
        let td = tempfile::tempdir().unwrap();
        let (cfg, _) = make_cfg(&td);
        let args = LogsArgs {
            level: Some("warn".into()),
            ..default_args()
        };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        let entries = parsed["payload"]["entries"].as_array().unwrap();
        // warn + error are kept; info is filtered out.
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn logs_remote_profile_returns_unsupported() {
        let td = tempfile::tempdir().unwrap();
        let (cfg, _) = make_cfg(&td);
        let args = default_args();
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            &r,
            &profile(ProfileMode::Url),
            &cfg,
            &args,
            &mut out,
            &mut err,
        );
        match result {
            Err(CliError::Unavailable { .. }) => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn logs_correlation_id_filter() {
        let td = tempfile::tempdir().unwrap();
        let (cfg, _) = make_cfg(&td);
        let args = LogsArgs {
            correlation_id: Some("c-2".into()),
            ..default_args()
        };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        let entries = parsed["payload"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["correlation_id"], "c-2");
    }

    #[test]
    fn logs_tail_bounds_output() {
        let td = tempfile::tempdir().unwrap();
        let (cfg, _) = make_cfg(&td);
        let args = LogsArgs {
            tail: Some(1),
            ..default_args()
        };
        let r = Renderer::new(OutputMode::Json);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        let entries = parsed["payload"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
    }

    fn missing_source_error(component: Option<&str>) -> CliError {
        let mut cfg = CliConfig::default().validate().unwrap();
        cfg.log_source.path = None;
        let args = LogsArgs {
            component: component.map(Into::into),
            ..default_args()
        };
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        )
        .expect_err("a config with no log source must not succeed")
    }

    #[test]
    fn logs_missing_source_is_unavailable_with_an_actionable_hint() {
        let err = missing_source_error(Some("agent"));
        let CliError::Unavailable { message, hint } = err else {
            panic!("expected Unavailable, got {err:?}");
        };
        assert!(message.contains("log_source.path"), "{message}");
        let hint = hint.expect("the failure must say what to use instead");
        assert!(hint.contains("--source"), "{hint}");
        // The hint must name a reader that exists on this platform, not
        // only repeat that the config is empty.
        #[cfg(target_os = "linux")]
        assert!(
            hint.contains("journalctl -u tensorplate-agent"),
            "the agent's journal unit must be named: {hint}"
        );
        #[cfg(target_os = "macos")]
        assert!(hint.contains("launcher"), "{hint}");
    }

    /// An upgraded host that kept its own copy of an older conffile still
    /// names `/var/log/tensorplate/tensorplate-agent.log`. That is the same
    /// question as an unconfigured source, so it gets the same answer
    /// rather than a bare `cannot stat`.
    #[test]
    fn a_configured_path_that_does_not_exist_says_where_the_logs_are() {
        let td = tempfile::tempdir().unwrap();
        let mut cfg = CliConfig::default().validate().unwrap();
        cfg.log_source.path = Some(td.path().join("tensorplate-agent.log"));
        let args = LogsArgs {
            component: Some("agent".into()),
            ..default_args()
        };
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        );
        let Err(CliError::Unavailable { message, hint }) = result else {
            panic!("expected Unavailable, got {result:?}");
        };
        assert!(message.contains("tensorplate-agent.log"), "{message}");
        assert!(hint.is_some_and(|h| h.contains("--source")));
    }

    /// A source the operator named themselves keeps the plain IO error:
    /// nothing about the install explains a path they chose.
    #[test]
    fn an_explicit_missing_source_stays_an_io_error() {
        let td = tempfile::tempdir().unwrap();
        let cfg = CliConfig::default().validate().unwrap();
        let args = LogsArgs {
            source_override: Some(td.path().join("nope.ndjson")),
            ..default_args()
        };
        let r = Renderer::new(OutputMode::Human);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            &r,
            &profile(ProfileMode::Local),
            &cfg,
            &args,
            &mut out,
            &mut err,
        );
        assert!(matches!(result, Err(CliError::Io(_))), "{result:?}");
    }

    #[test]
    fn the_journal_hint_names_the_unit_for_the_component_asked_for() {
        let observability = journal_hint(Some("observability"));
        assert!(
            observability.contains("journalctl -u tensorplate-observability"),
            "{observability}"
        );
        assert!(
            !observability.contains("journalctl -u tensorplate-agent"),
            "{observability}"
        );

        // The serving worker is supervised by the agent and shares its
        // journal; it has no unit of its own.
        for component in ["agent", "serving_worker", "runtime", "adapter"] {
            let hint = journal_hint(Some(component));
            assert!(
                hint.contains("journalctl -u tensorplate-agent"),
                "{component}: {hint}"
            );
            assert!(
                !hint.contains("journalctl -u tensorplate-observability"),
                "{component}: {hint}"
            );
        }

        // Without a component, both units are named.
        let any = journal_hint(None);
        assert!(any.contains("tensorplate-agent"), "{any}");
        assert!(any.contains("tensorplate-observability"), "{any}");
    }
}
