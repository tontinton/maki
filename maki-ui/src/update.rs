use std::sync::OnceLock;

use maki_storage::version;

static LATEST: OnceLock<String> = OnceLock::new();

pub use version::{CURRENT, is_newer};

pub fn latest_version() -> Option<&'static str> {
    LATEST.get().map(|s| s.as_str())
}

pub fn spawn_check() {
    smol::spawn(async {
        match version::fetch_latest_async().await {
            Ok(v) if is_newer(&v, CURRENT) => {
                let _ = LATEST.set(v);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "update check failed");
            }
        }
    })
    .detach();
}
