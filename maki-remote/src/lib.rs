//! Remote control: token-authed HTTP endpoint mirroring a live maki session.
//!
//! The wire protocol is a narrow projection of the agent's own event stream:
//! [`RemoteState`] fans out [`maki_agent::Envelope`] JSON plus a few synthetic
//! status frames, and every mutating endpoint lands on the same channel the
//! TUI already services, so remote actions behave like local ones.

mod dispatch;
mod server;
mod state;
pub mod tunnel;

pub use dispatch::Route;
pub use server::{RemoteRequest, RemoteServer};
pub use state::{PermissionFrame, RemoteState, RemoteUpdate};
pub use tunnel::{TunnelClient, TunnelError, run_tunnel};

use thiserror::Error;

/// Port an maki instance listens on while remote control is active.
pub const REMOTE_CONTROL_PORT_DOC: &str = "8687";

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("failed to bind {bind}:{port}: {source}")]
    Bind {
        bind: String,
        port: u16,
        source: std::io::Error,
    },
}
