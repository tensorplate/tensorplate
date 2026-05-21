# TensorPlate package lifecycle: reinstall, upgrade, uninstall, purge

V01-E14-F07 contract for the v0.1.0 packages. Operator-visible policy.

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
`tensorplate-agent.postinst configure`. dpkg restarts the agent and
observability units automatically (the units land via
`dh_installsystemd`). The agent re-warms the active deployment from
durable state; the previous deployment is still available for
`tensorplate rollback`.

## Unsupported downgrade

`apt install tensorplate-agent=<older-version>` is refused by the
upgrade preflight. To revert manually:

```bash
sudo systemctl stop tensorplate-agent
sudo systemctl stop tensorplate-observability
# Move durable state aside so the older agent cannot misinterpret it.
sudo mv /var/lib/tensorplate/state /var/lib/tensorplate/state.bak
sudo apt install --allow-downgrades tensorplate-*=<older-version>
```

The older agent will report "no active deployment" until you decide
whether to restore `state.bak` (manually verify the schema_version of
each journal first) or to redeploy from a known-good bundle.

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
