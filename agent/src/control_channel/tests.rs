// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use super::*;
use std::io::{BufRead, BufReader};
use tensorplate_protocol::worker_control::WorkerOp;

fn member() -> MemberRef {
    MemberRef::new("speech-tts", 5)
}
fn transport(stream: UnixStream) -> Transport {
    stream.set_nonblocking(true).unwrap();
    Transport {
        stream,
        buffer: Vec::new(),
        open: true,
        sequence: 0,
    }
}
fn response(request: &WorkerControlRequest) -> WorkerControlResponse {
    let answer = WorkerControlResponse::answer(request, member(), WorkerStatusOutcome::Ok);
    if request.op == WorkerOp::LedgerStatus {
        answer.with_ledger(LedgerStatus::default())
    } else {
        answer
    }
}
fn fence() -> RuntimeCommand {
    RuntimeCommand::AdmissionFence {
        transaction_id: "tx-one".into(),
    }
}

#[test]
fn contact_monitor_replays_the_contact_sequences_and_events() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/control_contact_sequences.json"
    ))
    .unwrap();
    for sequence in fixture.as_array().unwrap() {
        let mut monitor = ContactMonitor::default();
        let mut previous = ContactState::InContact;
        for sample in sequence["samples"].as_array().unwrap() {
            let expected = match sample["state"].as_str().unwrap() {
                "in_contact" => ContactState::InContact,
                "out_of_contact" => ContactState::OutOfContact,
                "failed" => ContactState::Failed,
                _ => unreachable!(),
            };
            let event = monitor.observe(
                Duration::from_millis(sample["at_ms"].as_u64().unwrap()),
                sample["reply"].as_bool().unwrap(),
            );
            assert_eq!(monitor.state(), expected, "{sample}");
            let wanted = if previous == expected {
                None
            } else {
                Some(match expected {
                    ContactState::InContact => ContactEvent::ContactRestored,
                    ContactState::OutOfContact => ContactEvent::OutOfContact,
                    ContactState::Failed => ContactEvent::MemberFailed,
                })
            };
            assert_eq!(event, wanted, "{sample}");
            previous = expected;
        }
    }
    let mut monitor = ContactMonitor::default();
    monitor.observe(Duration::from_millis(9999), false);
    assert_ne!(monitor.state(), ContactState::Failed);
    assert_eq!(
        monitor.observe(Duration::from_secs(10), true),
        Some(ContactEvent::MemberFailed)
    );
}

#[test]
fn pairing_and_member_errors_close_the_channel() {
    for fault in [
        "transaction_id",
        "op",
        "correlation_id",
        "generation",
        "mismatch",
        "invalid",
    ] {
        let (agent, worker) = UnixStream::pair().unwrap();
        let peer = thread::spawn(move || {
            let mut reader = BufReader::new(worker);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: WorkerControlRequest = decode_with_version_check(&line).unwrap();
            let mut answer = response(&request);
            match fault {
                "transaction_id" => answer.transaction_id = Some("tx-another".into()),
                "op" => answer.op = Some(WorkerOp::Retire),
                "correlation_id" => answer.correlation_id = Some("unissued".into()),
                "generation" => answer.member.as_mut().unwrap().generation += 1,
                "mismatch" => {
                    answer.member.as_mut().unwrap().generation += 1;
                    answer.status = WorkerStatusOutcome::MemberMismatch;
                }
                "invalid" => answer.schema_version = "9.9".into(),
                _ => unreachable!(),
            }
            reader
                .get_mut()
                .write_all(&encode_frame(&answer).unwrap())
                .unwrap();
        });
        let mut transport = transport(agent);
        let result = transport.exchange(
            fence(),
            &member(),
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
        );
        assert_eq!(
            result,
            Err(if matches!(fault, "generation" | "mismatch") {
                ControlChannelError::MemberMismatch
            } else {
                ControlChannelError::Protocol
            }),
            "{fault}"
        );
        assert!(!transport.open);
        peer.join().unwrap();
    }
}

#[test]
fn frame_limit_eof_and_fragmented_json_are_distinct() {
    for (length, expected) in [
        (WORKER_CONTROL_MAX_FRAME_BYTES, None),
        (
            WORKER_CONTROL_MAX_FRAME_BYTES + 1,
            Some(ControlChannelError::FrameTooLarge),
        ),
        (12, Some(ControlChannelError::Protocol)),
        (0, Some(ControlChannelError::Closed)),
    ] {
        let (agent, worker) = UnixStream::pair().unwrap();
        let peer = thread::spawn(move || {
            let mut reader = BufReader::new(worker);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: WorkerControlRequest = decode_with_version_check(&line).unwrap();
            let mut bytes = encode_frame(&response(&request)).unwrap();
            if length > bytes.len() {
                bytes.pop();
                bytes.resize(length - 1, b' ');
                bytes.push(b'\n');
            } else {
                bytes.truncate(length);
            }
            for chunk in bytes.chunks(37) {
                if reader.get_mut().write_all(chunk).is_err() {
                    break;
                }
            }
        });
        let mut transport = transport(agent);
        let result = transport.exchange(
            RuntimeCommand::LedgerStatus,
            &member(),
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
        );
        if let Some(error) = expected {
            assert_eq!(result, Err(error));
        } else {
            assert_eq!(result.unwrap().ledger, Some(LedgerStatus::default()));
        }
        peer.join().unwrap();
    }
}

#[test]
fn timed_out_partial_reply_is_drained_without_reapplying_the_mutation() {
    let (agent, worker) = UnixStream::pair().unwrap();
    let peer = thread::spawn(move || {
        let mut reader = BufReader::new(worker);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let first: WorkerControlRequest = decode_with_version_check(&line).unwrap();
        let bytes = encode_frame(&response(&first)).unwrap();
        reader.get_mut().write_all(&bytes[..20]).unwrap();
        thread::sleep(Duration::from_millis(100));
        reader.get_mut().write_all(&bytes[20..]).unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let second: WorkerControlRequest = decode_with_version_check(&line).unwrap();
        assert_eq!(first.op, WorkerOp::AdmissionFence);
        assert_eq!(second.op, WorkerOp::LedgerStatus);
        assert_ne!(first.correlation_id, second.correlation_id);
        reader
            .get_mut()
            .write_all(&encode_frame(&response(&second)).unwrap())
            .unwrap();
    });
    let mut transport = transport(agent);
    let stop = AtomicBool::new(false);
    assert_eq!(
        transport.exchange(
            fence(),
            &member(),
            Instant::now() + Duration::from_millis(40),
            &stop
        ),
        Err(ControlChannelError::Deadline)
    );
    assert!(transport.open);
    assert!(transport
        .exchange(
            RuntimeCommand::LedgerStatus,
            &member(),
            Instant::now() + Duration::from_secs(1),
            &stop
        )
        .is_ok());
    peer.join().unwrap();
}

#[test]
fn expired_or_stopped_calls_emit_no_request() {
    let (agent, mut peer) = UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut transport = transport(agent);
    assert_eq!(
        transport.exchange(fence(), &member(), Instant::now(), &AtomicBool::new(false)),
        Err(ControlChannelError::Deadline)
    );
    assert_eq!(
        transport.exchange(
            fence(),
            &member(),
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(true)
        ),
        Err(ControlChannelError::Stopped)
    );
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
}

#[test]
fn invalid_members_are_rejected_before_a_thread_or_request_exists() {
    for member in [
        MemberRef::new("invalid/path", 5),
        MemberRef::new("speech-tts", 0),
    ] {
        let (agent, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(matches!(
            ControlChannel::start(agent, member),
            Err(ControlChannelError::InvalidRequest)
        ));
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
}
