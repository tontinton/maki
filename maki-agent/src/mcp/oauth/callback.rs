use futures_lite::AsyncWriteExt;
use smol::net::TcpListener;

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
}

impl CallbackServer {
    /// `port` pins the loopback listener so the redirect URI can be
    /// pre-registered with the authorization server. Without it, fall back to
    /// `PREFERRED_PORT`, then to any free port. `path`/`hostname` override the
    /// advertised redirect URI; binding stays on 127.0.0.1, which `localhost`
    /// (and 127.0.0.1 itself) resolves to.
    pub async fn bind(
        port: Option<u16>,
        path: Option<&str>,
        hostname: Option<&str>,
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
        let path = path.unwrap_or(CALLBACK_PATH).to_string();
        let hostname = hostname.unwrap_or(DEFAULT_HOSTNAME).to_string();
        Ok(Self {
            port,
            hostname,
            path,
            listener,
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://{}:{}{}", self.hostname, self.port, self.path)
    }

    pub async fn wait_for_callback(self, expected_state: &str) -> Result<CallbackResult, String> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| format!("accept failed: {e}"))?;

            let mut buf = Vec::with_capacity(1024);
            let mut tmp = [0u8; 1024];
            loop {
                let n = futures_lite::AsyncReadExt::read(&mut stream, &mut tmp)
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

            let path = match request.lines().next() {
                Some(line) => line.split_whitespace().nth(1).unwrap_or(""),
                None => continue,
            };

            if !path.starts_with(self.path.as_str()) {
                let _ = respond(&mut stream, 404, "Not Found").await;
                continue;
            }

            let query = path.split('?').nth(1).unwrap_or("");
            let params = parse_query(query);

            if let Some(error) = params.iter().find(|(k, _)| k == "error") {
                let desc = params
                    .iter()
                    .find(|(k, _)| k == "error_description")
                    .map(|(_, v)| v.as_str())
                    .unwrap_or(&error.1);
                let _ = respond(&mut stream, 400, ERROR_HTML).await;
                return Err(format!("OAuth error: {desc}"));
            }

            let state = params
                .iter()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.clone());
            let code = params
                .iter()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.clone());

            let (Some(state), Some(code)) = (state, code) else {
                let _ = respond(&mut stream, 400, "Missing code or state").await;
                continue;
            };

            if state != expected_state {
                let _ = respond(&mut stream, 403, "State mismatch").await;
                continue;
            }

            let _ = respond(&mut stream, 200, SUCCESS_HTML).await;
            return Ok(CallbackResult { code });
        }
    }
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

async fn respond(
    stream: &mut smol::net::TcpStream,
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
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

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
            let server = CallbackServer::bind(None, None, None).await.unwrap();
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
            let server = CallbackServer::bind(None, Some("/callback"), None)
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

            let server = CallbackServer::bind(Some(port), Some("/callback"), None)
                .await
                .unwrap();
            assert_eq!(server.port, port);
            assert_eq!(
                server.redirect_uri(),
                format!("http://127.0.0.1:{port}/callback")
            );
        });
    }

    #[test]
    fn bind_advertises_custom_hostname() {
        smol::block_on(async {
            let server = CallbackServer::bind(None, None, Some("localhost"))
                .await
                .unwrap();
            assert_eq!(
                server.redirect_uri(),
                format!("http://localhost:{}{CALLBACK_PATH}", server.port)
            );
        });
    }

    #[test]
    fn bind_defaults_to_stock_path() {
        smol::block_on(async {
            let server = CallbackServer::bind(None, None, None).await.unwrap();
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

            let err = CallbackServer::bind(Some(port), None, None)
                .await
                .err()
                .unwrap();
            assert!(err.contains("failed to bind callback port"));
        });
    }
}
