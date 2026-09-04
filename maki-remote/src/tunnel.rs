//! Outbound tunnel to an anchor server. The anchor forwards browser traffic
//! over the WebSocket; this side answers it with the same dispatch logic the
//! standalone server uses, so both modes are identical from the user's view.

use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver},
    },
    thread,
};

use tungstenite::{
    Message as WsMessage, client::IntoClientRequest, protocol::WebSocket, stream::MaybeTlsStream,
};

use crate::{RemoteRequest, dispatch::Route};

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("connect to {url}: {source}")]
    Connect {
        url: String,
        source: tungstenite::Error,
    },
    #[error("io: {source}")]
    Io { source: std::io::Error },
    #[error("tunnel closed")]
    Closed,
}

/// A reply frame heading back to the anchor over the tunnel.
#[derive(Debug)]
pub enum TunnelReplyWire {
    Response {
        conn_id: u64,
        status: u16,
        body: Vec<u8>,
        final_chunk: bool,
    },
}

/// The anchor's handshake frame: the share link minted for this tunnel.
#[derive(Debug, serde::Deserialize)]
struct LinkFrame {
    link: String,
}

/// Browser traffic forwarded by the anchor: one HTTP-shaped request.
#[derive(Debug, serde::Deserialize)]
pub struct ForwardedRequest {
    conn_id: u64,
    method: String,
    path: String,
    #[serde(default)]
    body: Vec<u8>,
}

/// One streamed SSE chunk heading back through the tunnel.
pub enum TunnelReply {
    Single(serde_json::Value),
    /// An SSE stream opened: chunks flow through the producer channel until
    /// one arrives with `final_chunk: true`.
    StreamStart {
        conn_id: u64,
        status: u16,
        body: Vec<u8>,
    },
}

/// Everything the tunnel needs to answer forwarded traffic: the fan-out hub
/// for SSE and the request channel the event loop drains, both reused from
/// standalone mode.
pub struct TunnelClient {
    pub state: crate::state::RemoteState,
    pub requests: flume::Sender<RemoteRequest>,
    /// Registration token as 32 lowercase hex chars.
    pub token_hex: String,
    pub instance_name: String,
    dispatcher: crate::dispatch::Dispatcher,
}

impl TunnelClient {
    pub fn new(
        requests: flume::Sender<RemoteRequest>,
        token_hex: String,
        instance_name: String,
    ) -> Self {
        let state = crate::state::RemoteState::new();
        Self {
            state: state.clone(),
            requests: requests.clone(),
            token_hex,
            instance_name,
            dispatcher: crate::dispatch::Dispatcher { state, requests },
        }
    }

    fn dispatcher(&self) -> &crate::dispatch::Dispatcher {
        &self.dispatcher
    }
}

/// Runs the outbound tunnel until the anchor drops it or the process exits.
/// Returns the share URL the anchor minted for this tunnel, via `link_out`.
/// Blocking; belongs on a dedicated thread.
pub fn run_tunnel(
    anchor_url: &str,
    registration_token: &str,
    client: crate::tunnel::TunnelClient,
    link_out: flume::Sender<String>,
) -> Result<(), TunnelError> {
    let ws_url = format!("{}/ws", anchor_url.trim_end_matches('/'));
    let mut request =
        ws_url
            .as_str()
            .into_client_request()
            .map_err(|source| TunnelError::Connect {
                url: ws_url.clone(),
                source,
            })?;
    request
        .headers_mut()
        .insert("x-maki-registration", http_header_value(registration_token));
    let (mut socket, _response) =
        tungstenite::connect(request).map_err(|source| TunnelError::Connect {
            url: ws_url.clone(),
            source,
        })?;
    let write_stream = match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream.try_clone(),
        _ => Err(std::io::Error::other("tls tunnels unsupported")),
    }
    .map_err(|source| TunnelError::Io { source })?;
    let hello = serde_json::json!({
        "instance_name": client.instance_name,
        "registration_token": registration_token,
    })
    .to_string();
    if socket.send(WsMessage::text(hello)).is_err() {
        return Err(TunnelError::Closed);
    }
    // First anchor frame hands back the freshly minted control link.
    let first = match socket.read() {
        Ok(WsMessage::Text(text)) => text,
        _ => return Err(TunnelError::Closed),
    };
    let Ok(link_frame) = serde_json::from_str::<LinkFrame>(&first) else {
        return Err(TunnelError::Closed);
    };
    let _ = link_out.send(link_frame.link);
    // Split handles: the reader blocks on anchor frames, the writer thread
    // ships replies and SSE chunks as producers finish them. TCP is full
    // duplex, so the two tungstenite handles never contend.
    let (reply_tx, reply_rx): (_, Receiver<TunnelReplyWire>) = mpsc::channel();
    let writer_stream = write_stream;
    let writer = Arc::new(Mutex::new(WebSocket::from_raw_socket(
        writer_stream,
        tungstenite::protocol::Role::Client,
        None,
    )));
    let writer_handle = Arc::clone(&writer);
    thread::spawn(move || {
        while let Ok(reply) = reply_rx.recv() {
            let TunnelReplyWire::Response {
                conn_id,
                status,
                body,
                final_chunk,
            } = reply;
            let wire = serde_json::json!({"conn_id": conn_id, "status": status, "body": body, "final": final_chunk});
            if writer_handle
                .lock()
                .unwrap()
                .send(WsMessage::text(wire.to_string()))
                .is_err()
            {
                break;
            }
        }
    });
    loop {
        let message = match socket.read() {
            Ok(WsMessage::Text(text)) => text,
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Ok(parsed) = serde_json::from_str::<ForwardedRequest>(&message) else {
            continue;
        };
        handle_forwarded(&client, parsed, reply_tx.clone());
    }
    Ok(())
}

fn http_header_value(value: &str) -> tungstenite::http::HeaderValue {
    tungstenite::http::HeaderValue::from_str(value).expect("registration token is ascii")
}

/// Answers one forwarded request on the reply channel. Non-SSE routes reply
/// inline; SSE spawns a producer that streams frames until the stream ends.
fn handle_forwarded(
    client: &TunnelClient,
    request: ForwardedRequest,
    reply_tx: mpsc::Sender<TunnelReplyWire>,
) {
    // The anchor forwards the bare tail (it owns the link token); accept the
    // token-prefixed shape too so the dispatcher stays testable standalone.
    let path = request.path.strip_prefix('/').unwrap_or(&request.path);
    let tail = if path.len() > client.token_hex.len()
        && path[..client.token_hex.len()] == client.token_hex
    {
        &path[client.token_hex.len()..]
    } else {
        path
    };
    let tail = tail.trim_start_matches('/');
    let route = Route::from_tail(tail, &request.method);
    let body = String::from_utf8_lossy(&request.body).into_owned();
    let outcome = client.dispatcher().dispatch(route, &body);
    let conn_id = request.conn_id;
    let send = |status: u16, body: Vec<u8>, final_chunk: bool| {
        tracing::info!(conn_id, status, final_chunk, "tunnel: shipping reply");
        let _ = reply_tx.send(TunnelReplyWire::Response {
            conn_id,
            status,
            body,
            final_chunk,
        });
    };
    match outcome {
        crate::dispatch::DispatchOutcome::NotFound => send(404, b"not found".to_vec(), true),
        crate::dispatch::DispatchOutcome::Index => {
            send(200, crate::server::INDEX_HTML.as_bytes().to_vec(), true)
        }
        crate::dispatch::DispatchOutcome::Posted(status) => send(status, Vec::new(), true),
        crate::dispatch::DispatchOutcome::Json { status, body } => send(status, body, true),
        // SSE: the producer blocks on the fan-out channel and ships frames;
        // the writer thread forwards them until the stream ends.
        crate::dispatch::DispatchOutcome::Events { mut source } => {
            if reply_tx
                .send(TunnelReplyWire::Response {
                    conn_id,
                    status: 200,
                    body: Vec::new(),
                    final_chunk: false,
                })
                .is_err()
            {
                return;
            }
            std::thread::Builder::new()
                .name("remote-tunnel-sse".into())
                .spawn(move || {
                    while let Some(frame) = source.next_frame() {
                        if reply_tx
                            .send(TunnelReplyWire::Response {
                                conn_id,
                                status: 200,
                                body: frame,
                                final_chunk: false,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    let _ = reply_tx.send(TunnelReplyWire::Response {
                        conn_id,
                        status: 200,
                        body: Vec::new(),
                        final_chunk: true,
                    });
                })
                .expect("spawn tunnel SSE thread");
        }
    }
}
