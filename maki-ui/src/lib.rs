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
pub use theme::BUNDLED_THEMES;
pub mod update;
pub mod wrap;

mod agent;
mod event_loop;
mod input;
mod terminal;

use std::time::Instant;

use color_eyre::Result;
use maki_agent::ToolOutput;
use maki_providers::Message;
use maki_providers::TokenUsage;
use maki_storage::id::MakiId;

pub type AppSession = maki_storage::sessions::Session<Message, TokenUsage, ToolOutput>;

pub(crate) use agent::AgentCommand;
pub use components::PackRequest;
pub use event_loop::EventLoopParams;

/// How a UI generation ended. On `Reload`, each tab carries its in-memory
/// session so the caller reopens everything without re-reading from disk.
pub enum RunOutcome {
    Exit {
        session_id: Option<MakiId>,
        code: i32,
    },
    Reload {
        tabs: Vec<AppSession>,
        focused: usize,
        pack: Option<PackRequest>,
    },
}

pub fn run(params: EventLoopParams, initial_prompt: Option<String>) -> Result<RunOutcome> {
    let report = {
        let (_guard, mut terminal) = terminal::TerminalGuard::init()?;
        color_compat::init();
        let el = event_loop::EventLoop::new(&mut terminal, params)?;
        el.run(initial_prompt)?
    };
    let event_loop::ShutdownReport {
        exit,
        tabs,
        focused,
    } = report;
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
                .filter(|s| app::session_has_content(s))
                .map(|s| s.id);
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
