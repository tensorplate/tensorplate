# macOS Homebrew Lifecycle Validation

This runbook validates the complete Homebrew appliance on the current
in-lab Apple M-series target: a MacBook Pro with Apple M1 Pro, 16 GB unified
memory, and macOS 26. The M1 Pro run backs that exact row and is the currently
available hardware target for the broader M-series Preview compatibility
envelope; it does not claim per-SKU validation for other M-series chips. It
covers formula-graph closure, launchd behavior, filesystem and UDS contracts,
packaged-only discovery, the PyTorch MPS capability, deploy smoke, offline
checks, upgrade continuity from the CLI-only formula, rollback, and uninstall.
The installed-registry stage also proves that live M1 Pro detection selects
the exact Production row instead of the lower-priority M-series Preview
fallback, while retaining the fallback's 16 GiB admission ceiling.

The harness mutates Homebrew state. Close unrelated Homebrew work first and
run it only on the validation Mac. It refuses a dirty TensorPlate tap
checkout, preserves the original tap files, and restores the supplied
CLI-only baseline formula before exit.

## Prepare immutable inputs

Use an RC or release tag when one exists. Before the tag exists, the feature
gate may use a GitHub archive URL containing the exact feature-head commit.
Record that commit in the pull request. All six candidate formulae must share
one URL, checksum, and declared validation version.

Download the archive, compute its SHA-256 digest, copy the checked-in
templates to a temporary formula directory, and replace the placeholder URL
and checksum. For a commit-pinned rehearsal, add the same explicit
pre-release `version` line to every formula so Homebrew can order it after
the installed CLI-only baseline. Do not commit these rendered validation
formulae; the checked-in files remain release templates.

Export the exact historical CLI-only formula that matches the installed
baseline:

```bash
tap_repo="$(brew --repository tensorplate/tap)"
git -C "$tap_repo" log -S 'v0.1.2.tar.gz' --format='%H' -- Formula/tensorplate.rb
git -C "$tap_repo" show <matching-commit>:Formula/tensorplate.rb \
  > /private/tmp/tensorplate-baseline.rb
```

Confirm that the baseline command and tap checkout are clean:

```bash
tensorplate version
git -C "$tap_repo" status --short
brew trust --formula \
  tensorplate/tap/tensorplate-agent \
  tensorplate/tap/tensorplate-backend-python-pytorch \
  tensorplate/tap/tensorplate-cli \
  tensorplate/tap/tensorplate-observability \
  tensorplate/tap/tensorplate-serving
```

Before the mutating run, add `--preflight-only` to the command below. The
preflight writes the host, formula-pin, baseline, and tap-trust artifacts but
does not alter packages, tap files, or services. The harness disables
Homebrew's automatic dependency removal for the entire run.

## Run

Use the checked-in MPS smoke fixture as the deploy input. Its verified model
artifact selects a package-private backend that performs and synchronizes a
PyTorch tensor operation on `mps` during load. The deployment cannot become
active if MPS is unavailable or the tensor operation fails. The bundle is
test data; the CLI, agent, serving worker, backend module, interpreter,
descriptor, and platform registry must all resolve from the installed
formulae, never from the source checkout.

```bash
TP_HOMEBREW_LIFECYCLE_ALLOW=1 \
  tools/validation/macos-homebrew-lifecycle.sh \
    --candidate-formula-dir /private/tmp/tensorplate-candidate/Formula \
    --baseline-formula /private/tmp/tensorplate-baseline.rb \
    --bundle-dir test/models/bundles/v0_1/mps_python_pytorch_smoke \
    --evidence-dir /private/tmp/tensorplate-macos-evidence
```

The run is successful only when every stage in `summary.json` and
`sanitized-transcript.json` is `pass`. `host-facts.json` deliberately
excludes serial numbers, hardware UUIDs, and provisioning identifiers.
Attach the summary, sanitized transcript, host facts, formula pin, deploy
input, and deploy result to the pull request. Keep the raw `*.log` files
local; the transcript contains only allowlisted structured results and
excludes operator paths and environment values.

The current post-reconciliation Apple M1 Pro evidence is committed as the
[`curated record`](./evidence/macos-homebrew-lifecycle-m1pro-2026-08-16.json)
and its
[`sanitized transcript`](./evidence/macos-homebrew-lifecycle-m1pro-2026-08-16-transcript.json).
Both were produced from the immutable merged feature head recorded in the
formula pin and include the installed-agent exact-row decision. The
[`previous corrected rehearsal`](./evidence/macos-homebrew-lifecycle-m1pro-2026-08-02.json)
and
[`initial rehearsal`](./evidence/macos-homebrew-lifecycle-m1pro-2026-07-29.json)
are retained as historical evidence. Raw launchctl logs remain excluded
because they contain operator-local paths and environment values.

The MPS capability stage uses the Python interpreter inside the Homebrew
PyTorch formula and calls the packaged backend probe. The separate
deploy-smoke stage proves the package-installed sidecar itself loads through
MPS and reaches an active, ready deployment. The fixture is not a real model
and makes no SmolVLA support claim.

The offline stage runs the installed doctor with its agent probe skipped and
the PyTorch MPS probe under a macOS sandbox that denies network access. The
agent probe is skipped because the sandbox denies local socket access along
with external networking. The stage does not disable the Mac's network
interface and therefore does not disrupt the operator session.

## Rollback and recovery

The normal run stops both services, removes the candidate graph, restores the
historical CLI-only formula, and verifies the original version. Homebrew
preserves `etc` and `var` content across formula removal, and the run asserts
that a state marker survives the rollback. Homebrew removes trust entries for
formulae that disappear during uninstall; the harness re-adds only those
missing component entries for the later upgrade stage and removes exactly
the entries it added before exit.

If the harness is interrupted outside its cleanup path:

```bash
brew services stop tensorplate-agent
brew services stop tensorplate-observability
brew uninstall tensorplate tensorplate-agent \
  tensorplate-backend-python-pytorch tensorplate-cli \
  tensorplate-observability tensorplate-serving
cp /private/tmp/tensorplate-baseline.rb \
  "$(brew --repository tensorplate/tap)/Formula/tensorplate.rb"
HOMEBREW_NO_AUTO_UPDATE=1 brew install tensorplate/tap/tensorplate
git -C "$(brew --repository tensorplate/tap)" restore Formula/tensorplate.rb
```

Inspect `brew services list`, `tensorplate version`, and the tap worktree
before continuing. PyTorch and build dependencies may remain installed
because they can be shared with other formulae; do not remove them
automatically.

After the run, remove the temporary component trust entries:

```bash
brew untrust --formula \
  tensorplate/tap/tensorplate-agent \
  tensorplate/tap/tensorplate-backend-python-pytorch \
  tensorplate/tap/tensorplate-cli \
  tensorplate/tap/tensorplate-observability \
  tensorplate/tap/tensorplate-serving
```
