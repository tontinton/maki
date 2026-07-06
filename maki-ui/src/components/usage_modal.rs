use std::cmp::Reverse;
use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use maki_providers::{Model, ModelPricing, ProviderUsage, TokenUsage};
use maki_storage::sessions::StoredTokenUsage;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::ModalScroll;
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::status_bar::format_tokens;
use crate::theme;

const TITLE: &str = " Token usage ";
const PREFIX: &str = "  ";
const MODEL_COL_MIN: usize = 16;
const NUM_COL: usize = 7;
const COL_GAP: usize = 2;
const NO_USAGE_ENDPOINT: &str = "no usage endpoint for this provider";
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;

/// Live provider quota fetch, shared from the event loop. `Loading` is shown
/// until the background fetch completes; the modal reads this each render.
pub enum UsageFetchState {
    Loading,
    Ready(ProviderUsage),
    Unsupported,
    Error(String),
}

pub struct UsageModalContext<'a> {
    pub total: &'a TokenUsage,
    pub by_model: &'a HashMap<String, StoredTokenUsage>,
    pub model: &'a Model,
    pub fast: bool,
    pub quota: Option<&'a UsageFetchState>,
}

pub struct UsageModal {
    open: bool,
    scroll: ModalScroll,
}

impl UsageModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.scroll.reset();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if key_event.code == KeyCode::Esc || key::QUIT.matches(key_event) {
            self.close();
        }
        self.scroll.handle_key(key_event);
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, ctx: &UsageModalContext) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let theme = theme::current();
        let lines = build_lines(ctx, &theme);

        let total = lines.len() as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: 60,
            max_height_percent: 70,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let viewport_h = inner.height;
        self.scroll.update_dimensions(total, viewport_h);
        let scroll = self.scroll.offset();

        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);

        if total > viewport_h {
            render_vertical_scrollbar(frame, inner, total, scroll);
        }

        popup
    }
}

fn pricing_for(id: &str, current: &Model) -> Option<ModelPricing> {
    if id == current.id {
        return Some(current.pricing.clone());
    }
    Model::from_spec(id).ok().map(|m| m.pricing).or_else(|| {
        Model::from_spec(&format!("{}/{}", current.provider, id))
            .ok()
            .map(|m| m.pricing)
    })
}

fn build_lines(ctx: &UsageModalContext, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let fg = Style::new().fg(theme.foreground);

    lines.push(Line::from(Span::styled(
        format!("{PREFIX}Session total"),
        theme.keybind_section,
    )));

    let total_cost = if ctx.model.pricing.is_zero() {
        None
    } else {
        Some(ctx.total.cost(&ctx.model.pricing, ctx.fast))
    };
    lines.push(Line::from(totals_row(ctx.total, total_cost, theme)));

    if let Some(state) = ctx.quota {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}{} quota", ctx.model.provider.display_name()),
            theme.keybind_section,
        )));
        lines.extend(quota_lines(state, theme));
    }

    if ctx.by_model.is_empty() {
        return lines;
    }

    let mut entries: Vec<(&String, &StoredTokenUsage)> = ctx.by_model.iter().collect();
    entries.sort_by_key(|(_, u)| Reverse(u.total()));

    let model_w = entries
        .iter()
        .map(|(id, _)| id.chars().count())
        .max()
        .unwrap_or(0)
        .max(MODEL_COL_MIN);

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("{PREFIX}Per model"),
        theme.keybind_section,
    )));
    lines.push(Line::from(header_row(model_w, theme)));

    for (id, usage) in entries {
        let pricing = pricing_for(id, ctx.model);
        let cost = pricing
            .as_ref()
            .map(|p| TokenUsage::from(*usage).cost(p, ctx.fast));
        lines.push(Line::from(model_row(
            id,
            usage,
            cost,
            model_w,
            fg,
            theme.status_dim,
        )));
    }

    lines
}

fn totals_row(
    total: &TokenUsage,
    cost: Option<f64>,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::raw(PREFIX),
        Span::styled(
            format!(
                "in {:<7} out {:<7} cache read {:<7} cache write {:<7} total {:<7}",
                format_tokens(total.input),
                format_tokens(total.output),
                format_tokens(total.cache_read),
                format_tokens(total.cache_creation),
                format_tokens(total.context_tokens()),
            ),
            Style::new().fg(theme.foreground),
        ),
    ];
    if let Some(c) = cost {
        spans.push(Span::styled(format!("  ${c:.3}"), theme.accent));
    }
    spans
}

fn header_row(model_w: usize, theme: &crate::theme::Theme) -> Vec<Span<'static>> {
    let h = |label: &str| Span::styled(format!("{label:>NUM_COL$}"), theme.status_dim);
    let gap = || Span::raw(" ".repeat(COL_GAP));
    vec![
        Span::raw(PREFIX),
        Span::styled(
            format!("{:width$}", "model", width = model_w),
            theme.status_dim,
        ),
        gap(),
        h("in"),
        gap(),
        h("out"),
        gap(),
        h("cache"),
        gap(),
        h("total"),
        gap(),
        Span::styled(format!("{:>6}", "cost"), theme.status_dim),
    ]
}

fn model_row(
    id: &str,
    usage: &StoredTokenUsage,
    cost: Option<f64>,
    model_w: usize,
    fg: Style,
    dim: Style,
) -> Vec<Span<'static>> {
    let num = |v: u32| Span::styled(format!("{:>NUM_COL$}", format_tokens(v)), fg);
    let gap = || Span::raw(" ".repeat(COL_GAP));
    vec![
        Span::raw(PREFIX),
        Span::styled(format!("{id:<model_w$}"), fg),
        gap(),
        num(usage.input),
        gap(),
        num(usage.output),
        gap(),
        num(usage.cache_read),
        gap(),
        num(usage.total()),
        gap(),
        match cost {
            Some(c) => Span::styled(format!("{c:>6.3}"), fg),
            None => Span::styled(format!("{:>6}", "—"), dim),
        },
    ]
}

impl crate::components::Overlay for UsageModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn quota_lines(state: &UsageFetchState, theme: &crate::theme::Theme) -> Vec<Line<'static>> {
    let fg = Style::new().fg(theme.foreground);
    let dim = theme.status_dim;
    match state {
        UsageFetchState::Loading => {
            vec![Line::from(Span::styled(format!("{PREFIX}loading…"), dim))]
        }
        UsageFetchState::Unsupported => vec![Line::from(Span::styled(
            format!("{PREFIX}{NO_USAGE_ENDPOINT}"),
            dim,
        ))],
        UsageFetchState::Error(msg) => {
            vec![Line::from(Span::styled(format!("{PREFIX}{msg}"), dim))]
        }
        UsageFetchState::Ready(usage) => {
            let mut out = Vec::with_capacity(usage.limits.len() + 1);
            if let Some(plan) = &usage.plan {
                out.push(Line::from(Span::styled(
                    format!("{PREFIX}plan: {plan}"),
                    fg,
                )));
            }
            let tz = TimeZone::system();
            let label_w = usage
                .limits
                .iter()
                .map(|l| l.label.chars().count())
                .max()
                .unwrap_or(0);
            for limit in &usage.limits {
                let mut spans = vec![
                    Span::styled(format!("{PREFIX}{:<label_w$}", limit.label), fg),
                    Span::styled(format!("{:>3}%", limit.percentage), theme.accent),
                    Span::styled(" used", dim),
                ];
                if let Some(detail) = &limit.detail {
                    spans.push(Span::styled(format!("  {detail}"), dim));
                }
                if let Some(ms) = limit.reset_at {
                    spans.push(Span::styled(
                        format!("  Resets {}", format_reset(ms, &tz)),
                        dim,
                    ));
                }
                out.push(Line::from(spans));
            }
            out
        }
    }
}

fn format_reset(epoch_ms: u64, tz: &TimeZone) -> String {
    let secs = (epoch_ms / 1000) as i64;
    let Ok(ts) = Timestamp::from_second(secs) else {
        return epoch_ms.to_string();
    };
    let delta = secs - Timestamp::now().as_second();
    if (1..DAY).contains(&delta) {
        return relative(delta);
    }
    let zoned = ts.to_zoned(tz.clone());
    let fmt = if delta < WEEK {
        "%a %-I:%M %p"
    } else {
        "%b %-d, %-I:%M %p"
    };
    zoned.strftime(fmt).to_string()
}

fn relative(seconds: i64) -> String {
    let hrs = seconds / HOUR;
    let mins = (seconds % HOUR) / 60;
    if hrs > 0 {
        format!("in {hrs} hr {mins} min")
    } else {
        format!("in {mins} min")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use maki_providers::UsageLimit;
    use test_case::test_case;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test_case(key(KeyCode::Esc, KeyModifiers::NONE) ; "esc_closes")]
    #[test_case(key(KeyCode::Char('c'), KeyModifiers::CONTROL) ; "ctrl_c_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let mut modal = UsageModal::new();
        modal.toggle();
        assert!(modal.is_open());
        modal.handle_key(k);
        assert!(!modal.is_open());
    }

    #[test]
    fn toggle_open_close() {
        let mut modal = UsageModal::new();
        assert!(!modal.is_open());
        modal.toggle();
        assert!(modal.is_open());
        modal.toggle();
        assert!(!modal.is_open());
    }

    #[test]
    fn handle_key_ignores_arbitrary() {
        let mut modal = UsageModal::new();
        modal.toggle();
        modal.handle_key(key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(modal.is_open());
    }

    #[test]
    fn quota_ready_lines_include_labels_and_percentages() {
        let theme = crate::theme::current();
        let usage = ProviderUsage {
            plan: Some("lite".into()),
            limits: vec![
                UsageLimit {
                    label: "Current session".into(),
                    percentage: 16,
                    reset_at: Some(0),
                    detail: None,
                },
                UsageLimit {
                    label: "Usage credits".into(),
                    percentage: 4,
                    reset_at: None,
                    detail: Some("$2.33 spent".into()),
                },
            ],
        };
        let lines = quota_lines(&UsageFetchState::Ready(usage), &theme);
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("plan: lite"))
        );
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.content.contains("Current session"))
        );
        assert!(lines[1].spans.iter().any(|s| s.content.contains("16%")));
        assert!(lines[1].spans.iter().any(|s| s.content.contains("used")));
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|s| s.content.contains("Usage credits"))
        );
        assert!(lines[2].spans.iter().any(|s| s.content.contains("4%")));
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|s| s.content.contains("$2.33 spent"))
        );
    }

    #[test]
    fn quota_non_terminal_states_render_single_line() {
        let theme = crate::theme::current();
        assert_eq!(quota_lines(&UsageFetchState::Loading, &theme).len(), 1);
        let unsupported = quota_lines(&UsageFetchState::Unsupported, &theme);
        assert_eq!(unsupported.len(), 1);
        assert!(
            unsupported[0]
                .spans
                .iter()
                .any(|s| s.content.contains(NO_USAGE_ENDPOINT))
        );
        let err = quota_lines(&UsageFetchState::Error("nope".into()), &theme);
        assert_eq!(err.len(), 1);
        assert!(err[0].spans.iter().any(|s| s.content.contains("nope")));
    }

    #[test]
    fn relative_formats_future_windows() {
        assert_eq!(relative(30), "in 0 min");
        assert_eq!(relative(120), "in 2 min");
        assert_eq!(relative(3 * HOUR + 36 * 60), "in 3 hr 36 min");
        assert_eq!(relative(5 * HOUR), "in 5 hr 0 min");
    }
}
