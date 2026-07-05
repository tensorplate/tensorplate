# `tensorplate device`

Manage the local registry of SSH-reachable devices the CLI can remember. The
registry is a small, human-inspectable JSON file; these subcommands read and
write it and never touch a remote device.

```
tensorplate device add <name> --ssh <user@host> [--port <n>] [--run-as <user>] [--import-dir <path>] [--use]
tensorplate device list [--output <human|json>]
tensorplate device use <name>
tensorplate device remove <name>
tensorplate device rename <old> <new>
```

These commands are local-only: they read and write the registry and never
resolve a transport profile or contact an agent, so they keep working even when
the CLI config's default profile is a reserved/unsupported mode.

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
  for installs whose packaged service permissions place it elsewhere.
- `list` prints enrolled devices; the default is marked with `*`. `--output
  json` emits the standard envelope with a `payload.devices` array and
  `payload.default_device`.
- `use <name>` selects the default device.
- `remove <name>` deletes a local entry. Removing the default clears the default.
- `rename <old> <new>` renames a local entry, following the default if it moved.

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

`status`, `rollback`, `logs`, `doctor`, `infer`, and `version` route to the
selected device. `deploy` over `--device` is not available yet (remote deploy
staging lands in a later change). Path flags are interpreted where the file
lives: `logs --source` and `status --observability-snapshot` are device-local,
while `infer --input` is read locally and piped to the device over stdin and
`infer --output-file` is written locally. `logs --follow` is not supported over
`--device` yet.

With `--output json`, routed output preserves the standard envelope and adds a
top-level `device` object identifying the target; human output from the device
is forwarded verbatim. The CLI fails closed when SSH exits non-zero, the remote
output is malformed, or the remote protocol version is incompatible.
