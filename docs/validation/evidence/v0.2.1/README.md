# Release evidence bundles

One directory per Production support row, named by row id. Each holds the
`lifecycle-report.json` that row's support claim rests on, the stage logs
the report cites, and the `artifact-digest.txt` the harness wrote naming
the artifact it installed.

That sidecar is what says *what* the report's `subject.artifact_digest`
is a digest of, since the report itself has nowhere to carry it. Its
second column is a released file name or a public archive URL, never a
path off the machine that ran it — a property of how the harnesses write
it, not something to clean up afterwards.

`tools/release/check-evidence-bundles.sh` reads these during the release
workflow's evidence gate, and every build and publish job sits behind that
gate. A row whose directory is absent, whose report fails schema
validation, whose subject names a different version, or whose stages did
not all pass will block the final release.

Release candidates may publish with incomplete evidence so their artifacts
can be used to collect it. Build-only runs also report incomplete evidence
without blocking. A checker that fails to reach a verdict blocks every
mode; only an incomplete-evidence verdict receives those exemptions.

## Why these are tracked

The gate runs in a fresh checkout of the release ref and downloads
nothing. Evidence written only to `dist/` — which is gitignored — was
therefore unreachable from the job that has to read it, so no release
could ever have passed the gate. Committing the bundles makes the
passing direction achievable rather than theoretical.

## Sanitize before the first commit

These are published in a public repository, and a value that reaches a
commit stays in the branch history until that history is rewritten.
Before committing a bundle, remove from the report and from every log:

- cloud project ids and numbers, instance ids, account ids, and billing
  identifiers
- device serial numbers and GPU UUIDs
- host names, user names, machine and boot ids, and network addresses
- fleet, quota, or authentication status

Replace each with the synthetic value below rather than a note in the
log. These values say they are synthetic by themselves, they are the
forms the scanner below accepts, and they are plain text, so no JSON,
schema or matcher is affected. Replace a value the same way everywhere it appears:
a failing stage's `detail` quotes the tail of its log, and the two must
still agree.

| Identifier | Synthetic value |
| --- | --- |
| host name, in any journal prefix, `hostnamectl` or `uname` line | `tp-synthetic-host` |
| `.internal` or `.local` host name | `tp-synthetic-host`, without the suffix |
| account name in a home path | `/home/tp-synthetic-operator`, `/Users/tp-synthetic-operator` |
| machine id, boot id, journal directory id | 32 zeros |
| GPU or MIG UUID | `GPU-00000000-0000-0000-0000-000000000001` (count up the last group) |
| any other UUID | `00000000-0000-0000-0000-000000000001` |
| cloud project in a resource path | `projects/REDACTED/` |
| IPv4 address | `192.0.2.10`, or anything in `198.51.100.0/24` or `203.0.113.0/24` |
| IPv6 address | `2001:db8::10` |
| MAC address | `00:00:5e:00:53:01` |
| email address | an address at `example.com`, `example.org` or `example.net` |
| serial number or UDID | `REDACTED` |
| journal field other than the service's own | drop the field |

Loopback, `0.0.0.0`, the metadata server `169.254.169.254` and
`metadata.google.internal` are not identifiers and stay as recorded, as
do the product's per-invocation `cli-`, `tx-` and `deploy-` ids.
Credentials and planning identifiers have no synthetic form: neither
belongs in evidence, so remove them.

### Scan before `git add`

`tools/validation/check-evidence-publication.sh` is the check. List the
run's own host name and FQDN, account name, cloud project id and number,
instance id and zone in a literal file, one per line, kept **outside**
the repository (the scanner refuses a literal file inside the checkout
or inside a checkout that encloses it), then scan the bundle:

```bash
tools/validation/check-evidence-publication.sh \
  --literals <literal file outside the repository> \
  docs/validation/evidence/<version>/<row_id>
```

Exit 0 means publishable, 1 means findings, and 2 means the scan reached
no verdict, including when JSON decoding exceeds its work limit. An
incomplete scan does not clear a bundle for publication.
Each finding names a file, a line and an identifier class,
never the value it matched, and a path whose own name matched prints as
`path#N` instead. This includes account and project identifiers that only
become recognizable when directory names are read together. Scan
the sanitized copy and only then run `git add`: a finding after a commit
means rewriting the branch. CI runs the same scanner with
`--patterns-only` on every pull request, but it cannot know the
run's own names — only the local `--literals` scan can.

A name the evidence also carries as ordinary text, such as a host or
account name that is also a distribution, package, path or product name,
cannot be listed bare: it would match every line that legitimately holds
the word. List the forms that identify the machine or its operator
instead — the shell prompt, `<name>.local`, `hostname:` and `/etc/hosts`
lines, journal prefixes, `/home/<name>`, `uid=N(<name>)`,
`sudo: <name> :`, `for <name> from` — and before `git add` read every
remaining occurrence of the bare name in the bundle, the row file and the
commit message, and confirm each is that ordinary text. No scan can tell
them apart, so that reading is the check for such a name. A physical
device's machine id and serial numbers belong in its literal file too:
the patterns recognize a machine id or a serial only after its label, so
a bare value passes both modes.

- Remove authorization headers carrying Basic or Bearer credentials,
  including ones recorded as JSON fields. Encoding a value or field name
  in JSON does not sanitize it; the scan checks decoded fields and values
  and retains repeated object members for inspection.
- Never commit terminal captures, shell history, or archives. The
  scanner treats binary and non-UTF-8 files as findings because they
  cannot be reviewed.
- Derive `stages.tsv` and the report before editing any log. The Jetson
  adapter reads stage times from log file modification times, so a
  sanitized copy would restamp every stage.
- Retain the raw, unsanitized run privately alongside the release
  record — it is the artifact a later regression gets diffed against.
  Never put it, or the literal file, inside the checkout. The journal
  captures of the Ubuntu cloud and Jetson harnesses are the exception:
  both project each `journalctl` capture to the service's own fields as
  they record it, so the projected capture is what is retained and no
  raw journal copy exists. Every other file of those runs is still
  retained raw.

The full rules these follow are in `docs/validation/fixture-and-evidence-rules.md`.
