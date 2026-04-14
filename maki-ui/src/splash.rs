use crate::components::keybindings::key;
use crate::theme::{self, lerp_u8};
use crate::update;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::time::Instant;

const LOGO: &str = "maki";
const TAGLINE: &str = "the efficient coder";
const HELP_SEGMENTS: &[(&str, bool)] = &[
    (key::HELP.label, true),
    (" help", false),
    (" · ", false),
    ("/help", true),
    (" in chat", false),
];

const TIPS: &[(&str, &str)] = &[
    (
        key::FILE_PICKER.label,
        "to grab file paths with fuzzy search",
    ),
    (key::TASKS.label, "to see what your subagents are up to"),
    (key::SEARCH.label, "to find things in the conversation"),
    ("/btw", "to ask something without interrupting the session"),
    ("/memory", "to view, edit, and delete persistent notes"),
    ("/cd", "to switch to a different directory"),
];

const COLOR_TRANSITION_SECS: f32 = 0.4;

/// Seconds for the initial fade-in animation (ease-out cubic).
const FADE_DURATION: f32 = 1.6;
/// Seconds to wait before the logo starts appearing.
const LOGO_DELAY: f32 = 0.2;
/// Seconds over which the logo fades from dim to full brightness.
const LOGO_RAMP: f32 = 0.8;
/// Ascii chars mapped to increasing wave intensity (first must be space).
const FIELD_SYMS: &[&str] = &[" ", ".", ":", "+", "*"];
const FIELD_CHAR_MAX: f32 = (FIELD_SYMS.len() - 1) as f32;
/// Number of overlapping sine wave layers in the background field.
const WAVE_LAYERS: usize = 3;
/// Peak brightness multiplier for the field. Lower = subtler background.
const INTENSITY_SCALE: f32 = 0.3;
/// How quickly the field darkens toward the edges. Higher = tighter spotlight.
const VIGNETTE_SCALE: f32 = 0.25;
/// Base opacity for the dimmest field character (0.0–1.0). Higher = less contrast between chars.
const FIELD_BASE_OPACITY: f32 = 0.5;

const INV_TAU: f32 = 1.0 / std::f32::consts::TAU;
const TAU: f32 = std::f32::consts::TAU;
const PI: f32 = std::f32::consts::PI;
const FRAC_PI_2: f32 = std::f32::consts::FRAC_PI_2;
const BHASKARA_B: f32 = 4.0 / (PI * PI);

#[inline(always)]
fn fast_sin(x: f32) -> f32 {
    let x = x - (x * INV_TAU).floor() * TAU;
    let (x, sign) = if x > PI { (x - PI, -1.0_f32) } else { (x, 1.0) };
    let raw = BHASKARA_B * x * (PI - x);
    sign * (4.0 * raw) / (5.0 - raw)
}

#[inline(always)]
fn fast_sincos(x: f32) -> (f32, f32) {
    (fast_sin(x), fast_sin(x + FRAC_PI_2))
}

pub struct ColorTransition {
    from: (u8, u8, u8),
    to: (u8, u8, u8),
    start: Instant,
}

impl ColorTransition {
    pub fn new(color: Color) -> Self {
        let rgb = extract_rgb(color, (100, 140, 255));
        Self {
            from: rgb,
            to: rgb,
            start: Instant::now() - std::time::Duration::from_secs_f32(COLOR_TRANSITION_SECS),
        }
    }

    pub fn set(&mut self, color: Color) {
        let rgb = extract_rgb(color, (100, 140, 255));
        if rgb == self.to {
            return;
        }
        let now = Instant::now();
        self.from = self.resolve_rgb(now);
        self.to = rgb;
        self.start = now;
    }

    pub fn is_animating(&self) -> bool {
        Instant::now().duration_since(self.start).as_secs_f32() < COLOR_TRANSITION_SECS
    }

    pub fn resolve(&self) -> Color {
        let (r, g, b) = self.resolve_rgb(Instant::now());
        Color::Rgb(r, g, b)
    }

    fn resolve_rgb(&self, now: Instant) -> (u8, u8, u8) {
        let t = (now.duration_since(self.start).as_secs_f32() / COLOR_TRANSITION_SECS).min(1.0);
        let p = ease_out_cubic(t);
        (
            lerp_u8(self.from.0, self.to.0, p),
            lerp_u8(self.from.1, self.to.1, p),
            lerp_u8(self.from.2, self.to.2, p),
        )
    }
}

pub struct Splash {
    start: Instant,
    field_offset: f32,
    animate: bool,
    tip_idx: usize,
}

impl Default for Splash {
    fn default() -> Self {
        Self::new(true)
    }
}

impl Splash {
    pub fn new(animate: bool) -> Self {
        let mut rng = [0u8; 8];
        getrandom::fill(&mut rng).ok();
        let tip_idx = u32::from_le_bytes([rng[4], rng[5], rng[6], rng[7]]) as usize % TIPS.len();
        Self {
            start: Instant::now(),
            field_offset: (u64::from_le_bytes(rng) % 10_000) as f32,
            animate,
            tip_idx,
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer, accent: Color) {
        if area.width < 20 || area.height < 5 {
            return;
        }

        let t = self.start.elapsed().as_secs_f32();
        let fade = if t >= FADE_DURATION {
            1.0
        } else {
            ease_out_cubic(t / FADE_DURATION)
        };

        let new_version = update::latest_version();
        let block_height = 8;
        let top_y = area.y + area.height.saturating_sub(block_height) / 2;
        let tag_y = top_y + 1;
        let help_y = tag_y + 2;
        let tip_y = help_y + 2;

        if self.animate {
            self.render_field(area, buf, t + self.field_offset, fade, accent);
        }
        self.render_logo(area, buf, t, fade, top_y, accent);
        render_centered_faded(area, buf, fade, 0.75, tag_y, TAGLINE);
        self.render_help(area, buf, fade, help_y, accent);
        self.render_tip(area, buf, fade, tip_y, accent);
        render_version(area, buf, fade, area.y, new_version);
    }

    fn render_field(&self, area: Rect, buf: &mut Buffer, t: f32, fade: f32, accent: Color) {
        let theme = theme::current();
        let (ac_r, ac_g, ac_b) = extract_rgb(accent, (100, 140, 255));
        let (bg_r, bg_g, bg_b) = extract_rgb(theme.background, (15, 15, 25));

        let w = area.width as usize;
        let h = area.height as usize;
        if w == 0 || h == 0 {
            return;
        }
        let inv_w = 1.0 / w as f32;
        let inv_h = 1.0 / h as f32;

        let layers: [(f32, f32, f32, f32); WAVE_LAYERS] = std::array::from_fn(|i| {
            let lf = i as f32;
            (
                2.0 + lf * 1.8,
                1.5 + lf * 1.2,
                t * (0.3 + lf * 0.15) + lf * 2.094,
                1.0 / (1.5 + lf * 0.5),
            )
        });

        let weight_sum: f32 = layers.iter().map(|l| l.3).sum();
        let half_weight_sum = weight_sum * 0.5;
        let val_scale = (fade * INTENSITY_SCALE) / half_weight_sum;

        let style_lut: [(&str, Style); 4] = std::array::from_fn(|i| {
            let idx = i + 1;
            let frac = idx as f32 / FIELD_CHAR_MAX;
            let t = FIELD_BASE_OPACITY + frac * (1.0 - FIELD_BASE_OPACITY);
            (
                FIELD_SYMS[idx],
                Style::new().fg(Color::Rgb(
                    lerp_u8(bg_r, ac_r, t * 0.25),
                    lerp_u8(bg_g, ac_g, t * 0.175),
                    lerp_u8(bg_b, ac_b, t * 0.325),
                )),
            )
        });

        let vignette_inv = 1.0 / VIGNETTE_SCALE;

        // Single allocation for all per-column data: vx | sin0 | cos0 | sin1 | cos1 | sin2 | cos2
        // Contiguous SoA layout enables LLVM auto-vectorization of the inner wave loops.
        let mut col_data = vec![0.0_f32; w * (1 + WAVE_LAYERS * 2)];
        for col in 0..w {
            let nx = col as f32 * inv_w;
            let d = (nx - 0.5) * 2.0;
            col_data[col] = d * d;
            for i in 0..WAVE_LAYERS {
                let (s, c) = fast_sincos(nx * layers[i].0);
                col_data[w + i * 2 * w + col] = s * layers[i].3;
                col_data[w + (i * 2 + 1) * w + col] = c * layers[i].3;
            }
        }
        let vx = &col_data[..w];
        let col_sin: [&[f32]; WAVE_LAYERS] =
            std::array::from_fn(|i| &col_data[w + i * 2 * w..w + i * 2 * w + w]);
        let col_cos: [&[f32]; WAVE_LAYERS] =
            std::array::from_fn(|i| &col_data[w + (i * 2 + 1) * w..w + (i * 2 + 2) * w]);

        let col_start = vx.partition_point(|&v| v > vignette_inv);
        let col_end = w - vx
            .iter()
            .rev()
            .position(|&v| v <= vignette_inv)
            .unwrap_or(0);
        if col_start >= col_end {
            return;
        }

        let buf_width = buf.area().width as usize;
        let content = &mut buf.content;

        let mut vals = vec![0.0_f32; col_end - col_start];

        for row in 0..h {
            let ny = row as f32 * inv_h;
            let d = (ny - 0.5) * 2.0;
            let vy = d * d;

            let max_vx = vignette_inv - vy;
            if max_vx <= 0.0 {
                continue;
            }

            let row_sincos: [(f32, f32); WAVE_LAYERS] =
                std::array::from_fn(|i| fast_sincos(ny * layers[i].1 + layers[i].2));

            let rc_start = col_start + vx[col_start..col_end].partition_point(|&v| v > max_vx);
            let rc_end = col_end
                - vx[col_start..col_end]
                    .iter()
                    .rev()
                    .position(|&v| v <= max_vx)
                    .unwrap_or(0);

            let out = &mut vals[rc_start - col_start..rc_end - col_start];
            let vx_slice = &vx[rc_start..rc_end];

            // AUTOVECTORIZED - LLVM emits AVX (ymm, 8×f32) for these loops.
            // Do NOT add branches, function calls, or non-contiguous indexing here.
            // Verified via `perf annotate`.
            for i in 0..WAVE_LAYERS {
                let (sr, cr) = row_sincos[i];
                let cs = &col_sin[i][rc_start..rc_end];
                let cc = &col_cos[i][rc_start..rc_end];
                for j in 0..out.len() {
                    out[j] += cs[j] * cr + cc[j] * sr;
                }
            }
            for j in 0..out.len() {
                let vignette = 1.0 - (vx_slice[j] + vy) * VIGNETTE_SCALE;
                out[j] = (out[j] + half_weight_sum) * vignette * val_scale;
            }

            let y = area.y + row as u16;
            let row_offset = y as usize * buf_width + area.x as usize;

            for (j, val) in out.iter_mut().enumerate() {
                let idx = (*val * FIELD_CHAR_MAX + 0.5) as usize;
                *val = 0.0;
                if idx == 0 {
                    continue;
                }
                let (sym, style) = &style_lut[idx.min(FIELD_SYMS.len() - 1) - 1];

                if let Some(cell) = content.get_mut(row_offset + rc_start + j) {
                    cell.set_symbol(sym).set_style(*style);
                }
            }
        }
    }

    fn render_logo(
        &self,
        area: Rect,
        buf: &mut Buffer,
        t: f32,
        fade: f32,
        top_y: u16,
        accent: Color,
    ) {
        let theme = theme::current();
        let bg = theme.background;
        let (ac_r, ac_g, ac_b) = extract_rgb(accent, (100, 140, 255));
        let (bg_r, bg_g, bg_b) = extract_rgb(bg, (15, 15, 25));

        let logo_x = area.x + (area.width.saturating_sub(LOGO.len() as u16)) / 2;
        let alpha = 0.85 * ease_out_cubic(((t - LOGO_DELAY) / LOGO_RAMP).clamp(0.0, 1.0)) * fade;
        let style = Style::new()
            .fg(Color::Rgb(
                lerp_u8(bg_r, ac_r, alpha),
                lerp_u8(bg_g, ac_g, alpha),
                lerp_u8(bg_b, ac_b.saturating_add(15), alpha),
            ))
            .bg(bg)
            .add_modifier(Modifier::BOLD);

        for (col, ch) in LOGO.chars().enumerate() {
            let x = logo_x + col as u16;
            if x >= area.x + area.width || top_y >= area.y + area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, top_y)) {
                cell.set_char(ch).set_style(style);
            }
        }
    }

    fn render_help(&self, area: Rect, buf: &mut Buffer, fade: f32, help_y: u16, accent: Color) {
        if help_y >= area.y + area.height {
            return;
        }

        let theme = theme::current();
        let bg = theme.background;
        let ac = extract_rgb(accent, (100, 140, 255));
        let fg = extract_rgb(theme.foreground, (200, 200, 200));
        let bg_rgb = extract_rgb(bg, (15, 15, 25));

        let total_width: u16 = HELP_SEGMENTS.iter().map(|(s, _)| s.len() as u16).sum();
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: Vec<_> = HELP_SEGMENTS
            .iter()
            .map(|&(text, highlighted)| {
                let (target, alpha) = if highlighted { (ac, 0.75) } else { (fg, 0.5) };
                (text, faded_style(bg_rgb, target, alpha * fade, bg))
            })
            .collect();

        render_segments(area, buf, help_y, x_start, &segments);
    }

    fn render_tip(&self, area: Rect, buf: &mut Buffer, fade: f32, tip_y: u16, accent: Color) {
        if tip_y >= area.y + area.height {
            return;
        }

        let theme = theme::current();
        let bg = theme.background;
        let tip_rgb = extract_rgb(
            theme.todo_in_progress.fg.unwrap_or(Color::Yellow),
            (249, 226, 175),
        );
        let ac = extract_rgb(accent, (100, 140, 255));
        let fg = extract_rgb(theme.foreground, (200, 200, 200));
        let bg_rgb = extract_rgb(bg, (15, 15, 25));

        let (label, desc) = TIPS[self.tip_idx];
        let total_width = (5 + label.len() + 1 + desc.len()) as u16;
        let x_start = area.x + area.width.saturating_sub(total_width) / 2;

        let segments: &[(&str, Style)] = &[
            (
                "tip: ",
                faded_style(bg_rgb, tip_rgb, 0.75 * fade, bg).add_modifier(Modifier::BOLD),
            ),
            (label, faded_style(bg_rgb, ac, 0.75 * fade, bg)),
            (" ", Style::default()),
            (desc, faded_style(bg_rgb, fg, 0.5 * fade, bg)),
        ];

        render_segments(area, buf, tip_y, x_start, segments);
    }
}

fn render_version(area: Rect, buf: &mut Buffer, fade: f32, y: u16, new_version: Option<&str>) {
    if y >= area.y + area.height {
        return;
    }
    let theme = theme::current();
    let bg = theme.background;
    let text = match new_version {
        Some(v) => format!("v{} run maki update to get v{}", update::CURRENT, v),
        None => format!("v{}", update::CURRENT),
    };
    let style = faded_style(
        extract_rgb(bg, (15, 15, 25)),
        extract_rgb(theme.foreground, (200, 200, 200)),
        0.4 * fade,
        bg,
    );
    let x_start = area.x + area.width.saturating_sub(text.chars().count() as u16 + 1);
    render_segments(area, buf, y, x_start, &[(&text, style)]);
}

fn render_centered_faded(
    area: Rect,
    buf: &mut Buffer,
    fade: f32,
    intensity: f32,
    y: u16,
    text: &str,
) {
    if y >= area.y + area.height {
        return;
    }
    let theme = theme::current();
    let bg = theme.background;
    let style = faded_style(
        extract_rgb(bg, (15, 15, 25)),
        extract_rgb(theme.foreground, (200, 200, 200)),
        intensity * fade,
        bg,
    );
    let x_start = area.x + area.width.saturating_sub(text.chars().count() as u16) / 2;
    render_segments(area, buf, y, x_start, &[(text, style)]);
}

fn extract_rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => fallback,
    }
}

fn faded_style(bg: (u8, u8, u8), fg: (u8, u8, u8), alpha: f32, bg_color: Color) -> Style {
    Style::new()
        .fg(Color::Rgb(
            lerp_u8(bg.0, fg.0, alpha),
            lerp_u8(bg.1, fg.1, alpha),
            lerp_u8(bg.2, fg.2, alpha),
        ))
        .bg(bg_color)
}

fn render_segments(area: Rect, buf: &mut Buffer, y: u16, x_start: u16, segments: &[(&str, Style)]) {
    let x_end = area.x + area.width;
    let mut x = x_start;
    for &(text, style) in segments {
        for ch in text.chars() {
            if x >= x_end {
                return;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(ch).set_style(style);
            }
            x += 1;
        }
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn transition_at(from: (u8, u8, u8), to: (u8, u8, u8), offset: Duration) -> (u8, u8, u8) {
        let mut ct = ColorTransition::new(Color::Rgb(from.0, from.1, from.2));
        ct.set(Color::Rgb(to.0, to.1, to.2));
        ct.resolve_rgb(ct.start + offset)
    }

    #[test]
    fn interpolation_over_time() {
        let start = transition_at((0, 0, 0), (200, 200, 200), Duration::ZERO);
        assert_eq!(start, (0, 0, 0));

        let mid = transition_at((0, 0, 0), (200, 200, 200), Duration::from_millis(200));
        assert!(
            mid.0 > 0 && mid.0 < 200,
            "expected interpolated, got {}",
            mid.0
        );

        let done = transition_at((0, 0, 0), (255, 255, 255), Duration::from_millis(500));
        assert_eq!(done, (255, 255, 255));
    }

    #[test]
    fn chained_set_restarts_toward_new_target() {
        let mut ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        ct.set(Color::Rgb(200, 100, 50));
        ct.set(Color::Rgb(10, 20, 30));

        let done = ct.resolve_rgb(ct.start + Duration::from_secs(1));
        assert_eq!(done, (10, 20, 30));
    }

    #[test]
    fn is_animating_lifecycle() {
        let ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        assert!(!ct.is_animating(), "settled on construction");

        let mut ct = ColorTransition::new(Color::Rgb(0, 0, 0));
        ct.set(Color::Rgb(255, 0, 0));
        assert!(ct.is_animating(), "animating after set");
    }

    #[test]
    fn non_rgb_color_uses_fallback() {
        let ct = ColorTransition::new(Color::Blue);
        assert_eq!(
            ct.resolve_rgb(ct.start + Duration::from_secs(1)),
            (100, 140, 255)
        );
    }
}
