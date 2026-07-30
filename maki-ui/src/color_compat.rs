//! Not every terminal speaks 24-bit color, and the ones that do not
//! (Terminal.app is the usual suspect) misparse RGB escape sequences and
//! fill the screen with garbage. So before drawing anything we find out
//! whether the terminal really does RGB, and if not, swap every RGB cell
//! for the nearest of the 256 classic xterm colors.
//!
//! Detection follows neovim: first the environment, then terminfo caps,
//! and as a last resort a DECRQSS probe of the live terminal. The probe is
//! what saves us over SSH, where env vars are often stripped.
//! `MAKI_TRUECOLOR=1` or `=0` overrides everything.

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::style::Color;

const CUBE_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
const TRUECOLOR_TERM_PROGRAMS: [&str; 7] = [
    "iTerm.app",
    "WezTerm",
    "ghostty",
    "vscode",
    "Hyper",
    "Tabby",
    "rio",
];
const TRUECOLOR_TERMS: [&str; 10] = [
    "kitty",
    "alacritty",
    "wezterm",
    "ghostty",
    "foot",
    "contour",
    "konsole",
    "iterm",
    "mintty",
    "rio",
];
const VTE_TRUECOLOR_VERSION: u32 = 3600;

static TRUECOLOR: OnceLock<bool> = OnceLock::new();

/// Call in raw mode and before the input reader thread spawns: the DECRQSS
/// probe reads replies straight from the tty, and a running reader thread
/// would eat them.
pub(crate) fn init() {
    TRUECOLOR.get_or_init(detect);
}

pub(crate) fn downgrade_if_needed(buf: &mut Buffer) {
    if *TRUECOLOR.get_or_init(detect) {
        return;
    }
    for cell in &mut buf.content {
        cell.fg = downgrade(cell.fg);
        cell.bg = downgrade(cell.bg);
        cell.underline_color = downgrade(cell.underline_color);
    }
}

fn detect() -> bool {
    let (supported, source) = match truecolor_from_env(|var| std::env::var(var).ok()) {
        Some(v) => (v, "env"),
        None if terminfo_advertises() => (true, "terminfo"),
        None => (probe::terminal_supports_rgb(), "probe"),
    };
    tracing::info!(supported, source, "truecolor detection");
    supported
}

/// `Some` is a definite answer; `None` means the env does not say, so the
/// caller moves on to terminfo and the live probe.
fn truecolor_from_env(get: impl Fn(&str) -> Option<String>) -> Option<bool> {
    match get("MAKI_TRUECOLOR").as_deref() {
        Some("1" | "true") => return Some(true),
        Some("0" | "false") => return Some(false),
        Some(other) => tracing::warn!(value = other, "ignoring invalid MAKI_TRUECOLOR"),
        None => {}
    }
    let has =
        |var, needles: &[&str]| get(var).is_some_and(|v| needles.iter().any(|n| v.contains(n)));
    let advertised = has("COLORTERM", &["truecolor", "24bit"])
        || has("TERM", &["direct", "24bit"])
        || get("TERM").is_some_and(|t| {
            let t = t.to_ascii_lowercase();
            TRUECOLOR_TERMS.iter().any(|n| t.contains(n))
        })
        || get("TERM_PROGRAM").is_some_and(|p| {
            TRUECOLOR_TERM_PROGRAMS
                .iter()
                .any(|n| p.eq_ignore_ascii_case(n))
        })
        || get("KONSOLE_VERSION").is_some()
        || get("VTE_VERSION")
            .is_some_and(|v| v.parse::<u32>().is_ok_and(|v| v >= VTE_TRUECOLOR_VERSION));
    advertised.then_some(true)
}

fn terminfo_advertises() -> bool {
    let Ok(info) = termini::TermInfo::from_env() else {
        return false;
    };
    matches!(info.extended_cap("RGB"), Some(termini::Value::True))
        || matches!(info.extended_cap("Tc"), Some(termini::Value::True))
        || (info.extended_cap("setrgbf").is_some() && info.extended_cap("setrgbb").is_some())
}

/// A terminal that actually applied our RGB background echoes `48:2` (or
/// `48;2`) back in its DECRQSS report; one that ignored it leaves it out.
/// Only the `$r ... ST` payload counts, so interleaved input (mouse
/// reports, pastes) cannot spoof a match.
#[cfg(any(unix, test))]
fn decrqss_reply_supports_rgb(buf: &[u8]) -> bool {
    let Some(start) = find(buf, b"$r") else {
        return false;
    };
    let payload = &buf[start + 2..];
    let payload = find(payload, b"\x1b\\").map_or(payload, |end| &payload[..end]);
    find(payload, b"48:2").is_some() || find(payload, b"48;2").is_some()
}

/// The DA1 reply (`ESC [ ? ... c`) ends the probe: we request it last, and
/// every terminal answers it, even ones that ignore DECRQSS. Without it we
/// would sit out the full timeout on every non-RGB terminal.
#[cfg(any(unix, test))]
fn da1_answered(buf: &[u8]) -> bool {
    find(buf, b"\x1b[?").is_some_and(|start| buf[start + 3..].contains(&b'c'))
}

#[cfg(any(unix, test))]
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(unix)]
mod probe {
    use std::fs::File;
    use std::io::{Write, stdout};
    use std::os::fd::{AsRawFd, RawFd};
    use std::time::{Duration, Instant};

    use super::{da1_answered, decrqss_reply_supports_rgb};

    /// Set an RGB background, query it back with DECRQSS, reset, then DA1.
    const QUERY: &[u8] = b"\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\x1b[c";
    const TIMEOUT: Duration = Duration::from_millis(500);

    pub(super) fn terminal_supports_rgb() -> bool {
        try_probe().unwrap_or(false)
    }

    fn try_probe() -> Option<bool> {
        let (_owned, fd) = open_tty()?;
        let mut out = stdout().lock();
        out.write_all(QUERY).ok()?;
        out.flush().ok()?;
        let deadline = Instant::now() + TIMEOUT;
        let mut buf = Vec::with_capacity(64);
        while !da1_answered(&buf) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if !wait_readable(fd, remaining) {
                break;
            }
            let mut chunk = [0u8; 256];
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        }
        Some(decrqss_reply_supports_rgb(&buf))
    }

    fn open_tty() -> Option<(Option<File>, RawFd)> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return Some((None, libc::STDIN_FILENO));
        }
        let file = File::open("/dev/tty").ok()?;
        let fd = file.as_raw_fd();
        Some((Some(file), fd))
    }

    fn wait_readable(fd: RawFd, timeout: Duration) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 && pfd.revents & libc::POLLIN != 0 }
    }
}

#[cfg(not(unix))]
mod probe {
    /// No tty to poke on non-unix. Windows Terminal does RGB and crossterm
    /// maps colors for the legacy console, so assuming truecolor is safe.
    pub(super) fn terminal_supports_rgb() -> bool {
        true
    }
}

fn downgrade(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Indexed(nearest_indexed(r, g, b)),
        c => c,
    }
}

fn nearest_indexed(r: u8, g: u8, b: u8) -> u8 {
    let step = |c: u8| match c {
        0..=47 => 0usize,
        48..=114 => 1,
        _ => (c as usize - 35) / 40,
    };
    let (ri, gi, bi) = (step(r), step(g), step(b));
    let sq = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
    let dist = |cr, cg, cb| sq(cr, r) + sq(cg, g) + sq(cb, b);
    let cube_dist = dist(CUBE_STEPS[ri], CUBE_STEPS[gi], CUBE_STEPS[bi]);
    let avg = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let gray_idx = (avg.saturating_sub(3) / 10).min(23) as u8;
    let gray = 8 + 10 * gray_idx;
    if dist(gray, gray, gray) < cube_dist {
        232 + gray_idx
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(Color::Rgb(0, 0, 0), Color::Indexed(16); "black_maps_to_cube_origin")]
    #[test_case(Color::Rgb(255, 255, 255), Color::Indexed(231); "white_maps_to_cube_max")]
    #[test_case(Color::Rgb(255, 0, 0), Color::Indexed(196); "pure_red")]
    #[test_case(Color::Rgb(0, 255, 0), Color::Indexed(46); "pure_green")]
    #[test_case(Color::Rgb(0, 0, 255), Color::Indexed(21); "pure_blue")]
    #[test_case(Color::Rgb(0x80, 0x80, 0x80), Color::Indexed(244); "mid_gray_uses_gray_ramp")]
    #[test_case(Color::Rgb(0x28, 0x2a, 0x36), Color::Indexed(236); "dracula_bg_stays_dark")]
    #[test_case(Color::Reset, Color::Reset; "reset_passes_through")]
    #[test_case(Color::Indexed(42), Color::Indexed(42); "indexed_passes_through")]
    fn downgrade_color(input: Color, expected: Color) {
        assert_eq!(downgrade(input), expected);
    }

    #[test_case(&[("MAKI_TRUECOLOR", "1")], Some(true); "override_forces_truecolor")]
    #[test_case(&[("MAKI_TRUECOLOR", "0"), ("COLORTERM", "truecolor")], Some(false); "override_forces_downgrade")]
    #[test_case(&[("MAKI_TRUECOLOR", "true")], Some(true); "override_word_true")]
    #[test_case(&[("MAKI_TRUECOLOR", "false"), ("COLORTERM", "truecolor")], Some(false); "override_word_false")]
    #[test_case(&[("MAKI_TRUECOLOR", "off")], None; "invalid_override_ignored")]
    #[test_case(&[("MAKI_TRUECOLOR", "off"), ("COLORTERM", "truecolor")], Some(true); "invalid_override_falls_through")]
    #[test_case(&[("COLORTERM", "truecolor")], Some(true); "colorterm_advertises")]
    #[test_case(&[("TERM", "xterm-direct")], Some(true); "term_direct")]
    #[test_case(&[("TERM", "xterm-kitty")], Some(true); "term_in_truecolor_list")]
    #[test_case(&[("TERM_PROGRAM", "WezTerm")], Some(true); "term_program_wezterm")]
    #[test_case(&[("TERM_PROGRAM", "Apple_Terminal")], None; "apple_terminal_not_advertised")]
    #[test_case(&[("KONSOLE_VERSION", "230800")], Some(true); "konsole_version")]
    #[test_case(&[("VTE_VERSION", "3600")], Some(true); "vte_at_threshold")]
    #[test_case(&[("VTE_VERSION", "3599")], None; "vte_below_threshold")]
    #[test_case(&[("TERM", "xterm-256color")], None; "plain_256color_unknown")]
    fn env_detection(vars: &[(&str, &str)], expected: Option<bool>) {
        let get = |var: &str| {
            vars.iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| (*v).to_string())
        };
        assert_eq!(truecolor_from_env(get), expected);
    }

    #[test_case(b"\x1bP1$r0;48:2:1:2:3m\x1b\\\x1b[?65;1;9c", true; "kitty_style_colon_reply")]
    #[test_case(b"\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?65;1;9c", true; "semicolon_reply")]
    #[test_case(b"\x1bP1$r0m\x1b\\\x1b[?1;2c", false; "rgb_ignored_by_terminal")]
    #[test_case(b"\x1b[?1;2c", false; "decrqss_unanswered")]
    #[test_case(b"\x1b[<48;2;5M\x1bP1$r0m\x1b\\\x1b[?1;2c", false; "mouse_report_cannot_spoof")]
    #[test_case(b"\x1bP1$r0m\x1b\\48;2\x1b[?1;2c", false; "bytes_after_st_ignored")]
    fn decrqss_reply(buf: &[u8], expected: bool) {
        assert_eq!(decrqss_reply_supports_rgb(buf), expected);
    }

    #[test_case(b"\x1b[?65;1;9c", true; "da1_reply")]
    #[test_case(b"\x1bP1$r0;48:2:1:2:3m\x1b\\", false; "decrqss_only")]
    #[test_case(b"\x1b[?65;1;9", false; "partial_da1")]
    fn da1(buf: &[u8], expected: bool) {
        assert_eq!(da1_answered(buf), expected);
    }
}
