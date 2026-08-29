// SPDX-License-Identifier: Apache-2.0
//
// `doctor --record <dir>`: capture this machine's raw platform sources as
// committable fixtures.
//
// Record-first: the raw text is written even when interpretation fails,
// because the machines this exists for are exactly the ones detection
// cannot yet interpret — a multi-GPU host, an unknown SKU, a new OS
// image. A recording that only worked on supported machines would be
// useless for growing the support matrix.
//
// The emitted JSON is the exact shape `test/platform/host_identity/`
// commits, and the emitted text file is the exact shape
// `test/platform/accelerator/` commits, so a reviewed recording is a
// `git mv` away from being a fixture.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;
use tensorplate_platform::{
    identify, identify_accelerator, identify_jetson_accelerator, AcceleratorReport,
    AcceleratorSources, DetectedPlatform, HostReport, HostSources, NvidiaSmiProbe,
    PlatformRegistry, RowMatch, SystemHostProbe,
};

use crate::error::{CliError, CliResult};
use crate::output::Renderer;

/// The committed host-identity fixture shape, field order included.
#[derive(Serialize)]
struct FixtureDoc<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    row_id: Option<String>,
    matches_row: bool,
    provenance: &'static str,
    provenance_note: String,
    sources: &'a HostSources,
    #[serde(skip_serializing_if = "Option::is_none")]
    expect: Option<ExpectDoc>,
}

/// The committed `expect` block: what detection concluded, in the exact
/// strings a row must be written in.
#[derive(Serialize)]
struct ExpectDoc {
    architecture: String,
    vendor: String,
    os_name: String,
    os_version: String,
    image_identity: Option<String>,
    machine_type: Option<String>,
}

/// What one recording produced, for rendering and for tests.
pub struct RecordOutcome {
    pub fixture_path: PathBuf,
    pub accelerator_path: Option<PathBuf>,
    /// Human-readable lines: what was interpreted, what was not, and the
    /// byte-exact SKU verdict when a row was there to compare against.
    pub notes: Vec<String>,
}

pub fn run<W: Write>(renderer: Renderer, out: &mut W, dir: &Path) -> CliResult<()> {
    let host_sources = SystemHostProbe::new()
        .sources()
        .map_err(|e| CliError::Config(format!("host sources unreadable: {e}")))?;
    let accelerator_sources = NvidiaSmiProbe::new()
        .sources()
        .map_err(|e| CliError::Config(format!("accelerator sources unreadable: {e}")))?;
    let registry = PlatformRegistry::load_installed().ok();
    let outcome = record(
        &host_sources,
        &accelerator_sources,
        registry.as_ref(),
        dir,
        &utc_date_string(),
    )?;

    let mut human = format!("recorded {}\n", outcome.fixture_path.display());
    if let Some(txt) = &outcome.accelerator_path {
        human.push_str(&format!("recorded {}\n", txt.display()));
    }
    for note in &outcome.notes {
        human.push_str(&format!("  {note}\n"));
    }
    let payload = json!({
        "fixture": outcome.fixture_path,
        "accelerator": outcome.accelerator_path,
        "notes": outcome.notes,
    });
    renderer.ok(out, "doctor", human.trim_end(), payload, None, None)?;
    Ok(())
}

/// Record one machine. Separated from [`run`] so tests can drive it from
/// committed fixture sources instead of the live machine.
pub fn record(
    host_sources: &HostSources,
    accelerator_sources: &AcceleratorSources,
    registry: Option<&PlatformRegistry>,
    dir: &Path,
    date: &str,
) -> CliResult<RecordOutcome> {
    std::fs::create_dir_all(dir)?;
    let mut notes = Vec::new();

    // Interpret what can be interpreted; failures become notes, never
    // aborts. The raw text is the deliverable.
    let host_report: Option<HostReport> = match identify(host_sources) {
        Ok(report) => Some(report),
        Err(e) => {
            notes.push(format!(
                "host sources did not interpret: {e}; recording them anyway — \
                 the expect block must be filled in at review"
            ));
            None
        }
    };
    let accelerator: Option<AcceleratorReport> = match identify_accelerator(accelerator_sources) {
        Ok(found) => found,
        Err(e) => {
            notes.push(format!(
                "accelerator sources did not interpret: {e}; raw output recorded anyway"
            ));
            None
        }
    };

    // Resolution names the row this recording is evidence for, when the
    // machine and the registry agree on one. The accelerator identity
    // comes from wherever production gets it: the discrete report when
    // `nvidia-smi` answered, else the host sources — a Jetson's GPU is
    // named by the device tree, and without it the host profile matches
    // several candidate rows and names none.
    let accelerator_identity = accelerator
        .as_ref()
        .map(|accel| accel.identity.clone())
        .or_else(|| identify_jetson_accelerator(host_sources));
    let named_row = host_report.as_ref().and_then(|report| {
        let registry = registry?;
        let identity = report.identity.clone();
        let detected = match accelerator_identity.clone() {
            Some(accel) => DetectedPlatform::with_accelerator(identity, accel),
            None => DetectedPlatform::host_only(identity),
        };
        match registry.resolve(&detected) {
            RowMatch::Supported(row)
            | RowMatch::PlannedNotValidated(row)
            | RowMatch::Experimental(row) => Some((row.row_id().to_string(), true)),
            RowMatch::OutsideValidatedEnvironment {
                candidate: Some(row),
            } => Some((row.row_id().to_string(), false)),
            _ => None,
        }
    });
    if registry.is_none() {
        notes.push("no installed platform registry; recorded without row resolution".to_string());
    }

    // The byte-exact SKU verdict, replacing the manual `od -c` step. The
    // comparison is against the named row's declared SKU; a mismatch
    // corrects the row, never the recording.
    if let (Some(accel), Some((row_id, _))) = (accelerator.as_ref(), named_row.as_ref()) {
        if let Some(declared) = registry
            .and_then(|r| r.row(row_id))
            .and_then(tensorplate_platform::PlatformSupportRow::accelerator)
        {
            let observed = &accel.identity.sku;
            if *observed == declared.sku {
                notes.push(format!(
                    "SKU byte-identical to row `{row_id}`: {observed:?} ({} bytes)",
                    observed.len()
                ));
            } else {
                notes.push(format!(
                    "SKU MISMATCH against row `{row_id}`: observed {observed:?} ({} bytes), \
                     row declares {:?} ({} bytes) — correct the row, not the recording",
                    observed.len(),
                    declared.sku,
                    declared.sku.len()
                ));
            }
        }
    }

    let stem = named_row
        .as_ref()
        .map_or("recorded-host", |(row_id, _)| row_id.as_str());
    let mut provenance_note = format!("recorded by `tensorplate doctor --record` on {date}");
    if let Some(accel) = accelerator.as_ref() {
        if let Some(driver) = &accel.exact.driver_version {
            provenance_note.push_str(&format!("; driver {driver}"));
        }
        if let Some(uuid) = &accel.exact.uuid {
            provenance_note.push_str(&format!("; device {uuid}"));
        }
    }

    let doc = FixtureDoc {
        row_id: named_row.as_ref().map(|(row_id, _)| row_id.clone()),
        matches_row: named_row.as_ref().is_some_and(|(_, matches)| *matches),
        provenance: "recorded",
        provenance_note,
        sources: host_sources,
        expect: host_report.as_ref().map(|report| {
            let identity = &report.identity;
            ExpectDoc {
                architecture: identity.architecture.as_reported().to_string(),
                vendor: identity.vendor.as_reported().to_string(),
                os_name: identity.os_name.clone(),
                os_version: identity.os_version.clone(),
                image_identity: identity.image_identity.clone(),
                machine_type: identity.machine_type.clone(),
            }
        }),
    };
    let fixture_path = dir.join(format!("{stem}.json"));
    let mut body = serde_json::to_string_pretty(&doc)
        .map_err(|e| CliError::Config(format!("fixture serialization failed: {e}")))?;
    body.push('\n');
    std::fs::write(&fixture_path, body)?;

    // The raw accelerator answer, byte-for-byte, whenever there was one.
    let accelerator_path = match &accelerator_sources.nvidia_smi_query {
        Some(raw) => {
            let path = dir.join(format!(
                "{}.txt",
                named_row
                    .as_ref()
                    .map_or("recorded-accelerator", |(row_id, _)| row_id.as_str())
            ));
            std::fs::write(&path, raw)?;
            Some(path)
        }
        None => None,
    };

    Ok(RecordOutcome {
        fixture_path,
        accelerator_path,
        notes,
    })
}

/// Today as `YYYY-MM-DD` (UTC), without a clock dependency beyond std.
fn utc_date_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let year_day = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * year_day + 2) / 153;
    let d = year_day - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::Value;

    fn repo_path(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(rel)
    }

    fn registry() -> PlatformRegistry {
        PlatformRegistry::load(&repo_path("config/platform")).expect("committed registry loads")
    }

    /// Build [`HostSources`] from a committed fixture, the same way the
    /// platform fixture harness does, so a recording round-trips through
    /// the very shape the harness consumes.
    fn sources_from_fixture(name: &str) -> (HostSources, Value) {
        let body = std::fs::read_to_string(
            repo_path("test/platform/host_identity").join(format!("{name}.json")),
        )
        .expect("read fixture");
        let fixture: Value = serde_json::from_str(&body).expect("fixture parses");
        let s = &fixture["sources"];
        let text = |key: &str| s.get(key).and_then(Value::as_str).map(str::to_string);
        (
            HostSources {
                uname_machine: text("uname_machine"),
                os_release: text("os_release"),
                cpuinfo: text("cpuinfo"),
                nv_tegra_release: text("nv_tegra_release"),
                nvidia_jetpack_version: text("nvidia_jetpack_version"),
                device_tree_model: text("device_tree_model"),
                sw_vers_product_name: text("sw_vers_product_name"),
                sw_vers_product_version: text("sw_vers_product_version"),
                sw_vers_build_version: text("sw_vers_build_version"),
                cpu_brand: text("cpu_brand"),
                hw_memsize: text("hw_memsize"),
                gce_machine_type: text("gce_machine_type"),
                proc_meminfo: text("proc_meminfo"),
                pci_devices: text("pci_devices"),
            },
            fixture,
        )
    }

    fn accelerator_text(name: &str) -> String {
        std::fs::read_to_string(repo_path("test/platform/accelerator").join(format!("{name}.txt")))
            .expect("read accelerator fixture")
    }

    #[test]
    fn a_recording_round_trips_the_committed_fixture_shape() {
        // The property that makes the tool worth having: what it writes is
        // what the harness reads. Sources in, byte-identical sources out,
        // and the expect block agrees with the committed one.
        let (sources, committed) = sources_from_fixture("lab-jetson-orin-nano-l4t-r36.5");
        let dir = tempfile::tempdir().expect("tempdir");
        let out = record(
            &sources,
            &AcceleratorSources::default(),
            Some(&registry()),
            dir.path(),
            "2026-08-24",
        )
        .expect("recording succeeds");

        assert_eq!(
            out.fixture_path.file_name().and_then(|n| n.to_str()),
            Some("jetson-orin-nano-8gb-jp62.json"),
            "a resolved recording is named for its row"
        );
        assert!(out.accelerator_path.is_none(), "no nvidia-smi on a Jetson");

        let body = std::fs::read_to_string(&out.fixture_path).expect("read recording");
        let recorded: Value = serde_json::from_str(&body).expect("recording parses");
        assert_eq!(recorded["provenance"], "recorded");
        assert_eq!(recorded["matches_row"], true);
        assert_eq!(
            recorded["sources"], committed["sources"],
            "sources must round-trip byte-identical, present keys only"
        );
        assert_eq!(
            recorded["expect"], committed["expect"],
            "detection over recorded sources must agree with the committed expectation"
        );
    }

    #[test]
    fn an_accelerator_recording_is_byte_exact_and_carries_the_sku_verdict() {
        let (sources, _) = sources_from_fixture("ubuntu2404-x86-l4-g2s8");
        let raw = accelerator_text("ubuntu2404-x86-l4-g2s8");
        let dir = tempfile::tempdir().expect("tempdir");
        let out = record(
            &sources,
            &AcceleratorSources {
                nvidia_smi_query: Some(raw.clone()),
            },
            Some(&registry()),
            dir.path(),
            "2026-08-24",
        )
        .expect("recording succeeds");

        let txt = out.accelerator_path.expect("accelerator recorded");
        assert_eq!(
            txt.file_name().and_then(|n| n.to_str()),
            Some("ubuntu2404-x86-l4-g2s8.txt")
        );
        assert_eq!(
            std::fs::read_to_string(&txt).expect("read raw"),
            raw,
            "the raw answer must be byte-for-byte what the tool said"
        );
        assert!(
            out.notes.iter().any(|n| n.contains("byte-identical")),
            "the SKU verdict replaces the manual od -c step: {:?}",
            out.notes
        );
    }

    #[test]
    fn a_sku_mismatch_is_reported_and_still_recorded() {
        // A mismatch corrects the row, never the recording — so the
        // recording must exist to correct it FROM.
        let (sources, _) = sources_from_fixture("ubuntu2404-x86-l4-g2s8");
        let tampered =
            accelerator_text("ubuntu2404-x86-l4-g2s8").replace("NVIDIA L4", "NVIDIA L4X");
        let dir = tempfile::tempdir().expect("tempdir");
        let out = record(
            &sources,
            &AcceleratorSources {
                nvidia_smi_query: Some(tampered.clone()),
            },
            Some(&registry()),
            dir.path(),
            "2026-08-24",
        )
        .expect("a mismatch is a finding, not a failure");

        let txt = out.accelerator_path.expect("still recorded");
        assert_eq!(std::fs::read_to_string(&txt).expect("read"), tampered);
        // An unknown SKU matches no row, so there is no named row to
        // carry a verdict — the recording lands under the fallback name.
        assert!(txt.ends_with("recorded-accelerator.txt"), "got {txt:?}");
    }

    #[test]
    fn an_uninterpretable_accelerator_answer_is_still_recorded() {
        // The record-first property, and the machine it exists for: a
        // multi-GPU host, whose answer detection refuses today.
        let (sources, _) = sources_from_fixture("ubuntu2404-x86-l4-g2s8");
        let one = accelerator_text("ubuntu2404-x86-l4-g2s8");
        let two_gpus = format!("{one}{one}");
        let dir = tempfile::tempdir().expect("tempdir");
        let out = record(
            &sources,
            &AcceleratorSources {
                nvidia_smi_query: Some(two_gpus.clone()),
            },
            Some(&registry()),
            dir.path(),
            "2026-08-24",
        )
        .expect("recording must not depend on interpretation");

        let txt = out.accelerator_path.expect("raw answer recorded");
        assert_eq!(
            std::fs::read_to_string(&txt).expect("read"),
            two_gpus,
            "the uninterpretable answer is the deliverable"
        );
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("did not interpret") && n.contains("recorded anyway")),
            "the failure is a note, not an abort: {:?}",
            out.notes
        );
    }
}
