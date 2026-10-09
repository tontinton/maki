use std::fs;

use tracing::warn;

use crate::{StateDir, atomic_write};

const VIM_MODE_FILE: &str = "vim-mode";

/// What `/vim` last set. It outlives the run, so the next start comes back in
/// the mode the user left, whatever `ui.vim_mode` says.
pub fn persist_vim_mode(dir: &StateDir, enabled: bool) {
    let path = dir.path().join(VIM_MODE_FILE);
    if let Err(e) = atomic_write(&path, enabled.to_string().as_bytes()) {
        warn!(error = %e, path = %path.display(), "failed to persist vim mode");
    }
}

/// `None` until `/vim` has run once, or when the file holds anything else, so
/// a hand-edited value falls back to the config.
pub fn read_vim_mode(dir: &StateDir) -> Option<bool> {
    fs::read_to_string(dir.path().join(VIM_MODE_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_case::test_case;

    #[test]
    fn vim_mode_round_trip() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        assert_eq!(read_vim_mode(&dir), None);

        persist_vim_mode(&dir, true);
        assert_eq!(read_vim_mode(&dir), Some(true));

        persist_vim_mode(&dir, false);
        assert_eq!(read_vim_mode(&dir), Some(false));
    }

    #[test_case("" ; "empty")]
    #[test_case("yes" ; "not_a_bool")]
    fn unreadable_vim_mode_is_none(content: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        fs::write(dir.path().join(VIM_MODE_FILE), content).unwrap();
        assert_eq!(read_vim_mode(&dir), None);
    }
}
