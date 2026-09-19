//! Single-threaded ratatui event loop; the agent runs on smol tasks in a separate thread.
//! `AgentHandles` bundles all flume channels to the agent. `dispatch()` processes
//! `Action`s returned by `App::update()`. Scroll and drag events are coalesced from
//! the queue to avoid jank.

pub mod animation;
pub mod app;
pub mod chat;
mod clipboard;
mod clock;
mod color_compat;
mod components;
pub use components::command::{BUILTIN_COMMANDS, BuiltinCommand};
pub use components::keybindings;
mod highlight;
pub use highlight::highlight_ansi;
pub mod image;
mod markdown;
pub use markdown::text_to_lines;
mod render_worker;
pub mod repaint;
mod selection;
pub mod splash;
mod storage_writer;
mod text_buffer;
mod theme;
mod trust_card;
pub use theme::BUNDLED_THEMES;
pub use trust_card::ask_trust;
pub mod update;
pub mod wrap;

mod agent;
mod event_loop;
mod input;
mod terminal;
mod terminal_image;

use std::time::Instant;

use color_eyre::Result;
use maki_lua::PackPlan;
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_storage::sessions::{SAVE_FAILED, SessionClaim, SessionError};

/// The tabs keep their own name, but the type is the same one the drivers
/// persist, by construction rather than by coincidence.
pub use maki_agent::session::StoredSession as AppSession;

/// A session a tab has open, with the right to write it. Kept as one value so a
/// tab never gets a session without its claim, and the claim follows the
/// session across `/reload` and into every snapshot the storage writer queues.
pub struct OpenSession {
    pub session: AppSession,
    pub claim: SessionClaim,
}

impl OpenSession {
    pub fn fresh(model_spec: &str, cwd: &str, storage: &StateDir) -> Self {
        let claim = SessionClaim::fresh(storage);
        let mut session = AppSession::new(model_spec, cwd);
        session.id = claim.id();
        Self { session, claim }
    }

    pub fn load(id: MakiId, storage: &StateDir) -> Result<Self, SessionError> {
        let (session, claim) = AppSession::claim_and_load(id, storage)?;
        Ok(Self { session, claim })
    }
}

#[cfg(test)]
impl OpenSession {
    /// For sessions a test built by hand. Production code claims before it
    /// reads, see [`Self::load`].
    pub(crate) fn claimed(session: AppSession, storage: &StateDir) -> Self {
        let claim = SessionClaim::acquire(session.id, storage).expect("a session no test holds");
        Self { session, claim }
    }
}

/// Width of the controlling terminal, if any. Answers even when stdout is
/// redirected, so callers that care gate on [`std::io::IsTerminal`].
pub fn terminal_width() -> Option<u16> {
    crossterm::terminal::size().ok().map(|(w, _)| w)
}

pub use event_loop::EventLoopParams;

/// How a UI generation ended. On `Reload`, each tab carries its in-memory
/// session so the caller reopens everything without re-reading from disk, and
/// its claim, so no other process can take the session while the UI rebuilds.
pub enum RunOutcome {
    Exit {
        session_id: Option<MakiId>,
        code: i32,
    },
    Reload {
        tabs: Vec<OpenSession>,
        focused: usize,
        pack: Option<PackPlan>,
    },
}

pub fn run(params: EventLoopParams, initial_prompt: Option<String>) -> Result<RunOutcome> {
    let report = {
        let (_guard, mut terminal) = terminal::TerminalGuard::init()?;
        color_compat::init();
        let el = event_loop::EventLoop::new(&mut terminal, params)?;
        el.run(initial_prompt)?
    };
    // Nothing is going to ask for a file list after the last frame, and a walk
    // of a large tree has no business burning cores through teardown.
    maki_agent::cancel_walks();
    let event_loop::ShutdownReport {
        exit,
        tabs,
        focused,
        unsaved,
    } = report;
    for id in unsaved {
        eprintln!("{SAVE_FAILED}{id}");
    }
    Ok(match exit {
        components::ExitRequest::Reload => RunOutcome::Reload {
            tabs,
            focused,
            pack: None,
        },
        components::ExitRequest::Pack(request) => RunOutcome::Reload {
            tabs,
            focused,
            pack: Some(request),
        },
        exit => {
            let session_id = tabs
                .get(focused)
                .filter(|tab| app::session_has_content(&tab.session))
                .map(|tab| tab.session.id);
            let started = Instant::now();
            drop(tabs);
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "session buffers dropped"
            );
            RunOutcome::Exit {
                session_id,
                code: exit.code(),
            }
        }
    })
}
