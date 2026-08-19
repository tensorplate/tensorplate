# Homebrew formula templates

`Formula/` is the source of truth for the TensorPlate Homebrew formula graph:
five component formulas and the `tensorplate` meta-formula. Every template
uses the same placeholder source URL and checksum. The release publisher
renders all six from one tagged source archive and commits them together to
`tensorplate/homebrew-tap`, so component versions cannot drift.

The agent and observability templates also define independent Homebrew
services. Each starts when its launchd job is loaded, restarts after an
unsuccessful exit, and uses a five-second launch throttle. The serving worker
has no service definition: the agent owns and supervises that process.

`conf/` contains the three prefix-rendered runtime configs. The owning
component formula installs each config under Homebrew's `etc/tensorplate`,
creates its required state, runtime, and log directories under Homebrew's
`var`, and fails post-install if it cannot enforce the documented modes.
Agent and observability service output is routed to `var/log/tensorplate`;
structured diagnostics use `events.ndjson`.

The agent formula also installs the platform registry under
`share/tensorplate/platform`. The Python/PyTorch component installs its
backend descriptor under `share/tensorplate/backends/python_pytorch` and
renders the descriptor's interpreter to the PyTorch formula's private
Python. The agent and observability services plus the packaged CLI export the
matching discovery paths; the agent also exports interpreter and module paths
that are inherited by the serving worker and its sidecar. Native packages
retain their `/usr/share/tensorplate` defaults.

Validate the templates and renderer locally with:

```bash
test/release/verify_homebrew_formulas.sh
```
