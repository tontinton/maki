//! Request dispatch shared by the standalone HTTP server and the anchor
//! tunnel client, so both modes behave identically.

use std::time::Duration;

use flume::Sender;
use serde_json::json;

use crate::state::{PermissionFrame, RemoteState, RemoteUpdate};

pub const REQUEST_REPLY_TIMEOUT_SECS: u64 = 5;
pub const SSE_PING_SECS: u64 = 15;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Route {
    Index,
    Events,
    Prompt,
    Answer,
    Stop,
}

impl Route {
    pub fn from_tail(tail: &str, method: &str) -> Option<Route> {
        match (tail, method) {
            ("", "GET") => Some(Route::Index),
            ("events", "GET") => Some(Route::Events),
            ("prompt", "POST") => Some(Route::Prompt),
            ("answer", "POST") => Some(Route::Answer),
            ("stop", "POST") => Some(Route::Stop),
            _ => None,
        }
    }
}

/// Everything a mode needs to answer one request path.
pub struct Dispatcher {
    pub state: RemoteState,
    pub requests: Sender<crate::RemoteRequest>,
}

/// Outcome of dispatching one non-SSE route.
pub enum DispatchOutcome {
    Index,
    /// SSE: the caller streams frames produced by `SseSource` until it ends.
    Events {
        source: SseSource,
    },
    Posted(u16),
    NotFound,
}

impl Dispatcher {
    /// Match `/{token}/{tail}` and produce the response payload. Token check
    /// is the caller's job: both modes compare against their own secret.
    pub fn dispatch(&self, route: Option<Route>, body: &str) -> DispatchOutcome {
        let Some(route) = route else {
            return DispatchOutcome::NotFound;
        };
        match route {
            Route::Index => DispatchOutcome::Index,
            Route::Events => {
                let subscription = self.state.subscribe();
                let snapshot = self.snapshot_json();
                DispatchOutcome::Events {
                    source: SseSource::new(subscription, &snapshot),
                }
            }
            Route::Prompt | Route::Answer | Route::Stop => {
                let status = match self.dispatch_post(route, body) {
                    Some(()) => 200,
                    None => 400,
                };
                DispatchOutcome::Posted(status)
            }
        }
    }

    /// Asks the event loop for the current session snapshot. Empty object if
    /// the loop is wedged; the stream still opens with live events.
    fn snapshot_json(&self) -> serde_json::Value {
        let (tx, rx) = flume::bounded(1);
        if self
            .requests
            .try_send(crate::RemoteRequest::Snapshot { reply: tx })
            .is_err()
        {
            return json!({});
        }
        rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS))
            .unwrap_or_else(|_| json!({}))
    }

    fn dispatch_post(&self, route: Route, body: &str) -> Option<()> {
        let (request, reply_rx) = match route {
            Route::Prompt => {
                let text = parse_json_field(body, "text")?;
                let (tx, rx) = flume::unbounded();
                (crate::RemoteRequest::Prompt { text, reply: tx }, rx)
            }
            Route::Answer => {
                let value: serde_json::Value = serde_json::from_str(body).ok()?;
                let request_id = value.get("request_id")?.as_str()?.to_owned();
                let answer = value.get("answer")?.as_str()?.to_owned();
                let (tx, rx) = flume::unbounded();
                (
                    crate::RemoteRequest::Answer {
                        request_id,
                        answer,
                        reply: tx,
                    },
                    rx,
                )
            }
            Route::Stop => {
                let (tx, rx) = flume::unbounded();
                (crate::RemoteRequest::Stop { reply: tx }, rx)
            }
            Route::Index | Route::Events => return None,
        };
        self.requests.try_send(request).ok()?;
        // The loop answers fast (it only flips state or forwards); still,
        // bound the wait so a wedged UI thread cannot leak connections.
        match reply_rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS)) {
            Ok(Ok(())) => Some(()),
            Ok(Err(reason)) => {
                tracing::debug!(%reason, "remote control: request rejected");
                None
            }
            Err(_) => None,
        }
    }
}

/// The SSE frame source, shared by both modes. Pulls the fan-out channel and
/// formats frames; standalone mode reads it via `Read`, the tunnel sends each
/// frame as a WS text message.
pub struct SseSource {
    subscription: crate::state::Subscription,
    idle_deadline: Option<std::time::Instant>,
    buf: Vec<u8>,
}

impl SseSource {
    pub fn new(subscription: crate::state::Subscription, snapshot: &serde_json::Value) -> Self {
        let mut buf = Vec::new();
        write_frame(&mut buf, "snapshot", snapshot);
        Self {
            subscription,
            idle_deadline: None,
            buf,
        }
    }

    /// Blocks until the next frame is fully available. `None` ends the stream.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            let updates = &self.subscription.updates;
            let update = match self.idle_deadline {
                Some(deadline) => match updates.recv_deadline(deadline) {
                    Ok(update) => Some(update),
                    Err(flume::RecvTimeoutError::Timeout) => {
                        self.idle_deadline = None;
                        self.buf.extend_from_slice(b": ping\n\n");
                        None
                    }
                    Err(_) => return None,
                },
                None => match updates.recv() {
                    Ok(update) => Some(update),
                    Err(_) => return None,
                },
            };
            if let Some(update) = update {
                self.idle_deadline =
                    Some(std::time::Instant::now() + Duration::from_secs(SSE_PING_SECS));
                match update {
                    RemoteUpdate::Envelope { event, .. } => {
                        write_frame(&mut self.buf, "event", &event);
                    }
                    RemoteUpdate::Status { status, .. } => {
                        write_frame(&mut self.buf, "status", &json!(status));
                    }
                    RemoteUpdate::Permission { frame, .. } => {
                        write_frame(&mut self.buf, "permission", &frame_json(&frame));
                    }
                    RemoteUpdate::PermissionResolved { request_id, .. } => {
                        write_frame(
                            &mut self.buf,
                            "permission_resolved",
                            &json!({ "id": request_id }),
                        );
                    }
                    RemoteUpdate::Shutdown => return None,
                }
            }
        }
        Some(std::mem::take(&mut self.buf))
    }
}

pub fn write_frame(buf: &mut Vec<u8>, event: &str, payload: &serde_json::Value) {
    use std::io::Write;
    let _ = write!(buf, "event: {event}\ndata: {payload}\n\n");
}

fn frame_json(frame: &PermissionFrame) -> serde_json::Value {
    json!({
        "id": frame.id,
        "tool": frame.tool,
        "scopes": frame.scopes,
    })
}

pub fn parse_json_field(body: &str, field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get(field)?.as_str().map(str::to_owned)
}
