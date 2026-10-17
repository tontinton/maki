use std::sync::Arc;

use futures_lite::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use futures_rustls::TlsAcceptor;
use futures_rustls::rustls::ServerConfig;
use futures_rustls::rustls::crypto::aws_lc_rs;
use futures_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rcgen::{CertifiedKey, KeyPair};
use smol::net::TcpListener;
use tracing::debug;

use crate::mcp::config::CallbackScheme;

const PREFERRED_PORT: u16 = 19876;
const MAX_HEADER_SIZE: usize = 8192;
const CALLBACK_PATH: &str = "/mcp/oauth/callback";
const SUCCESS_HTML: &str =
    "<html><body><h1>Authentication successful</h1><p>You can close this tab.</p></body></html>";
const ERROR_HTML: &str =
    "<html><body><h1>Authentication failed</h1><p>Please try again.</p></body></html>";

pub struct CallbackResult {
    pub code: String,
}

const DEFAULT_HOSTNAME: &str = "127.0.0.1";

pub struct CallbackServer {
    pub port: u16,
    hostname: String,
    path: String,
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
}

impl CallbackServer {
    /// `port` pins the loopback listener so the redirect URI can be
    /// pre-registered with the authorization server. Without it, fall back to
    /// `PREFERRED_PORT`, then to any free port. `path`/`hostname` override the
    /// advertised redirect URI; binding stays on 127.0.0.1, which `localhost`
    /// (and 127.0.0.1 itself) resolves to. `Https` serves a throwaway
    /// self-signed certificate, so the browser warns once before the redirect
    /// lands.
    pub async fn bind(
        port: Option<u16>,
        path: Option<&str>,
        hostname: Option<&str>,
        scheme: CallbackScheme,
    ) -> Result<Self, String> {
        let hostname = hostname.unwrap_or(DEFAULT_HOSTNAME);
        let certified = match scheme {
            CallbackScheme::Http => None,
            CallbackScheme::Https => Some(self_signed(hostname)?),
        };
        Self::bind_with(port, path, hostname, certified).await
    }

    /// `certified`, when given, serves the callback over TLS.
    async fn bind_with(
        port: Option<u16>,
        path: Option<&str>,
        hostname: &str,
        certified: Option<CertifiedKey<KeyPair>>,
    ) -> Result<Self, String> {
        let requested = port.unwrap_or(PREFERRED_PORT);
        let listener = match TcpListener::bind(("127.0.0.1", requested)).await {
            Ok(l) => l,
            Err(_) if port.is_none() => TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("failed to bind callback server: {e}"))?,
            Err(e) => return Err(format!("failed to bind callback port {requested}: {e}")),
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let tls = certified.map(tls_acceptor).transpose()?;
        Ok(Self {
            port,
            hostname: hostname.to_string(),
            path: path.unwrap_or(CALLBACK_PATH).to_string(),
            listener,
            tls,
        })
    }

    pub fn redirect_uri(&self) -> String {
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        format!("{scheme}://{}:{}{}", self.hostname, self.port, self.path)
    }

    pub async fn wait_for_callback(self, expected_state: &str) -> Result<CallbackResult, String> {
        loop {
            let (tcp, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| format!("accept failed: {e}"))?;

            let outcome = match &self.tls {
                None => self.handle(tcp, expected_state).await,
                // Browsers drop the first handshake to show the certificate
                // warning, then reconnect once the user proceeds.
                Some(acceptor) => match acceptor.accept(tcp).await {
                    Ok(stream) => self.handle(stream, expected_state).await,
                    Err(e) => {
                        debug!(error = %e, "callback TLS handshake failed");
                        continue;
                    }
                },
            };
            if let Some(result) = outcome? {
                return result;
            }
        }
    }

    /// `Ok(None)` means the request was not the awaited redirect; keep listening.
    async fn handle<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut stream: S,
        expected_state: &str,
    ) -> Result<Option<Result<CallbackResult, String>>, String> {
        let mut buf = Vec::with_capacity(1024);
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream
                .read(&mut tmp)
                .await
                .map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.len() >= MAX_HEADER_SIZE || buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&buf);

        let Some(line) = request.lines().next() else {
            return Ok(None);
        };
        let path = line.split_whitespace().nth(1).unwrap_or("");

        if !path.starts_with(self.path.as_str()) {
            let _ = respond(&mut stream, 404, "Not Found").await;
            return Ok(None);
        }

        let query = path.split('?').nth(1).unwrap_or("");
        let params = parse_query(query);

        // RFC 6749 4.1.2.1: error redirects carry `state` too. Gating
        // everything on it keeps any local webpage from aborting a
        // pending flow through the predictable loopback port.
        let state = params.iter().find(|(k, _)| k == "state");
        if state.map(|(_, v)| v.as_str()) != Some(expected_state) {
            let _ = respond(&mut stream, 403, "State mismatch").await;
            return Ok(None);
        }

        if let Some(error) = params.iter().find(|(k, _)| k == "error") {
            let desc = params
                .iter()
                .find(|(k, _)| k == "error_description")
                .map(|(_, v)| v.as_str())
                .unwrap_or(&error.1);
            let _ = respond(&mut stream, 400, ERROR_HTML).await;
            return Ok(Some(Err(format!("OAuth error: {desc}"))));
        }

        let Some(code) = params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone())
        else {
            let _ = respond(&mut stream, 400, "Missing code").await;
            return Ok(None);
        };

        let _ = respond(&mut stream, 200, SUCCESS_HTML).await;
        Ok(Some(Ok(CallbackResult { code })))
    }
}

fn self_signed(hostname: &str) -> Result<CertifiedKey<KeyPair>, String> {
    rcgen::generate_simple_self_signed(vec![hostname.to_string()])
        .map_err(|e| format!("failed to generate callback certificate: {e}"))
}

fn tls_acceptor(certified: CertifiedKey<KeyPair>) -> Result<TlsAcceptor, String> {
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let config = ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .and_then(|b| {
            b.with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key)
        })
        .map_err(|e| format!("failed to configure callback TLS: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub(crate) fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut iter = s.bytes();
    while let Some(b) = iter.next() {
        match b {
            b'+' => bytes.push(b' '),
            b'%' => {
                let h = iter.next().unwrap_or(b'0');
                let l = iter.next().unwrap_or(b'0');
                bytes.push((hex_val(h) << 4) | hex_val(l));
            }
            _ => bytes.push(b),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

async fn respond<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    body: &str,
) -> Result<(), std::io::Error> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.close().await
}

#[cfg(test)]
mod tests {
    use futures_rustls::TlsConnector;
    use futures_rustls::rustls::pki_types::ServerName;
    use futures_rustls::rustls::{ClientConfig, RootCertStore};
    use test_case::test_case;

    use super::*;

    const TEST_HOSTNAME: &str = "localhost";

    #[test_case("code=abc&state=xyz", &[("code", "abc"), ("state", "xyz")] ; "basic_params")]
    #[test_case("msg=hello%20world&foo=bar%2Bbaz", &[("msg", "hello world"), ("foo", "bar+baz")] ; "percent_encoded")]
    #[test_case("name=%C3%A9", &[("name", "\u{e9}")] ; "multibyte_utf8")]
    #[test_case("", &[] ; "empty_string")]
    fn parse_query_params(input: &str, expected: &[(&str, &str)]) {
        let params = parse_query(input);
        assert_eq!(params.len(), expected.len());
        for (got, want) in params.iter().zip(expected) {
            assert_eq!(got.0, want.0);
            assert_eq!(got.1, want.1);
        }
    }

    #[test]
    fn callback_receives_code() {
        smol::block_on(async {
            let server = CallbackServer::bind(None, None, None, CallbackScheme::Http)
                .await
                .unwrap();
            let port = server.port;

            let handle = smol::spawn(async move { server.wait_for_callback("test-state").await });

            let mut stream = smol::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .unwrap();
            let req = format!(
                "GET {CALLBACK_PATH}?code=auth-code&state=test-state HTTP/1.1\r\nHost: localhost\r\n\r\n"
            );
            stream.write_all(req.as_bytes()).await.unwrap();

            let result = handle.await.unwrap();
            assert_eq!(result.code, "auth-code");
        });
    }

    #[test]
    fn callback_receives_code_on_custom_path() {
        smol::block_on(async {
            let server = CallbackServer::bind(None, Some("/callback"), None, CallbackScheme::Http)
                .await
                .unwrap();
            let port = server.port;

            let handle = smol::spawn(async move { server.wait_for_callback("test-state").await });

            let mut stream = smol::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .unwrap();
            let req =
                "GET /callback?code=auth-code&state=test-state HTTP/1.1\r\nHost: localhost\r\n\r\n";
            stream.write_all(req.as_bytes()).await.unwrap();

            let result = handle.await.unwrap();
            assert_eq!(result.code, "auth-code");
        });
    }

    #[test]
    fn bind_pins_requested_port() {
        smol::block_on(async {
            let probe = smol::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = probe.local_addr().unwrap().port();
            drop(probe);

            let server =
                CallbackServer::bind(Some(port), Some("/callback"), None, CallbackScheme::Http)
                    .await
                    .unwrap();
            assert_eq!(server.port, port);
            assert_eq!(
                server.redirect_uri(),
                format!("http://127.0.0.1:{port}/callback")
            );
        });
    }

    #[test_case(CallbackScheme::Http, "http" ; "http")]
    #[test_case(CallbackScheme::Https, "https" ; "https")]
    fn bind_advertises_custom_hostname(scheme: CallbackScheme, expected_scheme: &str) {
        smol::block_on(async {
            let server = CallbackServer::bind(None, None, Some(TEST_HOSTNAME), scheme)
                .await
                .unwrap();
            assert_eq!(
                server.redirect_uri(),
                format!(
                    "{expected_scheme}://{TEST_HOSTNAME}:{}{CALLBACK_PATH}",
                    server.port
                )
            );
        });
    }

    #[test]
    fn bind_defaults_to_stock_path() {
        smol::block_on(async {
            let server = CallbackServer::bind(None, None, None, CallbackScheme::Http)
                .await
                .unwrap();
            assert_eq!(
                server.redirect_uri(),
                format!("http://127.0.0.1:{}{CALLBACK_PATH}", server.port)
            );
        });
    }

    #[test]
    fn bind_pinned_port_in_use_is_error() {
        smol::block_on(async {
            let listener = smol::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();

            let err = CallbackServer::bind(Some(port), None, None, CallbackScheme::Http)
                .await
                .err()
                .unwrap();
            assert!(err.contains("failed to bind callback port"));
        });
    }

    #[test]
    fn https_callback_receives_code_over_tls() {
        smol::block_on(async {
            let certified = self_signed(TEST_HOSTNAME).unwrap();
            let mut roots = RootCertStore::empty();
            roots.add(certified.cert.der().clone()).unwrap();
            let server = CallbackServer::bind_with(None, None, TEST_HOSTNAME, Some(certified))
                .await
                .unwrap();
            let port = server.port;
            assert_eq!(
                server.redirect_uri(),
                format!("https://{TEST_HOSTNAME}:{port}{CALLBACK_PATH}")
            );

            let handle = smol::spawn(async move { server.wait_for_callback("test-state").await });

            let config =
                ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
                    .with_safe_default_protocol_versions()
                    .unwrap()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(config));

            let rejected = smol::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            drop(rejected);

            let tcp = smol::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let mut stream = connector
                .connect(ServerName::try_from(TEST_HOSTNAME).unwrap(), tcp)
                .await
                .unwrap();
            let req = format!(
                "GET {CALLBACK_PATH}?code=auth-code&state=test-state HTTP/1.1\r\nHost: {TEST_HOSTNAME}\r\n\r\n"
            );
            stream.write_all(req.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();

            let result = handle.await.unwrap();
            assert_eq!(result.code, "auth-code");
        });
    }
}
