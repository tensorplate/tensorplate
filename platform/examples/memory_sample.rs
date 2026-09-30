// SPDX-License-Identifier: Apache-2.0

//! Thin operator entry point; measurement and reduction live in the library.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::time::Duration;

use tensorplate_platform::memory_sampler::sampling::{
    run_samples, MemoryCollector, MonotonicClock, SamplePlan, SystemMemoryIo,
};
use tensorplate_platform::memory_sampler::ProcessIdentity;
use tensorplate_protocol::{BudgetDomainName, ProcessRole};

const USAGE: &str = "memory_sample --interval <Nms|Ns> --duration <Nms|Ns> --out <new-file> --phase <warm-idle|load> [--domain <guest_ram|device_vram>]... [--process <PID:agent|serving_worker|python_sidecar|external>]...";

fn duration(text: &str) -> Result<Duration, &'static str> {
    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = text.strip_suffix('s') {
        (number, 1000)
    } else {
        return Err("duration requires integer ms or s units");
    };
    let millis = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| *n > 0)
        .ok_or("duration must be positive and fit u64 milliseconds")?;
    Ok(Duration::from_millis(millis))
}

fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some_and(|a| a == "--help") {
        println!("{USAGE}");
        return Ok(true);
    }
    let (mut interval, mut length, mut out, mut phase) = (None, None, None, None);
    let (mut domains, mut processes) = (Vec::new(), Vec::new());
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("option requires a value")?;
        match flag.as_str() {
            "--interval" if interval.is_none() => interval = Some(duration(&value)?),
            "--duration" if length.is_none() => length = Some(duration(&value)?),
            "--out" if out.is_none() => out = Some(value),
            "--phase" if phase.is_none() && matches!(value.as_str(), "warm-idle" | "load") => {
                phase = Some(value);
            }
            "--domain" => domains.push(match value.as_str() {
                "guest_ram" => BudgetDomainName::GuestRam,
                "device_vram" => BudgetDomainName::DeviceVram,
                _ => return Err("unsupported domain".into()),
            }),
            "--process" => {
                let (pid, role) = value.split_once(':').ok_or("process requires PID:role")?;
                processes.push(ProcessIdentity {
                    pid: pid.parse()?,
                    role: match role {
                        "agent" => ProcessRole::Agent,
                        "serving_worker" => ProcessRole::ServingWorker,
                        "python_sidecar" => ProcessRole::PythonSidecar,
                        "external" => ProcessRole::External,
                        _ => return Err("unsupported process role".into()),
                    },
                });
            }
            _ => return Err("unknown, duplicate or invalid option".into()),
        }
    }
    let interval = interval.ok_or("--interval is required")?;
    let length = length.ok_or("--duration is required")?;
    let phase = phase.ok_or("--phase is required")?;
    if domains.is_empty() {
        domains = vec![BudgetDomainName::GuestRam, BudgetDomainName::DeviceVram];
    }
    let plan = SamplePlan::new(interval, length, domains, processes)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = BufWriter::new(options.open(out.ok_or("--out is required")?)?);
    let report = run_samples(
        &plan,
        &mut MonotonicClock::default(),
        &mut MemoryCollector::new(SystemMemoryIo),
        &mut output,
    )?;
    output.flush()?;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(
        &mut stdout,
        &serde_json::json!({
            "schema_version": "0.1", "phase": phase,
            "interval_ms": interval.as_millis(), "duration_ms": length.as_millis(),
            "report": report,
        }),
    )?;
    writeln!(stdout)?;
    Ok(report.complete)
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(3),
        Err(error) => {
            eprintln!("memory sampling failed: {error}\n{USAGE}");
            std::process::exit(1);
        }
    }
}
