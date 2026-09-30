// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[path = "support/control_worker.rs"]
mod worker;

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tensorplate_protocol::decode_with_version_check;
use tensorplate_protocol::worker_control::{WorkerControlRequest, WorkerControlResponse};

fn request(stream: &mut std::os::unix::net::UnixStream) {
    writeln!(stream, "{}", worker::LEDGER.lines().next().unwrap()).unwrap();
}

#[test]
fn worker_double_replays_the_golden_while_a_backend_is_blocked() {
    let (release, blocked) = mpsc::channel();
    let backend = thread::spawn(move || blocked.recv().unwrap());
    let (mut stream, peer) = worker::spawn(vec![worker::ledger(Duration::from_millis(25))]);
    stream
        .set_read_timeout(Some(Duration::from_millis(900)))
        .unwrap();
    let start = Instant::now();
    request(&mut stream);
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!backend.is_finished());
    assert_eq!(line.trim_end(), worker::LEDGER.lines().nth(1).unwrap());
    assert_eq!(peer.join().unwrap().len(), 1);
    release.send(()).unwrap();
    backend.join().unwrap();
}

#[test]
fn worker_double_can_close_or_answer_a_stale_generation() {
    let (stream, peer) = worker::spawn(vec![worker::Step::Eof]);
    let mut line = String::new();
    assert_eq!(BufReader::new(stream).read_line(&mut line).unwrap(), 0);
    peer.join().unwrap();
    let (mut stream, peer) = worker::spawn(vec![worker::Step::Reply {
        frame: worker::LEDGER.lines().nth(1).unwrap().into(),
        delay: Duration::ZERO,
        stale_generation: true,
    }]);
    request(&mut stream);
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: WorkerControlResponse = decode_with_version_check(&line).unwrap();
    let original: WorkerControlRequest =
        decode_with_version_check(worker::LEDGER.lines().next().unwrap()).unwrap();
    assert!(response.answers(&original).is_err());
    peer.join().unwrap();
}

#[test]
fn contact_sequences_are_monotonic_synthetic_samples() {
    let sequences: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/control_contact_sequences.json")).unwrap();
    for sequence in sequences.as_array().unwrap() {
        let mut previous = None;
        for sample in sequence["samples"].as_array().unwrap() {
            let at = sample["at_ms"].as_u64().unwrap();
            assert!(previous.map_or(true, |old| at > old));
            previous = Some(at);
            assert!(sample["reply"].is_boolean());
            assert!(matches!(
                sample["state"].as_str(),
                Some("in_contact" | "out_of_contact" | "failed")
            ));
        }
    }
}
