//! Is the terminal light or dark? We ask once at startup, and only when
//! `ui.theme` names a different theme for each.
//!
//! Like neovim, we ask for the background color (`OSC 11`) and judge it by
//! luminance, so the answer follows what is really on screen. A terminal
//! that stays quiet counts as dark, which is what Maki assumed before.
//!
//! We skip the `CSI ? 996 n` color scheme query on purpose. Our crossterm
//! cannot parse its `CSI ? 997 n` reply, and a reply that shows up late
//! would stall all input after it.

use std::sync::OnceLock;

use maki_config::ThemePair;

use crate::tty_probe::{self, find};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Background {
    Light,
    Dark,
}

static BACKGROUND: OnceLock<Option<Background>> = OnceLock::new();

/// Same rules as `tty_probe::query`: raw mode, before the input reader.
/// Only the first call decides. A `/reload` comes back here, but by then
/// the input reader owns the tty and would swallow the reply.
pub(crate) fn init(theme: Option<&ThemePair>) {
    BACKGROUND.get_or_init(|| {
        theme
            .is_some_and(ThemePair::follows_background)
            .then(detect)
    });
}

/// `None` when startup did not ask.
pub(crate) fn background() -> Option<Background> {
    BACKGROUND.get().copied().flatten()
}

fn detect() -> Background {
    let reply = tty_probe::query(b"\x1b]11;?\x1b\\").and_then(|buf| parse_osc11(&buf));
    let background = reply.map_or(Background::Dark, classify);
    tracing::info!(
        ?background,
        answered = reply.is_some(),
        "terminal background"
    );
    background
}

/// Neovim's threshold: perceived luminance below half is dark.
fn classify((r, g, b): (f64, f64, f64)) -> Background {
    if 0.299 * r + 0.587 * g + 0.114 * b < 0.5 {
        Background::Dark
    } else {
        Background::Light
    }
}

/// Finds `ESC ] 11 ; rgb:R/G/B` ended by BEL or ST and returns each channel
/// as 0.0..=1.0. Channels carry 1 to 4 hex digits, scaled by their width.
fn parse_osc11(buf: &[u8]) -> Option<(f64, f64, f64)> {
    let start = find(buf, b"\x1b]11;")? + 5;
    let body = &buf[start..];
    let end = body.iter().position(|&c| c == 0x07 || c == 0x1b)?;
    let spec = std::str::from_utf8(&body[..end]).ok()?;
    let spec = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("rgba:"))?;
    let mut parts = spec.split('/').map(|hex| {
        let len = u32::try_from(hex.len())
            .ok()
            .filter(|l| (1..=4).contains(l))?;
        let value = u32::from_str_radix(hex, 16).ok()?;
        Some(f64::from(value) / f64::from((1u32 << (4 * len)) - 1))
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(b"\x1b]11;rgb:2828/2a2a/3636\x1b\\\x1b[?62c", Some(Background::Dark); "dracula_st")]
    #[test_case(b"\x1b]11;rgb:efef/f1f1/f5f5\x07\x1b[?62c", Some(Background::Light); "latte_bel")]
    #[test_case(b"\x1b]11;rgb:ff/ff/ff\x07", Some(Background::Light); "two_digit_white")]
    #[test_case(b"\x1b]11;rgb:f/f/f\x07", Some(Background::Light); "one_digit_white")]
    #[test_case(b"\x1b]11;rgba:ffff/ffff/ffff/ffff\x07", Some(Background::Light); "rgba")]
    #[test_case(b"\x1b[<0;1;1M\x1b]11;rgb:ffff/ffff/ffff\x1b\\", Some(Background::Light); "after_mouse_report")]
    #[test_case(b"\x1b[?62c", None; "unanswered")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff\x07", None; "missing_channel")]
    #[test_case(b"\x1b]11;rgb:fffff/0/0\x07", None; "channel_too_wide")]
    #[test_case(b"\x1b]11;rgb:ffff/ffff/ffff", None; "unterminated")]
    fn background_from_reply(buf: &[u8], expected: Option<Background>) {
        assert_eq!(parse_osc11(buf).map(classify), expected);
    }
}
