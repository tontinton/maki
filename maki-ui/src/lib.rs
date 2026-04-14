//! Single-threaded ratatui event loop; the agent runs on smol tasks in a separate thread.
//! `AgentHandles` bundles all flume channels to the agent. `dispatch()` processes
//! `Action`s returned by `App::update()`. Scroll and drag events are coalesced from
//! the queue to avoid jank.

pub mod animation;
pub mod app;
pub mod chat;
mod components;
pub use components::command::{BUILTIN_COMMANDS, BuiltinCommand};
pub use components::keybindings;
mod highlight;
pub use highlight::highlight_ansi;
mod image;
mod markdown;
#[cfg(feature = "demo")]
mod mock;
mod render_worker;
mod selection;
pub mod splash;
mod storage_writer;
mod text_buffer;
mod theme;
pub mod update;

mod agent;
mod event_loop;
mod terminal;

use color_eyre::Result;
use maki_agent::ToolOutput;
use maki_providers::Message;
use maki_providers::TokenUsage;

pub type AppSession = maki_storage::sessions::Session<Message, TokenUsage, ToolOutput>;

pub(crate) use agent::AgentCommand;
pub use event_loop::EventLoopParams;

pub fn run(
    params: EventLoopParams,
    initial_prompt: Option<String>,
) -> Result<(Option<String>, i32)> {
    let (_guard, mut terminal) = terminal::TerminalGuard::init()?;
    let el = event_loop::EventLoop::new(&mut terminal, params)?;
    el.run(initial_prompt)
}
