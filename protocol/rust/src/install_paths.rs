// SPDX-License-Identifier: Apache-2.0
//
// packaging: TensorPlate v0.1.0 installed-filesystem layout.
//
// These constants are the single source of truth for the on-device
// layout shipped by the native packages (`packaging/debian/`). Maintainer
// scripts, packaging tests, the agent, the CLI, and the `tensorplate
// doctor` install probes all consult this module so that a change to a
// path is a one-place change.
//
// Contracts (mirrors `docs/install/filesystem-layout.md`):
//
//   /etc/tensorplate/                  config root         root:tensorplate 0750
//   /etc/tensorplate/agent.json        agent config        root:tensorplate 0640
//   /etc/tensorplate/observability.json                    root:tensorplate 0640
//   /etc/tensorplate/serving_worker.json                   root:tensorplate 0640
//   /etc/tensorplate/cli.json          CLI default profile root:tensorplate 0644
//
//   /var/lib/tensorplate/              durable state root  tensorplate:tensorplate 0750
//   /var/lib/tensorplate/state/        desired-state etc.  tensorplate:tensorplate 0750
//   /var/lib/tensorplate/bundles/staging                   tensorplate:tensorplate 0750
//   /var/lib/tensorplate/bundles/active                    tensorplate:tensorplate 0750
//   /var/lib/tensorplate/bundles/previous                  tensorplate:tensorplate 0750
//   /var/lib/tensorplate/bundles/quarantine                tensorplate:tensorplate 0750
//   /var/lib/tensorplate/worker-configs/                   tensorplate:tensorplate 0750
//
//   /var/log/tensorplate/              diagnostic logs     tensorplate:tensorplate 0750
//
//   /run/tensorplate/                  tmpfs (systemd-managed) 0750
//   /run/tensorplate/agent.sock        agent control UDS   tensorplate:tensorplate 0660
//
// Runtime / share paths are read-only:
//
//   /usr/bin/tensorplate-agent
//   /usr/bin/tensorplate-observability
//   /usr/bin/tensorplate                                   (CLI)
//   /usr/lib/tensorplate/tensorplate-serving               (agent-supervised)
//   /usr/lib/tensorplate/backends/python_pytorch/          (optional backend sources)
//   /usr/share/tensorplate/backends/<backend>/backend.json (backend descriptor)
//
// Paths are exposed as `&'static str` so tests and maintainer scripts
// can assert on them by-value. Helpers return [`PathBuf`] for callers
// that compose under them.

use std::path::PathBuf;

/// System user that owns durable state, log, and runtime directories.
pub const SYSTEM_USER: &str = "tensorplate";

/// Primary group for [`SYSTEM_USER`]. The agent control socket is
/// world-unreadable but group-accessible so the CLI can be granted
/// access via group membership without making the socket world-writable.
pub const SYSTEM_GROUP: &str = "tensorplate";

/// Root configuration directory.
pub const ETC_DIR: &str = "/etc/tensorplate";

/// Agent runtime config installed by `tensorplate-agent`.
pub const AGENT_CONFIG_PATH: &str = "/etc/tensorplate/agent.json";

/// Observability service runtime config installed by `tensorplate-observability`.
pub const OBSERVABILITY_CONFIG_PATH: &str = "/etc/tensorplate/observability.json";

/// Serving worker runtime config installed by `tensorplate-serving`.
pub const SERVING_WORKER_CONFIG_PATH: &str = "/etc/tensorplate/serving_worker.json";

/// CLI configuration installed by `tensorplate-cli`.
pub const CLI_CONFIG_PATH: &str = "/etc/tensorplate/cli.json";

/// Durable state root.
pub const STATE_DIR: &str = "/var/lib/tensorplate";

/// Desired-state and transaction journals.
pub const STATE_INNER_DIR: &str = "/var/lib/tensorplate/state";

/// Bundle staging root. Each verified bundle lands at
/// `<BUNDLE_STAGING_DIR>/<deployment_id>/`.
pub const BUNDLE_STAGING_DIR: &str = "/var/lib/tensorplate/bundles/staging";

/// Active deployment (symlink or directory) maintained by the deploy
/// transaction.
pub const BUNDLE_ACTIVE_DIR: &str = "/var/lib/tensorplate/bundles/active";

/// Previous deployment retained for rollback.
pub const BUNDLE_PREVIOUS_DIR: &str = "/var/lib/tensorplate/bundles/previous";

/// Quarantined bundles. Failed deploys land here for operator review.
pub const BUNDLE_QUARANTINE_DIR: &str = "/var/lib/tensorplate/bundles/quarantine";

/// Agent-rendered serving-worker configs (one per warming candidate).
pub const WORKER_CONFIG_DIR: &str = "/var/lib/tensorplate/worker-configs";

/// Diagnostic log directory. journald is preferred; on-disk JSON-lines
/// retention writes here.
pub const LOG_DIR: &str = "/var/log/tensorplate";

/// Runtime directory (tmpfs, managed by systemd `RuntimeDirectory=`).
pub const RUN_DIR: &str = "/run/tensorplate";

/// Agent control Unix domain socket path. CLI clients connect here.
pub const AGENT_SOCKET_PATH: &str = "/run/tensorplate/agent.sock";

/// Read-only directory holding backend descriptors. Each backend lives
/// at `<BACKEND_DESCRIPTOR_DIR>/<backend>/backend.json`.
pub const BACKEND_DESCRIPTOR_DIR: &str = "/usr/share/tensorplate/backends";

/// Descriptor path for the Python/PyTorch backend.
pub const PYTHON_PYTORCH_BACKEND_DESCRIPTOR: &str =
    "/usr/share/tensorplate/backends/python_pytorch/backend.json";

/// Installed location of the serving worker binary (no systemd unit; the
/// agent supervises this process — see V01-E09 and the F03 packaging
/// note).
pub const SERVING_BINARY_PATH: &str = "/usr/lib/tensorplate/tensorplate-serving";

/// Octal mode constants matching the layout documented in
/// `docs/install/filesystem-layout.md`. Use these in tests so any drift
/// from the package maintainer scripts is caught.
pub mod mode {
    /// Directory mode for config, state, log, and runtime roots.
    pub const DIR_0750: u32 = 0o0750;
    /// Mode for config files readable by the service group.
    pub const FILE_0640: u32 = 0o0640;
    /// Mode for the CLI config which operators may read directly.
    pub const FILE_0644: u32 = 0o0644;
    /// Mode for the agent control socket. Group-readable, world-unreadable.
    pub const SOCKET_0660: u32 = 0o0660;
}

/// Convenience helper returning the canonical config path for the agent.
#[must_use]
pub fn agent_config_path() -> PathBuf {
    PathBuf::from(AGENT_CONFIG_PATH)
}

/// Convenience helper returning the canonical state root for the agent.
#[must_use]
pub fn agent_state_dir() -> PathBuf {
    PathBuf::from(STATE_DIR)
}

/// Convenience helper returning the canonical staging directory.
#[must_use]
pub fn bundle_staging_dir() -> PathBuf {
    PathBuf::from(BUNDLE_STAGING_DIR)
}

/// Convenience helper returning the canonical agent socket path.
#[must_use]
pub fn agent_socket_path() -> PathBuf {
    PathBuf::from(AGENT_SOCKET_PATH)
}

/// All directories the install scripts must create. The packaging
/// verification suite asserts each one is present with the correct mode
/// and owner after a fresh install.
///
/// Returned in install order (parents before children) so a naive
/// caller can iterate and `mkdir` each entry without sorting.
#[must_use]
pub fn required_directories() -> &'static [&'static str] {
    &[
        ETC_DIR,
        STATE_DIR,
        STATE_INNER_DIR,
        // bundles/ parent created by staging entry's mkdir -p in scripts.
        BUNDLE_STAGING_DIR,
        BUNDLE_ACTIVE_DIR,
        BUNDLE_PREVIOUS_DIR,
        BUNDLE_QUARANTINE_DIR,
        WORKER_CONFIG_DIR,
        LOG_DIR,
        RUN_DIR,
        BACKEND_DESCRIPTOR_DIR,
    ]
}

/// All config files installed by the core packages.
#[must_use]
pub fn required_config_files() -> &'static [&'static str] {
    &[
        AGENT_CONFIG_PATH,
        OBSERVABILITY_CONFIG_PATH,
        SERVING_WORKER_CONFIG_PATH,
        CLI_CONFIG_PATH,
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::path::Path;

    #[test]
    fn etc_paths_are_under_etc_tensorplate() {
        for p in required_config_files() {
            assert!(
                Path::new(p).starts_with("/etc/tensorplate"),
                "{p} should live under /etc/tensorplate"
            );
        }
    }

    #[test]
    fn state_paths_are_under_var_lib() {
        for p in [
            STATE_DIR,
            STATE_INNER_DIR,
            BUNDLE_STAGING_DIR,
            BUNDLE_ACTIVE_DIR,
            BUNDLE_PREVIOUS_DIR,
            BUNDLE_QUARANTINE_DIR,
            WORKER_CONFIG_DIR,
        ] {
            assert!(
                Path::new(p).starts_with(STATE_DIR),
                "{p} should live under {STATE_DIR}"
            );
        }
    }

    #[test]
    fn runtime_directory_is_tmpfs_managed() {
        assert!(Path::new(AGENT_SOCKET_PATH).starts_with(RUN_DIR));
    }

    #[test]
    fn required_dirs_in_install_order() {
        let dirs = required_directories();
        // Parents come before children: /var/lib/tensorplate before
        // /var/lib/tensorplate/state, /etc/tensorplate before any
        // dependent file install.
        let state_idx = dirs.iter().position(|d| *d == STATE_DIR).unwrap();
        let state_inner_idx = dirs.iter().position(|d| *d == STATE_INNER_DIR).unwrap();
        assert!(state_idx < state_inner_idx);
        let bundle_staging_idx = dirs.iter().position(|d| *d == BUNDLE_STAGING_DIR).unwrap();
        assert!(state_idx < bundle_staging_idx);
    }

    #[test]
    fn modes_are_documented_octals() {
        // Trip-wire for accidental edits: the *value* of these constants
        // is what the maintainer scripts and packaging tests assert
        // against. If we ever loosen perms we want a deliberate diff.
        assert_eq!(mode::DIR_0750, 0o0750);
        assert_eq!(mode::FILE_0640, 0o0640);
        assert_eq!(mode::FILE_0644, 0o0644);
        assert_eq!(mode::SOCKET_0660, 0o0660);
    }

    #[test]
    fn backend_descriptor_for_python_pytorch_is_under_share() {
        assert!(
            Path::new(PYTHON_PYTORCH_BACKEND_DESCRIPTOR).starts_with(BACKEND_DESCRIPTOR_DIR),
            "backend descriptor must live under {BACKEND_DESCRIPTOR_DIR}"
        );
    }
}
