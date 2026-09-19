//! Where each tool runs: host plugin execution vs sandboxed child execution.
//!
//! Parent side this crate owns the `code_execution` bridge; child side it
//! provides the default [`ChildWorkload`](maki_sandbox::ChildWorkload):
//! bash plus Lua-plugin filesystem tools inside the sandbox, with trusted
//! tools forwarded to the parent.
#![cfg(all(feature = "sandbox", target_os = "linux"))]

pub mod bridge;
mod child_lua;
mod workload;

pub use bridge::run_sandbox_with;
pub use workload::install_child_workload;
