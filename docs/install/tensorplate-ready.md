# TensorPlate-ready hosts, the two-command install, and upgrades

APT can only install TensorPlate after the host knows about the
TensorPlate repository. That knowledge comes from exactly one thing: the
`tensorplate-apt-source` bootstrap package, which installs the archive
keyring (`/usr/share/keyrings/tensorplate-archive-keyring.gpg`) and the
Deb822 source (`/etc/apt/sources.list.d/tensorplate.sources`) for the
stable channel `https://packages.tensorplate.com/apt`.

A host with that package installed is **TensorPlate-ready**. On a
TensorPlate-ready Jetson, the entire runtime install is:

```bash
sudo apt update
sudo apt install tensorplate
```

Everything else in this document is about how hosts become
TensorPlate-ready, how to validate that state, and how upgrades flow
afterwards.

## Building a TensorPlate-ready image (provisioning runbook)

Image builders and provisioning flows make a host TensorPlate-ready by
installing the bootstrap package while preparing the rootfs or during
first-boot provisioning:

1. Download `tensorplate-apt-source_<version>_all.deb` from the latest
   TensorPlate GitHub Release and verify it against the release
   `SHA256SUMS` (and cosign bundle) per
   [`external-install.md`](./external-install.md).
2. Install it into the image:

   ```bash
   sudo dpkg -i tensorplate-apt-source_<version>_all.deb
   ```

   The package installs only the keyring and source file. It does not
   install runtime packages, start services, mutate runtime state, or run
   `apt update` — an image build stays fully offline after the `.deb` is
   fetched.
3. Validate the image before shipping it:

   ```bash
   tools/validation/tensorplate-ready-check.sh           # offline state check
   sudo tools/validation/tensorplate-ready-check.sh --online   # channel reachability + trust
   ```

   The check fails closed on a missing/armored keyring, a wrong or
   version-pinned repository URI, a `Signed-By` mismatch, unmanaged
   source files, trust warnings from `apt update`, or a missing
   `tensorplate` candidate.

### Fresh-boot validation (what the user experiences)

On first boot of a TensorPlate-ready Jetson, the runtime install must be
the two commands above and nothing else. Validation evidence captures:

- `sudo apt update` completes with the TensorPlate repository listed and
  **no** trust warnings.
- `sudo apt install tensorplate` installs `tensorplate-common`,
  `tensorplate-agent`, `tensorplate-serving`, `tensorplate-observability`,
  and `tensorplate-cli` (the optional Python backend stays uninstalled).
- `tensorplate doctor` reports green per the clean-install runbook.

Workstations are CLI-only: `sudo apt install tensorplate-cli` on Ubuntu
AMD64 (see [`macos-cli.md`](./macos-cli.md) for the macOS path).

## Stock Ubuntu/Jetson hosts (fallback bootstrap)

A stock image has never heard of TensorPlate, so
`sudo apt install tensorplate` fails with
`E: Unable to locate package tensorplate`. That is expected, not a bug:
configure the source once, then use the same two commands forever.

```bash
sudo dpkg -i tensorplate-apt-source_<version>_all.deb   # one-time bootstrap
sudo apt update
sudo apt install tensorplate
```

`install.sh` from the GitHub Release assets remains supported as the
no-APT fallback installer; the bootstrap path above is preferred because
it gives the host normal `apt upgrade` behavior afterwards.

### Failure cases

| Symptom | Meaning | Fix |
| --- | --- | --- |
| `E: Unable to locate package tensorplate` | Host is not TensorPlate-ready (stock image, or source file removed). | Run the one-time bootstrap above. |
| `NO_PUBKEY …` / `…is not signed` during `apt update` | Keyring missing, corrupt, or does not match the repository signature. | Reinstall `tensorplate-apt-source`; rerun `tensorplate-ready-check.sh`. |
| `tensorplate-apt-source` postinst fails: `required file missing or empty` | An admin deleted the source conffile; dpkg preserves that decision on reinstall. | `sudo apt purge tensorplate-apt-source`, then reinstall the bootstrap package. |
| Repository unreachable (404/timeout) during `apt update` | Network/CDN issue, or a pre-release host pointing at a channel that is not published yet. | Retry; verify the URI is the stable channel with `tensorplate-ready-check.sh`. |

## Upgrading a v0.1.1 host to v0.1.2 (GitHub-assets install → APT)

v0.1.1 was installed with `install.sh` from GitHub Release assets, before
the APT channel existed. The upgrade to v0.1.2 is the bootstrap plus the
same two commands — package names are identical, so APT upgrades the
installed set in place:

```bash
# 1. One-time bootstrap (from the v0.1.2 GitHub Release assets):
sudo dpkg -i tensorplate-apt-source_0.1.2-1_all.deb

# 2. The standard runtime install, which now performs the upgrade:
sudo apt update
sudo apt install tensorplate
```

Expectations (per the [package lifecycle contract](./lifecycle.md)):

- All five runtime packages move from `0.1.1-1` to the `0.1.2-1` channel
  versions; the `tensorplate` metapackage is added and owns the set from
  now on.
- Operator configuration under `/etc/tensorplate/`, desired state,
  bundles, and logs are preserved exactly as on any package upgrade.
- After the upgrade, `tensorplate doctor` must report green and
  `tensorplate version` must report the new version.
- Subsequent releases arrive through normal `apt update` /
  `apt upgrade` — the bootstrap step never repeats.

## Future-version discovery (release validation step)

Each release validation proves the channel keeps working for the *next*
release: publish a newer staging version to the same repository (staging
destination of the `APT Repository` workflow), then on an installed
host run nothing but:

```bash
sudo apt update
apt-cache policy tensorplate
```

The new version must appear as the candidate without touching
`tensorplate-apt-source`. CI rehearses this lifecycle end-to-end on a
disposable arm64 runner — previous-release baseline, stock-state failure
modes, bootstrap, signed-repo in-place upgrade, then a staged higher
version discovered by plain `apt update` — via
[`test/packaging/apt-lifecycle-e2e.sh`](../../test/packaging/apt-lifecycle-e2e.sh)
(`.github/workflows/apt-lifecycle.yml`, path-filtered to packaging and
release-tooling changes). Hardware behavior stays with the Jetson T4
checklist below.

## Jetson hardware validation checklist (T4, per release)

On the TensorPlate-ready Jetson validation target:

- [ ] `tensorplate-ready-check.sh` passes offline, then `--online`.
- [ ] Fresh-boot two-command install succeeds; capture the full apt
      transcript for the release evidence.
- [ ] Enable both services per the
      [clean-install runbook](./clean-install-runbook.md):
      `sudo systemctl enable --now tensorplate-agent
      tensorplate-observability`, then `tensorplate doctor` green.
- [ ] v0.1.1 → v0.1.2 upgrade flow above on a host installed from the
      v0.1.1 release assets; config/state preserved.
- [ ] Future-version discovery against the staging channel.

Record evidence with the release sign-off per
[`docs/release/runbook.md`](../release/runbook.md).
