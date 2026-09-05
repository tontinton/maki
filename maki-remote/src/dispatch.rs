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
    Command,
    Sessions,
    ModelGet,
    ModelPost,
    Commands,
    Options,
    OptionsPost,
}

impl Route {
    pub fn from_tail(tail: &str, method: &str) -> Option<Route> {
        match (tail, method) {
            ("", "GET") => Some(Route::Index),
            ("events", "GET") => Some(Route::Events),
            ("prompt", "POST") => Some(Route::Prompt),
            ("answer", "POST") => Some(Route::Answer),
            ("stop", "POST") => Some(Route::Stop),
            ("command", "POST") => Some(Route::Command),
            ("sessions", "GET") => Some(Route::Sessions),
            ("model", "GET") => Some(Route::ModelGet),
            ("model", "POST") => Some(Route::ModelPost),
            ("commands", "GET") => Some(Route::Commands),
            ("options", "GET") => Some(Route::Options),
            ("options", "POST") => Some(Route::OptionsPost),
            _ => None,
        }
    }
}

/// Split the optional `s/<session-id>/` prefix a session-scoped link puts in
/// front of every route, so one page URL pins the whole tab of requests to
/// that session. Returns the session id and the route under it.
pub fn parse_tail(tail: &str, method: &str) -> (Option<String>, Option<Route>) {
    let Some(rest) = tail.strip_prefix("s/") else {
        return (None, Route::from_tail(tail, method));
    };
    let (id, sub) = match rest.split_once('/') {
        Some((id, sub)) => (id, sub),
        None => (rest, ""),
    };
    (Some(id.to_owned()), Route::from_tail(sub, method))
}

/// Everything a mode needs to answer one request path.
pub struct Dispatcher {
    pub state: RemoteState,
    pub requests: Sender<crate::RemoteRequest>,
}

/// Outcome of dispatching one request path.
pub enum DispatchOutcome {
    Index,
    /// SSE: the caller streams frames produced by `SseSource` until it ends.
    Events {
        source: SseSource,
    },
    /// Status plus an error message when the request was rejected.
    Posted(u16, Option<String>),
    Json {
        status: u16,
        body: Vec<u8>,
    },
    NotFound,
}

impl Dispatcher {
    /// Match `/{token}[/{session}]/{tail}` and produce the response payload.
    /// Token check is the caller's job: both modes compare against their own
    /// secret.
    pub fn dispatch(
        &self,
        route: Option<Route>,
        session: Option<String>,
        body: &str,
    ) -> DispatchOutcome {
        let Some(route) = route else {
            return DispatchOutcome::NotFound;
        };
        match route {
            Route::Index => DispatchOutcome::Index,
            Route::Events => {
                let subscription = self.state.subscribe(session.clone());
                let snapshot = self.snapshot_json(session.clone());
                DispatchOutcome::Events {
                    source: SseSource::new(subscription, &snapshot, session),
                }
            }
            Route::Commands => match self.dispatch_value(|reply| crate::RemoteRequest::Commands {
                session: session.clone(),
                reply,
            }) {
                Some(value) => DispatchOutcome::Json {
                    status: 200,
                    body: serde_json::to_vec(&value).unwrap_or_default(),
                },
                None => DispatchOutcome::Json {
                    status: 503,
                    body: br#"{"error":"event loop wedged"}"#.to_vec(),
                },
            },
            Route::Options => match self.dispatch_value(|reply| crate::RemoteRequest::Options {
                session: session.clone(),
                reply,
            }) {
                Some(value) => DispatchOutcome::Json {
                    status: 200,
                    body: serde_json::to_vec(&value).unwrap_or_default(),
                },
                None => DispatchOutcome::Json {
                    status: 503,
                    body: br#"{"error":"event loop wedged"}"#.to_vec(),
                },
            },
            Route::OptionsPost => {
                let parsed: serde_json::Value = match serde_json::from_str(body) {
                    Ok(v) => v,
                    Err(_) => {
                        return DispatchOutcome::Json {
                            status: 400,
                            body: br#"{"error":"invalid json"}"#.to_vec(),
                        };
                    }
                };
                let yolo = parsed.get("yolo").and_then(|v| v.as_bool());
                let mode = parsed
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                if yolo.is_none() && mode.is_none() {
                    return DispatchOutcome::Json {
                        status: 400,
                        body: br#"{"error":"no fields: need yolo or mode"}"#.to_vec(),
                    };
                }
                match self.dispatch_value(|reply| crate::RemoteRequest::SetOptions {
                    session: session.clone(),
                    yolo,
                    mode,
                    reply,
                }) {
                    Some(value) => DispatchOutcome::Json {
                        status: 200,
                        body: serde_json::to_vec(&value).unwrap_or_default(),
                    },
                    None => DispatchOutcome::Json {
                        status: 503,
                        body: br#"{"error":"event loop wedged"}"#.to_vec(),
                    },
                }
            }
            Route::Sessions => match self.dispatch_sessions() {
                Some(value) => DispatchOutcome::Json {
                    status: 200,
                    body: serde_json::to_vec(&value).unwrap_or_default(),
                },
                None => DispatchOutcome::Json {
                    status: 503,
                    body: br#"{"error":"event loop wedged"}"#.to_vec(),
                },
            },
            Route::ModelGet => match self.dispatch_model_get(session) {
                Some(value) => DispatchOutcome::Json {
                    status: 200,
                    body: serde_json::to_vec(&value).unwrap_or_default(),
                },
                None => DispatchOutcome::Json {
                    status: 503,
                    body: br#"{"error":"event loop wedged"}"#.to_vec(),
                },
            },
            Route::ModelPost => match self.dispatch_model_set(session, body) {
                Some(Ok(value)) => DispatchOutcome::Json {
                    status: 200,
                    body: serde_json::to_vec(&value).unwrap_or_default(),
                },
                Some(Err(msg)) => DispatchOutcome::Json {
                    status: 400,
                    body: serde_json::to_vec(&json!({"error": msg})).unwrap_or_default(),
                },
                None => DispatchOutcome::Json {
                    status: 503,
                    body: br#"{"error":"event loop wedged"}"#.to_vec(),
                },
            },
            Route::Prompt | Route::Answer | Route::Stop | Route::Command => {
                match self.dispatch_post(route, session, body) {
                    Ok(()) => DispatchOutcome::Posted(200, None),
                    Err(reason) => DispatchOutcome::Posted(400, Some(reason)),
                }
            }
        }
    }

    /// One-shot request that answers with a JSON value (commands list,
    /// options). `None` when the loop cannot reply in time.
    fn dispatch_value(
        &self,
        make: impl FnOnce(Sender<serde_json::Value>) -> crate::RemoteRequest,
    ) -> Option<serde_json::Value> {
        let (tx, rx) = flume::bounded(1);
        self.requests.try_send(make(tx)).ok()?;
        rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS))
            .ok()
    }

    /// Asks the event loop for the current session snapshot. Empty object if
    /// the loop is wedged; the stream still opens with live events.
    fn snapshot_json(&self, session: Option<String>) -> serde_json::Value {
        let (tx, rx) = flume::bounded(1);
        if self
            .requests
            .try_send(crate::RemoteRequest::Snapshot { session, reply: tx })
            .is_err()
        {
            return json!({});
        }
        rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS))
            .unwrap_or_else(|_| json!({}))
    }

    fn dispatch_sessions(&self) -> Option<serde_json::Value> {
        let (tx, rx) = flume::bounded(1);
        self.requests
            .try_send(crate::RemoteRequest::Sessions { reply: tx })
            .ok()?;
        rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS))
            .ok()
    }

    fn dispatch_model_get(&self, session: Option<String>) -> Option<serde_json::Value> {
        let (tx, rx) = flume::bounded(1);
        self.requests
            .try_send(crate::RemoteRequest::ModelGet { session, reply: tx })
            .ok()?;
        rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS))
            .ok()
    }

    fn dispatch_model_set(
        &self,
        session: Option<String>,
        body: &str,
    ) -> Option<Result<serde_json::Value, String>> {
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return Some(Err("invalid json".into())),
        };
        let spec = value
            .get("spec")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let thinking = value
            .get("thinking")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let fast = value.get("fast").and_then(|v| v.as_bool());
        if spec.is_none() && thinking.is_none() && fast.is_none() {
            return Some(Err("no model fields: need spec, thinking or fast".into()));
        }
        let (tx, rx) = flume::bounded(1);
        self.requests
            .try_send(crate::RemoteRequest::ModelSet {
                session,
                spec,
                thinking,
                fast,
                reply: tx,
            })
            .ok()?;
        rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS))
            .ok()
    }

    fn dispatch_post(
        &self,
        route: Route,
        session: Option<String>,
        body: &str,
    ) -> Result<(), String> {
        let (request, reply_rx) = match route {
            Route::Prompt => {
                let text = parse_json_field(body, "text")
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| "missing text".to_owned())?;
                let (tx, rx) = flume::unbounded();
                (
                    crate::RemoteRequest::Prompt {
                        session,
                        text,
                        reply: tx,
                    },
                    rx,
                )
            }
            Route::Answer => {
                let value: serde_json::Value =
                    serde_json::from_str(body).map_err(|_| "invalid json".to_owned())?;
                let request_id = required_str(&value, "request_id")?;
                let answer = required_str(&value, "answer")?;
                let (tx, rx) = flume::unbounded();
                (
                    crate::RemoteRequest::Answer {
                        session,
                        request_id,
                        answer,
                        reply: tx,
                    },
                    rx,
                )
            }
            Route::Stop => {
                let (tx, rx) = flume::unbounded();
                (crate::RemoteRequest::Stop { session, reply: tx }, rx)
            }
            Route::Command => {
                let value: serde_json::Value =
                    serde_json::from_str(body).map_err(|_| "invalid json".to_owned())?;
                let cmdline = value
                    .get("cmdline")
                    .or_else(|| value.get("command"))
                    .or_else(|| value.get("text"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "missing cmdline".to_owned())?
                    .to_owned();
                let (tx, rx) = flume::unbounded();
                (
                    crate::RemoteRequest::Command {
                        session,
                        cmdline,
                        reply: tx,
                    },
                    rx,
                )
            }
            Route::Index
            | Route::Events
            | Route::Sessions
            | Route::ModelGet
            | Route::ModelPost
            | Route::Commands
            | Route::Options
            | Route::OptionsPost => {
                return Err("not a post route".to_owned());
            }
        };
        self.requests
            .try_send(request)
            .map_err(|_| "event loop wedged".to_owned())?;
        // The loop answers fast (it only flips state or forwards); still,
        // bound the wait so a wedged UI thread cannot leak connections.
        match reply_rx.recv_timeout(Duration::from_secs(REQUEST_REPLY_TIMEOUT_SECS)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => {
                tracing::debug!(%reason, "remote control: request rejected");
                Err(reason)
            }
            Err(_) => Err("event loop wedged".to_owned()),
        }
    }
}

fn required_str(value: &serde_json::Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("missing {field}"))
}

/// The SSE frame source, shared by both modes. Pulls the fan-out channel and
/// formats frames; standalone mode reads it via `Read`, the tunnel sends each
/// frame as a WS text message. With a session set, frames for other sessions
/// are dropped, so a scoped link never leaks its neighbours' traffic.
pub struct SseSource {
    subscription: crate::state::Subscription,
    session: Option<String>,
    idle_deadline: Option<std::time::Instant>,
    buf: Vec<u8>,
}

impl SseSource {
    pub fn new(
        subscription: crate::state::Subscription,
        snapshot: &serde_json::Value,
        session: Option<String>,
    ) -> Self {
        let mut buf = Vec::new();
        write_frame(&mut buf, "snapshot", snapshot);
        Self {
            subscription,
            session,
            idle_deadline: None,
            buf,
        }
    }

    /// Blocks until the next frame is fully available. `None` ends the stream.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        while self.buf.is_empty() {
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
            let Some(update) = update else { continue };
            self.idle_deadline =
                Some(std::time::Instant::now() + Duration::from_secs(SSE_PING_SECS));
            match update {
                RemoteUpdate::Shutdown => return None,
                update if self.mutes(&update) => {}
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
            }
        }
        Some(std::mem::take(&mut self.buf))
    }

    fn mutes(&self, update: &RemoteUpdate) -> bool {
        match (&self.session, update.session()) {
            (Some(watched), Some(session)) => watched != session,
            _ => false,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tail_splits_session_scope() {
        let (session, route) = parse_tail("s/abc/events", "GET");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(route, Some(Route::Events));

        let (session, route) = parse_tail("s/abc/", "GET");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(route, Some(Route::Index));

        let (session, route) = parse_tail("s/abc/prompt", "POST");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(route, Some(Route::Prompt));

        let (session, route) = parse_tail("events", "GET");
        assert_eq!(session, None);
        assert_eq!(route, Some(Route::Events));

        let (session, route) = parse_tail("s/abc/nope", "GET");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(route, None);
    }

    #[test]
    fn picker_routes_reach_the_dispatcher() {
        assert_eq!(Route::from_tail("commands", "GET"), Some(Route::Commands));
        assert_eq!(Route::from_tail("options", "GET"), Some(Route::Options));
        assert_eq!(
            Route::from_tail("options", "POST"),
            Some(Route::OptionsPost)
        );
        // A GET on the setter route is not a route at all.
        assert_eq!(Route::from_tail("options", "DELETE"), None);
    }

    #[test]
    fn scoped_source_drops_other_sessions() {
        let state = RemoteState::new();
        let sub = state.subscribe(None);
        let mut source = SseSource::new(sub, &json!({}), Some("watched".into()));
        state.send_status("other", "working");
        state.send_status("watched", "idle");
        // First frame is the snapshot; the next must be the watched session's.
        let snapshot = String::from_utf8(source.next_frame().unwrap()).unwrap();
        assert!(snapshot.contains("event: snapshot"));
        let frame = String::from_utf8(source.next_frame().unwrap()).unwrap();
        assert!(frame.contains("idle"), "frame: {frame}");
    }

    #[test]
    fn unscoped_source_keeps_every_session() {
        let state = RemoteState::new();
        let sub = state.subscribe(None);
        let mut source = SseSource::new(sub, &json!({}), None);
        state.send_status("any", "working");
        let snapshot = source.next_frame().unwrap();
        assert!(!snapshot.is_empty());
        let frame = String::from_utf8(source.next_frame().unwrap()).unwrap();
        assert!(frame.contains("working"), "frame: {frame}");
    }
}
