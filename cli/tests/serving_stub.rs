// SPDX-License-Identifier: Apache-2.0
//
// The stub serving worker the CLI integration tests share: it reads a whole
// request before it answers, however the request arrives.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::ServingStub;

const BODY: &[u8] = br#"{"inputs":[]}"#;

/// The length header is lower case here; the CLI's own requests, in the
/// other suites, send it capitalised.
fn request_head(content_length: usize) -> String {
    format!(
        "POST /infer HTTP/1.1\r\nhost: stub\r\ncontent-length: {content_length}\r\nconnection: close\r\n\r\n"
    )
}

fn connect(serving: &ServingStub) -> TcpStream {
    let stream = TcpStream::connect(&serving.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    stream
}

fn read_response(stream: &mut TcpStream) -> String {
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    response
}

#[test]
fn a_request_split_across_two_writes_is_read_whole_before_the_answer() {
    let serving = ServingStub::start("{}");
    let mut stream = connect(&serving);
    let head = request_head(BODY.len());
    stream.write_all(head.as_bytes()).expect("write head");
    // Longer than the stub's accept poll, so the head is read on its own.
    std::thread::sleep(Duration::from_millis(200));
    stream.write_all(BODY).expect("write body");

    let response = read_response(&mut stream);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let reset = stream.take_error().expect("socket error");
    assert!(
        reset.is_none(),
        "the stub closed before the body arrived and the connection was reset: {reset:?}"
    );
    assert_eq!(serving.requests(), vec![[head.as_bytes(), BODY].concat()]);
}

#[test]
fn a_client_that_never_sends_its_body_is_refused_after_the_read_timeout() {
    let serving = ServingStub::start_with_read_timeout("{}", Duration::from_millis(200));
    let mut stream = connect(&serving);
    let head = request_head(BODY.len());
    stream.write_all(head.as_bytes()).expect("write head");

    let response = read_response(&mut stream);
    assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
    assert_eq!(serving.requests(), vec![head.into_bytes()]);
}

#[test]
fn a_request_without_a_length_header_is_answered_at_the_end_of_its_head() {
    let serving = ServingStub::start("{}");
    let mut stream = connect(&serving);
    let head = "GET /health HTTP/1.1\r\nhost: stub\r\n\r\n";
    stream.write_all(head.as_bytes()).expect("write head");

    let response = read_response(&mut stream);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(serving.requests(), vec![head.as_bytes().to_vec()]);
}

#[test]
fn a_length_header_that_is_not_a_length_is_refused() {
    let serving = ServingStub::start("{}");
    for length in ["abc", "-1", "18446744073709551615"] {
        let mut stream = connect(&serving);
        let head = format!("POST /infer HTTP/1.1\r\ncontent-length: {length}\r\n\r\n");
        stream.write_all(head.as_bytes()).expect("write head");

        let response = read_response(&mut stream);
        assert!(
            response.starts_with("HTTP/1.1 400 "),
            "{length}: {response}"
        );
    }
}
