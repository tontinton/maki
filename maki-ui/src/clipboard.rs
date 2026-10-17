use std::env;
use std::io::Write;
use std::process::{Command, Stdio};

use arboard::Clipboard;
#[cfg(target_os = "linux")]
use arboard::{GetExtLinux, LinuxClipboardKind, SetExtLinux};
use shell_words::split;

use crate::terminal;

const CLIPBOARD_CMD_ENV: &str = "MAKI_CLIPBOARD_CMD";

pub(crate) enum CopyResult {
    Copied,
    Noop,
}

fn copy_command(cmd: &str, text: &str) -> Result<(), String> {
    let words = split(cmd).map_err(|e| e.to_string())?;
    let (program, args) = words.split_first().ok_or("empty command")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
        let _ = stdin.flush();
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
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
}

impl ClipboardState {
    pub(crate) fn new() -> Self {
        Self {
            native: Clipboard::new().ok(),
        }
    }

    pub(crate) fn copy_text(&mut self, text: &str) -> Result<CopyResult, String> {
        let env_cmd = env::var(CLIPBOARD_CMD_ENV).ok();
        let custom_cmd = env_cmd.as_deref().map(str::trim).filter(|s| !s.is_empty());
        Self::copy_text_impl(
            text,
            custom_cmd,
            copy_command,
            |t| self.copy_native(t),
            terminal::copy_to_clipboard,
        )
    }

    fn copy_text_impl<C, F, G>(
        text: &str,
        custom_cmd: Option<&str>,
        command: C,
        native: F,
        osc52: G,
    ) -> Result<CopyResult, String>
    where
        C: FnOnce(&str, &str) -> Result<(), String>,
        F: FnOnce(&str) -> Result<(), String>,
        G: FnOnce(&str) -> Result<(), String>,
    {
        if text.is_empty() {
            return Ok(CopyResult::Noop);
        }
        let cmd_err = match custom_cmd {
            Some(cmd) => match command(cmd, text) {
                Ok(()) => return Ok(CopyResult::Copied),
                Err(e) => Some(e),
            },
            None => None,
        };
        let native_err = match native(text) {
            Ok(()) => return Ok(CopyResult::Copied),
            Err(e) => e,
        };
        match osc52(text) {
            Ok(()) => Ok(CopyResult::Copied),
            Err(osc52_err) => match cmd_err {
                Some(cmd_err) => Err(format!(
                    "command ({cmd_err}); native ({native_err}); osc52 ({osc52_err})"
                )),
                None => Err(format!("native ({native_err}); osc52 ({osc52_err})")),
            },
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
    use test_case::test_case;

    const DUMMY_TEXT: &str = "hello";
    const EMPTY_TEXT: &str = "";
    const WHITESPACE_TEXT: &str = "   \n\t";
    const CMD_NAME: &str = "echo-clip";
    const CMD_FAIL_ERR: &str = "command exited with 1";
    const NATIVE_ERR: &str = "no display";
    const OSC52_ERR: &str = "stdout closed";

    #[test]
    fn empty_text_is_noop_and_skips_all_backends() {
        let cmd_called = Cell::new(false);
        let native_called = Cell::new(false);
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            EMPTY_TEXT,
            Some(CMD_NAME),
            |_, _| {
                cmd_called.set(true);
                Ok(())
            },
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
        assert!(!cmd_called.get());
        assert!(!native_called.get());
        assert!(!osc52_called.get());
    }

    #[test]
    fn custom_cmd_success_skips_native_and_osc52() {
        let native_called = Cell::new(false);
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            DUMMY_TEXT,
            Some(CMD_NAME),
            |cmd, text| {
                assert_eq!(cmd, CMD_NAME);
                assert_eq!(text, DUMMY_TEXT);
                Ok(())
            },
            |_| {
                native_called.set(true);
                Ok(())
            },
            |_| {
                osc52_called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(!native_called.get());
        assert!(!osc52_called.get());
    }

    #[test]
    fn custom_cmd_failure_falls_back_to_native() {
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            DUMMY_TEXT,
            Some(CMD_NAME),
            |_, _| Err(CMD_FAIL_ERR.into()),
            |_| Ok(()),
            |_| {
                osc52_called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(!osc52_called.get());
    }

    #[test]
    fn native_success_skips_osc52() {
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            DUMMY_TEXT,
            None,
            |_, _| Ok(()),
            |_| Ok(()),
            |_| {
                osc52_called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(!osc52_called.get());
    }

    #[test]
    fn native_failure_is_recovered_by_osc52() {
        let result = ClipboardState::copy_text_impl(
            DUMMY_TEXT,
            None,
            |_, _| Ok(()),
            |_| Err(NATIVE_ERR.into()),
            |_| Ok(()),
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
    }

    #[test]
    fn all_failing_composes_errors_with_command_first() {
        let result = ClipboardState::copy_text_impl(
            DUMMY_TEXT,
            Some(CMD_NAME),
            |_, _| Err(CMD_FAIL_ERR.into()),
            |_| Err(NATIVE_ERR.into()),
            |_| Err(OSC52_ERR.into()),
        );
        let Err(msg) = result else {
            panic!("expected Err when all backends fail");
        };
        assert_eq!(
            msg,
            format!("command ({CMD_FAIL_ERR}); native ({NATIVE_ERR}); osc52 ({OSC52_ERR})")
        );
    }

    #[test]
    fn both_native_and_osc52_failing_without_custom_cmd() {
        let result = ClipboardState::copy_text_impl(
            DUMMY_TEXT,
            None,
            |_, _| Ok(()),
            |_| Err(NATIVE_ERR.into()),
            |_| Err(OSC52_ERR.into()),
        );
        let Err(msg) = result else {
            panic!("expected Err when native and osc52 fail");
        };
        assert_eq!(msg, format!("native ({NATIVE_ERR}); osc52 ({OSC52_ERR})"));
    }

    #[test]
    fn whitespace_only_text_is_not_noop() {
        let cmd_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            WHITESPACE_TEXT,
            Some(CMD_NAME),
            |_, _| {
                cmd_called.set(true);
                Ok(())
            },
            |_| Ok(()),
            |_| Ok(()),
        );
        assert!(matches!(result, Ok(CopyResult::Copied)));
        assert!(cmd_called.get());
    }

    #[test_case("true", DUMMY_TEXT => Ok(()) ; "successful_command")]
    #[test_case("false", DUMMY_TEXT => matches Err(_) ; "failing_command")]
    #[test_case("", DUMMY_TEXT => matches Err(_) ; "empty_command")]
    fn copy_command_execution(cmd: &str, text: &str) -> Result<(), String> {
        copy_command(cmd, text)
    }

    #[test]
    fn copy_command_pipes_text_to_stdin() {
        let tmp = tempfile::NamedTempFile::new().expect("temp file");
        let path = tmp.path().to_str().expect("path string");
        let cmd = format!("sh -c 'cat > {path}'");
        copy_command(&cmd, DUMMY_TEXT).expect("copy_command");
        let content = std::fs::read_to_string(path).expect("read content");
        assert_eq!(content, DUMMY_TEXT);
    }
}
