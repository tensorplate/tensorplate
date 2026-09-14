# TensorPlate package lifecycle: reinstall, upgrade, uninstall, purge

packaging contract for the v0.1.0 packages. Operator-visible policy.

## Reinstall

`sudo apt install --reinstall tensorplate-agent` (or any other
tensorplate-* package).

| Preserved | Recreated |
| --- | --- |
| `/var/lib/tensorplate/state/` (desired state, transaction journals) | Layout directories with documented permissions. |
| `/var/lib/tensorplate/bundles/{staging,active,previous,quarantine}/` (and their contents) | The `tensorplate` system user and group. |
| `/var/log/tensorplate/` | The systemd unit files (re-enabled, not started). |
| Operator edits to `/etc/tensorplate/*.json` (managed as dpkg conffiles) | The backend descriptor under `/usr/share/tensorplate/backends/`. |

Reinstall does not start the services. Re-run `tensorplate doctor`
and then `systemctl restart tensorplate-agent tensorplate-observability`
if you want the new binaries to pick up immediately.

## Upgrade

`sudo apt upgrade tensorplate-agent` (or any tensorplate-* package).

A new package version triggers `tensorplate-agent.preinst upgrade`,
which runs
[`upgrade-preflight.sh`](../../packaging/scripts/upgrade-preflight.sh)
and refuses to proceed if:

1. Any installed config under `/etc/tensorplate/*.json` declares an
   unknown `schema_version`. The new build's supported list is the
   single source of truth — bump it deliberately in a schema migration.
2. `/var/lib/tensorplate/` is owned by a group other than
   `tensorplate`. Operator policy may set this; the preflight surfaces
   it so a half-applied upgrade does not silently leave the agent
   unable to read its own state.
3. The new package version is older than the installed one
   (`dpkg --compare-versions`). v0.1.0 has no rollback path through
   apt — see the manual reset procedure below.

When the preflight passes, dpkg unpacks the new package and runs
`tensorplate-agent.postinst configure`. dpkg does **not** restart the
services. The agent and observability `prerm` scripts stop both units
for the upgrade, and nothing in the packages starts them again: the
units are installed with `dh_installsystemd --no-start`. Start them
yourself after the upgrade:

```bash
sudo systemctl enable --now tensorplate-agent tensorplate-observability
```

The release `install.sh` runs that command itself after installing, so an
upgrade through it brings the services back. On start, the agent re-warms
the active deployment from durable state; the previous deployment is
still available for `tensorplate rollback`.

## Downgrade and rollback

`apt install tensorplate-agent=<older-version>` is refused by the upgrade
preflight, and `--allow-downgrades` does not change that: the preflight
compares the incoming version against the installed one and aborts, without
looking at durable state. Setting state aside therefore does not unlock a
downgrade — the guard is about version ordering, not about what state exists.

Rolling back means removing the installed TensorPlate packages and installing
the older version as a fresh install. `apt remove` keeps `/etc/tensorplate`
conffiles and everything under `/var/lib/tensorplate`, so operator config and
durable state survive the cycle.

Remove every installed TensorPlate package except `tensorplate-apt-source`,
rather than a fixed list. Any newer package left installed turns the older
install into a downgrade, which `apt-get -y` refuses without
`--allow-downgrades` — and the release `install.sh` runs `apt-get -y`.
`tensorplate-common` and `tensorplate-backend-python-pytorch` are the easy ones
to miss: the backend only Recommends the agent, so removing the agent leaves
it installed. `tensorplate-apt-source` only configures the APT channel and can
stay.

```bash
sudo systemctl stop tensorplate-agent tensorplate-observability
# Move durable state aside so the older agent cannot misinterpret it. If
# state.bak already exists from an earlier rollback, move that elsewhere
# first: mv would otherwise put state inside it.
sudo mv /var/lib/tensorplate/state /var/lib/tensorplate/state.bak
# `remove`, not `purge`: this keeps /etc/tensorplate and /var/lib/tensorplate.
sudo apt remove -y $(dpkg-query -W -f='${binary:Package} ${db:Status-Status}\n' 'tensorplate*' |
  awk '$1 != "tensorplate-apt-source" && $2 != "not-installed" && $2 != "config-files" {print $1}')
```

Then install the older release fresh. Its own `install.sh` installs the set
and enables and starts the services:

```bash
sudo bash <older-release>/install.sh --local-artifacts <older-release> \
  --yes --with-python-backend
```

Or install its packages directly, then check and start the services:

```bash
sudo apt install ./tensorplate-common_<older-version>_all.deb \
  ./tensorplate-agent_<older-version>_<arch>.deb \
  ./tensorplate-serving_<older-version>_<arch>.deb \
  ./tensorplate-observability_<older-version>_<arch>.deb \
  ./tensorplate-cli_<older-version>_<arch>.deb \
  ./tensorplate-backend-python-pytorch_<older-version>_all.deb
tensorplate doctor
sudo systemctl enable --now tensorplate-agent tensorplate-observability
```

Leave out `tensorplate-backend-python-pytorch` if it was not installed.

The older agent will report "no active deployment" until you decide whether
to restore `state.bak` (manually verify the schema_version of each journal
first) or to redeploy from a known-good bundle.

`<arch>` is `arm64` on Jetson and `amd64` on Ubuntu x86_64.
`test/packaging/apt-lifecycle-e2e.sh` rehearses this procedure with `dpkg`,
and asserts that the downgrade guard stays armed both with and without
durable state present. On the Ubuntu x86_64 cloud rows,
`tools/validation/ubuntu-l4-cloud-lifecycle.sh --baseline-assets-dir`
runs the upgrade and this rollback through each release's `install.sh`.

## Remove

`sudo apt remove tensorplate-agent` (or any tensorplate-* package).

| Preserved | Removed |
| --- | --- |
| `/var/lib/tensorplate/` (state, bundles) | The binary under `/usr/bin/tensorplate-*` or `/usr/lib/tensorplate/`. |
| `/var/log/tensorplate/` | The systemd unit files. |
| `/etc/tensorplate/*.json` (dpkg conffiles, prompted on edit) | |
| The `tensorplate` user and group | |

Remove is the default operator action for "stop running TensorPlate
but keep my deployments". Reinstalling the same package version
reactivates the prior state with zero data migration.

## Purge

`sudo apt purge tensorplate-agent`.

In addition to everything `remove` does, purge runs the postrm in
`purge` mode and clears:

- `/var/lib/tensorplate/state/`
- `/var/lib/tensorplate/bundles/`
- `/var/lib/tensorplate/worker-configs/`
- `/var/log/tensorplate/`
- `/run/tensorplate/`

It does **not** delete:

- The `tensorplate` user / group. Removing them would orphan files on
  appliances that share the user with other tooling; operator policy
  owns the user lifecycle.
- `/etc/tensorplate/` itself — purge only clears conffiles for the
  packages being purged. If you purge every `tensorplate-*` package,
  dpkg removes each conffile individually.

## Operator commands cheat sheet

```bash
# How is TensorPlate installed?
dpkg -l | grep tensorplate-

# Verify durable layout + binaries + units + backend descriptor.
tensorplate doctor

# Re-create the layout (idempotent; needs root).
sudo /usr/share/tensorplate/packaging/scripts/install-paths.sh

# Refuse to apply a known-bad upgrade ahead of time.
sudo /usr/share/tensorplate/packaging/scripts/upgrade-preflight.sh
```
