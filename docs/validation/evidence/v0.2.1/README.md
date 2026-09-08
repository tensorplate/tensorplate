# Release evidence bundles

One directory per Production support row, named by row id. Each holds the
`lifecycle-report.json` that row's support claim rests on, plus the stage
logs the report cites.

`tools/release/check-evidence-bundles.sh` reads these during the release
workflow's evidence gate, and every build and publish job sits behind that
gate. A row whose directory is absent, whose report fails schema
validation, whose subject names a different version, or whose stages did
not all pass will block the release.

## Why these are tracked

The gate runs in a fresh checkout of the release ref and downloads
nothing. Evidence written only to `dist/` — which is gitignored — was
therefore unreachable from the job that has to read it, so no release
could ever have passed the gate. Committing the bundles makes the
passing direction achievable rather than theoretical.

## Sanitize before the first commit

These are published in a public repository. Before committing a bundle,
remove from the report and from every log:

- cloud project ids, account ids, and billing identifiers
- device serial numbers and GPU UUIDs
- host names, user names, and internal network addresses
- fleet, quota, or authentication status

Replace them with synthetic values and say in the log that they are
synthetic. Retain the raw, unsanitized run privately alongside the
release record — it is the artifact a later regression gets diffed
against.

The full rules these follow are in `docs/validation/fixture-and-evidence-rules.md`.
