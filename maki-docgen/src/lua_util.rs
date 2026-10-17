use std::sync::{Arc, Mutex, MutexGuard};

use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;

static WINDOW: Mutex<()> = Mutex::new(());

fn window() -> MutexGuard<'static, ()> {
    WINDOW.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs a plugin load inside its own provider registration window.
///
/// Booting a host stages the providers its bundled plugins declare into one
/// registry the whole process shares, and docgen builds each page on its own
/// thread. Keeping a single window open at a time stops two hosts from staging
/// over each other and fighting for the same slug.
pub fn in_registration_window<T>(load: impl FnOnce() -> T) -> T {
    let _guard = window();
    maki_providers::plugin::begin_load();
    let loaded = load();
    maki_providers::plugin::commit_load();
    loaded
}

/// Like [`in_registration_window`], but the lock is held while `read` runs
/// too, so no other page swaps the providers out halfway through.
pub fn with_bundled_providers<T>(read: impl FnOnce() -> T) -> T {
    let _guard = window();
    maki_providers::plugin::begin_load();
    let _host =
        PluginHost::with_all_builtins(Arc::new(ToolRegistry::new())).expect("loading builtins");
    maki_providers::plugin::commit_load();
    read()
}
