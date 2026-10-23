use crate::components::form::{render_form, selected_prefix};
use crate::components::hint_line;
use crate::components::keybindings::key;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_lua::{PlanFormRow, PlanMenu, PlanRowAction};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const FORM_LABEL: &str = " Plan complete ";

const DISMISS_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃T/Esc"
} else {
    "Ctrl+T/Esc"
};
const HINT_PAIRS: &[(&str, &str)] = &[
    ("↑↓", "select"),
    ("Space", "toggle parallel"),
    ("Enter", "confirm"),
    (key::OPEN_EDITOR.label, "edit plan"),
    (DISMISS_KEYS, "dismiss"),
];

/// The rows the host proposes, i.e. the menu on a stock install. They are
/// what the `ui.plan_form.actions` chain starts from and what the form falls
/// back to when no Lua host answers, so the two cannot drift apart. The id is
/// what a layer targets a row by, so it is part of the contract and outlives
/// any relabelling.
const BUILTIN_ROWS: &[(&str, &str, &str, PlanRowAction)] = &[
    (
        "refine",
        "Refine plan",
        "Dismiss and keep editing the plan",
        PlanRowAction::Refine,
    ),
    (
        "clear_and_implement",
        "Clear context and implement",
        "Start fresh session, then implement the plan",
        PlanRowAction::ClearAndImplement,
    ),
    (
        "implement",
        "Implement plan",
        "Keep current context, implement the plan",
        PlanRowAction::Implement,
    ),
];

/// Rows drawn at once. Past this the list scrolls with the selection instead
/// of growing a form taller than the chat it sits under.
const MAX_VISIBLE_ROWS: usize = 8;
/// Sits between a row's label and its description. Render geometry belongs to
/// the form, not to the rows the `ui.plan_form.actions` chain hands around, so
/// a plugin's description lines up with the built-in ones without knowing this
/// exists.
const DESC_GUTTER: &str = "  ";
/// What a row too wide for the box ends with, so a cut label reads as cut.
const ELLIPSIS: &str = "…";

pub fn builtin_rows() -> Vec<PlanFormRow> {
    BUILTIN_ROWS
        .iter()
        .map(|(id, label, desc, action)| PlanFormRow {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
            desc: (*desc).to_owned(),
            action: Some(*action),
            plugin: None,
        })
        .collect()
}

/// The built-in menu, for every path that opens the form without a chain
/// behind it. Generation zero is the one no pick can echo a handler out of,
/// and these rows have none.
pub fn builtin_menu() -> PlanMenu {
    PlanMenu {
        generation: 0,
        rows: builtin_rows(),
    }
}

// 2 borders + 1 empty line + 1 hint bar
pub(crate) const CHROME_LINES: u16 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFormAction {
    Consumed,
    Passthrough,
    ClearAndImplement,
    Implement,
    OpenEditor,
    Hide,
    /// Plugin row picked, named by its position in the menu the
    /// `ui.plan_form.actions` chain built and the generation that menu was
    /// published with. App dispatches it to Lua. `then` is the built-in
    /// outcome the row kept, which runs once the handler answers. The form
    /// hides after emitting this, like the built-in outcomes.
    Plugin {
        row: usize,
        generation: u64,
        then: Option<PlanRowAction>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Shown,
    Hidden,
    UserDismissed,
}

pub struct PlanForm {
    visibility: Visibility,
    selected: usize,
    parallel: bool,
    /// The menu the `ui.plan_form.actions` chain answered with for the
    /// current draft, or the host's own rows when nothing layered it, plus
    /// the generation a pick carries back so the handlers it reaches are the
    /// ones behind the rows on screen.
    menu: PlanMenu,
}

impl PlanForm {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            selected: 0,
            parallel: false,
            menu: builtin_menu(),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    pub fn on_plan_ready(&mut self) {
        if self.visibility != Visibility::UserDismissed {
            self.visibility = Visibility::Shown;
            self.selected = 0;
        }
    }

    pub fn on_plan_drafting(&mut self) {
        self.visibility = Visibility::Hidden;
    }

    pub fn toggle(&mut self) {
        self.visibility = if self.is_visible() {
            Visibility::UserDismissed
        } else {
            self.selected = 0;
            Visibility::Shown
        };
    }

    pub fn hide(&mut self) {
        if self.is_visible() {
            self.visibility = Visibility::UserDismissed;
        }
    }

    pub fn parallel(&self) -> bool {
        self.parallel
    }

    /// The menu on screen, for a test checking which draft's rows the form is
    /// still holding.
    #[cfg(test)]
    pub fn menu(&self) -> &PlanMenu {
        &self.menu
    }

    /// Show the form with the menu the `ui.plan_form.actions` chain built
    /// for this draft. Empty answers fall back to the host's own rows: a
    /// plugin that wants no form keeps `ui.plan_form` closed instead.
    pub fn open_with(&mut self, menu: PlanMenu) {
        self.menu = if menu.rows.is_empty() {
            builtin_menu()
        } else {
            menu
        };
        self.on_plan_ready();
    }

    /// Drop the menu of a draft that is no longer the one on screen, without
    /// touching whether the form is showing. A handler behind a stale row
    /// answers for the path its own draft had, so the plan-toggle key must
    /// not be able to reach one. What is left is the rows the host can always
    /// run.
    pub fn forget_menu(&mut self) {
        self.menu = builtin_menu();
        self.selected = 0;
    }

    pub fn reset(&mut self) {
        self.visibility = Visibility::Hidden;
        self.selected = 0;
        self.menu = builtin_menu();
    }

    pub fn hint_line(&self) -> Option<Line<'static>> {
        if self.visibility != Visibility::UserDismissed {
            return None;
        }
        let t = theme::current();
        Some(Line::from(vec![
            Span::styled(" Plan ", Style::new().fg(t.foreground)),
            Span::styled(key::PLAN_TOGGLE.label, t.keybind_key),
            Span::raw(" "),
        ]))
    }

    /// Rows on screen at once: the menu, up to the cap, and never more than
    /// {available} leaves room for once the borders, the blank line and the
    /// hint bar are paid for. Every height and every draw goes through this,
    /// so a short terminal cannot clip off the selected row and the hints.
    fn visible_rows(&self, available: u16) -> usize {
        self.menu
            .rows
            .len()
            .min(MAX_VISIBLE_ROWS)
            .min(available.saturating_sub(CHROME_LINES) as usize)
    }

    /// Lines the form needs, given the {available} ones the caller has left.
    pub fn height(&self, available: u16) -> u16 {
        if self.is_visible() {
            self.visible_rows(available) as u16 + CHROME_LINES
        } else {
            0
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> PlanFormAction {
        if key::QUIT.matches(key_event)
            || key_event.code == KeyCode::Esc
            || key::PLAN_TOGGLE.matches(key_event)
        {
            return PlanFormAction::Hide;
        }
        if key::OPEN_EDITOR.matches(key_event) {
            return PlanFormAction::OpenEditor;
        }
        match key_event.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PlanFormAction::Consumed
            }
            KeyCode::Down => {
                let max = self.menu.rows.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(max);
                PlanFormAction::Consumed
            }
            KeyCode::Char(' ') => {
                self.parallel = !self.parallel;
                PlanFormAction::Consumed
            }
            KeyCode::Enter => self.row_action(self.selected),
            KeyCode::Tab => PlanFormAction::Passthrough,
            _ => PlanFormAction::Consumed,
        }
    }

    /// A row with a handler goes to its plugin first, carrying the built-in
    /// outcome it kept so the host can run that after. A row with neither is
    /// one the chain should have rejected, so it dismisses.
    fn row_action(&self, row: usize) -> PlanFormAction {
        let Some(entry) = self.menu.rows.get(row) else {
            return PlanFormAction::Hide;
        };
        if entry.plugin.is_some() {
            return PlanFormAction::Plugin {
                row,
                generation: self.menu.generation,
                then: entry.action,
            };
        }
        match entry.action {
            Some(PlanRowAction::ClearAndImplement) => PlanFormAction::ClearAndImplement,
            Some(PlanRowAction::Implement) => PlanFormAction::Implement,
            // Refine dismisses, and so does a row nobody can run.
            Some(PlanRowAction::Refine) | None => PlanFormAction::Hide,
        }
    }

    /// The window the selection sits at the bottom of, so navigating past the
    /// last drawn row scrolls the menu instead of moving a selection nobody
    /// can see.
    fn first_visible(&self, available: u16) -> usize {
        self.selected
            .saturating_sub(self.visible_rows(available).saturating_sub(1))
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() {
            return;
        }

        let t = theme::current();
        let visible = self.visible_rows(area.height);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible + 2);

        // The form measures one line per row, so a wide row is cut instead of
        // wrapping into a second line the box never reserved.
        let width = area.width.saturating_sub(2) as usize;
        let first = self.first_visible(area.height);
        for (i, row) in self.menu.rows.iter().enumerate().skip(first).take(visible) {
            let (prefix, style) = selected_prefix(&t, i == self.selected);
            let mut spans = vec![
                Span::styled(prefix, t.tool_dim),
                Span::styled(row.label.clone(), style),
            ];
            if !row.desc.is_empty() {
                spans.push(Span::styled(
                    format!("{DESC_GUTTER}{}", row.desc),
                    t.tool_dim,
                ));
            }
            if self.parallel {
                spans.push(Span::styled(" (parallel)", t.tool_dim.bold()));
            }
            lines.push(fit(spans, width));
        }
        lines.push(Line::default());
        lines.push(fit(hint_line(HINT_PAIRS).spans, width));

        render_form(&t, FORM_LABEL, frame, area, lines, (0, 0));
    }
}

/// {spans} cut to {width} columns, ending in an ellipsis when anything had to
/// go. One line in, one line out: the form reserves a row per menu entry and
/// cannot afford a second.
fn fit(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    if spans.iter().map(|s| s.content.width()).sum::<usize>() <= width {
        return Line::from(spans);
    }
    let budget = width.saturating_sub(ELLIPSIS.width());
    let mut used = 0;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let span_width = span.content.width();
        if used + span_width <= budget {
            used += span_width;
            out.push(span);
            continue;
        }
        let mut cut = String::with_capacity(span.content.len());
        for ch in span.content.chars() {
            let char_width = ch.width().unwrap_or(0);
            if used + char_width > budget {
                break;
            }
            used += char_width;
            cut.push(ch);
        }
        out.push(Span::styled(cut, span.style));
        break;
    }
    out.push(Span::raw(ELLIPSIS));
    Line::from(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::components::key;
    use test_case::test_case;

    const PLUGIN_ID: &str = "commit_and_implement";
    const PLUGIN_LABEL: &str = "Commit and implement";
    const PLUGIN: &str = "planner";
    const GENERATION: u64 = 7;
    /// A terminal the form never has to fight for room in.
    const TALL: u16 = 200;

    fn plugin_row(id: &str, then: Option<PlanRowAction>) -> PlanFormRow {
        PlanFormRow {
            id: id.to_owned(),
            label: PLUGIN_LABEL.to_owned(),
            desc: String::new(),
            action: then,
            plugin: Some(Arc::from(PLUGIN)),
        }
    }

    fn menu(rows: Vec<PlanFormRow>) -> PlanMenu {
        PlanMenu {
            generation: GENERATION,
            rows,
        }
    }

    fn last(form: &PlanForm) -> usize {
        form.menu.rows.len() - 1
    }

    #[test]
    fn on_plan_ready_shows_and_resets_selected() {
        let mut form = PlanForm::new();
        form.selected = 1;
        form.on_plan_ready();
        assert!(form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn on_plan_ready_respects_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_ready();
        assert!(!form.is_visible());
    }

    #[test]
    fn on_plan_drafting_clears_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_drafting();
        form.on_plan_ready();
        assert!(
            form.is_visible(),
            "drafting should clear dismiss so next ready shows"
        );
    }

    #[test]
    fn toggle_cycles_visibility() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert!(form.is_visible());
        form.toggle();
        assert!(!form.is_visible());
        form.toggle();
        assert!(form.is_visible());
    }

    #[test]
    fn reset_clears_state() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = 1;
        form.reset();
        assert!(!form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn hint_line_only_when_dismissed() {
        let mut form = PlanForm::new();
        assert!(form.hint_line().is_none());
        form.on_plan_ready();
        assert!(form.hint_line().is_none());
        form.hide();
        assert!(form.hint_line().is_some());
    }

    #[test]
    fn height_reflects_visibility() {
        let mut form = PlanForm::new();
        assert_eq!(form.height(TALL), 0);
        form.on_plan_ready();
        assert_eq!(form.height(TALL), BUILTIN_ROWS.len() as u16 + CHROME_LINES);
        form.hide();
        assert_eq!(form.height(TALL), 0);
    }

    #[test_case(0, KeyCode::Up,   0    ; "up_at_zero_stays")]
    #[test_case(0, KeyCode::Down, 1    ; "down_from_zero")]
    fn navigation(start: usize, code: KeyCode, expected: usize) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = start;
        assert_eq!(form.handle_key(key(code)), PlanFormAction::Consumed);
        assert_eq!(form.selected, expected);
    }

    #[test]
    fn down_at_max_stays_at_last_row() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = last(&form);
        let target = last(&form);
        assert_eq!(
            form.handle_key(key(KeyCode::Down)),
            PlanFormAction::Consumed
        );
        assert_eq!(form.selected, target);
    }

    #[test]
    fn up_from_max_moves_one_up() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = last(&form);
        let target = last(&form) - 1;
        assert_eq!(form.handle_key(key(KeyCode::Up)), PlanFormAction::Consumed);
        assert_eq!(form.selected, target);
    }

    #[test_case(0, PlanFormAction::Hide              ; "enter_at_0_refine")]
    #[test_case(1, PlanFormAction::ClearAndImplement ; "enter_at_1")]
    #[test_case(2, PlanFormAction::Implement          ; "enter_at_2")]
    fn enter_dispatches(selected: usize, expected: PlanFormAction) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = selected;
        assert_eq!(form.handle_key(key(KeyCode::Enter)), expected);
    }

    #[test]
    fn space_toggles_parallel() {
        let mut form = PlanForm::new();
        let initial = form.parallel();
        form.on_plan_ready();
        assert_eq!(form.parallel(), initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), !initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), initial);
    }

    #[test_case(key(KeyCode::Esc)              ; "esc")]
    #[test_case(key::QUIT.to_key_event()      ; "ctrl_c")]
    #[test_case(key::PLAN_TOGGLE.to_key_event(); "ctrl_t")]
    fn dismiss(k: KeyEvent) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(form.handle_key(k), PlanFormAction::Hide);
    }

    #[test]
    fn ctrl_o_opens_editor() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key::OPEN_EDITOR.to_key_event()),
            PlanFormAction::OpenEditor
        );
    }

    #[test]
    fn unknown_key_consumed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Char('x'))),
            PlanFormAction::Consumed
        );
    }

    #[test]
    fn tab_passes_through() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Tab)),
            PlanFormAction::Passthrough
        );
    }

    /// The chain owns the order, so the form draws the list it was handed
    /// without sorting it again behind the plugin's back.
    #[test]
    fn the_menu_is_drawn_in_the_order_the_chain_answered() {
        let mut form = PlanForm::new();
        let mut rows = builtin_rows();
        rows.insert(0, plugin_row(PLUGIN_ID, None));
        form.open_with(menu(rows));
        let labels: Vec<_> = form.menu.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels[0], PLUGIN_LABEL);
        assert_eq!(labels[1], BUILTIN_ROWS[0].1);
    }

    /// A pick names the row's position and the generation of the menu it came
    /// from, so a menu replaced between the draw and the key press cannot
    /// have the pick routed into its handlers.
    #[test]
    fn a_plugin_row_enter_names_its_position_and_generation() {
        let mut form = PlanForm::new();
        let mut rows = builtin_rows();
        rows.push(plugin_row(PLUGIN_ID, None));
        let row = rows.len() - 1;
        form.open_with(menu(rows));
        form.selected = row;
        assert_eq!(
            form.handle_key(key(KeyCode::Enter)),
            PlanFormAction::Plugin {
                row,
                generation: GENERATION,
                then: None,
            }
        );
    }

    /// Handler-then-action: a row that kept a built-in outcome carries it
    /// along for the host to run once the handler answers.
    #[test]
    fn a_plugin_row_carries_the_action_it_kept() {
        let mut form = PlanForm::new();
        form.open_with(menu(vec![plugin_row(
            PLUGIN_ID,
            Some(PlanRowAction::Implement),
        )]));
        assert_eq!(
            form.handle_key(key(KeyCode::Enter)),
            PlanFormAction::Plugin {
                row: 0,
                generation: GENERATION,
                then: Some(PlanRowAction::Implement),
            }
        );
    }

    /// A layer is free to drop a built-in row, and the outcome goes with it.
    #[test]
    fn a_chain_can_drop_a_builtin_row() {
        let mut form = PlanForm::new();
        let rows = vec![builtin_rows().remove(0)];
        form.open_with(menu(rows));
        assert_eq!(form.menu.rows.len(), 1);
        form.selected = 0;
        assert_eq!(form.handle_key(key(KeyCode::Enter)), PlanFormAction::Hide);
    }

    /// A plugin that wants no form keeps `ui.plan_form` closed instead of
    /// emptying the menu.
    #[test]
    fn an_empty_menu_falls_back_to_the_builtin_rows() {
        let mut form = PlanForm::new();
        form.open_with(menu(vec![]));
        assert_eq!(form.menu.rows.len(), BUILTIN_ROWS.len());
    }

    /// The menu belongs to one draft, so the next session starts from the
    /// rows the host knows.
    #[test]
    fn reset_restores_the_builtin_menu() {
        let mut form = PlanForm::new();
        form.open_with(menu(vec![plugin_row(PLUGIN_ID, None)]));
        form.reset();
        assert_eq!(form.menu, builtin_menu());
    }

    #[test]
    fn menu_length_drives_form_height() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        let base = form.height(TALL);
        let mut rows = builtin_rows();
        rows.push(plugin_row(PLUGIN_ID, None));
        rows.push(plugin_row("another", None));
        form.open_with(menu(rows));
        assert_eq!(form.height(TALL), base + 2);
    }

    /// The form measures the rows it draws, not the ones it was handed:
    /// `menu.len() + chrome` in a `u16` is an overflow a layer can reach on
    /// purpose.
    #[test]
    fn a_menu_past_the_viewport_stops_growing_the_form() {
        let mut form = PlanForm::new();
        let rows = (0..MAX_VISIBLE_ROWS * 10)
            .map(|i| plugin_row(&format!("r{i}"), None))
            .collect();
        form.open_with(menu(rows));
        assert_eq!(form.height(TALL), MAX_VISIBLE_ROWS as u16 + CHROME_LINES);
    }

    /// A selection below the window scrolls it, or the user navigates a row
    /// nothing on screen shows as selected.
    #[test]
    fn the_viewport_follows_the_selection() {
        let mut form = PlanForm::new();
        let rows = (0..MAX_VISIBLE_ROWS * 2)
            .map(|i| plugin_row(&format!("r{i}"), None))
            .collect();
        form.open_with(menu(rows));
        assert_eq!(
            form.first_visible(TALL),
            0,
            "the top of the menu fits as is"
        );

        form.selected = MAX_VISIBLE_ROWS - 1;
        assert_eq!(form.first_visible(TALL), 0, "the last row that fits");

        form.selected = MAX_VISIBLE_ROWS * 2 - 1;
        assert_eq!(form.first_visible(TALL), MAX_VISIBLE_ROWS);
        assert!(form.selected - form.first_visible(TALL) < form.visible_rows(TALL));
    }

    /// The layout clamps the form to what is left above the chat, and the
    /// selection sits at the bottom of the window, so a form that measured
    /// more rows than it was given would clip off the selected row and the
    /// hint bar.
    #[test_case(20, MAX_VISIBLE_ROWS ; "room_for_the_whole_window")]
    #[test_case(CHROME_LINES + 2, 2 ; "room_for_two_rows")]
    #[test_case(CHROME_LINES, 0 ; "room_for_the_chrome_only")]
    #[test_case(1, 0 ; "no_room_at_all")]
    fn a_short_terminal_shrinks_the_window_rather_than_clipping_it(
        available: u16,
        expected: usize,
    ) {
        let mut form = PlanForm::new();
        let rows = (0..MAX_VISIBLE_ROWS * 2)
            .map(|i| plugin_row(&format!("r{i}"), None))
            .collect();
        form.open_with(menu(rows));
        form.selected = MAX_VISIBLE_ROWS * 2 - 1;

        assert_eq!(form.visible_rows(available), expected);
        assert!(
            form.height(available) <= available.max(CHROME_LINES),
            "the form must fit the room it was given"
        );
        assert!(
            form.first_visible(available) <= form.selected,
            "the window has to keep the selection in it"
        );
    }

    /// The form reserves one line per row, so a row too wide for the box is
    /// cut instead of wrapping into a second line that pushes the hint bar
    /// out of it.
    #[test_case(80, false ; "a_row_that_fits_is_left_alone")]
    #[test_case(20, true ; "a_row_too_wide_is_cut")]
    fn a_long_row_is_cut_to_one_line(width: usize, cut: bool) {
        let spans = vec![
            Span::raw("  "),
            Span::raw(PLUGIN_LABEL.to_owned()),
            Span::raw(" ".repeat(4)),
        ];
        let line = fit(spans, width);
        assert!(line.width() <= width);
        assert_eq!(
            line.spans.last().map(|s| s.content.as_ref()) == Some(ELLIPSIS),
            cut
        );
    }
}
