//! The anchor's own HTTP: one port carries the browser API, the dashboard,
//! and the instance WebSocket tunnels. tiny_http is gone because it owns its
//! listener, and a WS upgrade needs the raw duplex socket; the shim below
//! keeps the request/response surface the handlers already speak.
//!
//! Every connection is answered then closed (`Connection: close`): no
//! keep-alive, no pipelining, no chunked bodies to parse, which is the whole
//! reason a hand-rolled layer stays small enough to trust. Browsers and the
//! dashboard's `fetch` reconnect freely; SSE holds its socket open until the
//! tunnel says final.

use std::io::{self, BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Request line plus headers may not exceed this.
const MAX_HEAD: usize = 64 * 1024;
/// Idle budget for a client mid-head or mid-body.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Field names double as `field`/`value` so the handlers keep their old
/// tiny_http spellings.
#[derive(Debug, Clone)]
pub struct Header {
    pub field: String,
    pub value: String,
}

impl Header {
    /// Same call shape tiny_http had; newline-free bytes only, so a header
    /// can never smuggle a second response line.
    pub fn from_bytes(name: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> io::Result<Header> {
        let (Some(field), Some(value)) = (header_string(name), header_string(value)) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid header bytes",
            ));
        };
        Ok(Header { field, value })
    }
}

fn header_string(bytes: impl AsRef<[u8]>) -> Option<String> {
    let bytes = bytes.as_ref();
    if bytes.iter().any(|b| *b == b'\r' || *b == b'\n' || *b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

#[derive(Debug)]
pub struct Response {
    status: u16,
    headers: Vec<Header>,
    body: Vec<u8>,
}

impl Response {
    pub fn from_data(body: Vec<u8>) -> Response {
        Response {
            status: 200,
            headers: Vec::new(),
            body,
        }
    }

    pub fn from_string(body: impl Into<String>) -> Response {
        Response::from_data(body.into().into_bytes())
    }

    pub fn empty(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_status_code(mut self, status: u16) -> Response {
        self.status = status;
        self
    }

    pub fn with_header(mut self, header: Header) -> Response {
        self.headers.push(header);
        self
    }
}

pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

pub fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason_phrase(response.status),
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    for header in &response.headers {
        stream.write_all(header.field.as_bytes())?;
        stream.write_all(b": ")?;
        stream.write_all(header.value.as_bytes())?;
        stream.write_all(b"\r\n")?;
    }
    stream.write_all(b"\r\n")?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// A parsed request head. `replay` is every raw byte consumed: a WebSocket
/// handshake re-feeds it whole to tungstenite, which parses the upgrade and
/// answers 101 itself.
pub struct Head {
    pub method: String,
    pub target: String,
    pub headers: Vec<Header>,
    pub content_length: usize,
    pub replay: Vec<u8>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.field.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    pub fn is_upgrade(&self) -> bool {
        self.method.eq_ignore_ascii_case("GET")
            && self
                .target
                .split_once('?')
                .map(|(p, _)| p)
                .unwrap_or(&self.target)
                == "/ws"
            && self
                .header("upgrade")
                .is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
            && self.header("sec-websocket-key").is_some()
    }
}

/// How to answer a malformed request.
#[derive(Debug)]
pub enum Reject {
    /// Client hung mid-head.
    TimedOut,
    /// Connection gone before a request line; answer nothing.
    Empty,
    BadRequest(&'static str),
    TooLarge,
    LengthRequired,
    Io,
}

impl Reject {
    pub fn response(&self) -> Option<Response> {
        match self {
            Reject::TimedOut => {
                Some(Response::from_string("request timed out").with_status_code(408))
            }
            Reject::Empty | Reject::Io => None,
            Reject::BadRequest(reason) => {
                Some(Response::from_string(*reason).with_status_code(400))
            }
            Reject::TooLarge => Some(Response::from_string("head too large").with_status_code(431)),
            Reject::LengthRequired => {
                Some(Response::from_string("chunked bodies unsupported").with_status_code(411))
            }
        }
    }
}

impl From<io::Error> for Reject {
    fn from(err: io::Error) -> Reject {
        match err.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Reject::TimedOut,
            _ => Reject::Io,
        }
    }
}

/// Read the request line and headers, collecting the exact bytes consumed.
pub fn read_head<R: BufRead>(reader: &mut R) -> Result<Head, Reject> {
    let mut replay = Vec::new();
    let line = read_line(reader, &mut replay)?.ok_or(Reject::Empty)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(Reject::BadRequest("malformed request line"));
    };
    let (method, target) = (method.to_owned(), target.to_owned());
    let mut headers = Vec::new();
    while let Some(line) = read_line(reader, &mut replay)? {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Reject::BadRequest("malformed header"));
        };
        headers.push(Header {
            field: name.trim().to_owned(),
            value: value.trim().to_owned(),
        });
    }
    let head = Head {
        method,
        target,
        headers,
        content_length: 0,
        replay,
    };
    if head.header("transfer-encoding").is_some() {
        return Err(Reject::LengthRequired);
    }
    Ok(Head {
        content_length: match head.header("content-length") {
            None => 0,
            Some(value) => value
                .trim()
                .parse()
                .map_err(|_| Reject::BadRequest("bad Content-Length"))?,
        },
        ..head
    })
}

fn read_line<R: BufRead>(reader: &mut R, replay: &mut Vec<u8>) -> Result<Option<String>, Reject> {
    let start = replay.len();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(idx) => {
                replay.extend_from_slice(&available[..=idx]);
                reader.consume(idx + 1);
            }
            None => {
                replay.extend_from_slice(available);
                let take = available.len();
                reader.consume(take);
                continue;
            }
        }
        break;
    }
    if replay.len() - start > MAX_HEAD {
        return Err(Reject::TooLarge);
    }
    let raw = &replay[start..];
    let trimmed = match raw {
        [rest @ .., b'\n'] => match rest {
            [head @ .., b'\r'] => head,
            head => head,
        },
        _ => raw,
    };
    Ok(Some(String::from_utf8_lossy(trimmed).into_owned()))
}

/// A parsed request. The body arrives fully buffered (the caller caps it),
/// and `sink` stays writable so `respond` and the raw SSE writer can finish
/// the connection.
pub struct Request {
    method: String,
    url: String,
    headers: Vec<Header>,
    body: Vec<u8>,
    body_pos: usize,
    sink: TcpStream,
    peer: SocketAddr,
}

impl Read for Request {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let rest = &self.body[self.body_pos..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.body_pos += n;
        Ok(n)
    }
}

impl Request {
    pub fn new(head: Head, body: Vec<u8>, sink: TcpStream, peer: SocketAddr) -> Request {
        Request {
            method: head.method,
            url: head.target,
            headers: head.headers,
            body,
            body_pos: 0,
            sink,
            peer,
        }
    }

    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn headers(&self) -> &[Header] {
        &self.headers
    }

    pub fn remote_addr(&self) -> Option<&SocketAddr> {
        Some(&self.peer)
    }

    /// Handlers keep the tiny_http body shape; `dyn Read` so `read_to_end`
    /// resolves exactly as it did before.
    pub fn as_reader(&mut self) -> &mut dyn Read {
        self
    }

    pub fn respond(self, response: Response) -> io::Result<()> {
        let mut sink = self.sink;
        write_response(&mut sink, &response)
    }

    /// Takes over the connection to stream bytes by hand (SSE), the role
    /// `into_writer` had.
    pub fn into_writer(self) -> TcpStream {
        self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn head(bytes: &[u8]) -> Result<Head, Reject> {
        read_head(&mut BufReader::new(bytes))
    }

    fn pair() -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    #[test]
    fn parses_line_and_headers_replaying_every_byte() {
        let raw = b"GET /links?instance=a HTTP/1.1\r\nCookie: x=1\r\n\r\n";
        let h = head(raw).unwrap();
        assert_eq!(h.method, "GET");
        assert_eq!(h.target, "/links?instance=a");
        assert_eq!(h.headers.len(), 1);
        assert_eq!(h.headers[0].field.as_str(), "Cookie");
        assert_eq!(h.header("cookie"), Some("x=1"));
        assert_eq!(h.content_length, 0);
        assert_eq!(h.replay, raw.to_vec(), "replay must be byte-exact");
    }

    #[test]
    fn accepts_bare_lf_and_counts_content_length() {
        let h = head(b"POST /api/login\nContent-Length: 7\n\n").unwrap();
        assert_eq!(h.method, "POST");
        assert_eq!(h.content_length, 7);
    }

    #[test]
    fn rejects_malformed_and_unsupported() {
        assert!(matches!(head(b"NOPE\n\n"), Err(Reject::BadRequest(_))));
        assert!(matches!(
            head(b"GET / x\nnovalue\n\n"),
            Err(Reject::BadRequest(_))
        ));
        assert!(matches!(
            head(b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(Reject::LengthRequired)
        ));
        assert!(matches!(
            head(b"GET / HTTP/1.1\r\nContent-Length: 99x\r\n\r\n"),
            Err(Reject::BadRequest(_))
        ));
        assert!(matches!(head(b""), Err(Reject::Empty)));
    }

    #[test]
    fn upgrade_detection_needs_the_full_handshake_shape() {
        let ws = head(
            b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .unwrap();
        assert!(ws.is_upgrade());
        let plain = head(b"GET /ws HTTP/1.1\r\n\r\n").unwrap();
        assert!(!plain.is_upgrade());
        let other =
            head(b"GET /x HTTP/1.1\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k\r\n\r\n").unwrap();
        assert!(!other.is_upgrade());
        let query =
            head(b"GET /ws?keep=1 HTTP/1.1\r\nUpgrade: Websocket\r\nsec-websocket-key: k\r\n\r\n")
                .unwrap();
        assert!(query.is_upgrade());
    }

    #[test]
    fn response_framing_is_valid_http() {
        let (mut server, mut client) = pair();
        let response = Response::from_data(b"hi".to_vec())
            .with_status_code(404)
            .with_header(Header::from_bytes("Content-Type", "text/plain").unwrap());
        write_response(&mut server, &response).unwrap();
        // The write side must go away, or the read below never sees EOF.
        drop(server);
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"), "got {out:?}");
        assert!(out.contains("Content-Length: 2\r\n"));
        assert!(out.contains("Content-Type: text/plain\r\n"));
        assert!(out.contains("Connection: close\r\n"));
        assert!(out.ends_with("\r\n\r\nhi"));
    }

    #[test]
    fn header_from_bytes_refuses_newline_injection() {
        assert!(Header::from_bytes("Location", "/ok\r\nSet-Cookie: evil").is_err());
    }

    #[test]
    fn reject_responses_are_well_defined() {
        assert_eq!(Reject::TimedOut.response().unwrap().status, 408);
        assert_eq!(Reject::LengthRequired.response().unwrap().status, 411);
        assert!(Reject::Empty.response().is_none());
        assert!(Reject::Io.response().is_none());
    }
}
