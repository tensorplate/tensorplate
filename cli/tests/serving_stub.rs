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

fn request_head(content_length: usize) -> String {
    format!(
        "POST /infer HTTP/1.1\r\nHost: stub\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
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
