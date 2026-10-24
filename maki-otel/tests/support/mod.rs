//! A one-shot HTTP collector: accepts a single request, answers 200, and
//! hands back what it saw.

#![allow(dead_code, reason = "each test binary uses a different part")]

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
const CONTENT_LENGTH: &str = "content-length";
/// Read by maki and by the OpenTelemetry SDK.
const ENV_PREFIXES: [&str; 2] = ["OTEL_", "MAKI_"];

pub struct Request {
    pub target: String,
    /// Names lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Clears the telemetry variables of the developer's shell, which would
/// otherwise change what a test checks or send its export elsewhere. Each
/// test binary holds one test, which calls this before starting any thread.
pub fn isolate_env() {
    for (key, _) in env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| ENV_PREFIXES.iter().any(|prefix| key.starts_with(prefix)))
        {
            unsafe { env::remove_var(key) };
        }
    }
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

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read header line");
        let Some((name, value)) = line.trim_end().split_once(':') else {
            break;
        };
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    let length = header(CONTENT_LENGTH).map_or(0, |v| v.parse().unwrap_or(0));
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect("read body");
    Request {
        target,
        headers,
        body,
    }
}
