use std::io::Write;
use std::process::{Command, Stdio};

use crate::app::image_paste::spawn_read;
use crate::terminal;
use arboard::Clipboard;
#[cfg(target_os = "linux")]
use arboard::{GetExtLinux, LinuxClipboardKind, SetExtLinux};
use flume::Receiver;
use shell_words::split;

pub(crate) enum CopyResult {
    Copied,
    Noop,
    /// The custom command runs off the UI thread; the result arrives here.
    Pending(Receiver<Result<(), String>>),
}

fn copy_command(cmd: &str, text: &str) -> Result<(), String> {
    let words = split(cmd).map_err(|e| e.to_string())?;
    let (program, args) = words.split_first().ok_or("empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("{program}: {e}"))?;
    }

    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    match stderr.trim() {
        "" => Err(format!("{program} exited with {}", output.status)),
        detail => Err(format!("{program} exited with {}: {detail}", output.status)),
    }
}

/// Reads the PRIMARY selection, the one middle-click pastes. Only Linux has
/// one, and on Wayland only when the compositor offers it. Blocks until the
/// app owning the selection answers, so never call it on the UI thread.
#[cfg(target_os = "linux")]
pub(crate) fn read_primary_selection() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    let text = clipboard
        .get()
        .clipboard(LinuxClipboardKind::Primary)
        .text()
        .ok()?;
    (!text.is_empty()).then_some(text)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_primary_selection() -> Option<String> {
    None
}

pub(crate) struct ClipboardState {
    native: Option<Clipboard>,
    command: Option<String>,
}

impl ClipboardState {
    pub(crate) fn new(command: Option<String>) -> Self {
        let command = command.filter(|c| !c.trim().is_empty());
        Self {
            native: command.is_none().then(|| Clipboard::new().ok()).flatten(),
            command,
        }
    }

    /// A configured command is the only way to copy: falling back would report
    /// success for backends that may silently drop the text.
    pub(crate) fn copy_text(&mut self, text: &str) -> Result<CopyResult, String> {
        match &self.command {
            Some(_) if text.is_empty() => Ok(CopyResult::Noop),
            Some(command) => {
                let (command, text) = (command.clone(), text.to_owned());
                Ok(CopyResult::Pending(spawn_read(move || {
                    copy_command(&command, &text)
                })))
            }
            None => {
                Self::copy_text_impl(text, |t| self.copy_native(t), terminal::copy_to_clipboard)
            }
        }
    }

    fn copy_text_impl<F, G>(text: &str, native: F, osc52: G) -> Result<CopyResult, String>
    where
        F: FnOnce(&str) -> Result<(), String>,
        G: FnOnce(&str) -> Result<(), String>,
    {
        if text.is_empty() {
            return Ok(CopyResult::Noop);
        }
        let native_err = match native(text) {
            Ok(()) => return Ok(CopyResult::Copied),
            Err(e) => e,
        };
        match osc52(text) {
            Ok(()) => Ok(CopyResult::Copied),
            Err(osc52_err) => Err(format!("native ({native_err}); osc52 ({osc52_err})")),
        }
    }

    fn copy_native(&mut self, text: &str) -> Result<(), String> {
        let Some(clipboard) = &mut self.native else {
            return Err("native clipboard unavailable".into());
        };

        #[cfg(target_os = "linux")]
        {
            let primary = clipboard
                .set()
                .clipboard(LinuxClipboardKind::Primary)
                .text(text);
            let system = clipboard
                .set()
                .clipboard(LinuxClipboardKind::Clipboard)
                .text(text);
            match (primary, system) {
                (_, Ok(())) | (Ok(()), _) => Ok(()),
                (Err(p), Err(s)) => Err(format!("primary: {p}; clipboard: {s}")),
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            clipboard.set_text(text).map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;
    use test_case::test_case;

    const DUMMY_TEXT: &str = "hello";
    const RECV_TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn empty_text_is_noop_and_skips_both_backends() {
        let native_called = Cell::new(false);
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            "",
            |_| {
                native_called.set(true);
                Ok(())
            },
            |_| {
                osc52_called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Noop)));
        assert!(!native_called.get(), "empty text must not reach native");
        assert!(!osc52_called.get(), "empty text must not reach osc52");
    }

    #[test]
    fn native_success_skips_osc52() {
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            "hello",
            |_| Ok(()),
            |_| {
                osc52_called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(
            !osc52_called.get(),
            "osc52 must not fire after native succeeds"
        );
    }

    #[test]
    fn native_failure_is_recovered_by_osc52() {
        let result =
            ClipboardState::copy_text_impl("hello", |_| Err("unavailable".into()), |_| Ok(()));
        assert!(matches!(result, Ok(CopyResult::Copied)));
    }

    #[test]
    fn both_failing_composes_errors_with_native_first() {
        let result = ClipboardState::copy_text_impl(
            "hello",
            |_| Err("no display".into()),
            |_| Err("stdout closed".into()),
        );
        let Err(msg) = result else {
            panic!("expected Err when both backends fail");
        };
        assert_eq!(msg, "native (no display); osc52 (stdout closed)");
    }

    #[test]
    fn whitespace_only_text_is_not_noop() {
        let native_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            "   \n\t",
            |_| {
                native_called.set(true);
                Ok(())
            },
            |_| Ok(()),
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(
            native_called.get(),
            "whitespace-only text must still be copied"
        );
    }

    #[test_case("true" => Ok(()) ; "successful_command")]
    #[test_case("false" => matches Err(_) ; "failing_command")]
    #[test_case("" => matches Err(_) ; "empty_command")]
    #[test_case("maki-no-such-clipboard-binary" => matches Err(_) ; "missing_program")]
    fn copy_command_execution(cmd: &str) -> Result<(), String> {
        copy_command(cmd, DUMMY_TEXT)
    }

    #[test]
    fn copy_command_pipes_text_to_stdin() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path string");
        copy_command(&format!("sh -c 'cat > {path}'"), DUMMY_TEXT).expect("copy_command");
        assert_eq!(std::fs::read_to_string(path).expect("read"), DUMMY_TEXT);
    }

    #[test]
    fn copy_command_error_includes_stderr() {
        let err = copy_command("sh -c 'echo boom >&2; exit 3'", DUMMY_TEXT).unwrap_err();
        assert!(err.contains("boom"), "{err}");
    }

    #[test_case("true", true ; "success_is_reported")]
    #[test_case("false", false ; "failure_is_reported")]
    fn configured_command_runs_off_thread_and_reports(cmd: &str, ok: bool) {
        let mut state = ClipboardState::new(Some(cmd.into()));
        let Ok(CopyResult::Pending(rx)) = state.copy_text(DUMMY_TEXT) else {
            panic!("a configured command must run asynchronously");
        };
        assert_eq!(rx.recv_timeout(RECV_TIMEOUT).expect("result").is_ok(), ok);
    }

    #[test]
    fn configured_command_with_empty_text_is_noop() {
        let mut state = ClipboardState::new(Some("true".into()));
        assert!(matches!(state.copy_text(""), Ok(CopyResult::Noop)));
    }

    #[test]
    fn blank_command_is_treated_as_unset() {
        assert!(ClipboardState::new(Some("  ".into())).command.is_none());
    }
}
