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

The provisioning manifest, `/usr/share/tensorplate/provisioning/manifest.json`,
is shipped by `tensorplate-cli`; its schema is
`protocol/schemas/provisioning_manifest.json`. For every bundle it lists every
file, the bundle's own `manifest.json` included, with the SHA-256 and size
that file must have. `--manifest` points at another one. The manifest this
release ships lists no bundles yet, so it provisions nothing until the model
bundles are pinned.

## What it does

1. Reads the manifest and finds `<name>`.
2. If `<into>/<name>` already exists, verifies it in place and stops. It
   must hold exactly the listed files, each with its size and digest, and
   pass deploy's own bundle check. Nothing is rewritten on success. On
   failure the directory is left as it was found.
3. Otherwise copies each listed file from `--from` into a partial root,
   `<into>/.<name>.partial`, hashing it as it is copied. A file reached
   through a symbolic link, at any component of its path, is refused, even
   when its bytes are right. A partial root left by an interrupted run is
   discarded first.
4. Requires the partial root to hold exactly the listed files and to pass the
   same bundle check `tensorplate deploy` makes, then renames it to
   `<into>/<name>`.

Any failure removes the partial root, so a failed run leaves nothing a deploy
could pick up.

`--from` is a local directory. Fetching from each file's pinned upstream
source is not part of this command yet.

## Outcomes

| Outcome | Exit | `error.context` |
| --- | ---: | --- |
| Provisioned, or already provisioned and verified | 0 | |
| The manifest cannot be read or is invalid | 12 | `manifest_invalid` |
| The manifest lists no such bundle | 12 | `unknown_bundle` |
| The import directory does not exist | 12 | `import_dir_missing` |
| A listed file is not in `--from` | 12 | `source_missing` |
| A file is, or sits under, a symbolic link | 12 | `symbolic_link` |
| A path is not a regular file | 12 | `not_regular` |
| A file's size differs from the manifest | 12 | `size_mismatch` |
| A file's SHA-256 differs from the manifest | 12 | `digest_mismatch` |
| An existing destination holds a file the manifest does not list | 12 | `unlisted_file` |
| The files verify but deploy's bundle check refuses them | 12 | `bundle_rejected` |
| An I/O error | 12 | `io` |

With `--output json`, success prints `{"bundle", "path", "files", "bytes",
"outcome"}` as the payload, where `outcome` is `provisioned` or `verified`.
