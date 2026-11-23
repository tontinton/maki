use crate::terminal::{self, ClipboardRoute};
use arboard::Clipboard;
#[cfg(target_os = "linux")]
use arboard::{GetExtLinux, LinuxClipboardKind, SetExtLinux};

pub(crate) enum CopyResult {
    Copied,
    Noop,
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

/// Blocks, so never call it on the UI thread.
pub(crate) fn read_text() -> Option<String> {
    let text = Clipboard::new().ok()?.get_text().ok()?;
    (!text.is_empty()).then_some(text)
}

pub(crate) struct ClipboardState {
    native: Option<Clipboard>,
    route: ClipboardRoute,
}

impl ClipboardState {
    pub(crate) fn new() -> Self {
        Self {
            native: Clipboard::new().ok(),
            route: terminal::caps().clipboard,
        }
    }

    /// Copies through the terminal alone, so a test never writes to the
    /// desktop's clipboard.
    #[cfg(test)]
    pub(crate) fn terminal_only() -> Self {
        Self {
            native: None,
            route: ClipboardRoute::NativeElseOsc52,
        }
    }

    pub(crate) fn copy_text(&mut self, text: &str) -> Result<CopyResult, String> {
        let route = self.route;
        Self::copy_text_impl(
            text,
            route,
            |t| self.copy_native(t),
            terminal::copy_to_clipboard,
        )
    }

    fn copy_text_impl<F, G>(
        text: &str,
        route: ClipboardRoute,
        native: F,
        osc52: G,
    ) -> Result<CopyResult, String>
    where
        F: FnOnce(&str) -> Result<(), String>,
        G: FnOnce(&str) -> Result<(), String>,
    {
        if text.is_empty() {
            return Ok(CopyResult::Noop);
        }
        let native_result = native(text);
        if native_result.is_ok() && route == ClipboardRoute::NativeElseOsc52 {
            return Ok(CopyResult::Copied);
        }
        match (native_result, osc52(text)) {
            (Ok(()), _) | (_, Ok(())) => Ok(CopyResult::Copied),
            (Err(native_err), Err(osc52_err)) => {
                Err(format!("native ({native_err}); osc52 ({osc52_err})"))
            }
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
    use crate::terminal::ClipboardRoute::{NativeAndOsc52, NativeElseOsc52};
    use std::cell::Cell;
    use test_case::test_case;

    fn ok() -> Result<(), String> {
        Ok(())
    }

    fn failed() -> Result<(), String> {
        Err("failed".into())
    }

    #[test_case(NativeElseOsc52, ok(),     ok()     => (true, false) ; "native_success_skips_osc52")]
    #[test_case(NativeElseOsc52, failed(), ok()     => (true, true)  ; "native_failure_falls_back_to_osc52")]
    #[test_case(NativeAndOsc52,  ok(),     ok()     => (true, true)  ; "both_sends_osc52_after_native_success")]
    #[test_case(NativeAndOsc52,  ok(),     failed() => (true, true)  ; "both_native_success_survives_osc52_failure")]
    #[test_case(NativeAndOsc52,  failed(), ok()     => (true, true)  ; "both_osc52_success_survives_native_failure")]
    #[test_case(NativeAndOsc52,  failed(), failed() => (false, true) ; "both_fails_only_when_both_fail")]
    fn copy_backends(
        route: ClipboardRoute,
        native: Result<(), String>,
        osc52: Result<(), String>,
    ) -> (bool, bool) {
        let osc52_called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            "hello",
            route,
            |_| native,
            |_| {
                osc52_called.set(true);
                osc52
            },
        );
        (matches!(result, Ok(CopyResult::Copied)), osc52_called.get())
    }

    #[test_case(NativeElseOsc52 ; "native_else_osc52")]
    #[test_case(NativeAndOsc52  ; "native_and_osc52")]
    fn empty_text_is_noop_and_skips_both_backends(route: ClipboardRoute) {
        let called = Cell::new(false);
        let result = ClipboardState::copy_text_impl(
            "",
            route,
            |_| {
                called.set(true);
                Ok(())
            },
            |_| {
                called.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(CopyResult::Noop)));
        assert!(!called.get(), "empty text must not reach a backend");
    }

    #[test]
    fn both_failing_composes_errors_with_native_first() {
        let result = ClipboardState::copy_text_impl(
            "hello",
            NativeElseOsc52,
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
            NativeElseOsc52,
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
