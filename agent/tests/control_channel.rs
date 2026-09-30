// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[path = "support/control_worker.rs"]
mod worker;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tensorplate_agent::control_channel::{
    ContactEvent, ContactState, ControlChannel, RuntimeCommand,
};
use tensorplate_agent::error::ControlChannelError;
use tensorplate_agent::worker::spawn_with_control;
use tensorplate_protocol::worker_control::{
    MemberRef, PressureDirective, PressureLevel, WorkerStatusOutcome,
};

fn member() -> MemberRef {
    MemberRef::new("speech-tts", 5)
}
fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn polling_remains_independent_while_the_callers_backend_is_blocked() {
    let (release, blocked) = mpsc::channel();
    let backend = thread::spawn(move || blocked.recv().unwrap());
    let (stream, peer) = worker::spawn(
        (0..3)
            .map(|_| worker::ledger(Duration::from_millis(25)))
            .collect(),
    );
    let channel = ControlChannel::start(stream, member()).unwrap();
    wait_until(Duration::from_secs(4), || {
        channel
            .snapshot()
            .ledger_received_at
            .is_some_and(|at| at >= Duration::from_secs(2))
    });
    assert!(!backend.is_finished());
    let snapshot = channel.snapshot();
    assert_eq!(snapshot.contact, ContactState::InContact);
    assert_eq!(snapshot.missed_polls, 0);
    assert!(snapshot.ledger_received_at.unwrap() < Duration::from_secs(3));
    drop(channel);
    let requests = peer.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests
        .windows(2)
        .all(|pair| pair[0].correlation_id != pair[1].correlation_id));
    release.send(()).unwrap();
    backend.join().unwrap();
}

#[test]
fn runtime_commands_use_the_golden_payloads_and_cannot_name_another_member() {
    let frames = [
        include_str!("../../protocol/rust/tests/fixtures/worker_control_admission_fence.jsonl"),
        include_str!("../../protocol/rust/tests/fixtures/worker_control_activate.jsonl"),
        include_str!("../../protocol/rust/tests/fixtures/worker_control_retire.jsonl"),
        include_str!("../../protocol/rust/tests/fixtures/worker_control_quota_assign.jsonl"),
        include_str!("../../protocol/rust/tests/fixtures/worker_control_pressure_directive.jsonl"),
    ];
    let mut steps = vec![worker::ledger(Duration::ZERO)];
    for frame in frames {
        steps.push(worker::Step::Reply {
            frame: frame.lines().nth(1).unwrap().into(),
            delay: Duration::ZERO,
            stale_generation: false,
        });
    }
    let (stream, peer) = worker::spawn(steps);
    let channel = ControlChannel::start(stream, member()).unwrap();
    wait_until(Duration::from_secs(1), || {
        channel.snapshot().ledger.is_some()
    });
    let quota = tensorplate_protocol::decode_with_version_check::<
        tensorplate_protocol::worker_control::WorkerControlRequest,
    >(frames[3].lines().next().unwrap())
    .unwrap()
    .quota
    .unwrap();
    let commands = [
        RuntimeCommand::AdmissionFence {
            transaction_id: "tx-one".into(),
        },
        RuntimeCommand::Activate {
            transaction_id: "tx-one".into(),
        },
        RuntimeCommand::Retire {
            transaction_id: "tx-one".into(),
            drain_timeout_ms: 30_000,
        },
        RuntimeCommand::QuotaAssign(quota),
        RuntimeCommand::PressureDirective(PressureDirective::level(PressureLevel::ShedAdmission)),
    ];
    for command in commands {
        let response = channel
            .submit(command)
            .unwrap()
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(response.status, WorkerStatusOutcome::Ok);
        assert_eq!(response.member, Some(member()));
    }
    assert!(matches!(
        channel.submit(RuntimeCommand::Activate {
            transaction_id: "runtime".into()
        }),
        Err(ControlChannelError::InvalidRequest)
    ));
    drop(channel);
    let requests = peer.join().unwrap();
    assert_eq!(requests.len(), 6);
    for (request, frame) in requests[1..].iter().zip(frames) {
        let mut expected: tensorplate_protocol::worker_control::WorkerControlRequest =
            tensorplate_protocol::decode_with_version_check(frame.lines().next().unwrap()).unwrap();
        expected.member = Some(member());
        expected.correlation_id.clone_from(&request.correlation_id);
        assert!((1..=1000).contains(&request.timeout_ms.unwrap()));
        expected.timeout_ms = request.timeout_ms;
        if expected.op.is_transactional() {
            expected.transaction_id = "tx-one".into();
        }
        assert_eq!(*request, expected);
    }
}

#[test]
fn eof_escalates_at_three_misses_and_ten_seconds_and_bounds_pending_work() {
    let (stream, peer) = UnixStream::pair().unwrap();
    drop(peer);
    let channel = ControlChannel::start(stream, member()).unwrap();
    let started = Instant::now();
    wait_until(Duration::from_secs(4), || {
        channel.snapshot().contact == ContactState::OutOfContact
    });
    assert!(started.elapsed() >= Duration::from_millis(2900));
    let out = channel.snapshot();
    assert_eq!(out.last_event, Some(ContactEvent::OutOfContact));
    assert_eq!(out.missed_polls, 3);
    let response = channel.submit(RuntimeCommand::LedgerStatus).unwrap();
    assert!(response
        .recv_timeout(Duration::from_millis(1200))
        .unwrap()
        .is_err());
    wait_until(Duration::from_secs(8), || {
        channel.snapshot().contact == ContactState::Failed
    });
    assert!(started.elapsed() >= Duration::from_millis(9900));
    let failed = channel.snapshot();
    assert_eq!(failed.last_event, Some(ContactEvent::MemberFailed));
    assert_eq!(failed.event_revision, 2);
    assert_eq!(failed.member, member());
}

#[test]
fn drop_interrupts_a_nonresponsive_peer_and_closes_the_agent_end() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let channel = ControlChannel::start(stream, member()).unwrap();
    let mut first = String::new();
    BufReader::new(&mut peer).read_line(&mut first).unwrap();
    let start = Instant::now();
    drop(channel);
    assert!(start.elapsed() < Duration::from_millis(250));
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}

#[test]
fn spawn_moves_the_child_end_to_stdin_and_exit_produces_eof() {
    let mut command = Command::new("python3");
    command.args(["-c", "import socket; s=socket.socket(fileno=0); s.sendall(b'ready\\n'); assert s.recv(16)==b'exit\\n'"]);
    command.stdout(Stdio::null());
    let (mut child, mut stream) = spawn_with_control(command).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut ready = [0; 6];
    stream.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready\n");
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let child_end = std::fs::read_link(format!("/proc/{}/fd/0", child.id())).unwrap();
        let agent_end =
            std::fs::read_link(format!("/proc/self/fd/{}", stream.as_raw_fd())).unwrap();
        assert!(child_end.to_string_lossy().starts_with("socket:["));
        assert_ne!(child_end, agent_end);
        let ends: Vec<_> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .collect();
        assert!(!ends.contains(&child_end), "parent retained the child end");
        assert_eq!(ends.iter().filter(|end| **end == agent_end).count(), 1);
        let info =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", stream.as_raw_fd())).unwrap();
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:\t"))
            .unwrap();
        assert_ne!(u32::from_str_radix(flags, 8).unwrap() & 0o2_000_000, 0);
    }
    stream.write_all(b"exit\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    assert!(spawn_with_control(Command::new("/tensorplate-no-such-worker")).is_err());
}

#[test]
fn a_paired_worker_error_proves_contact_without_publishing_a_ledger() {
    use tensorplate_protocol::worker_control::{WorkerControlResponse, WorkerError};
    use tensorplate_protocol::{decode_with_version_check, ErrorCode};
    let mut response: WorkerControlResponse =
        decode_with_version_check(worker::LEDGER.lines().nth(1).unwrap()).unwrap();
    response.status = WorkerStatusOutcome::NotReady;
    response.ledger = None;
    response.error = Some(WorkerError::new(ErrorCode::NotReady, "ledger unavailable"));
    let (stream, peer) = worker::spawn(vec![worker::Step::Reply {
        frame: serde_json::to_string(&response).unwrap(),
        delay: Duration::ZERO,
        stale_generation: false,
    }]);
    let channel = ControlChannel::start(stream, member()).unwrap();
    wait_until(Duration::from_millis(500), || {
        channel.snapshot().last_error == Some(ControlChannelError::Rejected)
    });
    let snapshot = channel.snapshot();
    assert_eq!(snapshot.contact, ContactState::InContact);
    assert_eq!(snapshot.missed_polls, 0);
    assert!(snapshot.ledger.is_none());
    drop(channel);
    peer.join().unwrap();
}

#[test]
fn blocked_exchange_keeps_the_queue_bounded_and_shutdown_resolves_waiters() {
    use tensorplate_protocol::decode_with_version_check;
    use tensorplate_protocol::worker_control::{
        encode_frame, LedgerStatus, WorkerControlRequest, WorkerControlResponse,
    };
    let (stream, peer) = UnixStream::pair().unwrap();
    let (started, first) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut reader = BufReader::new(peer);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: WorkerControlRequest = decode_with_version_check(&line).unwrap();
        let answer = WorkerControlResponse::answer(&request, member(), WorkerStatusOutcome::Ok)
            .with_ledger(LedgerStatus::default());
        reader
            .get_mut()
            .write_all(&encode_frame(&answer).unwrap())
            .unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        started.send(()).unwrap();
        blocked.recv().unwrap();
    });
    let channel = ControlChannel::start(stream, member()).unwrap();
    wait_until(Duration::from_secs(1), || {
        channel.snapshot().ledger.is_some()
    });
    let active = channel.submit(RuntimeCommand::LedgerStatus).unwrap();
    first.recv_timeout(Duration::from_secs(1)).unwrap();
    let queued: Vec<_> = (0..16)
        .map(|_| channel.submit(RuntimeCommand::LedgerStatus).unwrap())
        .collect();
    assert!(matches!(
        channel.submit(RuntimeCommand::LedgerStatus),
        Err(ControlChannelError::QueueFull)
    ));
    drop(channel);
    assert_eq!(
        active.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(ControlChannelError::Stopped)
    );
    for reply in queued {
        assert_eq!(
            reply.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(ControlChannelError::Stopped)
        );
    }
    release.send(()).unwrap();
    worker.join().unwrap();
}
