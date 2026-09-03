//! Lifecycle owner of the remote control server inside the event loop.
//!
//! The server runs on a dedicated thread (tiny_http is synchronous); this
//! module holds the handle, services its requests on the loop thread, and
//! mirrors session state out through [`maki_remote::RemoteState`].

use std::sync::Arc;

use flume::Sender;
use maki_config::RemoteControlConfig;
use maki_remote::{RemoteRequest, RemoteServer};

use crate::app::App;
use crate::components::Action;

/// Remote requests handled per drain, so a flood cannot starve rendering.
const REQUEST_BUDGET: usize = 64;

pub(crate) struct RemoteControl {
    server: Arc<RemoteServer>,
}

impl RemoteControl {
    /// Binds the listener and spawns the serving thread.
    fn start(
        config: &RemoteControlConfig,
        requests: Sender<RemoteRequest>,
    ) -> color_eyre::Result<(Self, String)> {
        let (server, url) = RemoteServer::bind(config, requests)?;
        let thread_server = Arc::clone(&server);
        std::thread::Builder::new()
            .name("remote-control".into())
            .spawn(move || thread_server.serve())
            .map_err(|e| color_eyre::eyre::eyre!("remote control thread: {e}"))?;
        Ok((Self { server }, url))
    }

    /// Unblocks the serving thread and closes SSE streams; the listener
    /// closes when the last `Arc<RemoteServer>` drops.
    fn stop(&self) {
        self.server.shutdown();
    }
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
    pub(crate) fn start(&mut self, config: &RemoteControlConfig) -> color_eyre::Result<String> {
        if let Some(old) = self.control.take() {
            old.stop();
        }
        let (control, url) = RemoteControl::start(config, self.requests_tx.clone())?;
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
        self.control.as_ref().map(|c| c.server.state().clone())
    }

    /// Services pending requests from remote clients, returning the actions
    /// the focused app owes the loop. Called every loop iteration, right
    /// after draining agent events.
    pub(crate) fn drain_requests(&self, app: &mut App) -> Vec<Action> {
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
