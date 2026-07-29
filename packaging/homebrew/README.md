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
macOS filesystem paths, installed configuration, and post-install behavior
are layered onto these templates by their owning runtime changes.

Validate the templates and renderer locally with:

```bash
test/release/verify_homebrew_formulas.sh
```
