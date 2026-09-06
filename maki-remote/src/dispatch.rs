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
    Center,
    Qr,
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
            ("center", "GET") => Some(Route::Center),
            ("qr", "GET") => Some(Route::Qr),
            _ => None,
        }
    }
}

/// Split the optional `s/<session-id>/` prefix a session-scoped link puts in
/// front of every route, so one page URL pins the whole tab of requests to
/// that session. Returns the session id and the route under it.
pub fn parse_tail(tail: &str, method: &str) -> (Option<String>, Option<Route>, String) {
    let (tail, query) = match tail.split_once('?') {
        Some((path, query)) => (path, query.to_owned()),
        None => (tail, String::new()),
    };
    let Some(rest) = tail.strip_prefix("s/") else {
        return (None, Route::from_tail(tail, method), query);
    };
    let (id, sub) = match rest.split_once('/') {
        Some((id, sub)) => (id, sub),
        None => (rest, ""),
    };
    (Some(id.to_owned()), Route::from_tail(sub, method), query)
}

const QR_TEXT_LIMIT: usize = 512;

/// Percent-decode one query value (`+` counts as a space, like forms).
fn url_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// QR is a convenience for share links, not a general-purpose render
/// service: the text must carry a 32-hex token segment.
fn is_link_text(text: &str) -> bool {
    let shaped =
        text.starts_with("http://") || text.starts_with("https://") || text.starts_with('/');
    shaped
        && text
            .split('/')
            .any(|seg| seg.len() == 32 && seg.bytes().all(|b| b.is_ascii_hexdigit()))
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
    /// A rendered QR, served as `image/svg+xml`.
    Svg(Vec<u8>),
    NotFound,
}

impl std::fmt::Debug for DispatchOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Index => f.write_str("Index"),
            Self::Events { .. } => f.write_str("Events"),
            Self::Posted(status, error) => {
                f.debug_tuple("Posted").field(status).field(error).finish()
            }
            Self::Json { status, .. } => f.debug_struct("Json").field("status", status).finish(),
            Self::Svg(body) => f.debug_struct("Svg").field("len", &body.len()).finish(),
            Self::NotFound => f.write_str("NotFound"),
        }
    }
}

impl Dispatcher {
    /// Match `/{token}[/{session}]/{tail}` and produce the response payload.
    /// Token check is the caller's job: both modes compare against their own
    /// secret.
    /// `viewer` tags the attached browser (`alice·control`, `anon·view`) so
    /// the status surfaces can say who is watching, not just how many.
    pub fn dispatch(
        &self,
        route: Option<Route>,
        session: Option<String>,
        query: &str,
        body: &str,
        viewer: &str,
    ) -> DispatchOutcome {
        let Some(route) = route else {
            return DispatchOutcome::NotFound;
        };
        match route {
            Route::Index => DispatchOutcome::Index,
            Route::Center => {
                let viewers = serde_json::json!(
                    self.state
                        .watchers()
                        .into_iter()
                        .map(|(session, tag)| serde_json::json!({"session": session, "tag": tag}))
                        .collect::<Vec<_>>()
                );
                let total = self.state.has_viewers();
                let body = serde_json::json!({ "viewers": viewers, "watching": total })
                    .to_string()
                    .into_bytes();
                DispatchOutcome::Json { status: 200, body }
            }
            Route::Qr => {
                // The text rides the query (`qr?text=...`); only share-link
                // shapes get rendered, so the endpoint cannot be pointed at
                // arbitrary payloads.
                let text = query_param(query, "text")
                    .map(url_decode)
                    .unwrap_or_default();
                if text.len() > QR_TEXT_LIMIT || !is_link_text(&text) {
                    DispatchOutcome::Json {
                        status: 400,
                        body: br#"{"error":"qr text must be a maki share link"}"#.to_vec(),
                    }
                } else {
                    match fast_qr::QRBuilder::new(text).build() {
                        Ok(code) => DispatchOutcome::Svg(
                            fast_qr::convert::svg::SvgBuilder::default()
                                .to_str(&code)
                                .into_bytes(),
                        ),
                        Err(_) => DispatchOutcome::Json {
                            status: 400,
                            body: br#"{"error":"text out of qr capacity"}"#.to_vec(),
                        },
                    }
                }
            }
            Route::Events => {
                let subscription = self.state.subscribe(session.clone(), viewer.to_owned());
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
                let value: serde_json::Value =
                    serde_json::from_str(body).map_err(|_| "invalid json".to_owned())?;
                let text = value
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_owned();
                let files: Vec<crate::RemoteFile> = value
                    .get("files")
                    .and_then(|f| serde_json::from_value(f.clone()).ok())
                    .unwrap_or_default();
                if text.trim().is_empty() && files.is_empty() {
                    return Err("missing text and files".to_owned());
                }
                let (tx, rx) = flume::unbounded();
                (
                    crate::RemoteRequest::Prompt {
                        session,
                        text,
                        files,
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
            | Route::OptionsPost
            | Route::Center
            | Route::Qr => {
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
                        // Keep the keepalive deadline rolling: a one-shot ping
                        // would leave the stream blocked on `recv()` forever, so
                        // a closed browser (broken pipe on the write, or a cancel
                        // frame in tunnel mode) would never be noticed and the
                        // subscriber would leak, inflating the viewer count.
                        self.idle_deadline =
                            Some(std::time::Instant::now() + Duration::from_secs(SSE_PING_SECS));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RemoteRequest, UploadMode};

    #[test]
    fn center_counts_attached_browsers_per_tab() {
        let state = RemoteState::new();
        let _tab = state.subscribe(Some("t1".into()), "t·view".into());
        let _any = state.subscribe(None, "t·view".into());
        let dispatcher = dispatcher_for(state);
        let DispatchOutcome::Json { status, body } =
            dispatcher.dispatch(Some(Route::Center), None, "", "", "alice·control")
        else {
            panic!("center answers json");
        };
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["watching"], true);
        assert_eq!(json["viewers"].as_array().unwrap().len(), 2);
        assert!(json["viewers"].to_string().contains("\"t1\""));
    }

    fn dispatcher_for(state: RemoteState) -> Dispatcher {
        let (tx, _rx) = flume::unbounded();
        Dispatcher {
            state,
            requests: tx,
        }
    }

    #[test]
    fn qr_renders_share_links_and_refuses_everything_else() {
        let token = "a".repeat(32);
        let dispatcher = dispatcher_for(RemoteState::new());
        let query = format!("text=http%3A%2F%2Fanchor.test%2F{token}%2F");
        let DispatchOutcome::Svg(svg) =
            dispatcher.dispatch(Some(Route::Qr), None, &query, "", "anon·view")
        else {
            panic!("a share link renders");
        };
        let svg = String::from_utf8(svg).unwrap();
        assert!(svg.starts_with("<svg"), "svg: {svg:.40}");
        let DispatchOutcome::Json { status, .. } = dispatcher.dispatch(
            Some(Route::Qr),
            None,
            "text=https://evil/whatever",
            "",
            "anon·view",
        ) else {
            panic!("off-shape text is refused");
        };
        assert_eq!(
            status, 400,
            "the qr endpoint is not a general-purpose encoder"
        );
        let DispatchOutcome::Json { status, .. } =
            dispatcher.dispatch(Some(Route::Qr), None, "", "", "anon·view")
        else {
            panic!("missing text is refused");
        };
        assert_eq!(status, 400);
    }

    #[test]
    fn prompt_uploads_reach_the_loop_as_files() {
        let (tx, rx) = flume::unbounded();
        let dispatcher = Dispatcher {
            state: RemoteState::new(),
            requests: tx,
        };
        let answerer = std::thread::spawn(move || {
            let RemoteRequest::Prompt {
                text, files, reply, ..
            } = rx.recv().unwrap()
            else {
                panic!("a prompt is expected")
            };
            assert_eq!(text, "look");
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].name, "shot.png");
            assert!(matches!(files[0].mode, UploadMode::Attach));
            let _ = reply.send(Ok(()));
        });
        let outcome = dispatcher.dispatch(
            Some(Route::Prompt),
            None,
            "",
            r#"{"text":"look","files":[{"name":"shot.png","media_type":"image/png","data":"AAA=","mode":"attach"}]}"#,
            "anon·control",
        );
        assert!(
            matches!(outcome, DispatchOutcome::Posted(200, None)),
            "{outcome:?}"
        );
        answerer.join().unwrap();
    }

    #[test]
    fn prompt_without_text_but_with_files_is_accepted() {
        let (tx, rx) = flume::unbounded();
        let dispatcher = Dispatcher {
            state: RemoteState::new(),
            requests: tx,
        };
        std::thread::spawn(move || {
            if let Ok(RemoteRequest::Prompt { reply, files, .. }) = rx.recv() {
                assert_eq!(files.len(), 1);
                let _ = reply.send(Ok(()));
            }
        });
        let outcome = dispatcher.dispatch(
            Some(Route::Prompt),
            None,
            "",
            r#"{"files":[{"name":"notes.md","data":"aGk=","mode":"save"}]}"#,
            "anon·control",
        );
        assert!(
            matches!(outcome, DispatchOutcome::Posted(200, None)),
            "{outcome:?}"
        );
    }

    #[test]
    fn parse_tail_keeps_the_query() {
        let (session, route, query) = parse_tail("qr?text=abc", "GET");
        assert!(session.is_none());
        assert!(matches!(route, Some(Route::Qr)));
        assert_eq!(query, "text=abc");
    }

    #[test]
    fn parse_tail_splits_session_scope() {
        let (session, route, _) = parse_tail("s/abc/events", "GET");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(route, Some(Route::Events));

        let (session, route, _) = parse_tail("s/abc/", "GET");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(route, Some(Route::Index));

        let (session, route, _) = parse_tail("s/abc/prompt", "POST");
        assert_eq!(session.as_deref(), Some("abc"));
        assert_eq!(route, Some(Route::Prompt));

        let (session, route, _) = parse_tail("events", "GET");
        assert_eq!(session, None);
        assert_eq!(route, Some(Route::Events));

        let (session, route, _) = parse_tail("s/abc/nope", "GET");
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
        let sub = state.subscribe(None, "t·view".into());
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
        let sub = state.subscribe(None, "t·view".into());
        let mut source = SseSource::new(sub, &json!({}), None);
        state.send_status("any", "working");
        let snapshot = source.next_frame().unwrap();
        assert!(!snapshot.is_empty());
        let frame = String::from_utf8(source.next_frame().unwrap()).unwrap();
        assert!(frame.contains("working"), "frame: {frame}");
    }
}
