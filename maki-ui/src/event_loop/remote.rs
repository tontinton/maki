//! Lifecycle owner of the remote control server inside the event loop.
//!
//! The server runs on a dedicated thread (tiny_http is synchronous); this
//! module holds the handle, services its requests on the loop thread, and
//! mirrors session state out through [`maki_remote::RemoteState`].

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use flume::Sender;
use maki_config::RemoteControlConfig;
use maki_remote::tunnel::{TunnelClient, TunnelOut, TunnelReport, run_tunnel};
use maki_remote::{RemoteRequest, RemoteServer};

use crate::app::App;
use crate::components::Action;

/// Remote requests handled per drain, so a flood cannot starve rendering.
const REQUEST_BUDGET: usize = 64;
const REMOTE_TUNNEL_STARTING: &str = "connecting to anchor...";
/// The most tunnel reports one frame can carry, so a reconnect storm cannot
/// starve the loop.
pub(crate) const REPORT_BUDGET: usize = 16;
const REMOTE_TUNNEL_STOPPED: &str = "remote control stopped";

pub(crate) enum RemoteControl {
    Standalone {
        server: Arc<RemoteServer>,
    },
    Tunnel {
        state: maki_remote::RemoteState,
        reports: flume::Receiver<TunnelReport>,
        out: std::sync::mpsc::Sender<TunnelOut>,
        shutdown: Arc<AtomicBool>,
        /// Set by the tunnel thread when it returns (gave up or was refused);
        /// reconnect attempts keep the thread alive.
        thread_done: Arc<AtomicBool>,
        /// Anchor origin; the handshake only returns the token, and a bare
        /// token is not a URL a user can open.
        base_url: String,
    },
}

impl RemoteControl {
    /// Binds the listener and spawns the serving thread, or dials the anchor
    /// and runs the tunnel thread. `anchor` wins when configured.
    fn start(
        config: &RemoteControlConfig,
        anchor: Option<&maki_config::AnchorConfig>,
        requests: Sender<RemoteRequest>,
    ) -> color_eyre::Result<(Self, String)> {
        if let Some(anchor) = anchor {
            let Some((url, name, token)) = anchor
                .complete()
                .map(|(u, n, t)| (u.to_owned(), n.to_owned(), t.to_owned()))
            else {
                color_eyre::eyre::bail!("anchor config needs url, name and token together");
            };
            validate_token_hex(&token)?;
            let client = TunnelClient::new(requests, token.clone(), name.clone());
            // The client keeps the tunnel's outbound sender (replies and
            // session-index pushes); the slot holds a clone, the receiver
            // moves into the tunnel thread with the client.
            let out = client.out();
            let state = client.state.clone();
            let anchor_url = url.clone();
            let shutdown = Arc::new(AtomicBool::new(false));
            // The anchor mints a real share link per registration and the
            // tunnel thread narrates drops and refusals; all of it lands on
            // this channel for the loop to flash. Until the first link
            // arrives, tell the user the tunnel is coming up.
            let (reports_tx, reports) = flume::bounded::<TunnelReport>(REPORT_BUDGET);
            let thread_shutdown = Arc::clone(&shutdown);
            let thread_done = Arc::new(AtomicBool::new(false));
            let spawn_done = Arc::clone(&thread_done);
            std::thread::Builder::new()
                .name("remote-tunnel".into())
                .spawn(move || {
                    run_tunnel(&anchor_url, &token, &client, reports_tx, &thread_shutdown);
                    spawn_done.store(true, Ordering::Relaxed);
                    tracing::debug!("anchor tunnel thread exited");
                })
                .map_err(|e| color_eyre::eyre::eyre!("remote tunnel thread: {e}"))?;
            Ok((
                Self::Tunnel {
                    state,
                    reports,
                    out,
                    shutdown,
                    thread_done,
                    base_url: url,
                },
                REMOTE_TUNNEL_STARTING.to_owned(),
            ))
        } else {
            let (server, url) = RemoteServer::bind(config, requests)?;
            let thread_server = Arc::clone(&server);
            std::thread::Builder::new()
                .name("remote-control".into())
                .spawn(move || thread_server.serve())
                .map_err(|e| color_eyre::eyre::eyre!("remote control thread: {e}"))?;
            Ok((Self::Standalone { server }, url))
        }
    }

    /// Unblocks the serving thread and closes SSE streams; the listener
    /// closes when the last `Arc<RemoteServer>` drops. A tunnel is stopped
    /// cooperatively: its reader wakes on the socket poll timeout, sees the
    /// flag, and says goodbye, so `/rc` again never leaves a stale tunnel
    /// attached and `/rc off` actually tears it down.
    fn stop(&self) {
        match self {
            Self::Standalone { server } => server.shutdown(),
            Self::Tunnel { shutdown, .. } => shutdown.store(true, Ordering::Relaxed),
        }
    }

    fn thread_done(&self) -> bool {
        match self {
            Self::Standalone { .. } => false,
            Self::Tunnel { thread_done, .. } => thread_done.load(Ordering::Relaxed),
        }
    }

    /// The openable URL for a handshake token: the anchor origin joined with
    /// the token it minted for this tunnel.
    fn full_link_url(&self, token: &str) -> String {
        match self {
            Self::Standalone { .. } => String::new(),
            Self::Tunnel { base_url, .. } => {
                format!("{}/{token}/", base_url.trim_end_matches('/'))
            }
        }
    }

    /// The tunnel thread's next lifecycle report, if any.
    fn poll(&mut self) -> Option<TunnelReport> {
        match self {
            Self::Standalone { .. } => None,
            Self::Tunnel { reports, .. } => reports.try_recv().ok(),
        }
    }
}

/// One step of the tunnel's life, already phrased for the user.
pub(crate) enum TunnelHappen {
    /// A share link arrived; `reconnected` distinguishes it from the first.
    Link { url: String, reconnected: bool },
    /// A lifecycle line worth flashing: link lost, anchor refused, thread gone.
    Notice(String),
}

fn validate_token_hex(token: &str) -> color_eyre::Result<()> {
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        color_eyre::eyre::bail!("anchor.token must be 32 hex chars from `maki-anchor tokens add`");
    }
    Ok(())
}

/// The event loop's per-generation remote control state. The server is `None`
/// while remote control is off; `/rc` fills it, `/rc off` clears it, and
/// `/rc` again replaces it.
pub(crate) struct RemoteSlot {
    control: Option<RemoteControl>,
    requests_rx: flume::Receiver<RemoteRequest>,
    requests_tx: flume::Sender<RemoteRequest>,
    /// Last session index shipped to the anchor, so unchanged lists cost a
    /// string compare instead of a tunnel frame.
    last_pushed_index: Option<String>,
    /// The tunnel's link is live (a Link report arrived, and no Lost/Refused
    /// since). Standalone mode is always live while the server exists.
    link_up: bool,
    /// Whether a link has flashed this session, to word reconnect notices.
    link_shown: bool,
}

impl RemoteSlot {
    pub(crate) fn new() -> Self {
        let (tx, rx) = flume::unbounded();
        Self {
            control: None,
            requests_rx: rx,
            requests_tx: tx,
            last_pushed_index: None,
            link_up: false,
            link_shown: false,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.control.is_some()
    }

    /// Starts (or restarts) the server. Returns the public URL to flash.
    /// A fresh token is minted every start.
    pub(crate) fn start(
        &mut self,
        config: &RemoteControlConfig,
        anchor: Option<&maki_config::AnchorConfig>,
    ) -> color_eyre::Result<String> {
        if let Some(old) = self.control.take() {
            old.stop();
        }
        self.last_pushed_index = None;
        self.link_up = false;
        self.link_shown = false;
        let (control, url) = RemoteControl::start(config, anchor, self.requests_tx.clone())?;
        self.control = Some(control);
        Ok(url)
    }

    /// Stops the server. Returns whether one was running.
    pub(crate) fn stop(&mut self) -> bool {
        let Some(old) = self.control.take() else {
            return false;
        };
        old.stop();
        true
    }

    /// The live fan-out hub, or nothing while remote control is off.
    pub(crate) fn state(&self) -> Option<maki_remote::RemoteState> {
        self.control.as_ref().map(|c| match c {
            RemoteControl::Standalone { server } => server.state().clone(),
            RemoteControl::Tunnel { state, .. } => state.clone(),
        })
    }

    /// Is the remote currently reachable by browsers?
    pub(crate) fn link_up(&self) -> bool {
        match &self.control {
            None => false,
            Some(RemoteControl::Standalone { .. }) => true,
            Some(RemoteControl::Tunnel { .. }) => self.link_up,
        }
    }

    /// Browsers attached to one tab, across whichever mode is running.
    pub(crate) fn viewers(&self, session: &str) -> usize {
        self.state()
            .map(|state| state.viewers(session))
            .unwrap_or(0)
    }

    /// One tunnel lifecycle event, drained from the tunnel thread's reports.
    /// A thread that exited without a final word is reported as stopped, and
    /// the slot is cleared so `/rc` state and the indicator agree.
    pub(crate) fn poll_tunnel(&mut self) -> Option<TunnelHappen> {
        let mut control = self.control.take()?;
        let happen = match control.poll() {
            Some(TunnelReport::Link(token)) => {
                self.link_up = true;
                let reconnected = std::mem::replace(&mut self.link_shown, true);
                Some(TunnelHappen::Link {
                    url: control.full_link_url(&token),
                    reconnected,
                })
            }
            Some(TunnelReport::Lost(message)) => {
                self.link_up = false;
                Some(TunnelHappen::Notice(message))
            }
            Some(TunnelReport::Refused(reason)) => {
                self.link_up = false;
                Some(TunnelHappen::Notice(format!(
                    "anchor refused the tunnel: {reason}"
                )))
            }
            None if control.thread_done() => {
                self.link_up = false;
                Some(TunnelHappen::Notice(REMOTE_TUNNEL_STOPPED.to_owned()))
            }
            None => None,
        };
        // The slot keeps its control while the thread lives; a dead thread is
        // no remote control, whatever its last report said.
        if !control.thread_done() {
            self.control = Some(control);
        }
        happen
    }

    /// Ship the session index to the anchor when it changed. Only tunnel mode
    /// has an anchor to tell; standalone mode answers `/sessions` live.
    pub(crate) fn push_session_index(&mut self, index: &serde_json::Value) {
        let Some(RemoteControl::Tunnel { out, .. }) = &self.control else {
            return;
        };
        let text = index.to_string();
        if self.last_pushed_index.as_deref() == Some(text.as_str()) {
            return;
        }
        if out
            .send(TunnelOut::Push(serde_json::json!({ "sessions": index })))
            .is_ok()
        {
            self.last_pushed_index = Some(text);
        }
    }

    /// Service queued remote requests, returning the actions owed per
    /// session index. Unscoped requests target `focused`; an `s/<id>/`-scoped
    /// request lands on that tab or is rejected as not live.
    pub(crate) fn drain_requests_with<F>(
        &self,
        sessions: &mut [super::SessionRuntime],
        focused: usize,
        sessions_fn: F,
    ) -> Vec<(usize, Vec<Action>)>
    where
        F: Fn() -> serde_json::Value,
    {
        let mut grouped: Vec<(usize, Vec<Action>)> = Vec::new();
        for _ in 0..REQUEST_BUDGET {
            let Ok(request) = self.requests_rx.try_recv() else {
                break;
            };
            let idx = match request.session() {
                None => Some(focused),
                Some(id) => super::parse_session_id(id)
                    .ok()
                    .and_then(|id| sessions.iter().position(|rt| rt.id() == id)),
            };
            let Some(idx) = idx else {
                reject(request, "session not live");
                continue;
            };
            let actions = handle_request(&mut sessions[idx].app, request, &sessions_fn());
            match grouped.iter_mut().find(|(i, _)| *i == idx) {
                Some(entry) => entry.1.extend(actions),
                None if !actions.is_empty() => grouped.push((idx, actions)),
                None => {}
            }
        }
        grouped
    }
}

/// Run one request against its resolved app; replies travel back to the HTTP
/// handler, and any actions the request owes come out for the loop to
/// dispatch.
fn handle_request(
    app: &mut App,
    request: RemoteRequest,
    sessions_value: &serde_json::Value,
) -> Vec<Action> {
    let outcome: Result<Vec<Action>, String> = match request {
        RemoteRequest::Prompt { text, reply, .. } => {
            let outcome = app.submit_remote_prompt(text);
            let _ = reply.send(outcome.as_ref().map(|_| ()).map_err(Clone::clone));
            outcome
        }
        RemoteRequest::Answer {
            request_id,
            answer,
            reply,
            ..
        } => {
            let outcome = app.answer_remote_permission(&request_id, &answer);
            let _ = reply.send(outcome);
            Ok(vec![])
        }
        RemoteRequest::Stop { reply, .. } => {
            let outcome = app.stop_remote_run();
            let _ = reply.send(outcome.as_ref().map(|_| ()).map_err(Clone::clone));
            outcome
        }
        RemoteRequest::Command { cmdline, reply, .. } => {
            let outcome = app.run_remote_command(&cmdline);
            let _ = reply.send(outcome.as_ref().map(|_| ()).map_err(Clone::clone));
            outcome
        }
        RemoteRequest::Sessions { reply } => {
            let _ = reply.send(sessions_value.clone());
            Ok(vec![])
        }
        RemoteRequest::ModelGet { reply, .. } => {
            let _ = reply.send(app.remote_model_get());
            Ok(vec![])
        }
        RemoteRequest::ModelSet {
            spec,
            thinking,
            fast,
            reply,
            ..
        } => {
            let outcome = app.remote_model_set(spec.as_deref(), thinking.as_deref(), fast);
            let _ = reply.send(outcome.clone());
            outcome.map(|_| vec![])
        }
        RemoteRequest::Snapshot { reply, .. } => {
            let _ = reply.send(app.remote_snapshot());
            Ok(vec![])
        }
        RemoteRequest::Commands { reply, .. } => {
            let _ = reply.send(app.remote_commands());
            Ok(vec![])
        }
        RemoteRequest::Options { reply, .. } => {
            let _ = reply.send(app.remote_options());
            Ok(vec![])
        }
        RemoteRequest::SetOptions {
            yolo, mode, reply, ..
        } => {
            let mut options = None;
            if let Some(on) = yolo {
                options = Some(app.remote_set_yolo(on));
            }
            if let Some(mode) = mode.as_deref()
                && let Ok(o) = app.remote_set_mode(mode)
            {
                options = Some(o);
            }
            let value = options.unwrap_or_else(|| app.remote_options());
            let _ = reply.send(value);
            Ok(vec![])
        }
    };
    outcome.unwrap_or_default()
}

fn reject(request: RemoteRequest, reason: &str) {
    match request {
        RemoteRequest::Prompt { reply, .. }
        | RemoteRequest::Answer { reply, .. }
        | RemoteRequest::Stop { reply, .. }
        | RemoteRequest::Command { reply, .. } => {
            let _ = reply.send(Err(reason.to_owned()));
        }
        RemoteRequest::ModelSet { reply, .. } => {
            let _ = reply.send(Err(reason.to_owned()));
        }
        RemoteRequest::Sessions { reply }
        | RemoteRequest::ModelGet { reply, .. }
        | RemoteRequest::Snapshot { reply, .. }
        | RemoteRequest::Commands { reply, .. }
        | RemoteRequest::Options { reply, .. }
        | RemoteRequest::SetOptions { reply, .. } => {
            let _ = reply.send(serde_json::json!({"error": reason}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_reply() -> (RemoteRequest, flume::Receiver<Result<(), String>>) {
        let (tx, rx) = flume::unbounded();
        (
            RemoteRequest::Prompt {
                session: Some("dead".into()),
                text: "hi".into(),
                reply: tx,
            },
            rx,
        )
    }

    #[test]
    fn reject_reports_to_a_prompt_reply_channel() {
        let (request, rx) = prompt_reply();
        reject(request, "session not live");
        assert_eq!(rx.recv().unwrap().unwrap_err(), "session not live");
    }

    #[test]
    fn reject_reports_to_a_value_reply_channel() {
        let (tx, rx) = flume::unbounded();
        reject(
            RemoteRequest::Snapshot {
                session: None,
                reply: tx,
            },
            "gone",
        );
        assert_eq!(rx.recv().unwrap()["error"], "gone");
    }

    #[test]
    fn scoped_request_for_a_dead_session_is_rejected_and_drains() {
        // Empty runtime list: the resolver finds no session for the scoped id,
        // so the request must be rejected rather than hit index 0.
        let slot = RemoteSlot::new();
        let (tx, rx) = flume::unbounded();
        slot.requests_tx
            .send(RemoteRequest::Stop {
                session: Some("nope".into()),
                reply: tx,
            })
            .unwrap();
        let grouped = slot.drain_requests_with(&mut [], 0, || serde_json::json!({}));
        assert!(grouped.is_empty());
        assert_eq!(rx.recv().unwrap().unwrap_err(), "session not live");
    }

    #[test]
    fn stop_on_a_fresh_slot_reports_nothing_was_running() {
        assert!(!RemoteSlot::new().stop());
    }

    fn tunnel_slot() -> (RemoteSlot, flume::Sender<TunnelReport>, Arc<AtomicBool>) {
        let (tx, reports) = flume::bounded(4);
        let (out, _sink) = std::sync::mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut slot = RemoteSlot::new();
        slot.control = Some(RemoteControl::Tunnel {
            state: maki_remote::RemoteState::new(),
            reports,
            out,
            shutdown: Arc::clone(&shutdown),
            thread_done: Arc::new(AtomicBool::new(false)),
            base_url: "https://maki.example.com/".into(),
        });
        (slot, tx, shutdown)
    }

    #[test]
    fn tunnel_stop_flips_the_shutdown_flag() {
        let (mut slot, _tx, shutdown) = tunnel_slot();
        let control = slot.control.take().unwrap();
        control.stop();
        assert!(shutdown.load(Ordering::Relaxed), "tunnel must observe stop");
    }

    #[test]
    fn link_reports_become_a_full_url_and_track_the_state() {
        let (mut slot, tx, _shutdown) = tunnel_slot();
        tx.send(TunnelReport::Link("a".repeat(32))).unwrap();
        let TunnelHappen::Link { url, reconnected } = slot.poll_tunnel().expect("link") else {
            panic!("expected a link report");
        };
        assert_eq!(url, format!("https://maki.example.com/{}/", "a".repeat(32)));
        assert!(!reconnected, "the first link is not a reconnect");
        assert!(slot.link_up());
        tx.send(TunnelReport::Lost("anchor closed the link".into()))
            .unwrap();
        let TunnelHappen::Notice(message) = slot.poll_tunnel().expect("notice") else {
            panic!("expected a notice");
        };
        assert_eq!(message, "anchor closed the link");
        assert!(!slot.link_up(), "a lost link must dim the indicator");
        tx.send(TunnelReport::Link("b".repeat(32))).unwrap();
        let TunnelHappen::Link { url, reconnected } = slot.poll_tunnel().expect("link 2") else {
            panic!("expected a link report");
        };
        assert!(reconnected, "a link after a drop reads as a reconnect");
        assert!(url.starts_with("https://maki.example.com/"), "{url}");
    }

    #[test]
    fn a_refused_tunnel_stops_without_retry_and_clears_the_slot() {
        let (mut slot, tx, _shutdown) = tunnel_slot();
        tx.send(TunnelReport::Refused(
            "token or instance name rejected by the anchor".into(),
        ))
        .unwrap();
        let TunnelHappen::Notice(message) = slot.poll_tunnel().expect("refusal") else {
            panic!("expected a notice");
        };
        assert!(message.contains("refused"), "{message}");
        assert!(!slot.link_up());
        drop(tx);
        let Some(RemoteControl::Tunnel { thread_done, .. }) = &slot.control else {
            panic!("tunnel control expected");
        };
        thread_done.store(true, Ordering::Relaxed);
        let TunnelHappen::Notice(message) = slot.poll_tunnel().expect("thread gone") else {
            panic!("expected the stop notice");
        };
        assert_eq!(message, REMOTE_TUNNEL_STOPPED);
        assert!(!slot.is_running(), "a dead tunnel must clear the slot");
    }

    #[test]
    fn handle_request_reports_a_prompt_accepted_like_a_local_submit() {
        let mut app = crate::app::tests::test_app();
        let (tx, rx) = flume::unbounded();
        let actions = handle_request(
            &mut app,
            RemoteRequest::Prompt {
                session: None,
                text: "hello web".into(),
                reply: tx,
            },
            &serde_json::json!({}),
        );
        assert_eq!(rx.recv().unwrap(), Ok(()));
        assert!(!actions.is_empty(), "starting a run owes the loop actions");
    }

    #[test]
    fn handle_request_reports_the_rejection_reason() {
        let mut app = crate::app::tests::test_app();
        let (tx, rx) = flume::unbounded();
        let actions = handle_request(
            &mut app,
            RemoteRequest::Stop {
                session: None,
                reply: tx,
            },
            &serde_json::json!({}),
        );
        assert_eq!(rx.recv().unwrap().unwrap_err(), "no run is active");
        assert!(actions.is_empty());
    }

    #[test]
    fn handle_request_answers_snapshot_and_model_from_the_target_app() {
        let mut app = crate::app::tests::test_app();
        let (tx, rx) = flume::unbounded();
        handle_request(
            &mut app,
            RemoteRequest::Snapshot {
                session: None,
                reply: tx,
            },
            &serde_json::json!({}),
        );
        let snapshot = rx.recv().unwrap();
        assert!(snapshot["session_id"].is_string(), "snapshot: {snapshot}");
        assert!(snapshot["messages"].is_array());

        let (tx, rx) = flume::unbounded();
        handle_request(
            &mut app,
            RemoteRequest::ModelGet {
                session: None,
                reply: tx,
            },
            &serde_json::json!({}),
        );
        assert!(rx.recv().unwrap()["spec"].is_string());
    }

    #[test]
    fn sessions_request_answers_with_the_provided_table() {
        let mut app = crate::app::tests::test_app();
        let (tx, rx) = flume::unbounded();
        let table = serde_json::json!({"sessions": [], "focused": "x"});
        handle_request(&mut app, RemoteRequest::Sessions { reply: tx }, &table);
        assert_eq!(rx.recv().unwrap(), table);
    }
}
