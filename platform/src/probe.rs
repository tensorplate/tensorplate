// SPDX-License-Identifier: Apache-2.0
//
// Reading host identity sources off a live machine.
//
// This is the only part of detection that touches the world. It gathers
// raw source content and hands it to [`crate::detect::identify`], which is
// pure — so the interesting logic stays testable from fixtures and this
// module stays small enough to review by eye.
//
// Every source is best-effort: a missing file or an absent command yields
// `None`, not an error, because absence is how platforms are told apart.
// Detection fails only when what is present cannot be interpreted at all.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::detect::{identify, HostReport, HostSources};
use crate::error::PlatformProbeError;
use crate::identity::{HostIdentity, HostProbe};

/// The GCE metadata service, addressed by its link-local IP rather than by
/// name so a broken resolver cannot turn detection into a DNS timeout.
const METADATA_ADDR: &str = "169.254.169.254:80";
const METADATA_PATH: &str = "/computeMetadata/v1/instance/machine-type";

/// How long the metadata service gets. It is on the local link and answers
/// in single-digit milliseconds; anything slower is a machine that is not
/// on GCE, and detection must not stall a service start over it.
const METADATA_TIMEOUT: Duration = Duration::from_millis(250);

/// Reads host identity from the running machine.
#[derive(Clone, Debug, Default)]
pub struct SystemHostProbe {
    /// Root to read system files under. Empty in production; tests point
    /// it at a staged tree.
    root: Option<std::path::PathBuf>,
}

impl SystemHostProbe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read system files under `root` instead of `/`.
    #[must_use]
    pub fn with_root(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn path(&self, absolute: &str) -> std::path::PathBuf {
        match &self.root {
            Some(root) => root.join(absolute.trim_start_matches('/')),
            None => std::path::PathBuf::from(absolute),
        }
    }

    fn read(&self, absolute: &str) -> Option<String> {
        read_lossy(&self.path(absolute))
    }

    /// Gather every source this machine offers.
    #[must_use]
    pub fn sources(&self) -> HostSources {
        let staged = self.root.is_some();
        HostSources {
            uname_machine: run("uname", &["-m"]),
            os_release: self.read("/etc/os-release"),
            cpuinfo: self.read("/proc/cpuinfo"),
            nv_tegra_release: self.read("/etc/nv_tegra_release"),
            nvidia_jetpack_version: (!staged)
                .then(|| run("dpkg-query", &["-W", "-f=${Version}", "nvidia-jetpack"]))
                .flatten(),
            device_tree_model: self.read("/proc/device-tree/model"),
            sw_vers_product_name: (!staged)
                .then(|| run("sw_vers", &["-productName"]))
                .flatten(),
            sw_vers_product_version: (!staged)
                .then(|| run("sw_vers", &["-productVersion"]))
                .flatten(),
            sw_vers_build_version: (!staged)
                .then(|| run("sw_vers", &["-buildVersion"]))
                .flatten(),
            cpu_brand: (!staged)
                .then(|| run("sysctl", &["-n", "machdep.cpu.brand_string"]))
                .flatten(),
            gce_machine_type: self.gce_machine_type(),
        }
    }

    /// Detect the host, keeping the exact facts alongside the identity.
    ///
    /// # Errors
    ///
    /// As [`crate::detect::identify`].
    pub fn detect(&self) -> Result<HostReport, PlatformProbeError> {
        identify(&self.sources())
    }

    /// The machine type, on machines that have one.
    ///
    /// The metadata service is only contacted when the host already looks
    /// like a Compute Engine instance. A physical workstation must come
    /// back with no machine type — its row declares none — and must never
    /// pay a network timeout to find that out.
    fn gce_machine_type(&self) -> Option<String> {
        if !self.looks_like_gce() {
            return None;
        }
        query_metadata(METADATA_ADDR, METADATA_PATH, METADATA_TIMEOUT)
    }

    fn looks_like_gce(&self) -> bool {
        // Set by the firmware, so it is readable without privileges and
        // without asking the network anything.
        self.read("/sys/class/dmi/id/product_name")
            .is_some_and(|name| name.trim() == "Google Compute Engine")
    }
}

impl HostProbe for SystemHostProbe {
    fn detect_host(&self) -> Result<HostIdentity, PlatformProbeError> {
        self.detect().map(|report| report.identity)
    }
}

fn read_lossy(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Run a command and return its trimmed stdout, or `None` if the binary is
/// absent or it failed. A missing tool is a platform that does not have
/// it, not an error.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// One bounded HTTP/1.0 GET against the metadata service.
///
/// Hand-rolled rather than pulling in an HTTP client: this is a single
/// fixed request to a link-local address, and every phase carries a
/// timeout so detection cannot hang a service start.
fn query_metadata(addr: &str, path: &str, timeout: Duration) -> Option<String> {
    let socket = addr.parse().ok()?;
    let mut stream = std::net::TcpStream::connect_timeout(&socket, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: metadata.google.internal\r\nMetadata-Flavor: Google\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;

    // Bounded read: the answer is a short resource name, and an unbounded
    // read from an unauthenticated endpoint is not something to offer.
    let mut buffer = Vec::new();
    stream.take(8 * 1024).read_to_end(&mut buffer).ok()?;
    let response = String::from_utf8_lossy(&buffer);

    let (head, body) = response
        .split_once("\r\n\r\n")
        .or_else(|| response.split_once("\n\n"))?;
    let status_ok = head
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200"));
    if !status_ok {
        return None;
    }
    let body = body.trim();
    (!body.is_empty()).then(|| body.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_that_is_not_gce_is_never_asked_for_a_machine_type() {
        // The row for the physical workstation declares no machine type, so
        // detection must produce none — without a network round trip.
        let staging = std::env::temp_dir().join(format!("tp-probe-{}", std::process::id()));
        std::fs::create_dir_all(staging.join("sys/class/dmi/id")).expect("stage");
        std::fs::write(
            staging.join("sys/class/dmi/id/product_name"),
            "Precision 7960 Tower\n",
        )
        .expect("write product name");

        let probe = SystemHostProbe::with_root(&staging);
        assert!(!probe.looks_like_gce());
        assert_eq!(probe.gce_machine_type(), None);

        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn a_gce_host_is_recognized_from_firmware_alone() {
        let staging = std::env::temp_dir().join(format!("tp-probe-gce-{}", std::process::id()));
        std::fs::create_dir_all(staging.join("sys/class/dmi/id")).expect("stage");
        std::fs::write(
            staging.join("sys/class/dmi/id/product_name"),
            "Google Compute Engine\n",
        )
        .expect("write product name");

        assert!(SystemHostProbe::with_root(&staging).looks_like_gce());

        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn an_unreachable_metadata_service_yields_no_machine_type() {
        // Port 9 discards; connecting must fail or time out rather than
        // producing a value or hanging.
        let started = std::time::Instant::now();
        assert_eq!(
            query_metadata(
                "169.254.169.254:9",
                METADATA_PATH,
                Duration::from_millis(100)
            ),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "detection must stay bounded, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn missing_commands_and_files_are_absence_not_failure() {
        assert_eq!(run("tp-definitely-not-a-real-binary", &[]), None);
        assert_eq!(read_lossy(Path::new("/tp/definitely/not/here")), None);
    }
}
