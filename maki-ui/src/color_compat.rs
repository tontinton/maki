//! Terminals without 24-bit color support (e.g. Terminal.app) misparse RGB
//! SGR sequences, corrupting the whole screen. When truecolor is not
//! advertised, downgrade every RGB cell to the nearest xterm-256 color.

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::style::Color;

const CUBE_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];

pub(crate) fn downgrade_if_needed(buf: &mut Buffer) {
    static TRUECOLOR: OnceLock<bool> = OnceLock::new();
    if *TRUECOLOR.get_or_init(truecolor_supported) {
        return;
    }
    for cell in &mut buf.content {
        cell.fg = downgrade(cell.fg);
        cell.bg = downgrade(cell.bg);
        cell.underline_color = downgrade(cell.underline_color);
    }
}

fn truecolor_supported() -> bool {
    truecolor_from_env(|var| std::env::var(var).ok())
}

fn truecolor_from_env(get: impl Fn(&str) -> Option<String>) -> bool {
    if let Some(v) = get("MAKI_TRUECOLOR") {
        return v != "0";
    }
    let has =
        |var, needles: &[&str]| get(var).is_some_and(|v| needles.iter().any(|n| v.contains(n)));
    has("COLORTERM", &["truecolor", "24bit"]) || has("TERM", &["direct"])
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

    #[test_case(0, 0, 0, 16; "black_maps_to_cube_origin")]
    #[test_case(255, 255, 255, 231; "white_maps_to_cube_max")]
    #[test_case(255, 0, 0, 196; "pure_red")]
    #[test_case(0, 255, 0, 46; "pure_green")]
    #[test_case(0, 0, 255, 21; "pure_blue")]
    #[test_case(0x80, 0x80, 0x80, 244; "mid_gray_uses_gray_ramp")]
    #[test_case(0x28, 0x2a, 0x36, 236; "dracula_bg_stays_dark")]
    fn nearest(r: u8, g: u8, b: u8, expected: u8) {
        assert_eq!(nearest_indexed(r, g, b), expected);
    }

    #[test_case(Some("1"), None, true; "override_forces_truecolor")]
    #[test_case(Some("0"), Some("truecolor"), false; "override_forces_downgrade")]
    #[test_case(None, Some("truecolor"), true; "colorterm_advertises")]
    #[test_case(None, None, false; "nothing_advertised")]
    fn env_detection(maki: Option<&str>, colorterm: Option<&str>, expected: bool) {
        let get = |var: &str| match var {
            "MAKI_TRUECOLOR" => maki.map(String::from),
            "COLORTERM" => colorterm.map(String::from),
            _ => None,
        };
        assert_eq!(truecolor_from_env(get), expected);
    }

    #[test]
    fn non_rgb_colors_pass_through() {
        assert_eq!(downgrade(Color::Reset), Color::Reset);
        assert_eq!(downgrade(Color::Indexed(42)), Color::Indexed(42));
    }
}
