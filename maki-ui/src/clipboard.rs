use crate::terminal;
use arboard::Clipboard;
#[cfg(target_os = "linux")]
use arboard::{LinuxClipboardKind, SetExtLinux};

pub(crate) enum CopyResult {
    Copied,
    Noop,
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
        Self::copy_text_impl(text, |t| self.copy_native(t), terminal::copy_to_clipboard)
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
}
