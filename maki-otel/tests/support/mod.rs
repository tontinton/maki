//! A one-shot HTTP collector: accepts a single request, answers 200, and
//! hands back what it saw.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";

pub struct Request {
    pub target: String,
    pub body: Vec<u8>,
}

pub fn serve_once() -> (String, JoinHandle<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let request = read_request(&stream);
        (&stream).write_all(OK_RESPONSE).expect("write response");
        (&stream).flush().ok();
        request
    });
    (endpoint, handle)
}

fn read_request(stream: &TcpStream) -> Request {
    let mut reader = BufReader::new(stream);
    let mut target = String::new();
    reader.read_line(&mut target).expect("read request line");
    let target = target
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();

    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read header line");
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect("read body");
    Request { target, body }
}
