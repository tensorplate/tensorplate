// SPDX-License-Identifier: Apache-2.0
//
// Reading discrete NVIDIA accelerator identity off a live machine.
//
// Split the same way host detection is: this module's parsing is pure and
// driven by recorded text, and the part that touches the world is a thin
// wrapper that runs one command. Real GPUs live in a cloud project and a
// lab, so everything interesting has to be testable from a fixture.
//
// `nvidia-smi` rather than NVML. NVML means linking a vendor SDK, which
// would put a CUDA build dependency on a crate that agent, CLI, and
// observability all depend on — and the value we need is a string the
// tool already prints. The command boundary also keeps vendor types out
// of the public contract, which the accelerator record requires.
//
// The SKU is compared verbatim against a row, so what matters is that the
// string here is the string `nvidia-smi --query-gpu=name` prints. That is
// the named source for every value a row matches on.

use std::io::ErrorKind;
use std::process::Command;

use crate::error::PlatformProbeError;
use crate::identity::{AcceleratorIdentity, AcceleratorProbe};

/// Fields asked of `nvidia-smi`, in the order they are parsed back.
///
/// `memory.total` is recorded but never matched on: the row records a
/// nominal capacity (24 GiB for an L4) while the tool reports the usable
/// framebuffer (24564 MiB), and those are not the same number. Matching on
/// it would make every supported card miss its row.
const QUERY_FIELDS: &str = "name,memory.total,driver_version,uuid,mig.mode.current";

/// The exact strings the accelerator reported, kept alongside the
/// row-comparable identity for evidence recording and telemetry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactAcceleratorFacts {
    /// Product name exactly as reported, e.g. `NVIDIA A100-SXM4-40GB`.
    pub reported_name: String,
    /// Total framebuffer in bytes as reported, converted from the MiB the
    /// tool prints. This is the observed number, not the row's nominal one.
    pub memory_total_bytes: Option<u64>,
    /// Driver version, e.g. `550.54.15`.
    pub driver_version: Option<String>,
    /// Device UUID, which identifies the exact card in evidence.
    pub uuid: Option<String>,
    /// MIG mode exactly as reported: `Enabled`, `Disabled`, or `[N/A]` on
    /// a device that cannot partition.
    pub mig_mode: Option<String>,
}

/// A detected accelerator: the row-comparable identity plus the exact
/// facts it was read from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceleratorReport {
    pub identity: AcceleratorIdentity,
    pub exact: ExactAcceleratorFacts,
}

/// Raw accelerator sources gathered from the machine.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AcceleratorSources {
    /// stdout of the `nvidia-smi` query, or `None` when the tool is not
    /// installed at all.
    pub nvidia_smi_query: Option<String>,
}

/// Interpret gathered sources into an accelerator report.
///
/// `Ok(None)` means this machine has no discrete NVIDIA accelerator, which
/// is a normal answer — it is how the CPU-only rows are told apart from
/// the GPU ones.
///
/// # Errors
///
/// Returns [`PlatformProbeError::Unrecognized`] when the tool answered but
/// its answer cannot be interpreted as one device: a malformed row, or
/// more than one GPU. Every row this release claims is single-GPU, so
/// quietly taking the first device would resolve a two-GPU host to a row
/// whose evidence was never collected on it.
pub fn identify_accelerator(
    sources: &AcceleratorSources,
) -> Result<Option<AcceleratorReport>, PlatformProbeError> {
    let Some(raw) = sources.nvidia_smi_query.as_deref() else {
        return Ok(None);
    };
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    match lines.len() {
        // The tool is installed and answered with no devices. Treated as
        // absence rather than a failure: the probe only reaches here on a
        // successful exit, and a successful query listing nothing is the
        // tool's way of saying there is no GPU.
        0 => Ok(None),
        1 => parse_device(lines[0]).map(Some),
        n => Err(PlatformProbeError::Unrecognized {
            source_name: "nvidia-smi".to_string(),
            detail: format!(
                "{n} accelerators reported; every supported row is single-GPU, so no row's \
                 evidence covers this machine"
            ),
        }),
    }
}

/// Parse one CSV row of [`QUERY_FIELDS`].
fn parse_device(line: &str) -> Result<AcceleratorReport, PlatformProbeError> {
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    // The query names five fields; anything else means the tool answered
    // in a shape this code was not written against, and guessing which
    // column is the name would be worse than saying so.
    if fields.len() != 5 {
        return Err(PlatformProbeError::Unrecognized {
            source_name: "nvidia-smi".to_string(),
            detail: format!(
                "expected {} comma-separated fields for `{QUERY_FIELDS}`, got {}: `{line}`",
                5,
                fields.len()
            ),
        });
    }
    // The product name goes through the same no-value filter as every
    // other column. It is the one field that becomes a match key, so
    // letting a sentinel through would turn "I could not read this card"
    // into the affirmative claim "this card is off-matrix" — the exact
    // collapse of unreadable into unsupported this module exists to avoid.
    let Some(name) = optional(fields[0]) else {
        return Err(PlatformProbeError::Unrecognized {
            source_name: "nvidia-smi".to_string(),
            detail: format!(
                "device reported no usable product name (`{}`); the name is what a row is \
                 matched on, so an unreadable one is not an unsupported card",
                fields[0]
            ),
        });
    };

    let (partitioned, mig_mode) = parse_mig_mode(fields[4], line)?;

    Ok(AcceleratorReport {
        identity: AcceleratorIdentity {
            // Verbatim: the row records exactly what this tool prints, so
            // any normalization here would be a second spelling of the
            // same fact and a way for the two to drift apart.
            sku: name.clone(),
            partitioned,
        },
        exact: ExactAcceleratorFacts {
            reported_name: name,
            memory_total_bytes: optional(fields[1]).and_then(|mib| mebibytes_to_bytes(&mib)),
            driver_version: optional(fields[2]),
            uuid: optional(fields[3]),
            mig_mode,
        },
    })
}

/// Interpret `mig.mode.current` into (partitioned, recorded value).
///
/// Whitelisted rather than "Enabled or else false". Partitioning is
/// rejected before any SKU comparison, so a value this code does not
/// understand defaulting to *not partitioned* is the one direction that
/// fails open: a partitioned card would resolve to its row and be served
/// at a capacity that row's evidence was never collected at. An
/// uninterpretable MIG state is therefore an error, not a `false`.
fn parse_mig_mode(field: &str, line: &str) -> Result<(bool, Option<String>), PlatformProbeError> {
    // Deliberately NOT `optional()`. That filter treats every bracketed
    // value as "no value", which is right for evidence columns and wrong
    // here: `[N/A]` means this card cannot partition, while
    // `[Unknown Error]` means the state could not be read. Collapsing them
    // would put an unreadable partitioning state back into the
    // not-partitioned bucket, which is the direction that fails open.
    let value = field.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("[N/A]") || value.eq_ignore_ascii_case("N/A")
    {
        // The card has no MIG at all. Recording the sentinel would put a
        // meaningless string into release evidence.
        return Ok((false, None));
    }
    let value = value.to_string();
    match value.to_ascii_lowercase().as_str() {
        "enabled" => Ok((true, Some(value))),
        "disabled" => Ok((false, Some(value))),
        _ => Err(PlatformProbeError::Unrecognized {
            source_name: "nvidia-smi".to_string(),
            detail: format!(
                "unrecognized mig.mode.current `{value}` in `{line}`; an uninterpretable \
                 partitioning state must not read as unpartitioned"
            ),
        }),
    }
}

/// A field the tool had no value for. `nvidia-smi` prints `[N/A]` and
/// variants rather than leaving a column empty.
fn optional(field: &str) -> Option<String> {
    let value = field.trim();
    // Bracketed sentinels are how this tool says it could not read a
    // field on an otherwise successful query. They are values about the
    // TOOL, never about the card.
    if value.is_empty()
        || value.eq_ignore_ascii_case("N/A")
        || (value.starts_with('[') && value.ends_with(']'))
    {
        return None;
    }
    Some(value.to_string())
}

/// `--format=nounits` prints `memory.total` as a bare MiB count.
///
/// Saturating rather than wrapping: this is an attacker-irrelevant local
/// tool, but a value large enough to overflow is a value this code did not
/// understand, and silently wrapping it would record a tiny framebuffer
/// for an enormous card.
fn mebibytes_to_bytes(field: &str) -> Option<u64> {
    field
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024 * 1024)
}

/// Reads discrete NVIDIA accelerator identity by running `nvidia-smi`.
#[derive(Clone, Debug, Default)]
pub struct NvidiaSmiProbe {
    /// Tool to run. Tests point this at a stub that prints a fixture.
    program: Option<String>,
}

impl NvidiaSmiProbe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `program` instead of `nvidia-smi`.
    #[must_use]
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: Some(program.into()),
        }
    }

    fn program(&self) -> &str {
        self.program.as_deref().unwrap_or("nvidia-smi")
    }

    /// Gather accelerator sources from this machine.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformProbeError::Unreadable`] when the tool is present
    /// but does not answer. A host with the NVIDIA userland installed and
    /// a driver that will not load is a broken GPU machine, not a machine
    /// without a GPU — and reporting it as the latter would resolve it to
    /// a CPU-only row, which is exactly the wrong answer to hand an
    /// operator whose driver is broken.
    pub fn sources(&self) -> Result<AcceleratorSources, PlatformProbeError> {
        let program = self.program();
        let output = match Command::new(program)
            .args([
                &format!("--query-gpu={QUERY_FIELDS}"),
                "--format=csv,noheader,nounits",
            ])
            .output()
        {
            Ok(output) => output,
            // The tool not being installed is how a machine says it has no
            // NVIDIA stack. Unlike host detection — where every command is
            // one the platform requires — this one is asked speculatively
            // of every machine, so absence is a fact rather than a fault.
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(AcceleratorSources {
                    nvidia_smi_query: None,
                })
            }
            Err(err) => {
                return Err(PlatformProbeError::Unreadable {
                    source_name: program.to_string(),
                    detail: err.to_string(),
                })
            }
        };
        if !output.status.success() {
            // The documented "no devices" answer is a fact about the
            // machine, not a broken source — the same distinction the host
            // probe draws for `dpkg-query`. A GCP image with drivers baked
            // in, booted on a shape with no GPU, is a real CPU-only host
            // and must be free to match a CPU-only row.
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            if said.to_ascii_lowercase().contains("no devices were found") {
                return Ok(AcceleratorSources {
                    nvidia_smi_query: None,
                });
            }
            return Err(PlatformProbeError::Unreadable {
                source_name: program.to_string(),
                detail: if let Some(code) = output.status.code() {
                    format!(
                        "`{program} --query-gpu` exited {code}: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )
                } else {
                    format!("`{program} --query-gpu` was terminated by a signal")
                },
            });
        }
        Ok(AcceleratorSources {
            nvidia_smi_query: Some(String::from_utf8_lossy(&output.stdout).into_owned()),
        })
    }

    /// Detect the accelerator, keeping the exact facts alongside identity.
    ///
    /// # Errors
    ///
    /// As [`Self::sources`] and [`identify_accelerator`].
    pub fn detect(&self) -> Result<Option<AcceleratorReport>, PlatformProbeError> {
        identify_accelerator(&self.sources()?)
    }
}

impl AcceleratorProbe for NvidiaSmiProbe {
    fn detect_accelerator(&self) -> Result<Option<AcceleratorIdentity>, PlatformProbeError> {
        Ok(self.detect()?.map(|report| report.identity))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sources(text: &str) -> AcceleratorSources {
        AcceleratorSources {
            nvidia_smi_query: Some(text.to_string()),
        }
    }

    /// A throwaway executable standing in for `nvidia-smi`, so the probe's
    /// own command handling is exercised rather than only the parser.
    #[cfg(unix)]
    fn write_stub(body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "tp-nvidia-smi-stub-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[test]
    fn a_machine_without_the_tool_has_no_accelerator() {
        // The CPU-only rows are told apart from the GPU ones exactly here.
        assert_eq!(
            identify_accelerator(&AcceleratorSources::default()).expect("absence is not an error"),
            None
        );
    }

    #[test]
    fn a_successful_query_listing_nothing_is_absence() {
        assert_eq!(
            identify_accelerator(&sources("\n  \n")).expect("no devices"),
            None
        );
    }

    #[test]
    fn more_than_one_accelerator_is_refused_rather_than_narrowed() {
        // Every row this release claims is single-GPU. Taking the first
        // device would resolve a two-GPU host to a row whose evidence was
        // never collected on it.
        let err = identify_accelerator(&sources(
            "NVIDIA L4, 24564, 550.54.15, GPU-aaa, [N/A]\n\
             NVIDIA L4, 24564, 550.54.15, GPU-bbb, [N/A]\n",
        ))
        .expect_err("two GPUs must not silently become one");
        match err {
            PlatformProbeError::Unrecognized { detail, .. } => {
                assert!(detail.contains('2'), "names the count: {detail}");
            }
            other @ PlatformProbeError::Unreadable { .. } => {
                panic!("expected Unrecognized, got {other:?}")
            }
        }
    }

    #[test]
    fn a_short_row_is_refused_rather_than_guessed_at() {
        let err = identify_accelerator(&sources("NVIDIA L4, 24564\n"))
            .expect_err("a row in an unexpected shape must not be interpreted");
        assert!(matches!(err, PlatformProbeError::Unrecognized { .. }));
    }

    #[test]
    fn a_name_the_tool_could_not_read_is_refused_rather_than_becoming_a_sku() {
        // The one field that becomes a match key. Letting a sentinel
        // through turns "I could not read this card" into the affirmative
        // claim "this card is off-matrix", which is the collapse of
        // unreadable into unsupported that detection exists to avoid — and
        // it would be recorded as the product name in release evidence.
        for name in [
            "",
            "[N/A]",
            "N/A",
            "[Not Supported]",
            "[Unknown Error]",
            "[Insufficient Permissions]",
        ] {
            let err = identify_accelerator(&sources(&format!(
                "{name}, 24564, 550.54.15, GPU-aaa, [N/A]\n"
            )))
            .expect_err("an unreadable product name is not a SKU");
            assert!(
                matches!(err, PlatformProbeError::Unrecognized { .. }),
                "`{name}` must be Unrecognized, got {err:?}"
            );
        }
    }

    #[test]
    fn an_uninterpretable_partitioning_state_is_an_error_not_unpartitioned() {
        // The one direction that fails OPEN. Partitioning is rejected
        // before any SKU comparison, so a MIG value this code does not
        // understand defaulting to `false` would let a partitioned card
        // resolve to its row and be served at a capacity that row's
        // evidence was never collected at.
        for mode in ["[Unknown Error]", "Pending", "Enabling", "yes", "1"] {
            let line = format!("NVIDIA A100-SXM4-40GB, 40960, 550.54.15, GPU-a, {mode}\n");
            match identify_accelerator(&sources(&line)) {
                Err(PlatformProbeError::Unrecognized { detail, .. }) => {
                    assert!(detail.contains(mode), "names the value: {detail}");
                }
                other => panic!("`{mode}` must not read as unpartitioned, got {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_host_with_the_userland_but_no_gpu_has_no_accelerator_rather_than_a_broken_one() {
        // A cloud image with drivers baked in, booted on a shape with no
        // GPU, is a real CPU-only host. Reporting it as unreadable would
        // stop it matching the CPU-only row it actually is.
        let stub = write_stub("echo 'No devices were found' >&2; exit 6");
        assert_eq!(
            NvidiaSmiProbe::with_program(stub.to_string_lossy())
                .sources()
                .expect("the documented no-devices answer is absence")
                .nvidia_smi_query,
            None
        );
        std::fs::remove_file(&stub).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_probe_asks_for_exactly_the_columns_the_parser_reads_back() {
        // Nothing else binds QUERY_FIELDS to the positional indices in
        // `parse_device`. Reorder the query and every column silently
        // shifts: the MIG flag would be read from a different field and a
        // partitioned card could resolve to its row.
        let stub = write_stub(
            "printf '%s' \"$*\" > \"$0.argv\";              echo 'NVIDIA L4, 23034, 550.54.15, GPU-a, Disabled'",
        );
        let report = NvidiaSmiProbe::with_program(stub.to_string_lossy())
            .detect()
            .expect("stub answers")
            .expect("one device");
        assert_eq!(report.identity.sku, "NVIDIA L4");

        let argv = std::fs::read_to_string(format!("{}.argv", stub.display())).expect("argv");
        assert!(
            argv.contains(&format!("--query-gpu={QUERY_FIELDS}")),
            "the query must ask for exactly the columns parse_device indexes: {argv}"
        );
        assert!(
            argv.contains("--format=csv,noheader,nounits"),
            "nounits is what makes memory.total a bare MiB count: {argv}"
        );
        std::fs::remove_file(&stub).ok();
        std::fs::remove_file(format!("{}.argv", stub.display())).ok();
    }

    #[test]
    fn the_sku_is_carried_through_verbatim() {
        // Rows record exactly what the tool prints. Any normalization here
        // would be a second spelling of the same fact.
        let report = identify_accelerator(&sources(
            "NVIDIA A100-SXM4-40GB, 40960, 550.54.15, GPU-abc, Disabled\n",
        ))
        .expect("parses")
        .expect("one device");
        assert_eq!(report.identity.sku, "NVIDIA A100-SXM4-40GB");
        assert_eq!(report.exact.reported_name, "NVIDIA A100-SXM4-40GB");
        assert_eq!(report.exact.driver_version.as_deref(), Some("550.54.15"));
        assert_eq!(report.exact.uuid.as_deref(), Some("GPU-abc"));
    }

    #[test]
    fn only_enabled_means_partitioned() {
        for (mode, expected) in [
            ("Enabled", true),
            ("enabled", true),
            ("Disabled", false),
            ("disabled", false),
            ("[N/A]", false),
        ] {
            let report = identify_accelerator(&sources(&format!(
                "NVIDIA A100-SXM4-40GB, 40960, 550.54.15, GPU-abc, {mode}\n"
            )))
            .expect("parses")
            .expect("one device");
            assert_eq!(
                report.identity.partitioned, expected,
                "mig.mode.current `{mode}` must read as partitioned={expected}"
            );
        }
    }

    #[test]
    fn a_device_that_cannot_partition_reports_no_mig_mode_rather_than_a_false_one() {
        // `[N/A]` is "this card has no MIG", not "MIG state unknown".
        // Recording it as a value would put a meaningless string into
        // release evidence.
        let report =
            identify_accelerator(&sources("NVIDIA L4, 24564, 550.54.15, GPU-abc, [N/A]\n"))
                .expect("parses")
                .expect("one device");
        assert_eq!(report.exact.mig_mode, None);
        assert!(!report.identity.partitioned);
    }

    #[test]
    fn memory_is_recorded_in_bytes_from_the_reported_mebibytes() {
        let report =
            identify_accelerator(&sources("NVIDIA L4, 24564, 550.54.15, GPU-abc, [N/A]\n"))
                .expect("parses")
                .expect("one device");
        assert_eq!(
            report.exact.memory_total_bytes,
            Some(24_564 * 1024 * 1024),
            "the observed framebuffer is recorded as reported"
        );
    }

    #[test]
    fn the_reported_framebuffer_is_not_the_rows_nominal_capacity() {
        // Why memory must never become a match dimension: an L4's row
        // records 24 GiB and the tool reports 24564 MiB. Matching on it
        // would make every supported card miss its own row.
        let report =
            identify_accelerator(&sources("NVIDIA L4, 24564, 550.54.15, GPU-abc, [N/A]\n"))
                .expect("parses")
                .expect("one device");
        let nominal_24_gib: u64 = 25_769_803_776;
        assert_ne!(report.exact.memory_total_bytes, Some(nominal_24_gib));
    }

    #[test]
    fn an_unparseable_memory_value_does_not_sink_the_whole_reading() {
        // The SKU is what a row matches on. Losing the framebuffer costs
        // evidence detail; losing the SKU would report a supported card as
        // unsupported.
        let report = identify_accelerator(&sources(
            "NVIDIA L4, [Not Supported], 550.54.15, GPU-abc, [N/A]\n",
        ))
        .expect("parses")
        .expect("one device");
        assert_eq!(report.identity.sku, "NVIDIA L4");
        assert_eq!(report.exact.memory_total_bytes, None);
    }

    #[test]
    fn an_absurd_memory_value_is_dropped_rather_than_wrapped() {
        let report = identify_accelerator(&sources(&format!(
            "NVIDIA L4, {}, 550.54.15, GPU-abc, [N/A]\n",
            u64::MAX
        )))
        .expect("parses")
        .expect("one device");
        assert_eq!(report.exact.memory_total_bytes, None);
    }

    #[test]
    fn a_missing_tool_is_absence_but_a_failing_one_is_an_error() {
        // A driver that will not load is a broken GPU machine, not a
        // machine without a GPU: reporting absence would resolve it to a
        // CPU-only row and tell an operator with a broken driver that
        // their platform is fine.
        assert_eq!(
            NvidiaSmiProbe::with_program("tp-definitely-not-nvidia-smi")
                .sources()
                .expect("a missing tool is absence")
                .nvidia_smi_query,
            None
        );
        let err = NvidiaSmiProbe::with_program("false")
            .sources()
            .expect_err("a tool that fails must not read as absence");
        assert!(matches!(err, PlatformProbeError::Unreadable { .. }));
    }
}
