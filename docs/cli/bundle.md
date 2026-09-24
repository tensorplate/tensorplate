# `tensorplate bundle provision`

```text
tensorplate bundle provision <name> --from <dir> [--manifest <file>] [--into <dir>]
```

Puts a bundle that the provisioning manifest lists into
`/var/lib/tensorplate/bundles/import/<name>/`, verified file by file, so
`tensorplate deploy` can take it from there. It runs in the operator's shell
on the host it provisions: it never talks to the agent, and `--device`
refuses it.

## The trust root

The provisioning manifest's schema is
`protocol/schemas/provisioning_manifest.json`. For every bundle it lists every
file, the bundle's own `manifest.json` included, with the SHA-256 and size
that file must have. The `tensorplate-cli` Debian package ships one at
`/usr/share/tensorplate/provisioning/manifest.json`, and the
`tensorplate-agent` Debian package creates the import directory. The
Homebrew packages ship neither, so on macOS pass both `--manifest` and
`--into`. The manifest this release ships lists no bundles yet, so it
provisions nothing until the model bundles are pinned.

## What it does

1. Reads the manifest and finds `<name>`.
2. If `<into>/<name>` already exists, verifies it in place and stops. It
   must be a real directory, not a symbolic link, holding exactly the listed
   files, each with its size and digest, with no symbolic link below it, and
   pass the bundle parser `tensorplate deploy` runs first. It is never
   written to, whether it verifies or not: a directory that fails is
   reported and left as it was found, and a deploy could still pick it up.
3. Otherwise creates a partial root, `<into>/.<name>.partial`. It is
   created exclusively: if one exists, from another run of this command
   that is still going or was interrupted, the run is refused and that
   directory is left alone.
4. Copies each listed file from `--from` into the partial root, hashing it
   as it is copied. A file reached through a symbolic link, at any
   component of its path below `--from`, is refused, even when its bytes
   are right. A file is opened only after its path was checked, and what
   was opened must be that same regular file. Nothing is read past the
   size the manifest pins.
5. Requires the partial root to pass the bundle parser `tensorplate
   deploy` runs first, then renames it to `<into>/<name>`. Deploy's other
   checks, against the device and the agent's configuration, still apply
   when it deploys.

Any failure in steps 3 to 5 removes the partial root this run created, so
such a run never creates a directory a deploy could pick up.

Directories it creates are mode `0755` and files `0644`, whatever the
operator's umask: the agent reads the bundle through its other bits, and
only the operator's user can write it.

`--from` is a local directory. Fetching from each file's pinned upstream
source is not part of this command yet.

`tensorplate device prune` treats every directory in the import directory
alike, so it can remove a provisioned bundle, or a partial root while a run
is using it. Do not prune while provisioning, and provision again after a
prune removed a bundle you still need.

## Outcomes

| Outcome | Exit | `error.context` |
| --- | ---: | --- |
| Provisioned, or already provisioned and verified | 0 | |
| The manifest cannot be read or is invalid | 12 | `manifest_invalid` |
| The manifest lists no such bundle | 12 | `unknown_bundle` |
| The import directory is not a directory, or is a symbolic link | 12 | `import_dir_missing` |
| A partial root for this bundle already exists | 12 | `partial_root_exists` |
| A listed file is not in `--from` | 12 | `source_missing` |
| A file in `--from` is, or sits under, a symbolic link | 12 | `symbolic_link` |
| A path in `--from` is not a regular file | 12 | `not_regular` |
| A file in `--from` changed between being checked and being opened | 12 | `changed_during_run` |
| A file's size differs from the manifest | 12 | `size_mismatch` |
| A file's SHA-256 differs from the manifest | 12 | `digest_mismatch` |
| The files verify but deploy's bundle parser refuses them | 12 | `bundle_rejected` |
| `<into>/<name>` already exists and is not exactly this bundle; the message says why | 12 | `destination_mismatch` |
| An I/O error | 12 | `io` |

With `--output json`, success prints `{"bundle", "path", "files", "bytes",
"outcome"}` as the payload, where `outcome` is `provisioned` or `verified`.
