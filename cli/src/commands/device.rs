// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate device` — manage the local SSH device registry.
//
// These subcommands only read and write the local `devices.json` registry.
// They follow the kubeconfig mental model: `add` records access metadata for
// a device, `use` selects the default, and the remaining subcommands manage
// the local entries. Remote reachability preflights, metadata sync, and
// routing normal commands over SSH are handled by the remote adapter and are
// not part of this registry-management surface.

use std::io::Write;

use serde_json::{json, Value};

use crate::args::{DeviceAddArgs, DeviceCommand};
use crate::error::{CliError, CliResult};
use crate::output::Renderer;
use crate::registry::{DeviceEntry, DeviceRegistry, DEFAULT_REMOTE_IMPORT_DIR};
use crate::remote::{self, SshRunner};

const COMMAND: &str = "device";

/// Run a `device` subcommand against the local registry.
///
/// # Errors
///
/// Returns a typed [`CliError`] when the registry path cannot be resolved,
/// the registry is malformed, the requested device does not exist, or the
/// registry cannot be written.
pub fn run<O: Write, E: Write>(
    runner: &dyn SshRunner,
    renderer: &Renderer,
    command: DeviceCommand,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()> {
    let path = DeviceRegistry::resolve_path()?;
    let mut registry = DeviceRegistry::load(&path)?;
    match command {
        DeviceCommand::Add(args) => {
            add(runner, renderer, &mut registry, &path, args, stdout, stderr)
        }
        DeviceCommand::List => list(renderer, &registry, stdout),
        DeviceCommand::Use(name) => use_device(renderer, &mut registry, &path, &name, stdout),
        DeviceCommand::Sync(name) => sync(
            runner,
            renderer,
            &mut registry,
            &path,
            name.as_deref(),
            stdout,
        ),
        DeviceCommand::Remove(name) => remove(renderer, &mut registry, &path, &name, stdout),
        DeviceCommand::Rename { old, new } => {
            rename(renderer, &mut registry, &path, &old, &new, stdout)
        }
    }
}

fn add<O: Write, E: Write>(
    runner: &dyn SshRunner,
    renderer: &Renderer,
    registry: &mut DeviceRegistry,
    path: &std::path::Path,
    args: DeviceAddArgs,
    stdout: &mut O,
    stderr: &mut E,
) -> CliResult<()> {
    if registry.devices.contains_key(&args.name) {
        return Err(CliError::Usage(format!(
            "device `{}` is already enrolled; remove it first or choose another name",
            args.name
        )));
    }
    let name = args.name.clone();
    let ssh_target = args.ssh_target.clone();
    // Record the import dir on every entry so the registry identity carries
    // the staging location the remote deploy path needs; default it unless the
    // operator pinned a per-install location.
    let import_dir = args
        .import_dir
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_REMOTE_IMPORT_DIR));
    let remote_tensorplate = args
        .run_as
        .as_ref()
        .map(|_| std::path::PathBuf::from(remote::DEFAULT_REMOTE_TENSORPLATE));
    let entry = DeviceEntry {
        ssh_target: args.ssh_target,
        ssh_port: args.port,
        remote_tensorplate,
        remote_run_as: args.run_as,
        remote_import_dir: Some(import_dir.clone()),
        ..DeviceEntry::default()
    };
    registry.devices.insert(name.clone(), entry);
    // Set as default when explicitly requested, or when no default exists yet.
    let set_default = args.use_as_default || registry.default_device.is_none();
    if set_default {
        registry.default_device = Some(name.clone());
    }
    // Structural validation first (rejects an unsafe ssh_target before we shell
    // out), then the reachability preflight unless it was skipped. The device is
    // not persisted if either fails.
    registry.validate()?;
    if !args.no_verify {
        // The entry was just inserted, so `get` is always `Some`; the `if let`
        // avoids a panic path for the impossible case.
        if let Some(enrolled) = registry.devices.get(&name) {
            if enrolled.remote_run_as.is_some() {
                remote::verify_run_as_binary(runner, enrolled)?;
            }
            remote::preflight_reachable(runner, enrolled)?;
        }
    }
    registry.save(path)?;

    let mut human = if set_default {
        format!("added device `{name}` ({ssh_target}) and set it as the default")
    } else {
        format!("added device `{name}` ({ssh_target})")
    };
    if args.no_verify {
        human.push_str(" (reachability not verified)");
    }
    if !set_default {
        renderer.info(
            stderr,
            &format!("run `tensorplate device use {name}` to make it the default"),
        )?;
    }
    let payload = json!({
        "name": name,
        "ssh_target": ssh_target,
        "remote_import_dir": import_dir.display().to_string(),
        "default": set_default,
    });
    renderer.ok(stdout, COMMAND, &human, payload, None, None)
}

fn sync<O: Write>(
    runner: &dyn SshRunner,
    renderer: &Renderer,
    registry: &mut DeviceRegistry,
    path: &std::path::Path,
    name: Option<&str>,
    stdout: &mut O,
) -> CliResult<()> {
    let target = match name {
        Some(n) => n.to_string(),
        None => registry.default_device.clone().ok_or_else(|| {
            CliError::Usage(
                "device sync requires a <name> or a default device (`tensorplate device use <name>`)"
                    .into(),
            )
        })?,
    };
    let entry = registry.devices.get(&target).cloned().ok_or_else(|| {
        CliError::Usage(format!(
            "device `{target}` is not enrolled; run `tensorplate device list`"
        ))
    })?;
    // Fetch facts first; a failure here leaves the registry untouched, so a
    // failed sync is non-destructive.
    let facts = remote::fetch_version_facts(runner, &entry)?;
    let Some(updated) = registry.devices.get_mut(&target) else {
        return Err(CliError::Internal(
            "device entry disappeared during sync".into(),
        ));
    };
    updated.agent_version = facts.agent_version;
    updated.protocol_version = facts.protocol_version;
    updated.last_seen = Some(remote::now_rfc3339());
    let agent_version = updated.agent_version.clone();
    let protocol_version = updated.protocol_version.clone();
    let last_seen = updated.last_seen.clone();
    registry.validate()?;
    registry.save(path)?;
    let human = format!(
        "synced device `{target}` (cli version {}, protocol {})",
        agent_version.as_deref().unwrap_or("unknown"),
        protocol_version.as_deref().unwrap_or("unknown"),
    );
    let payload = json!({
        "name": target,
        "agent_version": agent_version,
        "protocol_version": protocol_version,
        "last_seen": last_seen,
    });
    renderer.ok(stdout, COMMAND, &human, payload, None, None)
}

fn list<O: Write>(renderer: &Renderer, registry: &DeviceRegistry, stdout: &mut O) -> CliResult<()> {
    let default = registry.default_device.as_deref();
    let human = if registry.devices.is_empty() {
        "no devices enrolled".to_string()
    } else {
        let mut lines = Vec::new();
        for (name, entry) in &registry.devices {
            let marker = if Some(name.as_str()) == default {
                "*"
            } else {
                " "
            };
            let port = entry.ssh_port.map(|p| format!(":{p}")).unwrap_or_default();
            let run_as = entry
                .remote_run_as
                .as_deref()
                .map(|u| format!("  run-as={u}"))
                .unwrap_or_default();
            lines.push(format!(
                "{marker} {name}\t{}{port}{run_as}",
                entry.ssh_target
            ));
        }
        lines.join("\n")
    };
    let devices: Vec<Value> = registry
        .devices
        .iter()
        .map(|(name, entry)| {
            json!({
                "name": name,
                "ssh_target": entry.ssh_target,
                "ssh_port": entry.ssh_port,
                "remote_run_as": entry.remote_run_as,
                "remote_import_dir": entry.remote_import_dir,
                "default": Some(name.as_str()) == default,
                "last_seen": entry.last_seen,
                "agent_version": entry.agent_version,
                "protocol_version": entry.protocol_version,
                "device_family": entry.device_family,
                "available_backends": entry.available_backends,
            })
        })
        .collect();
    let payload = json!({
        "default_device": registry.default_device,
        "devices": devices,
    });
    renderer.ok(stdout, COMMAND, &human, payload, None, None)
}

fn use_device<O: Write>(
    renderer: &Renderer,
    registry: &mut DeviceRegistry,
    path: &std::path::Path,
    name: &str,
    stdout: &mut O,
) -> CliResult<()> {
    require_enrolled(registry, name)?;
    registry.default_device = Some(name.to_string());
    registry.validate()?;
    registry.save(path)?;
    let human = format!("default device set to `{name}`");
    renderer.ok(
        stdout,
        COMMAND,
        &human,
        json!({ "default_device": name }),
        None,
        None,
    )
}

fn remove<O: Write>(
    renderer: &Renderer,
    registry: &mut DeviceRegistry,
    path: &std::path::Path,
    name: &str,
    stdout: &mut O,
) -> CliResult<()> {
    if registry.devices.remove(name).is_none() {
        return Err(not_enrolled(name));
    }
    let cleared_default = registry.default_device.as_deref() == Some(name);
    if cleared_default {
        registry.default_device = None;
    }
    registry.validate()?;
    registry.save(path)?;
    let human = if cleared_default {
        format!("removed device `{name}` (it was the default; no default device is set now)")
    } else {
        format!("removed device `{name}`")
    };
    renderer.ok(
        stdout,
        COMMAND,
        &human,
        json!({ "removed": name, "cleared_default": cleared_default }),
        None,
        None,
    )
}

fn rename<O: Write>(
    renderer: &Renderer,
    registry: &mut DeviceRegistry,
    path: &std::path::Path,
    old: &str,
    new: &str,
    stdout: &mut O,
) -> CliResult<()> {
    if old == new {
        return Err(CliError::Usage(
            "device rename requires distinct <old> and <new> names".into(),
        ));
    }
    require_enrolled(registry, old)?;
    if registry.devices.contains_key(new) {
        return Err(CliError::Usage(format!(
            "device `{new}` is already enrolled"
        )));
    }
    let entry = registry
        .devices
        .remove(old)
        .ok_or_else(|| not_enrolled(old))?;
    registry.devices.insert(new.to_string(), entry);
    if registry.default_device.as_deref() == Some(old) {
        registry.default_device = Some(new.to_string());
    }
    registry.validate()?;
    registry.save(path)?;
    let human = format!("renamed device `{old}` to `{new}`");
    renderer.ok(
        stdout,
        COMMAND,
        &human,
        json!({ "old": old, "new": new }),
        None,
        None,
    )
}

fn require_enrolled(registry: &DeviceRegistry, name: &str) -> CliResult<()> {
    if registry.devices.contains_key(name) {
        Ok(())
    } else {
        Err(not_enrolled(name))
    }
}

fn not_enrolled(name: &str) -> CliError {
    CliError::Usage(format!(
        "device `{name}` is not enrolled; run `tensorplate device add {name} --ssh <user@host>`"
    ))
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

    fn add_args(name: &str, target: &str, use_default: bool) -> DeviceAddArgs {
        DeviceAddArgs {
            name: name.to_string(),
            ssh_target: target.to_string(),
            port: None,
            run_as: None,
            import_dir: None,
            use_as_default: use_default,
            // Registry-mutation tests skip the SSH preflight; dedicated tests
            // exercise it with a stub runner.
            no_verify: true,
        }
    }

    struct StubRunner {
        status: i32,
        stdout: Vec<u8>,
    }

    impl StubRunner {
        fn ok(stdout: &str) -> Self {
            Self {
                status: 0,
                stdout: stdout.as_bytes().to_vec(),
            }
        }

        fn failing(status: i32) -> Self {
            Self {
                status,
                stdout: Vec::new(),
            }
        }

        fn output(&self) -> crate::remote::RemoteOutput {
            crate::remote::RemoteOutput {
                status: self.status,
                stdout: self.stdout.clone(),
                stderr: String::new(),
            }
        }
    }

    impl SshRunner for StubRunner {
        fn run(
            &self,
            _entry: &DeviceEntry,
            _args: &[String],
            _stdin: Option<&[u8]>,
        ) -> CliResult<crate::remote::RemoteOutput> {
            Ok(self.output())
        }

        fn run_raw(
            &self,
            _entry: &DeviceEntry,
            _command: &[String],
            _stdin: Option<&[u8]>,
        ) -> CliResult<crate::remote::RemoteOutput> {
            Ok(self.output())
        }
    }

    struct Harness {
        _td: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl Harness {
        fn new() -> Self {
            let td = tempfile::TempDir::new().unwrap();
            let path = td.path().join("devices.json");
            Self { _td: td, path }
        }

        fn load(&self) -> DeviceRegistry {
            DeviceRegistry::load(&self.path).unwrap()
        }
    }

    fn run_add(h: &Harness, args: DeviceAddArgs) -> (String, String) {
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        let mut out = Vec::new();
        let mut err = Vec::new();
        add(
            &StubRunner::ok(""),
            &renderer,
            &mut registry,
            &h.path,
            args,
            &mut out,
            &mut err,
        )
        .unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn first_add_becomes_default_without_use_flag() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        let reg = h.load();
        assert_eq!(reg.default_device.as_deref(), Some("orin"));
        let entry = reg.devices.get("orin").unwrap();
        assert_eq!(entry.ssh_target, "reid@orin.local");
        // Every enrolled device carries the import dir the deploy path needs.
        assert_eq!(
            entry.remote_import_dir.as_deref(),
            Some(std::path::Path::new(DEFAULT_REMOTE_IMPORT_DIR))
        );
    }

    #[test]
    fn add_import_dir_flag_overrides_default() {
        let h = Harness::new();
        let mut args = add_args("orin", "reid@orin.local", false);
        args.import_dir = Some(std::path::PathBuf::from("/srv/tp/import"));
        run_add(&h, args);
        assert_eq!(
            h.load()
                .devices
                .get("orin")
                .unwrap()
                .remote_import_dir
                .as_deref(),
            Some(std::path::Path::new("/srv/tp/import"))
        );
    }

    #[test]
    fn add_run_as_records_verified_default_binary_path() {
        let h = Harness::new();
        let mut args = add_args("orin", "reid@orin.local", false);
        args.run_as = Some("tensorplate".into());
        run_add(&h, args);
        let entry = h.load().devices.get("orin").unwrap().clone();
        assert_eq!(
            entry.remote_tensorplate.as_deref(),
            Some(std::path::Path::new(remote::DEFAULT_REMOTE_TENSORPLATE))
        );
        assert_eq!(entry.remote_run_as.as_deref(), Some("tensorplate"));
    }

    #[test]
    fn second_add_keeps_existing_default_unless_use_flag() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        let (_out, err) = run_add(&h, add_args("nano", "reid@nano.local", false));
        let reg = h.load();
        assert_eq!(reg.default_device.as_deref(), Some("orin"));
        assert!(err.contains("device use nano"));
    }

    #[test]
    fn add_with_use_flag_switches_default() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        run_add(&h, add_args("nano", "reid@nano.local", true));
        assert_eq!(h.load().default_device.as_deref(), Some("nano"));
    }

    #[test]
    fn add_rejects_duplicate_name() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        let err = add(
            &StubRunner::ok(""),
            &renderer,
            &mut registry,
            &h.path,
            add_args("orin", "reid@other.local", false),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn add_preflight_saves_when_reachable_and_rejects_when_not() {
        // Reachable: default-on preflight passes, device is saved.
        let h = Harness::new();
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        let mut args = add_args("orin", "reid@orin.local", false);
        args.no_verify = false;
        let ok_runner = StubRunner::ok(
            r#"{"schema_version":"0.1","command":"status","status":"ok","payload":{}}"#,
        );
        add(
            &ok_runner,
            &renderer,
            &mut registry,
            &h.path,
            args,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(h.load().devices.contains_key("orin"));

        // Unreachable: preflight fails, device is NOT saved.
        let h2 = Harness::new();
        let mut registry2 = h2.load();
        let mut args2 = add_args("orin", "reid@orin.local", false);
        args2.no_verify = false;
        let err = add(
            &StubRunner::failing(3),
            &renderer,
            &mut registry2,
            &h2.path,
            args2,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Unavailable { .. }));
        assert!(!h2.path.exists());
    }

    #[test]
    fn sync_updates_cached_facts() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        let runner = StubRunner::ok(
            r#"{"schema_version":"0.1","command":"version","status":"ok","payload":{"cli":"0.1.5","protocol":"0.1","bundle_format":"0.1"}}"#,
        );
        sync(
            &runner,
            &renderer,
            &mut registry,
            &h.path,
            Some("orin"),
            &mut Vec::new(),
        )
        .unwrap();
        let entry = h.load().devices.get("orin").unwrap().clone();
        assert_eq!(entry.agent_version.as_deref(), Some("0.1.5"));
        assert_eq!(entry.protocol_version.as_deref(), Some("0.1"));
        assert!(entry.last_seen.is_some());
    }

    #[test]
    fn use_unknown_device_errors() {
        let h = Harness::new();
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        let err =
            use_device(&renderer, &mut registry, &h.path, "ghost", &mut Vec::new()).unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn remove_default_clears_default() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        remove(&renderer, &mut registry, &h.path, "orin", &mut Vec::new()).unwrap();
        let reg = h.load();
        assert!(reg.devices.is_empty());
        assert!(reg.default_device.is_none());
    }

    #[test]
    fn rename_moves_entry_and_follows_default() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        rename(
            &renderer,
            &mut registry,
            &h.path,
            "orin",
            "orin-lab",
            &mut Vec::new(),
        )
        .unwrap();
        let reg = h.load();
        assert!(reg.devices.contains_key("orin-lab"));
        assert!(!reg.devices.contains_key("orin"));
        assert_eq!(reg.default_device.as_deref(), Some("orin-lab"));
    }

    #[test]
    fn rename_rejects_existing_target() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        run_add(&h, add_args("nano", "reid@nano.local", false));
        let renderer = Renderer::new(OutputMode::Human);
        let mut registry = h.load();
        let err = rename(
            &renderer,
            &mut registry,
            &h.path,
            "orin",
            "nano",
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
    }

    #[test]
    fn list_json_reports_devices_and_default() {
        let h = Harness::new();
        run_add(&h, add_args("orin", "reid@orin.local", false));
        let renderer = Renderer::new(OutputMode::Json);
        let registry = h.load();
        let mut out = Vec::new();
        list(&renderer, &registry, &mut out).unwrap();
        let parsed: Value = serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed["command"], "device");
        assert_eq!(parsed["payload"]["default_device"], "orin");
        assert_eq!(parsed["payload"]["devices"][0]["name"], "orin");
        assert_eq!(parsed["payload"]["devices"][0]["default"], true);
    }
}
