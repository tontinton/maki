use std::cmp::Reverse;
use std::collections::HashMap;

use arc_swap::ArcSwapOption;

use crossterm::event::{KeyCode, KeyEvent};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use maki_config::ClockFormat;
use maki_providers::{Model, ModelUsageRow, ProviderUsage, TokenUsage, format_tokens, model_cost};
use maki_storage::sessions::StoredTokenUsage;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::ModalScroll;
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar_in_border;
use crate::repaint::{Dirty, Watch};
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

/// Live provider quota fetch, shared from the event loop. A detached task
/// drops the answer into the slot, and [`UsageModal::poll`] is what notices.
pub enum UsageFetchState {
    Loading,
    Ready(ProviderUsage),
    Unsupported,
    Error(String),
}

pub struct UsageModalContext<'a> {
    pub total: &'a TokenUsage,
    /// What the session billed, from [`maki_providers::session_cost`]. `None`
    /// means nothing here is priced, so the modal shows tokens only.
    pub total_cost: Option<f64>,
    pub by_model: &'a HashMap<String, StoredTokenUsage>,
    pub model: &'a Model,
    pub fast: bool,
    pub clock_format: ClockFormat,
}

pub struct UsageModal {
    open: bool,
    scroll: ModalScroll,
    quota: Watch<UsageFetchState>,
}

impl UsageModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
            quota: Watch::default(),
        }
    }

    /// Picks up a finished quota fetch. Nothing wakes the loop when the task
    /// stores its result, so an unpolled modal sits on `Loading` until the
    /// user happens to press a key.
    pub fn poll(&mut self, slot: &ArcSwapOption<UsageFetchState>) -> Dirty {
        if !self.open {
            return Dirty::NO;
        }
        self.quota.poll(slot.load_full())
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.scroll.reset();
    }

    /// Keeps the last answer: `/usage` refetches on every open, and until that
    /// lands it beats a blank panel.
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
        let lines = build_lines(ctx, self.quota.get(), &theme);

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
            render_vertical_scrollbar_in_border(frame, inner, u32::from(total), u32::from(scroll));
        }

        let hint = Line::from(vec![
            Span::raw(" "),
            Span::styled("Ctrl+R", theme.keybind_key),
            Span::styled(" reload ", theme.tool_dim),
        ]);
        let hint_w = hint.width() as u16;
        let hint_area = Rect {
            x: popup.x + popup.width.saturating_sub(hint_w + 1),
            y: popup.y + popup.height.saturating_sub(1),
            width: hint_w,
            height: 1,
        };
        frame.render_widget(Paragraph::new(hint), hint_area);

        popup
    }
}

fn build_lines(
    ctx: &UsageModalContext,
    quota: Option<&UsageFetchState>,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    let fg = Style::new().fg(theme.foreground);

    lines.push(Line::from(Span::styled(
        format!("{PREFIX}Session total"),
        theme.keybind_section,
    )));

    lines.push(Line::from(totals_row(ctx.total, ctx.total_cost, theme)));

    if let Some(state) = quota {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{PREFIX}{} quota", ctx.model.provider_display_name()),
            theme.keybind_section,
        )));
        lines.extend(quota_lines(state, theme, ctx.clock_format));
        if let Some(ready) = provider_usage(state)
            && !ready.by_model_today.is_empty()
        {
            let model_w = model_col_width(ready.by_model_today.iter().map(|row| &row.model));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("{PREFIX}By model (provider, today)"),
                theme.keybind_section,
            )));
            lines.push(Line::from(provider_by_model_header(model_w, theme)));
            for row in &ready.by_model_today {
                lines.push(Line::from(provider_by_model_row(row, model_w, fg)));
            }
        }
    }

    if ctx.by_model.is_empty() {
        return lines;
    }

    let mut entries: Vec<(&String, &StoredTokenUsage)> = ctx.by_model.iter().collect();
    entries.sort_by_key(|(_, u)| Reverse(u.total()));

    let model_w = model_col_width(entries.iter().map(|(id, _)| id));

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("{PREFIX}Per model"),
        theme.keybind_section,
    )));
    lines.push(Line::from(header_row(model_w, theme)));

    for (id, usage) in entries {
        let cost = model_cost(id, usage, ctx.model, ctx.fast);
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

fn model_col_width(names: impl IntoIterator<Item = impl AsRef<str>>) -> usize {
    names
        .into_iter()
        .map(|name| name.as_ref().chars().count())
        .max()
        .unwrap_or(0)
        .max(MODEL_COL_MIN)
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

fn provider_usage(state: &UsageFetchState) -> Option<&ProviderUsage> {
    if let UsageFetchState::Ready(usage) = state {
        Some(usage)
    } else {
        None
    }
}

fn provider_by_model_header(model_w: usize, theme: &crate::theme::Theme) -> Vec<Span<'static>> {
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
        h("total"),
        gap(),
        Span::styled(format!("{:>7}", "spend"), theme.status_dim),
    ]
}

fn provider_by_model_row(row: &ModelUsageRow, model_w: usize, fg: Style) -> Vec<Span<'static>> {
    let num = |v: u64| Span::styled(format!("{:>NUM_COL$}", format_tokens(v)), fg);
    let gap = || Span::raw(" ".repeat(COL_GAP));
    let dollars = row.spend_microdollars as f64 / 1_000_000.0;
    vec![
        Span::raw(PREFIX),
        Span::styled(format!("{:<width$}", row.model, width = model_w), fg),
        gap(),
        num(row.input_tokens),
        gap(),
        num(row.output_tokens),
        gap(),
        num(row.total_tokens),
        gap(),
        Span::styled(format!("{dollars:>7.4}"), fg),
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

fn quota_lines(
    state: &UsageFetchState,
    theme: &crate::theme::Theme,
    clock: ClockFormat,
) -> Vec<Line<'static>> {
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
                let mut spans = vec![Span::styled(
                    format!("{PREFIX}{:<label_w$}", limit.label),
                    fg,
                )];
                if let Some(pct) = limit.percentage {
                    spans.push(Span::styled(format!("{pct:>3}%"), theme.accent));
                    spans.push(Span::styled(" used", dim));
                }
                if let Some(detail) = &limit.detail {
                    spans.push(Span::styled(format!("  {detail}"), dim));
                }
                if let Some(ms) = limit.reset_at {
                    spans.push(Span::styled(
                        format!("  Resets {}", format_reset(ms, &tz, clock)),
                        dim,
                    ));
                }
                out.push(Line::from(spans));
            }
            out
        }
    }
}

fn format_reset(epoch_ms: u64, tz: &TimeZone, clock: ClockFormat) -> String {
    let secs = (epoch_ms / 1000) as i64;
    let Ok(ts) = Timestamp::from_second(secs) else {
        return epoch_ms.to_string();
    };
    let delta = secs - Timestamp::now().as_second();
    if (1..DAY).contains(&delta) {
        return relative(delta);
    }
    let zoned = ts.to_zoned(tz.clone());
    let clock = crate::clock::hm(clock);
    let fmt = if delta < WEEK {
        format!("%a {clock}")
    } else {
        format!("%b %-d, {clock}")
    };
    zoned.strftime(&fmt).to_string()
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
    use crate::components::{buffer_text, test_model};
    use crate::repaint::expect::{OWED, QUIET};
    use crossterm::event::KeyModifiers;
    use maki_providers::UsageLimit;
    use std::sync::Arc;
    use test_case::test_case;

    const RECORDED_COST: f64 = 0.123;
    const RECORDED_TEXT: &str = "0.123";
    /// 1M input tokens at the test model's $3/1M: what the modal would print if
    /// it re-priced the counters.
    const REPRICED_TEXT: &str = "3.000";
    const ONE_MILLION: u32 = 1_000_000;
    const ONE_MILLION_TEXT: &str = "1.0m";
    const UNKNOWN_MODEL: &str = "a-model-no-table-has-ever-heard-of";
    const NO_COST_TEXT: &str = "—";

    fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

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
                    percentage: Some(16),
                    reset_at: Some(0),
                    detail: None,
                },
                UsageLimit {
                    label: "Usage credits".into(),
                    percentage: Some(4),
                    reset_at: None,
                    detail: Some("$2.33 spent".into()),
                },
            ],
            by_model_today: vec![],
        };
        let lines = quota_lines(&UsageFetchState::Ready(usage), &theme, ClockFormat::Hour24);
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
        let clock = ClockFormat::Hour24;
        assert_eq!(
            quota_lines(&UsageFetchState::Loading, &theme, clock).len(),
            1
        );
        let unsupported = quota_lines(&UsageFetchState::Unsupported, &theme, clock);
        assert_eq!(unsupported.len(), 1);
        assert!(
            unsupported[0]
                .spans
                .iter()
                .any(|s| s.content.contains(NO_USAGE_ENDPOINT))
        );
        let err = quota_lines(&UsageFetchState::Error("nope".into()), &theme, clock);
        assert_eq!(err.len(), 1);
        assert!(err[0].spans.iter().any(|s| s.content.contains("nope")));
    }

    fn stored(cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input: ONE_MILLION,
            cost,
            ..Default::default()
        }
    }

    fn modal_rows(
        total: &TokenUsage,
        total_cost: Option<f64>,
        by_model: &HashMap<String, StoredTokenUsage>,
        model: &Model,
    ) -> Vec<String> {
        let ctx = UsageModalContext {
            total,
            total_cost,
            by_model,
            model,
            fast: false,
            clock_format: ClockFormat::Hour24,
        };
        line_texts(&build_lines(&ctx, None, &crate::theme::current()))
    }

    /// A recorded cost is what the turn was billed, and re-pricing its tokens
    /// restates the bill every time a provider moves its rates (DeepSeek moves
    /// them twice a day). A model the tables cannot resolve shows nothing,
    /// since charging it the selected model's rates invents a bill.
    #[test]
    fn model_rows_show_what_was_recorded_and_never_todays_price() {
        let model = test_model();
        let total = TokenUsage {
            input: 2 * ONE_MILLION,
            ..Default::default()
        };
        let by_model = HashMap::from([
            (model.id.clone(), stored(Some(RECORDED_COST))),
            (UNKNOWN_MODEL.to_string(), stored(None)),
        ]);

        let rows = modal_rows(&total, Some(RECORDED_COST), &by_model, &model);
        let row = |id: &str| {
            rows.iter()
                .find(|t| t.contains(id))
                .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"))
                .clone()
        };

        let recorded_row = row(&model.id);
        assert!(recorded_row.contains(RECORDED_TEXT), "{recorded_row}");
        assert!(!recorded_row.contains(REPRICED_TEXT), "{recorded_row}");

        let unknown_row = row(UNKNOWN_MODEL);
        assert!(unknown_row.contains(NO_COST_TEXT), "{unknown_row}");
        assert!(!unknown_row.contains(REPRICED_TEXT), "{unknown_row}");
    }

    /// The session's bill arrives already computed, from the turns that paid it.
    /// These counters would price to [`REPRICED_TEXT`] against the selected
    /// model, so a modal doing its own arithmetic prints a different number,
    /// and "$0.000" for a session nothing priced.
    #[test_case(Some(RECORDED_COST) => Some(RECORDED_TEXT.to_string()) ; "prints_the_bill_it_was_handed")]
    #[test_case(None                => None                            ; "unpriced_session_shows_tokens_only")]
    fn totals_row_never_re_prices_the_counters(total_cost: Option<f64>) -> Option<String> {
        let model = test_model();
        assert!(!model.pricing.is_zero(), "the fallback must be tempting");
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        let rows = modal_rows(&total, total_cost, &HashMap::new(), &model);
        // With no breakdown, the totals row is the only one carrying counters.
        let totals = rows
            .iter()
            .find(|t| t.contains(ONE_MILLION_TEXT))
            .unwrap_or_else(|| panic!("no totals row: {rows:?}"));
        totals
            .split_once('$')
            .map(|(_, cost)| cost.trim().to_string())
    }

    fn slot(state: UsageFetchState) -> ArcSwapOption<UsageFetchState> {
        ArcSwapOption::from_pointee(state)
    }

    fn render(modal: &mut UsageModal) -> String {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let model = test_model();
        let ctx = UsageModalContext {
            total: &TokenUsage::default(),
            total_cost: None,
            by_model: &HashMap::new(),
            model: &model,
            fast: false,
            clock_format: ClockFormat::Hour24,
        };
        terminal
            .draw(|f| {
                modal.view(f, f.area(), &ctx);
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// Nothing wakes the loop when the fetch stores its answer, so the modal
    /// has to notice on its own, and exactly once: the slot keeps holding the
    /// same `Arc` for as long as the modal stays open, and a poll that cannot
    /// tell "still there" from "just arrived" repaints on every tick. A closed
    /// modal is not on screen, so it must not pick anything up.
    #[test]
    fn poll_owes_a_frame_only_for_a_value_the_open_modal_has_not_seen() {
        let slot = slot(UsageFetchState::Loading);
        let mut modal = UsageModal::new();

        assert_eq!(
            modal.poll(&slot),
            Dirty::NO,
            "a closed modal ignores the slot"
        );
        assert!(modal.quota.get().is_none());

        modal.toggle();
        assert_eq!(modal.poll(&slot), Dirty::YES, "{OWED}");
        assert_eq!(modal.poll(&slot), Dirty::NO, "{QUIET}");

        slot.store(Some(Arc::new(UsageFetchState::Unsupported)));
        assert_eq!(
            modal.poll(&slot),
            Dirty::YES,
            "a refetch owes a frame, whatever it holds"
        );
    }

    /// `view` renders what the modal owns, never the shared slot. Reading the
    /// slot mid render is what forced the old loop to paint constantly. Closing
    /// keeps the last answer, so a reopen has something to show while the
    /// refetch is on its way, and owes no frame for what is already drawn.
    #[test]
    fn quota_reaches_the_screen_only_after_a_poll_and_survives_a_reopen() {
        let slot = slot(UsageFetchState::Unsupported);
        let mut modal = UsageModal::new();
        modal.toggle();

        assert!(
            !render(&mut modal).contains(NO_USAGE_ENDPOINT),
            "an unpolled quota must not appear on screen"
        );
        assert_eq!(modal.poll(&slot), Dirty::YES, "{OWED}");
        assert!(render(&mut modal).contains(NO_USAGE_ENDPOINT));

        modal.close();
        modal.toggle();
        assert_eq!(modal.poll(&slot), Dirty::NO, "{QUIET}");
        assert!(
            render(&mut modal).contains(NO_USAGE_ENDPOINT),
            "a reopened modal still shows the last answer it saw"
        );
    }

    #[test]
    fn relative_formats_future_windows() {
        assert_eq!(relative(30), "in 0 min");
        assert_eq!(relative(120), "in 2 min");
        assert_eq!(relative(3 * HOUR + 36 * 60), "in 3 hr 36 min");
        assert_eq!(relative(5 * HOUR), "in 5 hr 0 min");
    }
}
