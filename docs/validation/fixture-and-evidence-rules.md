# Fixture and evidence rules

Normative for every fixture, recording, coverage claim and push in this
repository. Each rule was earned by a real review finding, rules 1 to 9 in
the v0.2.1 cycle; none is speculative. Violations are correctness bugs,
not style.

## Recording

1. **Record first, interpret second.** The raw output is the deliverable.
   A capture must succeed on unsupported machines, such as an unknown
   SKU or a readable multi-GPU host, and when a source answer cannot be
   interpreted, such as a malformed accelerator row. Preserve every raw
   device row in either case. Interpretation failures become notes, never
   aborts; a readable but unsupported topology is not such a failure.

2. **Recordings replace transcriptions, and a mismatch corrects the row —
   never the recording.** A transcribed fixture proves the parser and the
   matching path; only a recorded one proves the strings are what the
   fleet reports. When a recorded value contradicts a transcribed row
   fact, the row is wrong.

3. **Published fixtures are sanitized.** No live cloud account or project
   identifiers, no device UUIDs or serials, no internal fleet, quota, or
   auth status. Replace identifiers with clearly synthetic values, say so
   where the fixture is described, and retain the unsanitized capture
   privately with the release evidence. Matching must never read a field
   that sanitization touches. **Sanitize before the first commit**: a
   later sanitization commit leaves the identifiers in the branch
   history, and the branch must then be rewritten before merge.
   `tensorplate doctor --record` labels its output as private raw evidence,
   and the fixture harness rejects live-looking GCP project numbers and
   device UUIDs outside the repository's explicit synthetic namespaces or
   legacy synthetic allowlist. Adding an exception is therefore a visible,
   reviewable code change rather than an accidental paste. Lifecycle
   evidence is checked by `tools/validation/check-evidence-publication.sh`,
   run with a private literal file before `git add` and with patterns
   only in CI on every pull request; its synthetic values are listed in
   `docs/validation/evidence/v0.2.1/README.md`. Where a harness can
   record only what belongs in evidence, it does so instead of leaving an
   editing job behind: the Ubuntu cloud harness projects each journal
   capture to the service's own fields as it takes it, so the projected
   capture is the record and no raw copy of it is kept.

## Asserting

4. **A coverage claim must be executable.** "Harness-asserted" means a
   test fails when the artifact lies. Harnesses that map fixtures to rows
   by filename skip anything not named for a row — an extra recording
   needs an explicit paired test that drives it end to end and resolves
   it to the row it claims to cover.

5. **Match-key strings are compared byte-for-byte,** printed quoted with
   their byte lengths on mismatch. Trimming and eyeballing both miss the
   non-breaking space.

6. **Every guard is mutation-checked before it is trusted.** Break the
   thing the test guards and watch it fail; if it cannot fail, it is not
   a test. A control case must discriminate — prove the check rejects for
   its own reason and not as a blanket refusal.

## Claiming

7. **Identity derivations fail closed.** Detection answers only for
   inputs it has been told about, at the granularity the source can
   support; an unknown input yields no answer, never a guess. Deriving a
   more precise claim than the source carries (a patch release from a
   product line, a validation claim from a name match) is how unvalidated
   hardware gets admitted as validated.

8. **Externally sourced facts are verified at the source when recorded,**
   not recalled — vendor archives, cloud catalogs, package channels — and
   the comment says where the fact came from, so the next reader can
   re-verify instead of re-trusting.

9. **Prose is evidence too.** A document must not describe completed work
   as pending, assert what a test does not enforce, or carry operational
   detail the repository's audience has no use for. Stale prose fails
   review the same as stale code.

## Publishing

10. **Nothing private reaches the public repository, and every push is
    scanned before it happens.** A push here is permanent: a force-push
    does not withdraw a commit that forks, pull request refs and caches
    already hold, and a pull request's body becomes its squash-merge
    commit message. So no credential, cloud account fact, host or device
    identity, or reference to private planning material appears in a
    file, a file name, a commit message or author, a branch name, or a
    pull request's title or body. `tools/validation/check-public-hygiene.sh`
    scans all of them as committed: every changed file as each commit of
    the range wrote it, merges included, the commits' messages and
    authors, the branch checked out, and the pull request text it is
    given. It works in two tiers:

    - **Evidence**: recorded evidence and fixtures of real machines,
      under `docs/validation/evidence/` and `test/platform/` in any letter
      case, go through `check-evidence-publication.sh` as rule 3
      describes: patterns only in CI, and with the private literal file
      when `--literals FILE` is given. They go through the source policy
      as well, and a symlink standing in for one of these directories is
      a finding. Every platform row's evidence location is under one of
      these, or under an ignored build path such as `dist/` that is never
      committed.
    - **Source**: every file, and all commit and pull request text, is
      checked against a narrow set of shapes that never belong in public
      source. These are private keys, documented token prefixes, and a
      kubeconfig key or HTTP credential after its key, on the key's line
      or a later one (pretty-printed JSON, a YAML block scalar, with any
      YAML anchor, tag or comment between); cloud service accounts,
      `projects/<number>` paths and keyed project numbers of six or more
      digits; and home directories other than the synthetic operator's
      and the GitHub runner's. They also cover device
      UUIDs outside the all-zero namespace and IP addresses outside the
      loopback, unspecified, "this network" and documentation ranges (the
      metadata address and macOS's `fe80::1` also pass), judged by value:
      an IPv4-mapped address is its IPv4 address however it is written,
      and any other address with a dotted quad is IPv6. Last come private
      planning references, as shapes only. A four-part version reads as
      an address unless a pip pin, Debian revision or wheel name makes it
      a version. The evidence scanner's host, journal, UUID, serial,
      e-mail, MAC, machine-id, cloud-project and planning classes are not
      part of the source policy: negative tests, synthetic identities and
      the scanners' own patterns carry those legitimately. A binary file
      is a finding of its own, so each new image or recording needs a
      reviewed allowlist line, and its printable text is still checked for
      the long shapes.

    Run it before every push, with the title and body the pull request
    will carry, and run the tree scan too. The tree scan fails when a
    change leaves an allowlist count wrong. Add `--literals FILE` when the
    change touches evidence:

    ```bash
    tools/validation/check-public-hygiene.sh --base origin/develop --local \
      --message pr-title.txt --message pr-body.md
    tools/validation/check-public-hygiene.sh --tree
    ```

    CI runs the same script on every pull request, and again when its
    title or body is edited, without `--local`. A finding before a push is
    fixed by rewriting the branch so that no commit carries the value: a
    later commit that removes it does not clear the finding, because the
    earlier commit is still published. A value found after a push is
    already disclosed. A credential is rotated, and neither a force-push
    nor a later commit undoes the disclosure. The only override is
    `tools/validation/public-hygiene-allowlist.txt`, reviewed like code:
    one exact path, class, count and reason per line. It accepts up to that
    many findings of that class in that file, counting every occurrence,
    never commit or pull request text, never a credential, and never a
    file under an evidence path or a finding only the evidence scanner
    makes, since evidence is sanitized rather than excepted. The evidence
    scanner reports one finding per line, class and length, so a value
    only it decodes could not be counted against an entry. A count
    bounds how many values are accepted, not which: a value replaced one
    for one keeps the count and is left to review of the diff.
    `test/validation/public_hygiene_test.sh` asserts that the tree passes
    as committed with every count exact.
