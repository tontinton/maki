use shell_words::split;
use std::io::{Write, stdout};
use std::path::Path;

use color_eyre::Result;
use crossterm::Command;
use crossterm::ExecutableCommand;
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

pub(crate) struct TerminalGuard;

enum TerminalMux {
    Zellij,
    Tmux,
    Screen,
    None,
}

impl TerminalMux {
    fn detect() -> Self {
        if std::env::var_os("ZELLIJ").is_some() {
            Self::Zellij
        } else if std::env::var_os("TMUX").is_some() {
            Self::Tmux
        } else if std::env::var_os("STY").is_some() {
            Self::Screen
        } else {
            Self::None
        }
    }

    // Wrap a control sequence so terminal multiplexers pass it through to the
    // outer terminal. tmux and screen take a DCS-wrapped payload with every
    // internal ESC byte doubled. Otherwise the ESC in the OSC52 `ESC \`
    // terminator ends the DCS early and truncates the payload. Zellij has no
    // passthrough protocol but intercepts OSC52 itself, so we emit the raw
    // sequence and let zellij forward or honor it per its `copy_command`
    // config.
    fn wrap_for_mux(&self, sequence: String) -> String {
        match self {
            Self::Zellij | Self::None => sequence,
            Self::Tmux => {
                let escaped = sequence.replace('\u{1b}', "\u{1b}\u{1b}");
                format!("\u{1b}Ptmux;{escaped}\u{1b}\\")
            }
            Self::Screen => {
                let escaped = sequence.replace('\u{1b}', "\u{1b}\u{1b}");
                format!("\u{1b}P{escaped}\u{1b}\\")
            }
        }
    }
}

impl TerminalGuard {
    pub(crate) fn init() -> Result<(Self, ratatui::DefaultTerminal)> {
        let terminal = ratatui::init();
        stdout().execute(EnableBracketedPaste)?;
        stdout().execute(EnableMouseCapture)?;
        push_keyboard_enhancement();
        Ok((Self, terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        pop_terminal_modes();
        ratatui::restore();
    }
}

pub(crate) fn suspend() {
    pop_terminal_modes();
    terminal::disable_raw_mode().ok();
    stdout().execute(LeaveAlternateScreen).ok();
}

fn pop_terminal_modes() {
    stdout().execute(PopKeyboardEnhancementFlags).ok();
    stdout().execute(DisableMouseCapture).ok();
    stdout().execute(DisableBracketedPaste).ok();
}

pub(crate) fn resume(terminal: &mut ratatui::DefaultTerminal) {
    stdout().execute(EnterAlternateScreen).ok();
    stdout().execute(EnableBracketedPaste).ok();
    stdout().execute(EnableMouseCapture).ok();
    terminal::enable_raw_mode().ok();
    push_keyboard_enhancement();
    let _ = terminal.clear();
}

fn push_keyboard_enhancement() {
    if let Err(e) = stdout().execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    )) {
        tracing::warn!(error = %e, "failed to enable keyboard enhancement (Kitty protocol)");
    }
}

pub(crate) fn edit_temp_content(
    content: &str,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<String, String> {
    let tmp = tempfile::Builder::new()
        .prefix("maki-input-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| format!("Failed to create temp file: {e}"))?;

    std::fs::write(tmp.path(), content).map_err(|e| format!("Failed to write temp file: {e}"))?;

    open_in_editor(tmp.path(), terminal)?;

    std::fs::read_to_string(tmp.path()).map_err(|e| format!("Failed to read edited content: {e}"))
}

pub(crate) fn open_in_editor(
    path: &Path,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<(), String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .map_err(|_| "Set $VISUAL or $EDITOR to open files".to_string())?;

    let args = split(&editor).map_err(|e| format!("Failed to parse $VISUAL or $EDITOR: {e}"))?;

    if args.is_empty() {
        return Err("Empty $VISUAL or $EDITOR".to_string());
    }

    suspend();

    let result = std::process::Command::new(&args[0])
        .args(&args[1..])
        .arg(path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    resume(terminal);

    match result {
        Ok(status) if !status.success() => Err(format!(
            "{editor} exited with {status} - set $VISUAL or $EDITOR"
        )),
        Err(e) => Err(format!(
            "Failed to open {editor}: {e} - set $VISUAL or $EDITOR"
        )),
        Ok(_) => Ok(()),
    }
}

pub(crate) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut sequence = String::new();
    CopyToClipboard::to_clipboard_from(text)
        .write_ansi(&mut sequence)
        .map_err(|e| e.to_string())?;
    let sequence = TerminalMux::detect().wrap_for_mux(sequence);
    let mut stdout = stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Faithfully simulates how a DCS-passthrough mux (tmux/screen) parses the
    // body between its opener and ST terminator: `ESC ESC` collapses to a
    // single ESC, `ESC \` ends the DCS, and any other byte is emitted verbatim.
    // Panics on malformed input so tests fail loudly on unexpected bytes.
    fn parse_dcs_passthrough(wrapped: &str, prefix: &str) -> String {
        let body = wrapped
            .strip_prefix(prefix)
            .unwrap_or_else(|| panic!("missing DCS prefix {prefix:?} in {wrapped:?}"));
        let bytes = body.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        loop {
            match bytes.get(i) {
                None => panic!("DCS body missing ST terminator: {body:?}"),
                Some(&0x1B) => match bytes.get(i + 1) {
                    Some(&0x1B) => {
                        out.push(0x1B);
                        i += 2;
                    }
                    Some(&b'\\') => {
                        assert_eq!(
                            i + 2,
                            bytes.len(),
                            "unexpected trailing bytes after DCS ST: {:?}",
                            &bytes[i + 2..]
                        );
                        return String::from_utf8(out).expect("utf-8 body");
                    }
                    Some(b) => panic!("unexpected byte 0x{b:02x} after ESC inside DCS"),
                    None => panic!("lone trailing ESC in DCS body"),
                },
                Some(&b) => {
                    out.push(b);
                    i += 1;
                }
            }
        }
    }

    // OSC52 payload using ST terminator — matches what crossterm emits, and
    // exercises the case where the DCS body contains an ESC both at the
    // start (opening the OSC) and mid-body (opening the ST terminator).
    const OSC52_WITH_ST: &str = "\u{1b}]52;c;SGVsbG8=\u{1b}\\";

    #[test]
    fn none_is_identity() {
        assert_eq!(
            TerminalMux::None.wrap_for_mux(OSC52_WITH_ST.to_string()),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn zellij_is_identity_because_it_intercepts_osc52() {
        // Zellij consumes OSC52 itself rather than forwarding; wrapping in a
        // DCS it doesn't understand would eat the whole sequence.
        assert_eq!(
            TerminalMux::Zellij.wrap_for_mux(OSC52_WITH_ST.to_string()),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn tmux_wrap_survives_tmux_passthrough_parser() {
        let wrapped = TerminalMux::Tmux.wrap_for_mux(OSC52_WITH_ST.to_string());
        assert_eq!(
            parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn screen_wrap_survives_screen_passthrough_parser() {
        let wrapped = TerminalMux::Screen.wrap_for_mux(OSC52_WITH_ST.to_string());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}P"), OSC52_WITH_ST);
    }

    // A payload with ESC bytes at multiple positions: if any of them gets left
    // undoubled, the first undoubled `ESC \` would close the DCS early and
    // truncate the payload, and this round-trip would fail.
    #[test]
    fn tmux_preserves_payload_with_multiple_interior_esc_bytes() {
        let payload = "\u{1b}A\u{1b}B\u{1b}C\u{1b}\\";
        let wrapped = TerminalMux::Tmux.wrap_for_mux(payload.to_string());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"), payload);
    }

    // End-to-end: feed the actual crossterm OSC52 output through the wrapper,
    // confirming the full real pipeline isn't lossy.
    #[test]
    fn tmux_wrap_roundtrips_crossterm_osc52_output() {
        let mut sequence = String::new();
        CopyToClipboard::to_clipboard_from("hello, world!")
            .write_ansi(&mut sequence)
            .expect("crossterm write_ansi");
        let wrapped = TerminalMux::Tmux.wrap_for_mux(sequence.clone());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"), sequence);
    }
}
