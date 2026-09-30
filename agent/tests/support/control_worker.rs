// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tensorplate_protocol::decode_with_version_check;
use tensorplate_protocol::worker_control::{
    encode_frame, WorkerControlRequest, WorkerControlResponse,
};

pub const LEDGER: &str =
    include_str!("../../../protocol/rust/tests/fixtures/worker_control_ledger_status.jsonl");

pub enum Step {
    Reply {
        frame: String,
        delay: Duration,
        stale_generation: bool,
    },
    Eof,
}

pub fn ledger(delay: Duration) -> Step {
    Step::Reply {
        frame: LEDGER.lines().nth(1).unwrap().into(),
        delay,
        stale_generation: false,
    }
}

/// Synthetic peer: the response payload is a published golden frame, with request echoes rebound.
pub fn spawn(steps: Vec<Step>) -> (UnixStream, JoinHandle<Vec<WorkerControlRequest>>) {
    let (agent, worker) = UnixStream::pair().unwrap();
    let join = thread::spawn(move || {
        let mut input = BufReader::new(worker);
        let mut requests = Vec::new();
        for step in steps {
            let Step::Reply {
                frame,
                delay,
                stale_generation,
            } = step
            else {
                break;
            };
            let mut line = String::new();
            if input.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let request: WorkerControlRequest = decode_with_version_check(&line).unwrap();
            let mut response: WorkerControlResponse = decode_with_version_check(&frame).unwrap();
            assert_eq!(Some(request.op), response.op);
            response.correlation_id.clone_from(&request.correlation_id);
            response.transaction_id = Some(request.transaction_id.clone());
            response.member.clone_from(&request.member);
            if stale_generation {
                response.member.as_mut().unwrap().generation += 1;
            }
            requests.push(request);
            thread::sleep(delay);
            if input
                .get_mut()
                .write_all(&encode_frame(&response).unwrap())
                .is_err()
            {
                break;
            }
        }
        requests
    });
    (agent, join)
}
