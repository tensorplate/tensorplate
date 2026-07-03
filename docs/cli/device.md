# `tensorplate device`

Manage the local registry of SSH-reachable devices the CLI can remember. The
registry is a small, human-inspectable JSON file; these subcommands read and
write it and never touch a remote device.

```
tensorplate device add <name> --ssh <user@host> [--port <n>] [--run-as <user>] [--use]
tensorplate device list [--output <human|json>]
tensorplate device use <name>
tensorplate device remove <name>
tensorplate device rename <old> <new>
```

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
  the device-local agent.
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
