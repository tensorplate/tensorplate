# macOS Homebrew Lifecycle Validation

This runbook validates the complete Homebrew appliance on the supported
in-lab MacBook Pro: Apple M1 Pro, 16 GB unified memory, and macOS 26 or
newer. It covers formula-graph closure, launchd behavior, filesystem and UDS
contracts, packaged-only discovery, the PyTorch MPS capability, deploy smoke,
offline checks, upgrade continuity from the CLI-only formula, rollback, and
uninstall.

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

Use the checked-in dependency-free fixture bundle as the deploy input. The
bundle is test data; the CLI, agent, serving worker, backend module,
interpreter, descriptor, and platform registry must all resolve from the
installed formulae, never from the source checkout.

```bash
TP_HOMEBREW_LIFECYCLE_ALLOW=1 \
  tools/validation/macos-homebrew-lifecycle.sh \
    --candidate-formula-dir /private/tmp/tensorplate-candidate/Formula \
    --baseline-formula /private/tmp/tensorplate-baseline.rb \
    --bundle-dir test/models/bundles/v0_1/smolvla_python_pytorch \
    --evidence-dir /private/tmp/tensorplate-macos-evidence
```

The run is successful only when every stage in `summary.json` is `pass`.
`host-facts.json` deliberately excludes serial numbers, hardware UUIDs, and
provisioning identifiers. Review every log for other local-only information
before attaching the evidence directory to a pull request.

The MPS stage uses the Python interpreter inside the Homebrew PyTorch
formula and calls the packaged backend capability probe. The deploy-smoke
fixture exercises the package-installed `python_pytorch` sidecar boundary
without claiming that the fixture is a real SmolVLA model.

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
