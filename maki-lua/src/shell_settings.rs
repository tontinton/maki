use std::sync::LazyLock;

use arc_swap::ArcSwap;
use maki_config::ShellPreference;

static SHELL: LazyLock<ArcSwap<ShellPreference>> =
    LazyLock::new(|| ArcSwap::from_pointee(ShellPreference::Auto));

pub fn set_shell_preference(pref: ShellPreference) {
    SHELL.store(std::sync::Arc::new(pref));
}

pub(crate) fn shell_preference() -> ShellPreference {
    SHELL.load_full().as_ref().clone()
}
