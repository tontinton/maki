use std::{
    net::TcpListener,
    sync::{Arc, Mutex, mpsc::channel},
    thread,
};

use tiny_http::{Header, Response, Server};
use tungstenite::{Message as WsMessage, protocol::WebSocket};

use crate::{
    hub::{self, Hub, TunnelCommand, TunnelPush},
    store::{SessionRow, Store},
};

const MAX_BODY: usize = 10 * 1024 * 1024;

#[derive(Debug, serde::Deserialize)]
struct HelloFrame {
    instance_name: String,
    registration_token: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum TunnelWireFrame {
    Response {
        conn_id: u64,
        status: u16,
        body: Vec<u8>,
        #[serde(rename = "final")]
        final_chunk: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Strip the leading `/{token}` path component used by the standalone remote UI.
fn split_token_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix('/')?;
    let (token, tail) = rest.split_once('/')?;
    Some((token, tail))
}

pub fn serve(addr: &str, store: Arc<Store>) -> Result<(), ServerError> {
    let listener = TcpListener::bind(addr).map_err(|source| ServerError::Bind {
        addr: addr.to_string(),
        source,
    })?;
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let server = Arc::new(Server::from_listener(listener, None).expect("tiny_http server"));
    let hub = Hub::new();

    // The instance tunnel lives on its own listener: browser traffic goes
    // through tiny_http, tunnels through a plain socket we drive directly.
    let ws_addr = format!(
        "{}:{}",
        addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr),
        port + 1
    );
    let ws_listener = TcpListener::bind(&ws_addr).map_err(|source| ServerError::Bind {
        addr: ws_addr.clone(),
        source,
    })?;
    let ws_store = Arc::clone(&store);
    let ws_hub = Arc::clone(&hub);
    thread::spawn(move || {
        for socket in ws_listener.incoming() {
            let Ok(socket) = socket else {
                continue;
            };
            let _ = socket.set_nodelay(true);
            let hub = Arc::clone(&ws_hub);
            let store = Arc::clone(&ws_store);
            thread::spawn(move || {
                let Ok(websocket) = tungstenite::accept(socket) else {
                    return;
                };
                drive_tunnel(websocket, hub, store);
            });
        }
    });

    tracing::info!(addr, ws_addr, "anchor listening");

    loop {
        let request = match server.recv() {
            Ok(request) => request,
            Err(err) => {
                tracing::warn!(error = %err, "accept failed");
                continue;
            }
        };
        let hub = Arc::clone(&hub);
        let store = Arc::clone(&store);
        thread::spawn(move || {
            handle_request(request, hub, store);
        });
    }
}

fn handle_request(request: tiny_http::Request, hub: Arc<Hub>, store: Arc<Store>) {
    let method = request.method().to_string();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("");

    if path == "/ws" {
        // The tunnel endpoint lives on the WS listener (HTTP port + 1).
        let response = Response::from_string("use the ws port")
            .with_status_code(426)
            .with_header(Header::from_bytes(&b"Content-Type"[..], b"text/plain".as_ref()).unwrap());
        let _ = request.respond(response);
        return;
    }

    let mut request = request;
    let (status, content_type, body) = route(path, &mut request, &hub, &store);
    let response = Response::from_data(body)
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap());
    let _ = request.respond(response);
    tracing::debug!(method, path, status, "request");
}

fn route(
    path: &str,
    request: &mut tiny_http::Request,
    hub: &Hub,
    store: &Arc<Store>,
) -> (u16, String, Vec<u8>) {
    if let Some((token, tail)) = split_token_path(path) {
        return proxy_remote(token, tail, request, hub, store);
    }
    if path == "/instances" {
        return json_list_instances(store);
    }
    if path == "/sessions" {
        return json_list_sessions(store);
    }
    if let Some(rest) = path.strip_prefix("/sessions/") {
        let Ok(instance_id) = rest.parse::<i64>() else {
            return (400, "text/plain".to_string(), b"bad instance id".to_vec());
        };
        return match store.sessions_for_instance(instance_id) {
            Ok(rows) => {
                let body = serde_json::to_vec(&rows).unwrap_or_default();
                (200, "application/json".to_string(), body)
            }
            Err(err) => (
                500,
                "text/plain".to_string(),
                format!("store error: {err}").into_bytes(),
            ),
        };
    }
    (404, "text/plain".to_string(), b"not found".to_vec())
}

fn json_list_instances(store: &Arc<Store>) -> (u16, String, Vec<u8>) {
    match store.list_instances() {
        Ok(rows) => {
            let body = serde_json::to_vec(&rows).unwrap_or_default();
            (200, "application/json".to_string(), body)
        }
        Err(err) => (
            500,
            "text/plain".to_string(),
            format!("store error: {err}").into_bytes(),
        ),
    }
}

fn json_list_sessions(store: &Arc<Store>) -> (u16, String, Vec<u8>) {
    match store.list_sessions() {
        Ok(rows) => {
            let body = serde_json::to_vec(&rows).unwrap_or_default();
            (200, "application/json".to_string(), body)
        }
        Err(err) => (
            500,
            "text/plain".to_string(),
            format!("store error: {err}").into_bytes(),
        ),
    }
}

/// Persist a session-index push from an instance.
fn handle_push(store: &Arc<Store>, push: TunnelPush) {
    let TunnelPush::SessionIndex {
        instance_name,
        sessions,
    } = push;
    let Ok(instance) = store.instance_by_name(&instance_name) else {
        tracing::warn!(instance_name, "push from unknown instance");
        return;
    };
    for entry in sessions {
        let row = SessionRow {
            instance_id: instance.id,
            external_id: entry.session_id,
            title: entry.title,
            model: entry.model,
            cwd: entry.cwd,
            status: entry.status,
            cost_cents: entry.cost_cents,
            tokens_in: entry.tokens_in,
            tokens_out: entry.tokens_out,
            context_window: entry.context_window,
            updated_at: crate::store::now_unix(),
        };
        if let Err(err) = store.upsert_session(&row) {
            tracing::warn!(error = %err, instance_name, "session upsert failed");
        }
    }
}

/// `/{token}/{tail}` -> the session id when `tail` is `/s/{session_id}` shaped,
/// used for session-scoped links.
fn request_path_session(tail: &str) -> Option<String> {
    let rest = tail.strip_prefix("s/")?;
    Some(rest.split('/').next().unwrap_or(rest).to_owned())
}

fn proxy_remote(
    token: &str,
    tail: &str,
    request: &mut tiny_http::Request,
    hub: &Hub,
    store: &Arc<Store>,
) -> (u16, String, Vec<u8>) {
    let link = match store.link_by_token(token) {
        Ok(link) => link,
        Err(_) => {
            return (
                404,
                "text/plain".to_string(),
                b"invalid or expired link".to_vec(),
            );
        }
    };
    if !hub.is_online(link.instance_id) {
        return (503, "text/plain".to_string(), b"instance offline".to_vec());
    }

    // A session-scoped link only opens that session; others 404 at the
    // instance (which re-checks the path against its own token anyway).
    if let Some(session_id) = link.external_session_id.as_deref() {
        let requested = request_path_session(tail);
        if requested.is_none_or(|id| id != session_id) {
            return (
                404,
                "text/plain".to_string(),
                b"link is scoped to another session".to_vec(),
            );
        }
    }

    let method = request.method().as_str().to_string();
    let mut body = Vec::new();
    if request.as_reader().read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
        return (413, "text/plain".to_string(), b"body too large".to_vec());
    }

    let forwarded = serde_json::json!({
        "method": method,
        "path": format!("/{token}/{tail}"),
        "headers": {},
        "body": body,
    })
    .to_string();

    let (_conn_id, rx) = match hub.request(link.instance_id, forwarded) {
        Ok(pair) => pair,
        Err(err) => {
            return (
                502,
                "text/plain".to_string(),
                format!("tunnel: {err}").into_bytes(),
            );
        }
    };
    let first = match hub.wait_first(&rx) {
        Ok(first) => first,
        Err(err) => {
            return (
                502,
                "text/plain".to_string(),
                format!("tunnel: {err}").into_bytes(),
            );
        }
    };
    let status = first.status;
    let mut acc = first.body;
    if !first.final_chunk {
        loop {
            let chunk = match hub.wait_first(&rx) {
                Ok(chunk) => chunk,
                Err(err) => {
                    return (
                        502,
                        "text/plain".to_string(),
                        format!("tunnel: {err}").into_bytes(),
                    );
                }
            };
            acc.extend_from_slice(&chunk.body);
            if chunk.final_chunk {
                break;
            }
        }
    }
    (status, "application/json".to_string(), acc)
}

fn drive_tunnel(mut websocket: WebSocket<std::net::TcpStream>, hub: Arc<Hub>, store: Arc<Store>) {
    let hello = match websocket.read() {
        Ok(WsMessage::Text(text)) => text,
        _ => return,
    };
    let Ok(parsed) = serde_json::from_str::<HelloFrame>(&hello) else {
        return;
    };
    let instance = match store.instance_by_registration_token(&parsed.registration_token) {
        Ok(instance) if instance.name == parsed.instance_name => instance,
        _ => {
            let _ = websocket.send(WsMessage::text("auth failed"));
            return;
        }
    };
    store.touch_instance(instance.id).ok();
    let (cmd_tx, cmd_rx) = channel::<TunnelCommand>();
    hub.attach(instance.id, cmd_tx);

    let write_socket = websocket.into_inner();
    let read_socket = match write_socket.try_clone() {
        Ok(read_socket) => read_socket,
        Err(_) => {
            hub.detach(instance.id);
            return;
        }
    };
    // Only the writer thread touches this handle; reader uses its own clone.
    let writer = Arc::new(Mutex::new(WebSocket::from_raw_socket(
        write_socket,
        tungstenite::protocol::Role::Server,
        None,
    )));
    let writer_hub = Arc::clone(&hub);
    let writer_id = instance.id;
    let writer_handle = Arc::clone(&writer);
    thread::spawn(move || {
        while let Ok(command) = cmd_rx.recv() {
            let TunnelCommand::Request { conn_id, request } = command;
            let mut wire = serde_json::Map::new();
            wire.insert("conn_id".into(), conn_id.into());
            if let Ok(inner) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&request)
            {
                wire.extend(inner);
            }
            let wire = serde_json::Value::Object(wire).to_string();
            let mut guard = writer_handle.lock().unwrap();
            if guard.send(WsMessage::text(wire)).is_err() {
                break;
            }
        }
        writer_hub.detach(writer_id);
    });

    let mut reader =
        WebSocket::from_raw_socket(read_socket, tungstenite::protocol::Role::Server, None);
    loop {
        let message = match reader.read() {
            Ok(message) => message,
            Err(_) => break,
        };
        match message {
            WsMessage::Text(text) => {
                if let Ok(push) = serde_json::from_str::<TunnelPush>(&text) {
                    handle_push(&store, push);
                    continue;
                }
                let Ok(frame) = serde_json::from_str::<TunnelWireFrame>(&text) else {
                    continue;
                };
                let TunnelWireFrame::Response {
                    conn_id,
                    status,
                    body,
                    final_chunk,
                } = frame;
                hub.deliver_response(
                    conn_id,
                    hub::TunnelResponse {
                        status,
                        body,
                        final_chunk,
                    },
                );
            }
            WsMessage::Ping(payload) => {
                let _ = writer.lock().unwrap().send(WsMessage::Pong(payload));
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    hub.detach(instance.id);
    store.touch_instance(instance.id).ok();
}
