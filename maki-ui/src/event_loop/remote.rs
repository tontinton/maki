//! Lifecycle owner of the remote control server inside the event loop.
//!
//! The server runs on a dedicated thread (tiny_http is synchronous); this
//! module holds the handle, services its requests on the loop thread, and
//! mirrors session state out through [`maki_remote::RemoteState`].

use std::sync::Arc;

use flume::Sender;
use maki_config::RemoteControlConfig;
use maki_remote::tunnel::{TunnelClient, run_tunnel};
use maki_remote::{RemoteRequest, RemoteServer};

use crate::app::App;
use crate::components::Action;

/// Remote requests handled per drain, so a flood cannot starve rendering.
const REQUEST_BUDGET: usize = 64;
const REMOTE_TUNNEL_STARTING: &str = "connecting to anchor...";

pub(crate) enum RemoteControl {
    Standalone {
        server: Arc<RemoteServer>,
    },
    Tunnel {
        state: maki_remote::RemoteState,
        link_rx: Option<flume::Receiver<String>>,
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
            let client_state = maki_remote::RemoteState::new();
            let client = TunnelClient::new(requests, token.clone(), name.clone());
            let anchor_url = url.clone();
            // The anchor mints the real share link during the handshake; it
            // lands on this channel and the loop flashes it as soon as it
            // arrives. Until then, tell the user the tunnel is coming up.
            let (link_tx, link_rx) = flume::bounded::<String>(1);
            std::thread::Builder::new()
                .name("remote-tunnel".into())
                .spawn(move || {
                    if let Err(e) = run_tunnel(&anchor_url, &token, client, link_tx) {
                        tracing::warn!(error = %e, "anchor tunnel ended");
                    }
                })
                .map_err(|e| color_eyre::eyre::eyre!("remote tunnel thread: {e}"))?;
            Ok((
                Self::Tunnel {
                    state: client_state,
                    link_rx: Some(link_rx),
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
    /// closes when the last `Arc<RemoteServer>` drops.
    fn stop(&self) {
        if let Self::Standalone { server } = self {
            server.shutdown();
        }
    }

    /// The anchor-minted link once the tunnel handshake completes.
    fn tunnel_link(&mut self) -> Option<String> {
        if let Self::Tunnel { link_rx, .. } = self {
            let link = link_rx.as_ref().and_then(|rx| rx.try_recv().ok());
            if link.is_some() {
                *link_rx = None;
            }
            link
        } else {
            None
        }
    }
}

fn validate_token_hex(token: &str) -> color_eyre::Result<()> {
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        color_eyre::eyre::bail!("anchor.token must be 32 hex chars from `maki-anchor tokens add`");
    }
    Ok(())
}

/// The event loop's per-generation remote control state. The server is `None`
/// while remote control is off; `/rc` fills it, `/rc` again replaces it.
pub(crate) struct RemoteSlot {
    control: Option<RemoteControl>,
    requests_rx: flume::Receiver<RemoteRequest>,
    requests_tx: flume::Sender<RemoteRequest>,
}

impl RemoteSlot {
    pub(crate) fn new() -> Self {
        let (tx, rx) = flume::unbounded();
        Self {
            control: None,
            requests_rx: rx,
            requests_tx: tx,
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

    /// The anchor-minted share link, once the tunnel handshake completes.
    pub(crate) fn tunnel_link(&mut self) -> Option<String> {
        self.control.as_mut().and_then(RemoteControl::tunnel_link)
    }

    /// Services pending requests from remote clients, returning the actions
    /// the focused app owes the loop. Called every loop iteration, right
    /// after draining agent events.
    #[allow(dead_code)]
    pub(crate) fn drain_requests(&self, app: &mut App) -> Vec<Action> {
        let snap = serde_json::json!({
            "sessions": [{
                "id": app.state.session.id.to_string(),
                "title": app.state.session.title.clone(),
                "cwd": app.state.session.cwd.clone(),
                "model": app.state.model.spec(),
                "status": if app.status == crate::components::Status::Streaming { "working" } else { "idle" },
                "focused": true,
            }],
            "focused": app.state.session.id.to_string(),
        });
        self.drain_requests_with(app, move || snap.clone())
    }

    pub(crate) fn drain_requests_with<F>(&self, app: &mut App, sessions_fn: F) -> Vec<Action>
    where
        F: Fn() -> serde_json::Value,
    {
        let mut actions = Vec::new();
        for _ in 0..REQUEST_BUDGET {
            let Ok(request) = self.requests_rx.try_recv() else {
                break;
            };
            let outcome: Result<Vec<Action>, String> = match request {
                RemoteRequest::Prompt { text, reply } => {
                    let outcome = app.submit_remote_prompt(text);
                    let _ = reply.send(outcome.as_ref().map(|_| ()).map_err(Clone::clone));
                    outcome
                }
                RemoteRequest::Answer {
                    request_id,
                    answer,
                    reply,
                } => {
                    let outcome = app.answer_remote_permission(&request_id, &answer);
                    let _ = reply.send(outcome);
                    Ok(vec![])
                }
                RemoteRequest::Stop { reply } => {
                    let outcome = app.stop_remote_run();
                    let _ = reply.send(outcome.as_ref().map(|_| ()).map_err(Clone::clone));
                    outcome
                }
                RemoteRequest::Command { cmdline, reply } => {
                    let outcome = app.run_remote_command(&cmdline);
                    let _ = reply.send(outcome.as_ref().map(|_| ()).map_err(Clone::clone));
                    outcome
                }
                RemoteRequest::Sessions { reply } => {
                    let value = sessions_fn();
                    let _ = reply.send(value);
                    continue;
                }
                RemoteRequest::ModelGet { reply } => {
                    let _ = reply.send(app.remote_model_get());
                    continue;
                }
                RemoteRequest::ModelSet {
                    spec,
                    thinking,
                    fast,
                    reply,
                } => {
                    let outcome = app.remote_model_set(spec.as_deref(), thinking.as_deref(), fast);
                    let _ = reply.send(outcome.clone().map_err(|e| e.clone()));
                    match outcome {
                        Ok(_) => Ok(vec![]),
                        Err(e) => Err(e),
                    }
                }
                RemoteRequest::Snapshot { reply } => {
                    let _ = reply.send(app.remote_snapshot());
                    continue;
                }
            };
            actions.extend(outcome.into_iter().flatten());
        }
        actions
    }
}
