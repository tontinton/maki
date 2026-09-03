//! Outbound tunnel to an anchor server. The anchor forwards browser traffic
//! over the WebSocket; this side answers it with the same dispatch logic the
//! standalone server uses, so both modes are identical from the user's view.

use std::sync::mpsc::{self, Receiver};

use tungstenite::{Message as WsMessage, client::IntoClientRequest};

use crate::{RemoteRequest, dispatch::Route};

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("connect to {url}: {source}")]
    Connect {
        url: String,
        source: tungstenite::Error,
    },
    #[error("tunnel closed")]
    Closed,
}

/// Browser traffic forwarded by the anchor: one HTTP-shaped request.
#[derive(Debug, serde::Deserialize)]
pub struct ForwardedRequest {
    conn_id: u64,
    method: String,
    path: String,
    #[serde(default)]
    body: String,
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
/// Blocking; belongs on a dedicated thread.
pub fn run_tunnel(
    anchor_url: &str,
    registration_token: &str,
    client: crate::tunnel::TunnelClient,
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
    let hello = serde_json::json!({
        "instance_name": client.instance_name,
        "registration_token": registration_token,
    })
    .to_string();
    if socket.send(WsMessage::text(hello)).is_err() {
        return Err(TunnelError::Closed);
    }
    // SSE replies outlive one request: their frames are pushed from producer
    // threads into this channel and the tunnel loop forwards them as they
    // arrive, interleaved with answering new requests.
    let (stream_tx, stream_rx): (_, Receiver<StreamChunk>) = mpsc::channel();
    loop {
        let message = match socket.read() {
            Ok(WsMessage::Text(text)) => text,
            Ok(WsMessage::Ping(payload)) => {
                let _ = socket.send(WsMessage::Pong(payload));
                continue;
            }
            Ok(WsMessage::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Ok(parsed) = serde_json::from_str::<ForwardedRequest>(&message) else {
            continue;
        };
        match handle_forwarded(&client, parsed, stream_tx.clone()) {
            TunnelReply::Single(reply) => {
                let serialized = reply.to_string();
                if socket.send(WsMessage::text(serialized)).is_err() {
                    break;
                }
            }
            TunnelReply::StreamStart {
                conn_id,
                status,
                body,
            } => {
                let start = serde_json::json!({
                    "conn_id": conn_id, "status": status, "body": body, "final": false,
                })
                .to_string();
                if socket.send(WsMessage::text(start)).is_err() {
                    break;
                }
                // Pump stream frames as they are produced; blocks until the
                // SSE ends (client disconnect, shutdown), which is correct:
                // one browser tab holds the tunnel while it watches.
                while let Ok(chunk) = stream_rx.recv() {
                    let wire = serde_json::json!({
                        "conn_id": chunk.conn_id,
                        "status": 200,
                        "body": chunk.body,
                        "final": chunk.final_chunk,
                    })
                    .to_string();
                    if socket.send(WsMessage::text(wire)).is_err() {
                        break;
                    }
                    if chunk.final_chunk {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

struct StreamChunk {
    conn_id: u64,
    body: Vec<u8>,
    final_chunk: bool,
}

fn http_header_value(value: &str) -> tungstenite::http::HeaderValue {
    tungstenite::http::HeaderValue::from_str(value).expect("registration token is ascii")
}

fn handle_forwarded(
    client: &TunnelClient,
    request: ForwardedRequest,
    stream_tx: mpsc::Sender<StreamChunk>,
) -> TunnelReply {
    let token_hex = client.token_hex.as_str();
    let Some(path) = request.path.strip_prefix('/') else {
        return TunnelReply::Single(response_frame(
            request.conn_id,
            404,
            b"not found".to_vec(),
            true,
        ));
    };
    let Some(rest) = path.strip_prefix(token_hex) else {
        return TunnelReply::Single(response_frame(
            request.conn_id,
            404,
            b"invalid link".to_vec(),
            true,
        ));
    };
    let tail = rest.strip_prefix('/').unwrap_or(rest);
    let route = Route::from_tail(tail, &request.method);
    let outcome = client.dispatcher().dispatch(route, &request.body);
    match outcome {
        crate::dispatch::DispatchOutcome::NotFound => TunnelReply::Single(response_frame(
            request.conn_id,
            404,
            b"not found".to_vec(),
            true,
        )),
        crate::dispatch::DispatchOutcome::Index => TunnelReply::Single(response_frame(
            request.conn_id,
            200,
            crate::server::INDEX_HTML.as_bytes().to_vec(),
            true,
        )),
        crate::dispatch::DispatchOutcome::Posted(status) => {
            TunnelReply::Single(response_frame(request.conn_id, status, Vec::new(), true))
        }
        // SSE: the producer thread blocks on the fan-out channel and ships
        // frames; the tunnel loop forwards them until the stream ends. The
        // anchor serializes requests per connection, so the browser tab's
        // SSE owns the tunnel while other posts queue briefly.
        crate::dispatch::DispatchOutcome::Events { mut source } => {
            let conn_id = request.conn_id;
            let tx = stream_tx.clone();
            std::thread::Builder::new()
                .name("remote-tunnel-sse".into())
                .spawn(move || {
                    while let Some(frame) = source.next_frame() {
                        if tx
                            .send(StreamChunk {
                                conn_id,
                                body: frame,
                                final_chunk: false,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    let _ = tx.send(StreamChunk {
                        conn_id,
                        body: Vec::new(),
                        final_chunk: true,
                    });
                })
                .expect("spawn tunnel SSE thread");
            TunnelReply::StreamStart {
                conn_id,
                status: 200,
                body: Vec::new(),
            }
        }
    }
}

fn response_frame(
    conn_id: u64,
    status: u16,
    body: Vec<u8>,
    final_chunk: bool,
) -> serde_json::Value {
    serde_json::json!({"conn_id": conn_id, "status": status, "body": body, "final": final_chunk})
}
