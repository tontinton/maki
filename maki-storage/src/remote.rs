use std::fs;

use tracing::warn;

use crate::StateDir;

const REMOTE_ALWAYS_FILE: &str = "remote_always";

/// `/rc always` lives outside the lua config so the switch is per user,
/// set from the TUI, and survives restarts without editing init.lua.
pub fn persist_always(dir: &StateDir, on: bool) {
    if let Err(e) = fs::write(
        dir.path().join(REMOTE_ALWAYS_FILE),
        if on { "true" } else { "false" },
    ) {
        warn!(error = %e, "failed to persist remote always flag");
    }
}

pub fn read_always(dir: &StateDir) -> Option<bool> {
    match fs::read_to_string(dir.path().join(REMOTE_ALWAYS_FILE))
        .ok()?
        .trim()
    {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn always_round_trips() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        assert_eq!(read_always(&dir), None);
        persist_always(&dir, true);
        assert_eq!(read_always(&dir), Some(true));
        persist_always(&dir, false);
        assert_eq!(read_always(&dir), Some(false));
    }
}
