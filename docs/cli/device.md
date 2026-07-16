# `tensorplate device`

Manage the local registry of SSH-reachable devices the CLI can remember. The
registry is a small, human-inspectable JSON file; these subcommands read and
write it and never touch a remote device.

```
tensorplate device add <name> --ssh <user@host> [--port <n>] [--run-as <user>] [--import-dir <path>] [--use] [--no-verify]
tensorplate device list [--output <human|json>]
tensorplate device use <name>
tensorplate device sync [<name>]
tensorplate device prune <name> [--keep <n>] [--older-than <dur>]
tensorplate device remove <name>
tensorplate device rename <old> <new>
```

`list`, `use`, `remove`, and `rename` are local-only: they read and write the
registry and never resolve a transport profile, so they keep working even when
the CLI config's default profile is a reserved/unsupported mode. `add`
(reachability preflight), `sync`, and `prune` reach the device over SSH.

## Registry location

The registry path resolves independently of the CLI config, in this order:

1. `$TENSORPLATE_DEVICE_REGISTRY` — an explicit file path (used by tests/CI).
2. `$XDG_CONFIG_HOME/tensorplate/devices.json`.
3. `~/.config/tensorplate/devices.json`.

A missing registry means "no devices enrolled", not an error. Writes are atomic
(temp file + rename). Schema:
[`config/schemas/devices.json`](../../config/schemas/devices.json).

The registry stores only access metadata (SSH target, optional port, optional
run-as user) and cached device facts. It **never** stores SSH keys, passwords,
or agent secrets — authentication stays with your SSH client and
`~/.ssh/config`.

## Subcommands

- `add <name> --ssh <user@host>` records a device. `--port` pins a non-default
  SSH port; `--run-as <user>` records a non-interactive run-as user for reaching
  the device-local agent. Every entry records a remote import directory (the
  staging location for copied bundles); it defaults to
  `/var/lib/tensorplate/bundles/import` and `--import-dir <path>` overrides it
  for installs whose packaged service permissions place it elsewhere. `add`
  runs a reachability preflight by default (see below); `--no-verify` skips it
  for offline/pre-enrollment.
- `list` prints enrolled devices; the default is marked with `*`. `--output
  json` emits the standard envelope with a `payload.devices` array and
  `payload.default_device`.
- `use <name>` selects the default device.
- `prune <name>` reclaims staged remote import storage (see Deploy staging).
  Requires `--keep <n>` (most-recent) and/or `--older-than <dur>` (e.g. `7d`,
  `24h`); it always keeps the active deployment's import.
- `sync [<name>]` refreshes cached facts (remote CLI version, protocol version,
  and last-seen time) for the named device, or the default when omitted. A
  failed sync is non-destructive — it leaves the entry as it was.
- `remove <name>` deletes a local entry. Removing the default clears the default.
- `rename <old> <new>` renames a local entry, following the default if it moved.

## Enrollment preflight

By default `device add` verifies the device is reachable before saving it: it
runs `tensorplate --local status --output json` over SSH (through the configured
run-as mode) and refuses to save if that fails. On a packaged install the agent
control socket is group-accessible, so the usual path is simply to add the SSH
user to the `tensorplate` group (`sudo usermod -aG tensorplate <user>`, then
re-login) — the same one-time grant MicroK8s uses. The failure hint names the
fallbacks (configure `--run-as` with a non-interactive sudoers rule, or SSH as a
user that already reaches the socket). If the remote is older than 0.1.5 the
preflight reports that explicitly and asks you to upgrade the device. Pass
`--no-verify` to skip the preflight and record the device unconditionally.

When `--run-as <user>` is configured, `add` also vets the remote `tensorplate`
binary (default `/usr/bin/tensorplate`, or the pinned path): it must be
absolute, owned by root, and not group/other-writable, because the sudoers rule
grants execution as the run-as user.

## Default device selection

- `add --use` sets the newly added device as the default.
- Plain `add` sets the default only when no default exists yet; otherwise it
  leaves the current default unchanged and prints a hint to run
  `tensorplate device use <name>`.

## Example

```sh
tensorplate device add orin-lab --ssh reid@orin-lab.local
tensorplate device add nano --ssh reid@nano.local --use
tensorplate device list
# * nano       reid@nano.local
#   orin-lab   reid@orin-lab.local
tensorplate device use orin-lab
```

## Routing normal commands to a device

Once a device is selected, normal commands run against it over SSH. The CLI
shells out to `ssh` and re-invokes the *remote* `tensorplate` with `--local`, so
the device resolves its own config and never recursively routes.

```sh
tensorplate device use orin-lab
tensorplate status                 # runs on orin-lab
tensorplate --device nano logs --tail 100
tensorplate --local status         # force the local path, ignoring the default
```

Target selection precedence, highest first:

1. `--local` (bypass the registry and use the current process).
2. `--device <name>` (an explicit registry entry).
3. `--profile <name>` / `--agent-url` (an explicit local transport).
4. the registry's default device (`device use`).
5. local, when nothing is selected.

`status`, `rollback`, `logs`, `doctor`, `infer`, `version`, and `deploy` route to
the selected device. Devices enrolled with `--run-as` route through a structured,
non-interactive `sudo -n -u <user> -- …` invocation. Path flags are
interpreted where the file lives: `logs --source` and
`status --observability-snapshot` are device-local, while `infer --input` is read
locally and piped to the device over stdin and `infer --output-file` is written
locally. `logs --follow` is not supported over `--device` yet.

## Deploy staging

`deploy <bundle>` over `--device` copies the local bundle to a staged import
path on the device — `<import-dir>/<deployment-id>/` (the import dir is the one
recorded at enrollment, default `/var/lib/tensorplate/bundles/import`) — using
`rsync` when available and `scp` otherwise, then runs the remote deploy
transaction against that path with the original flags forwarded
(`--deployment-id`, `--expected-digest`, `--no-wait`, `--wait-timeout-ms`,
`--label`). A deployment id is generated when one is not supplied so each import
is named by its deployment id.

The import dir is created group-writable (sticky `1775`,
`tensorplate:tensorplate`) by the package install, so a copy user in the
`tensorplate` group can stage bundles with no manual setup. Imports are kept by
default; reclaim them with `device prune`:

```sh
tensorplate device prune orin-lab --keep 3         # keep the 3 newest imports
tensorplate device prune orin-lab --older-than 7d  # delete imports older than 7 days
```

`prune` requires at least one of `--keep`/`--older-than`, always keeps the active
deployment's import, and never deletes an import that survives either policy — so
a just-staged (newest) import is not reclaimed out from under an in-flight deploy.

With `--output json`, routed output preserves the standard envelope and adds a
top-level `device` object identifying the target; human output from the device
is forwarded verbatim. The CLI fails closed when SSH exits non-zero, the remote
output is malformed, or the remote protocol version is incompatible.
