# Homebrew formula templates

`Formula/` is the source of truth for the TensorPlate Homebrew formula graph:
five component formulas and the `tensorplate` meta-formula. Every template
uses the same placeholder source URL and checksum. The release publisher
renders all six from one tagged source archive and commits them together to
`tensorplate/homebrew-tap`, so component versions cannot drift.

The templates deliberately contain only component build and smoke-test
behavior. Service definitions, macOS filesystem paths, configuration, and
post-install behavior are added with the features that own those runtime
contracts.

Validate the templates and renderer locally with:

```bash
test/release/verify_homebrew_formulas.sh
```
