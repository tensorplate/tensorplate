// SPDX-License-Identifier: Apache-2.0

//! Parsers for independent command/proc captures and operator sampling.

pub mod sampling;

use std::collections::{HashMap, HashSet};

use roxmltree::{Document, Node, ParsingOptions};
use tensorplate_protocol::{
    json_numbers::MAX_SAFE_BYTES, BudgetDomainName, ByteObservation, DomainMemory,
    MemoryObservation, MemorySource, ProcessAggregate, ProcessMemory, ProcessRole,
    UnavailableReason,
};

const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
const MAX_XML_NODES: u32 = 100_000;
const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

/// An invalid capture is never converted to a zero-valued observation.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum MemorySampleError {
    #[error("memory capture exceeds the parser size bound")]
    Oversized,
    #[error("malformed memory capture: {0}")]
    Malformed(&'static str),
    #[error("memory capture requires exactly one whole device")]
    DeviceCount,
    #[error("memory observation is inconsistent: {0}")]
    Inconsistent(&'static str),
}

type SampleResult<T> = Result<T, MemorySampleError>;

/// Process ownership is supplied by supervision, not guessed from command output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub role: ProcessRole,
}

/// A failed/missing proc read remains distinguishable from a process using zero bytes.
#[derive(Clone, Copy, Debug)]
pub struct ProcStatus<'a> {
    pub process: ProcessIdentity,
    pub status: Option<&'a str>,
}

fn missing() -> ByteObservation {
    ByteObservation::unavailable(UnavailableReason::MissingField)
}

fn unsupported() -> ByteObservation {
    ByteObservation::unavailable(UnavailableReason::Unsupported)
}

fn bounded(text: &str) -> SampleResult<()> {
    if text.len() > MAX_CAPTURE_BYTES {
        return Err(MemorySampleError::Oversized);
    }
    Ok(())
}

fn integer(text: &str) -> SampleResult<u64> {
    if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
        return Err(MemorySampleError::Malformed(
            "expected an unsigned decimal integer",
        ));
    }
    text.parse()
        .map_err(|_| MemorySampleError::Malformed("integer overflow"))
}

fn byte_value(text: Option<&str>, unit: &str, scale: u64) -> SampleResult<ByteObservation> {
    let Some(text) = text else {
        return Ok(missing());
    };
    let text = text.trim();
    if matches!(text, "N/A" | "[N/A]" | "Not Supported" | "[Not Supported]") {
        return Ok(unsupported());
    }
    let mut parts = text.split_whitespace();
    let number = integer(parts.next().unwrap_or_default())?;
    if parts.next() != Some(unit) || parts.next().is_some() {
        return Err(MemorySampleError::Malformed(
            "unexpected memory unit or trailing data",
        ));
    }
    let bytes = number
        .checked_mul(scale)
        .filter(|n| *n <= MAX_SAFE_BYTES)
        .ok_or(MemorySampleError::Malformed("byte count overflow"))?;
    Ok(ByteObservation::Available { bytes })
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> SampleResult<Option<Node<'a, 'input>>> {
    let mut found = node.children().filter(|n| n.has_tag_name(name));
    let first = found.next();
    if found.next().is_some() {
        return Err(MemorySampleError::Malformed("duplicate XML field"));
    }
    Ok(first)
}

fn leaf<'a>(node: Option<Node<'a, '_>>, name: &str) -> SampleResult<Option<&'a str>> {
    let Some(node) = node else {
        return Ok(None);
    };
    let Some(field) = child(node, name)? else {
        return Ok(None);
    };
    if field.children().any(|n| n.is_element()) {
        return Err(MemorySampleError::Malformed("nested XML value"));
    }
    let texts: Vec<_> = field.children().filter(Node::is_text).collect();
    if texts.len() != 1 {
        return Err(MemorySampleError::Malformed(
            "missing or fragmented XML value",
        ));
    }
    Ok(texts[0].text())
}

fn pid(text: &str) -> SampleResult<u32> {
    u32::try_from(integer(text.trim())?)
        .ok()
        .filter(|v| *v > 0)
        .ok_or(MemorySampleError::Malformed("invalid process ID"))
}

fn roles(identities: &[ProcessIdentity]) -> SampleResult<HashMap<u32, ProcessRole>> {
    let mut result = HashMap::new();
    for identity in identities {
        if identity.pid == 0 || result.insert(identity.pid, identity.role).is_some() {
            return Err(MemorySampleError::Malformed(
                "duplicate or zero attribution PID",
            ));
        }
    }
    Ok(result)
}

fn observation(
    domain: BudgetDomainName,
    source: MemorySource,
    totals: DomainMemory,
    processes: ProcessAggregate,
    sampled_monotonic_ns: u64,
) -> SampleResult<MemoryObservation> {
    MemoryObservation::new(domain, source, totals, processes, sampled_monotonic_ns)
        .map_err(MemorySampleError::Inconsistent)
}

/// Parse one full `nvidia-smi -q -x` capture; no other capture fills absent fields.
pub fn parse_nvidia_smi_xml(
    text: &str,
    identities: &[ProcessIdentity],
    sampled_monotonic_ns: u64,
) -> SampleResult<MemoryObservation> {
    bounded(text)?;
    let roles = roles(identities)?;
    // The recorded document declares an external DTD. roxmltree never fetches it.
    let document = Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: true,
            nodes_limit: MAX_XML_NODES,
        },
    )
    .map_err(|_| MemorySampleError::Malformed("invalid XML"))?;
    let root = document.root_element();
    if !root.has_tag_name("nvidia_smi_log") {
        return Err(MemorySampleError::Malformed("unexpected XML root"));
    }
    let gpus: Vec<_> = root.children().filter(|n| n.has_tag_name("gpu")).collect();
    if gpus.len() != 1
        || leaf(Some(root), "attached_gpus")?
            .map(integer)
            .transpose()?
            .is_some_and(|n| n != 1)
    {
        return Err(MemorySampleError::DeviceCount);
    }
    let gpu = gpus[0];
    let memory = child(gpu, "fb_memory_usage")?;
    let totals = DomainMemory {
        capacity: byte_value(leaf(memory, "total")?, "MiB", MIB)?,
        available: byte_value(leaf(memory, "free")?, "MiB", MIB)?,
        used: byte_value(leaf(memory, "used")?, "MiB", MIB)?,
        driver_reserved: byte_value(leaf(memory, "reserved")?, "MiB", MIB)?,
    };
    let processes = if let Some(container) = child(gpu, "processes")? {
        let mut processes = Vec::new();
        for process in container.children().filter(Node::is_element) {
            if !process.has_tag_name("process_info") {
                return Err(MemorySampleError::Malformed("unknown process record"));
            }
            let pid = pid(leaf(Some(process), "pid")?
                .ok_or(MemorySampleError::Malformed("missing process ID"))?)?;
            processes.push(ProcessMemory {
                pid,
                role: roles.get(&pid).copied().unwrap_or(ProcessRole::External),
                bytes: byte_value(leaf(Some(process), "used_memory")?, "MiB", MIB)?,
            });
        }
        if container
            .children()
            .any(|n| n.is_text() && !n.text().unwrap_or_default().trim().is_empty())
        {
            ProcessAggregate::Unavailable {
                reason: UnavailableReason::Unsupported,
            }
        } else {
            ProcessAggregate::Available { processes }
        }
    } else {
        ProcessAggregate::Unavailable {
            reason: UnavailableReason::MissingField,
        }
    };
    observation(
        BudgetDomainName::DeviceVram,
        MemorySource::NvidiaSmiXml,
        totals,
        processes,
        sampled_monotonic_ns,
    )
}

struct Csv<'a> {
    header: Vec<&'a str>,
    rows: Vec<Vec<&'a str>>,
}

impl<'a> Csv<'a> {
    fn parse(text: &'a str) -> SampleResult<Self> {
        bounded(text)?;
        let mut lines = text.lines();
        let header: Vec<_> = lines
            .next()
            .ok_or(MemorySampleError::Malformed("missing CSV header"))?
            .split(',')
            .map(str::trim)
            .collect();
        let mut unique = HashSet::new();
        if header.iter().any(|h| h.is_empty() || !unique.insert(*h)) {
            return Err(MemorySampleError::Malformed(
                "empty or duplicate CSV header",
            ));
        }
        let rows: Vec<Vec<_>> = lines
            .map(|line| line.split(',').map(str::trim).collect())
            .collect();
        if rows.iter().any(|row| row.len() != header.len()) {
            return Err(MemorySampleError::Malformed(
                "CSV row width disagrees with header",
            ));
        }
        Ok(Self { header, rows })
    }

    fn get(&self, row: &[&'a str], key: &str) -> Option<&'a str> {
        self.header.iter().position(|h| *h == key).map(|i| row[i])
    }
}

/// Parse the recorded header-bearing two-query CSV format as its own observation.
pub fn parse_nvidia_smi_csv(
    gpu: &str,
    apps: Option<&str>,
    identities: &[ProcessIdentity],
    sampled_monotonic_ns: u64,
) -> SampleResult<MemoryObservation> {
    let roles = roles(identities)?;
    let gpu = Csv::parse(gpu)?;
    if gpu.rows.len() != 1 {
        return Err(MemorySampleError::DeviceCount);
    }
    let row = &gpu.rows[0];
    let totals = DomainMemory {
        capacity: byte_value(gpu.get(row, "memory.total [MiB]"), "MiB", MIB)?,
        available: byte_value(gpu.get(row, "memory.free [MiB]"), "MiB", MIB)?,
        used: byte_value(gpu.get(row, "memory.used [MiB]"), "MiB", MIB)?,
        driver_reserved: missing(),
    };
    let processes = if let Some(apps) = apps {
        let apps = Csv::parse(apps)?;
        let uuid = gpu.get(row, "uuid").filter(|s| !s.is_empty());
        if uuid.is_none() || !apps.header.contains(&"gpu_uuid") || !apps.header.contains(&"pid") {
            ProcessAggregate::Unavailable {
                reason: UnavailableReason::MissingField,
            }
        } else {
            let mut processes = Vec::new();
            for row in &apps.rows {
                if apps.get(row, "gpu_uuid") != uuid {
                    return Err(MemorySampleError::Malformed(
                        "CSV queries refer to different devices",
                    ));
                }
                let pid = pid(apps.get(row, "pid").unwrap_or_default())?;
                processes.push(ProcessMemory {
                    pid,
                    role: roles.get(&pid).copied().unwrap_or(ProcessRole::External),
                    bytes: byte_value(apps.get(row, "used_gpu_memory [MiB]"), "MiB", MIB)?,
                });
            }
            ProcessAggregate::Available { processes }
        }
    } else {
        ProcessAggregate::Unavailable {
            reason: UnavailableReason::SourceUnavailable,
        }
    };
    observation(
        BudgetDomainName::DeviceVram,
        MemorySource::NvidiaSmiCsv,
        totals,
        processes,
        sampled_monotonic_ns,
    )
}

fn proc_field<'a>(text: &'a str, key: &str) -> SampleResult<Option<&'a str>> {
    bounded(text)?;
    let mut found = text
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| *name == key)
        .map(|(_, value)| value.trim());
    let first = found.next();
    if found.next().is_some() {
        return Err(MemorySampleError::Malformed("duplicate proc field"));
    }
    Ok(first)
}

/// Parse host totals and the requested PIDs; VmHWM and MemFree are not substitutes.
pub fn parse_proc_memory(
    meminfo: Option<&str>,
    statuses: &[ProcStatus<'_>],
    sampled_monotonic_ns: u64,
) -> SampleResult<MemoryObservation> {
    let (capacity, available) = if let Some(meminfo) = meminfo {
        (
            byte_value(proc_field(meminfo, "MemTotal")?, "kB", KIB)?,
            byte_value(proc_field(meminfo, "MemAvailable")?, "kB", KIB)?,
        )
    } else {
        let value = ByteObservation::unavailable(UnavailableReason::SourceUnavailable);
        (value, value)
    };
    let used = match (capacity.bytes(), available.bytes()) {
        (Some(total), Some(free)) => ByteObservation::Available {
            bytes: total
                .checked_sub(free)
                .ok_or(MemorySampleError::Inconsistent(
                    "MemAvailable exceeds MemTotal",
                ))?,
        },
        _ => missing(),
    };
    let mut processes = Vec::new();
    for status in statuses {
        let bytes = if let Some(text) = status.status {
            let observed_pid =
                proc_field(text, "Pid")?.ok_or(MemorySampleError::Malformed("missing proc PID"))?;
            if pid(observed_pid)? != status.process.pid {
                return Err(MemorySampleError::Malformed(
                    "proc status belongs to another PID",
                ));
            }
            byte_value(proc_field(text, "VmRSS")?, "kB", KIB)?
        } else {
            ByteObservation::unavailable(UnavailableReason::SourceUnavailable)
        };
        processes.push(ProcessMemory {
            pid: status.process.pid,
            role: status.process.role,
            bytes,
        });
    }
    observation(
        BudgetDomainName::GuestRam,
        MemorySource::Proc,
        DomainMemory {
            capacity,
            available,
            used,
            driver_reserved: unsupported(),
        },
        ProcessAggregate::Available { processes },
        sampled_monotonic_ns,
    )
}
