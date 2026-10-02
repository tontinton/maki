#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use maki_sandbox::Sandbox;
use maki_sandbox::namespace::NamespaceConfig;

/// Build a sandbox over `root`, or report the host as unable to isolate and
/// return `None` so the test passes without asserting anything.
///
/// Only [`maki_sandbox::SandboxError::IsolationUnavailable`] skips. Every other
/// failure is a real bug in the sandbox, so it panics rather than hiding behind
/// a green test.
pub fn sandbox_for(root: &Path) -> Option<Arc<Sandbox>> {
    let config = NamespaceConfig::new(
        vec![],
        vec![],
        root.to_path_buf(),
        "test".into(),
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
    );
    match Sandbox::new(config) {
        Ok(sandbox) => Some(sandbox),
        Err(e) if e.is_isolation_unavailable() => {
            eprintln!("skipping: this host cannot isolate processes: {e}");
            None
        }
        Err(e) => panic!("Sandbox::new failed for a reason that is not host support: {e}"),
    }
}
