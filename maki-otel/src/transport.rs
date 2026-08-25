//! Where encoded batches go: an OTLP collector over HTTP or gRPC, or the log
//! file.

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::time::{Duration, Instant};

use flate2::Compression as GzipLevel;
use flate2::write::GzEncoder;
use isahc::config::{Configurable, VersionNegotiation};
use isahc::{AsyncReadResponseExt, HttpClient, ResponseExt};
use thiserror::Error;

use crate::encode::otlp::{LogsPayload, MetricsPayload};
use crate::encode::{json, otlp};
use crate::settings::{Compression, Protocol, Signal, SignalSettings};

const HEADER_CONTENT_TYPE: &str = "content-type";
const HEADER_CONTENT_ENCODING: &str = "content-encoding";
const HEADER_GRPC_ENCODING: &str = "grpc-encoding";
const HEADER_GRPC_ACCEPT_ENCODING: &str = "grpc-accept-encoding";
const HEADER_GRPC_STATUS: &str = "grpc-status";
const HEADER_GRPC_MESSAGE: &str = "grpc-message";
const HEADER_GRPC_TIMEOUT: &str = "grpc-timeout";
const HEADER_TE: &str = "te";
const HEADER_RETRY_AFTER: &str = "retry-after";

const CONTENT_TYPE_PROTOBUF: &str = "application/x-protobuf";
const CONTENT_TYPE_JSON: &str = "application/json";
const CONTENT_TYPE_GRPC: &str = "application/grpc+proto";
const ENCODING_GZIP: &str = "gzip";
const TE_TRAILERS: &str = "trailers";

const GRPC_OK: i64 = 0;
const GRPC_DEADLINE_EXCEEDED: i64 = 4;
const GRPC_RESOURCE_EXHAUSTED: i64 = 8;
const GRPC_UNAVAILABLE: i64 = 14;

const MAX_ATTEMPTS: u32 = 5;
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// Cap on the error text kept from a rejected export, so a collector that
/// answers with an HTML page cannot flood the log file.
const MAX_ERROR_BODY: usize = 512;

pub type ExportFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ExportError>> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("collector answered HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("collector answered gRPC status {code}: {message}")]
    Grpc { code: i64, message: String },
    #[error("request failed: {0}")]
    Request(#[from] isahc::Error),
    #[error("export did not finish within {0:?}")]
    Timeout(Duration),
}

pub enum Payload<'a> {
    Metrics(MetricsPayload<'a>),
    Logs(LogsPayload<'a>),
}

/// `deadline` bounds the whole export, retries included. The pipeline owns
/// it, so the `*_export_timeout` settings mean the same thing for every
/// transport; `SignalSettings::timeout` only bounds a single attempt.
pub trait Transport: Send + Sync {
    fn export<'a>(&'a self, payload: &'a Payload<'a>, deadline: Instant) -> ExportFuture<'a>;
}

/// Writes each batch to the log file as OTLP/JSON. Safe next to a TUI, where
/// anything on stdout or stderr would corrupt the screen.
pub struct ConsoleTransport;

impl Transport for ConsoleTransport {
    fn export<'a>(&'a self, payload: &'a Payload<'a>, _deadline: Instant) -> ExportFuture<'a> {
        Box::pin(async move {
            let (signal, body) = match payload {
                Payload::Metrics(p) => (Signal::Metrics, json::encode_metrics(p)),
                Payload::Logs(p) => (Signal::Logs, json::encode_logs(p)),
            };
            tracing::info!(
                otel.signal = ?signal,
                otel.payload = %String::from_utf8_lossy(&body),
                "otel console export"
            );
            Ok(())
        })
    }
}

pub struct OtlpTransport {
    client: HttpClient,
    settings: SignalSettings,
}

impl OtlpTransport {
    pub fn new(settings: SignalSettings) -> Result<Self, isahc::Error> {
        let mut builder = HttpClient::builder().timeout(settings.timeout);
        if settings.protocol == Protocol::Grpc {
            // Cleartext h2c needs prior knowledge; curl will not upgrade for us.
            builder = builder.version_negotiation(VersionNegotiation::http2());
        }
        Ok(Self {
            client: builder.build()?,
            settings,
        })
    }

    /// Encodes, compresses and frames once, up front: the bytes on the wire
    /// are identical on every retry.
    fn prepare(&self, payload: &Payload<'_>) -> Vec<u8> {
        let body = match (self.settings.protocol, payload) {
            (Protocol::HttpJson, Payload::Metrics(p)) => json::encode_metrics(p),
            (Protocol::HttpJson, Payload::Logs(p)) => json::encode_logs(p),
            (_, Payload::Metrics(p)) => otlp::encode_metrics(p),
            (_, Payload::Logs(p)) => otlp::encode_logs(p),
        };
        let gzip = self.settings.compression == Compression::Gzip;
        let body = if gzip { gzip_bytes(&body) } else { body };
        if self.settings.protocol == Protocol::Grpc {
            grpc_frame(&body, gzip)
        } else {
            body
        }
    }

    async fn send_once(&self, body: &[u8]) -> Result<(), ExportError> {
        let grpc = self.settings.protocol == Protocol::Grpc;
        let gzip = self.settings.compression == Compression::Gzip;

        let mut request = isahc::Request::post(&self.settings.url).header(
            HEADER_CONTENT_TYPE,
            match self.settings.protocol {
                Protocol::Grpc => CONTENT_TYPE_GRPC,
                Protocol::HttpJson => CONTENT_TYPE_JSON,
                Protocol::HttpProtobuf => CONTENT_TYPE_PROTOBUF,
            },
        );
        if grpc {
            request = request
                .header(HEADER_TE, TE_TRAILERS)
                .header(
                    HEADER_GRPC_TIMEOUT,
                    format!("{}m", self.settings.timeout.as_millis()),
                )
                .header(HEADER_GRPC_ACCEPT_ENCODING, ENCODING_GZIP);
            if gzip {
                request = request.header(HEADER_GRPC_ENCODING, ENCODING_GZIP);
            }
        } else if gzip {
            request = request.header(HEADER_CONTENT_ENCODING, ENCODING_GZIP);
        }
        for (key, value) in &self.settings.headers {
            request = request.header(key.as_str(), value.as_str());
        }

        let request = request.body(body.to_vec()).map_err(isahc::Error::from)?;
        let mut response = self.client.send_async(request).await?;

        let status = response.status().as_u16();
        let grpc_status_header = header(&response, HEADER_GRPC_STATUS);
        let grpc_message = header(&response, HEADER_GRPC_MESSAGE);
        let retry_after = header(&response, HEADER_RETRY_AFTER);

        // The body must be drained before trailers are readable, and reading it
        // also gives us the error text on a plain HTTP failure.
        let body = response.text().await.unwrap_or_default();

        if grpc {
            let code = grpc_status_header
                .as_deref()
                .and_then(|v| v.parse::<i64>().ok())
                .or_else(|| {
                    response
                        .trailer()
                        .try_get()
                        .and_then(|t| t.get(HEADER_GRPC_STATUS))
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<i64>().ok())
                });
            return match code {
                Some(GRPC_OK) => Ok(()),
                Some(code) => Err(ExportError::Grpc {
                    code,
                    message: grpc_message.unwrap_or_else(|| truncate(&body)),
                }),
                // No status at all means the server never spoke gRPC.
                None => Err(ExportError::Http {
                    status,
                    message: truncate(&body),
                }),
            };
        }

        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(ExportError::Http {
            status,
            message: match retry_after {
                Some(value) => format!("{}; retry-after: {value}", truncate(&body)),
                None => truncate(&body),
            },
        })
    }
}

impl Transport for OtlpTransport {
    fn export<'a>(&'a self, payload: &'a Payload<'a>, deadline: Instant) -> ExportFuture<'a> {
        Box::pin(async move {
            let body = self.prepare(payload);
            let mut last = None;
            for attempt in 0..MAX_ATTEMPTS {
                match self.send_once(&body).await {
                    Ok(()) => return Ok(()),
                    Err(err) if !is_retryable(&err) => return Err(err),
                    Err(err) => last = Some(err),
                }
                let delay = backoff(attempt);
                if Instant::now() + delay >= deadline {
                    break;
                }
                smol::Timer::after(delay).await;
            }
            Err(last.unwrap_or_else(|| {
                ExportError::Timeout(deadline.saturating_duration_since(Instant::now()))
            }))
        })
    }
}

fn header(response: &isahc::Response<isahc::AsyncBody>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn truncate(body: &str) -> String {
    crate::truncate_chars(body.trim(), MAX_ERROR_BODY)
}

fn is_retryable(error: &ExportError) -> bool {
    match error {
        ExportError::Http { status, .. } => is_retryable_http(*status),
        ExportError::Grpc { code, .. } => is_retryable_grpc(*code),
        ExportError::Request(_) => true,
        ExportError::Timeout(_) => false,
    }
}

fn is_retryable_http(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

fn is_retryable_grpc(code: i64) -> bool {
    matches!(
        code,
        GRPC_UNAVAILABLE | GRPC_RESOURCE_EXHAUSTED | GRPC_DEADLINE_EXCEEDED
    )
}

/// Exponential with full jitter, so a collector coming back up does not get
/// every maki process at once.
fn backoff(attempt: u32) -> Duration {
    let exponential = BACKOFF_BASE.saturating_mul(1 << attempt.min(10));
    let capped = exponential.min(BACKOFF_CAP);
    capped.mul_f64(fastrand::f64().mul_add(0.5, 0.5))
}

fn gzip_bytes(body: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(body.len() / 2), GzipLevel::fast());
    encoder
        .write_all(body)
        .and_then(|()| encoder.finish())
        .expect("gzip to a Vec cannot fail")
}

/// gRPC length-prefixed message: one compression flag, then a big-endian
/// length, then the payload.
fn grpc_frame(body: &[u8], compressed: bool) -> Vec<u8> {
    let mut framed = Vec::with_capacity(body.len() + 5);
    framed.push(u8::from(compressed));
    framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    framed.extend_from_slice(body);
    framed
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const BODY: &[u8] = b"hello";

    #[test_case(429, true; "too_many_requests")]
    #[test_case(502, true; "bad_gateway")]
    #[test_case(503, true; "unavailable")]
    #[test_case(504, true; "gateway_timeout")]
    #[test_case(400, false; "bad_request")]
    #[test_case(401, false; "unauthorized")]
    #[test_case(404, false; "not_found")]
    #[test_case(500, false; "internal_error")]
    fn http_retry_policy(status: u16, expected: bool) {
        assert_eq!(is_retryable_http(status), expected);
    }

    #[test_case(14, true; "unavailable")]
    #[test_case(8, true; "resource_exhausted")]
    #[test_case(4, true; "deadline_exceeded")]
    #[test_case(3, false; "invalid_argument")]
    #[test_case(7, false; "permission_denied")]
    #[test_case(16, false; "unauthenticated")]
    fn grpc_retry_policy(code: i64, expected: bool) {
        assert_eq!(is_retryable_grpc(code), expected);
    }

    #[test]
    fn transport_errors_always_retry() {
        assert!(is_retryable(&ExportError::Grpc {
            code: GRPC_UNAVAILABLE,
            message: String::new(),
        }));
        assert!(!is_retryable(&ExportError::Timeout(Duration::ZERO)));
    }

    #[test]
    fn grpc_frame_prefixes_flag_and_big_endian_length() {
        assert_eq!(
            grpc_frame(BODY, false),
            vec![0, 0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']
        );
    }

    #[test]
    fn grpc_frame_sets_the_compression_flag() {
        assert_eq!(grpc_frame(BODY, true)[0], 1);
    }

    #[test]
    fn grpc_frame_length_is_four_bytes_even_when_large() {
        let big = vec![0u8; 300];
        let framed = grpc_frame(&big, false);
        assert_eq!(&framed[1..5], &300u32.to_be_bytes());
        assert_eq!(framed.len(), 305);
    }

    #[test]
    fn gzip_output_has_the_gzip_magic_bytes() {
        let compressed = gzip_bytes(&vec![b'a'; 1024]);
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
        assert!(compressed.len() < 1024);
    }

    /// Without the clamp, the shift in `backoff` would overflow and panic in
    /// debug builds on a large attempt count.
    #[test]
    fn backoff_is_capped_and_survives_absurd_attempt_counts() {
        assert!(backoff(0) <= BACKOFF_BASE);
        assert!(backoff(u32::MAX) <= BACKOFF_CAP);
    }

    #[test]
    fn error_bodies_are_truncated() {
        let long = "x".repeat(MAX_ERROR_BODY * 2);
        assert_eq!(truncate(&long).len(), MAX_ERROR_BODY);
        assert_eq!(truncate("  short  "), "short");
    }
}

/// Exercises the real HTTP path end to end against a socket: headers, body,
/// and the status handling, without a collector.
#[cfg(test)]
mod http_tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};

    use super::*;
    use crate::attr::AttrSet;
    use crate::encode::otlp::MetricsPayload;
    use crate::metrics::{COMMIT_COUNT, DataPoint, MetricData, Value};
    use crate::settings::{Compression, Protocol, SignalSettings, Temporality};

    const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
    const FAIL_RESPONSE: &[u8] = b"HTTP/1.1 400 Bad Request\r\ncontent-length: 3\r\n\r\nbad";
    const RETRYABLE_RESPONSE: &[u8] =
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    const HEADER_KEY: &str = "x-api-key";
    const HEADER_VALUE: &str = "secret";
    const EXPORT_DEADLINE: Duration = Duration::from_secs(5);

    struct Captured {
        headers: Vec<String>,
        body: Vec<u8>,
    }

    fn serve_once(response: &'static [u8]) -> (String, std::thread::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let captured = read_request(&stream);
            (&stream).write_all(response).expect("write response");
            (&stream).flush().ok();
            captured
        });
        (url, handle)
    }

    fn read_request(stream: &TcpStream) -> Captured {
        let mut reader = BufReader::new(stream);
        let mut headers = Vec::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header line");
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
            headers.push(line);
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).expect("read body");
        Captured { headers, body }
    }

    fn transport(url: String, compression: Compression) -> OtlpTransport {
        OtlpTransport::new(SignalSettings {
            protocol: Protocol::HttpProtobuf,
            url,
            headers: vec![(HEADER_KEY.to_string(), HEADER_VALUE.to_string())],
            timeout: Duration::from_secs(5),
            compression,
        })
        .expect("build transport")
    }

    fn sample_metrics() -> Vec<MetricData> {
        vec![MetricData {
            def: &COMMIT_COUNT,
            points: vec![DataPoint {
                attrs: AttrSet::new(),
                start_time_unix_nano: 1,
                time_unix_nano: 2,
                value: Value::Int(1),
            }],
        }]
    }

    fn export_at(transport: &OtlpTransport, deadline: Instant) -> Result<(), ExportError> {
        let resource = AttrSet::new();
        let metrics = sample_metrics();
        let payload = Payload::Metrics(MetricsPayload {
            resource: &resource,
            temporality: Temporality::Delta,
            metrics: &metrics,
        });
        smol::block_on(transport.export(&payload, deadline))
    }

    fn export(transport: &OtlpTransport) -> Result<(), ExportError> {
        export_at(transport, Instant::now() + EXPORT_DEADLINE)
    }

    fn has_header(captured: &Captured, needle: &str) -> bool {
        captured
            .headers
            .iter()
            .any(|h| h.to_ascii_lowercase().contains(needle))
    }

    #[test]
    fn posts_protobuf_with_configured_headers() {
        let (url, server) = serve_once(OK_RESPONSE);
        export(&transport(url, Compression::None)).expect("export should succeed");
        let captured = server.join().unwrap();
        assert!(captured.headers[0].starts_with("POST"));
        assert!(has_header(&captured, CONTENT_TYPE_PROTOBUF));
        assert!(has_header(&captured, HEADER_VALUE));
        assert!(!captured.body.is_empty());
    }

    #[test]
    fn gzip_sets_the_encoding_header_and_the_magic_bytes() {
        let (url, server) = serve_once(OK_RESPONSE);
        export(&transport(url, Compression::Gzip)).expect("export should succeed");
        let captured = server.join().unwrap();
        assert!(has_header(&captured, ENCODING_GZIP));
        assert_eq!(&captured.body[..2], &[0x1f, 0x8b]);
    }

    #[test]
    fn a_non_retryable_status_fails_without_a_second_request() {
        let (url, server) = serve_once(FAIL_RESPONSE);
        let error = export(&transport(url, Compression::None)).expect_err("400 should fail");
        server.join().unwrap();
        assert!(
            matches!(error, ExportError::Http { status: 400, .. }),
            "unexpected error: {error}"
        );
    }

    /// The server refuses once and then accepts; joining it proves the second
    /// request actually went out.
    #[test]
    fn a_retryable_status_is_retried_until_success() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for response in [RETRYABLE_RESPONSE, OK_RESPONSE] {
                let (stream, _) = listener.accept().expect("accept");
                read_request(&stream);
                (&stream).write_all(response).expect("write response");
                (&stream).flush().ok();
            }
        });
        export(&transport(url, Compression::None)).expect("the retry should succeed");
        server.join().unwrap();
    }

    /// A second attempt would hit a dropped listener and surface a connect
    /// error instead, so the 503 also proves there was no retry.
    #[test]
    fn a_spent_deadline_stops_after_the_first_attempt() {
        let (url, server) = serve_once(RETRYABLE_RESPONSE);
        let error = export_at(&transport(url, Compression::None), Instant::now())
            .expect_err("no budget for a retry");
        server.join().unwrap();
        assert!(
            matches!(error, ExportError::Http { status: 503, .. }),
            "unexpected error: {error}"
        );
    }

    /// Compression happens before framing: the length must cover the
    /// compressed bytes and the gzip magic must sit inside the frame.
    #[test]
    fn grpc_gzip_compresses_before_framing() {
        let transport = OtlpTransport::new(SignalSettings {
            protocol: Protocol::Grpc,
            url: "http://127.0.0.1:1".to_string(),
            headers: Vec::new(),
            timeout: Duration::from_secs(1),
            compression: Compression::Gzip,
        })
        .expect("build transport");
        let resource = AttrSet::new();
        let metrics = sample_metrics();
        let payload = Payload::Metrics(MetricsPayload {
            resource: &resource,
            temporality: Temporality::Delta,
            metrics: &metrics,
        });
        let framed = transport.prepare(&payload);
        assert_eq!(framed[0], 1, "compression flag");
        assert_eq!(&framed[1..5], &((framed.len() - 5) as u32).to_be_bytes());
        assert_eq!(&framed[5..7], &[0x1f, 0x8b], "gzip magic inside the frame");
    }
}
