# `tensorplate bundle provision`

```text
tensorplate bundle provision <name> [--from <dir>] [--manifest <file>] [--into <dir>]
```

Provisions a verified bundle into
`/var/lib/tensorplate/bundles/import/<name>/` for `tensorplate deploy`.
It runs in the operator's shell on that host. It never talks to the agent;
`--device` refuses it.

## Trust root and references

The manifest uses `protocol/schemas/provisioning_manifest.json`. It pins every
file, including the bundle's `manifest.json`, by SHA-256 and byte size. The
operator's `--manifest` is the trust root, verified exactly like the example
shipped at `/usr/share/tensorplate/provisioning/manifest.json` by
`tensorplate-cli`. This is an example, not a model catalog.

The packaged example contains `stt-whisper-candidate` and
`tts-kokoro-candidate`, the Whisper CT2 and Kokoro/af_heart validation references.
They use the candidate-only unary runner contract and grant no qualified
speech claim. [Reference provenance](../../packaging/provisioning/README.md)
records their immutable upstream revisions and conversion limits. No conversion
runs on this host.

Without `--from`, each file must declare either `url` or `source_path`:

- `url` is an HTTPS location pinned by the file digest. HTTP is accepted only
  for `localhost` or `127.0.0.1` test origins. Credentials and fragments are
  refused. Redirects must stay on HTTPS; TLS verification remains enabled.
- `source_path` names a file relative to the provisioning manifest, such as
  the packaged entry or bundle manifest. Escapes and symbolic links below
  that directory are refused.

`--from` instead copies the listed files from a local directory, ignoring the
fetch sources. Manifests without fetch sources remain valid for this path.
Homebrew ships neither the example manifest nor the import directory; on
macOS supply `--manifest` and an existing `--into` directory.

```sh
tensorplate bundle provision stt-whisper-candidate
tensorplate bundle provision tts-kokoro-candidate
tensorplate deploy /var/lib/tensorplate/bundles/import/stt-whisper-candidate
```

Provisioning verifies integrity; deploy still applies all device, runtime and
admission checks. Serving with egress denied requires completed online
provisioning and identity contact within the same kernel boot; ordinary host
reboot recovery requires a new metadata contact.

## Transfer, verification and retry

An existing destination is verified in place: exactly the listed files, no
symbolic links, matching sizes and digests, and acceptance by deploy's bundle
parser. No files are written or downloaded. A mismatch leaves it as found.

A new run creates `<into>/.<name>.partial` exclusively. This also locks its
transfer cache, `<into>/.<name>.download`, for the whole run. A concurrent run
or a partial root left after a killed process is refused. Once no run is
active, remove only the stale partial root and run again to retain downloads.

The cache is a private mode-0700 directory with flat digest-named files; it
has no deployable bundle manifest. Transfers use `curl`, with its config file
disabled, a 15-second connection timeout and a 600-second transfer timeout per
file. Completed cached files are reverified and skipped. Partial files resume
using an HTTP byte range. If the origin cannot resume, the command fails and
keeps the partial bytes; remove the cache to retry from zero.

Network or I/O failures keep cached bytes for retry and remove this run's
partial root. A size or digest mismatch removes the cache and partial root.
Unlisted cache files and links are refused. After every file verifies, the
command copies into the partial root, checks deploy's bundle parser and
atomically renames the root into place. A failed rename keeps verified cached
bytes for retry. After publication, cache cleanup is best effort: a cleanup
failure leaves harmless digest-named files and the verified bundle remains a
success. Only a verified bundle is published. Bundle directories are mode
`0755` and files `0644`, independent of umask.

`tensorplate device prune` can remove any directory in the import directory,
including a transfer cache or a live partial root. Do not prune while
provisioning. Provision again after a prune removes a bundle you need.

## Outcomes

Provisioning failures exit 12. JSON errors use stable `error.context` tokens:

| Token | Meaning |
| --- | --- |
| `manifest_invalid`, `unknown_bundle` | Invalid trust root or absent name |
| `import_dir_missing`, `partial_root_exists` | Missing import directory or concurrent/stale run |
| `fetch_source_missing`, `fetch_failed` | Missing source, invalid cache or failed curl transfer; the latter reports curl's exit code without upstream response text |
| `source_missing`, `symbolic_link`, `not_regular`, `changed_during_run` | Unsafe or unavailable local file |
| `size_mismatch`, `digest_mismatch` | Bytes disagree with the manifest |
| `bundle_rejected`, `destination_mismatch` | Invalid bundle or different existing destination |
| `io` | File I/O failure |

Success exits 0. With `--output json`, the payload contains `bundle`, `path`,
`files`, `bytes` and `outcome` (`provisioned` or `verified`).
