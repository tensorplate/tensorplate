# Fixture and evidence rules

Normative for every fixture, recording, and coverage claim in this
repository. Each rule was earned by a real review finding in the v0.2.1
cycle; none is speculative. Violations are correctness bugs, not style.

## Recording

1. **Record first, interpret second.** The raw output is the deliverable.
   A capture must succeed on machines detection cannot interpret — an
   unknown SKU, a multi-GPU host, a new OS image — because those are the
   machines a capture exists for. Interpretation failures become notes,
   never aborts.

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
