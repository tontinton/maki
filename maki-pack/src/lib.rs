//! Durable state for packages maki installs itself.
//!
//! This crate owns what is installed, at which commit, from which source, and
//! what the user approved. It deliberately knows nothing about Lua: the runtime
//! owns which packages a session has loaded, and the two never hold a second
//! opinion about each other's facts.

pub mod approvals;
pub mod git;
pub mod lock;
pub mod lockfile;
pub mod manager;
pub mod paths;
mod spec;
mod version;

pub use spec::{Spec, derive_name, name_is_safe};
pub use version::Version;
