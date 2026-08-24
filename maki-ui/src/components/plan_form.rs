use std::sync::Arc;

use crate::components::form::{render_form, selected_prefix};
use crate::components::hint_line;
use crate::components::keybindings::key;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

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

/// Built-in menu items expressed as row records so plugin rows can slot
/// alongside them, sorted by the same `order` field. Numbers leave room
/// between and after built-ins for plugin defaults (500) and future
/// additions.
const BUILTIN_REFINE_ORDER: i64 = 0;
const BUILTIN_CLEAR_AND_IMPLEMENT_ORDER: i64 = 1_000;
const BUILTIN_IMPLEMENT_ORDER: i64 = 2_000;

/// The three built-in outcomes the form can produce; a fourth ("open
/// editor") is a keybinding, not a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinAction {
    Refine,
    ClearAndImplement,
    Implement,
}

#[derive(Debug, Clone)]
struct BuiltinRow {
    label: &'static str,
    desc: &'static str,
    order: i64,
    action: BuiltinAction,
}

const BUILTIN_ROWS: &[BuiltinRow] = &[
    BuiltinRow {
        label: "Refine plan",
        desc: "  Dismiss and keep editing the plan",
        order: BUILTIN_REFINE_ORDER,
        action: BuiltinAction::Refine,
    },
    BuiltinRow {
        label: "Clear context and implement",
        desc: "  Start fresh session, then implement the plan",
        order: BUILTIN_CLEAR_AND_IMPLEMENT_ORDER,
        action: BuiltinAction::ClearAndImplement,
    },
    BuiltinRow {
        label: "Implement plan",
        desc: "  Keep current context, implement the plan",
        order: BUILTIN_IMPLEMENT_ORDER,
        action: BuiltinAction::Implement,
    },
];

/// One plugin-registered row, mirrored from the Lua snapshot each time the
/// snapshot generation changes.
#[derive(Debug, Clone)]
pub struct PluginPlanRow {
    pub plugin: Arc<str>,
    pub name: Arc<str>,
    pub label: Arc<str>,
    pub desc: Arc<str>,
    pub order: i64,
}

// 2 borders + 1 empty line + 1 hint bar
const CHROME_LINES: u16 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFormAction {
    Consumed,
    Passthrough,
    ClearAndImplement,
    Implement,
    OpenEditor,
    Hide,
    /// Plugin-registered row picked; App dispatches to the Lua handler.
    /// The form hides after emitting this, same as the built-in outcomes.
    Plugin {
        plugin: Arc<str>,
        name: Arc<str>,
    },
}

#[derive(Debug, Clone)]
enum RowSource {
    Builtin(BuiltinAction),
    Plugin(PluginPlanRow),
}

#[derive(Debug, Clone)]
struct MenuRow {
    label: String,
    desc: String,
    order: i64,
    source: RowSource,
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
    /// `true` blocks `on_plan_ready` from auto-showing the form so a
    /// plan-viewer plugin can own the UI. The user's manual `Ctrl+T`
    /// still opens the form, so the built-in stays reachable.
    suppressed: bool,
    /// Rows registered by plugins. Rebuilt on each snapshot generation
    /// change so a removal takes effect the next frame.
    plugin_rows: Vec<PluginPlanRow>,
    /// Cached merged menu: built-ins + plugin rows sorted by order.
    /// Recomputed when either side changes.
    menu: Vec<MenuRow>,
    plugin_snapshot_generation: u64,
}

impl PlanForm {
    pub fn new() -> Self {
        let mut form = Self {
            visibility: Visibility::Hidden,
            selected: 0,
            parallel: false,
            suppressed: false,
            plugin_rows: Vec::new(),
            menu: Vec::new(),
            plugin_snapshot_generation: 0,
        };
        form.rebuild_menu();
        form
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    pub fn on_plan_ready(&mut self) {
        if self.suppressed {
            return;
        }
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

    /// Set/read the suppression flag. Returns the previous value.
    pub fn set_suppressed(&mut self, hidden: bool) -> bool {
        let prev = self.suppressed;
        self.suppressed = hidden;
        prev
    }

    pub fn is_suppressed(&self) -> bool {
        self.suppressed
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Replace the plugin-row set. Called by App when the snapshot
    /// generation advances. Clamps `selected` if the previously focused
    /// row disappeared.
    pub fn set_plugin_rows(&mut self, rows: Vec<PluginPlanRow>, generation: u64) {
        if generation == self.plugin_snapshot_generation && rows.len() == self.plugin_rows.len() {
            return;
        }
        self.plugin_snapshot_generation = generation;
        self.plugin_rows = rows;
        self.rebuild_menu();
        if self.selected >= self.menu.len() {
            self.selected = self.menu.len().saturating_sub(1);
        }
    }

    pub fn reset(&mut self) {
        self.visibility = Visibility::Hidden;
        self.selected = 0;
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

    pub fn height(&self) -> u16 {
        if self.is_visible() {
            self.menu.len() as u16 + CHROME_LINES
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
                let max = self.menu.len().saturating_sub(1);
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

    fn row_action(&self, index: usize) -> PlanFormAction {
        let Some(row) = self.menu.get(index) else {
            return PlanFormAction::Hide;
        };
        match &row.source {
            RowSource::Builtin(BuiltinAction::Refine) => PlanFormAction::Hide,
            RowSource::Builtin(BuiltinAction::ClearAndImplement) => {
                PlanFormAction::ClearAndImplement
            }
            RowSource::Builtin(BuiltinAction::Implement) => PlanFormAction::Implement,
            RowSource::Plugin(row) => PlanFormAction::Plugin {
                plugin: Arc::clone(&row.plugin),
                name: Arc::clone(&row.name),
            },
        }
    }

    fn rebuild_menu(&mut self) {
        let mut menu: Vec<MenuRow> = BUILTIN_ROWS
            .iter()
            .map(|b| MenuRow {
                label: b.label.to_owned(),
                desc: b.desc.to_owned(),
                order: b.order,
                source: RowSource::Builtin(b.action),
            })
            .collect();
        for row in &self.plugin_rows {
            menu.push(MenuRow {
                label: row.label.as_ref().to_owned(),
                desc: if row.desc.is_empty() {
                    String::new()
                } else {
                    format!("  {}", row.desc)
                },
                order: row.order,
                source: RowSource::Plugin(row.clone()),
            });
        }
        menu.sort_by_key(|r| r.order);
        self.menu = menu;
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() {
            return;
        }

        let t = theme::current();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.menu.len() + 2);

        for (i, row) in self.menu.iter().enumerate() {
            let (prefix, style) = selected_prefix(&t, i == self.selected);
            let mut spans = vec![
                Span::styled(prefix, t.tool_dim),
                Span::styled(row.label.clone(), style),
                Span::styled(row.desc.clone(), t.tool_dim),
            ];
            if self.parallel {
                spans.push(Span::styled(" (parallel)", t.tool_dim.bold()));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        lines.push(hint_line(HINT_PAIRS));

        render_form(&t, FORM_LABEL, frame, area, lines, (0, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use test_case::test_case;

    fn plugin_row(name: &str, label: &str, order: i64) -> PluginPlanRow {
        PluginPlanRow {
            plugin: Arc::from("plug"),
            name: Arc::from(name),
            label: Arc::from(label),
            desc: Arc::from(""),
            order,
        }
    }

    fn last(form: &PlanForm) -> usize {
        form.menu.len() - 1
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
        assert_eq!(form.height(), 0);
        form.on_plan_ready();
        assert_eq!(form.height(), BUILTIN_ROWS.len() as u16 + CHROME_LINES);
        form.hide();
        assert_eq!(form.height(), 0);
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

    // Suppression + plugin-row tests below cover the additions this PR
    // brings to the form. Existing tests above are unchanged so any
    // regression in built-in behavior surfaces at the same assertion.

    #[test]
    fn suppressed_form_skips_on_plan_ready() {
        let mut form = PlanForm::new();
        assert!(!form.set_suppressed(true));
        form.on_plan_ready();
        assert!(!form.is_visible(), "suppressed form must stay hidden");
        assert!(form.is_suppressed());
    }

    #[test]
    fn manual_toggle_overrides_suppression() {
        let mut form = PlanForm::new();
        form.set_suppressed(true);
        form.on_plan_ready();
        assert!(!form.is_visible());
        form.toggle();
        assert!(
            form.is_visible(),
            "ctrl+t must still open the form as an escape hatch"
        );
    }

    #[test]
    fn set_suppressed_returns_previous() {
        let mut form = PlanForm::new();
        assert!(!form.set_suppressed(true));
        assert!(form.set_suppressed(true));
        assert!(form.set_suppressed(false));
    }

    #[test]
    fn plugin_row_sorted_between_builtins() {
        let mut form = PlanForm::new();
        form.set_plugin_rows(vec![plugin_row("mid", "Middle row", 500)], 1);
        // Refine(0), plugin(500), ClearAndImplement(1000), Implement(2000).
        assert_eq!(form.menu.len(), 4);
        assert_eq!(form.menu[1].label, "Middle row");
    }

    #[test]
    fn plugin_row_enter_emits_plugin_action() {
        let mut form = PlanForm::new();
        form.set_plugin_rows(vec![plugin_row("do-it", "Do it", 500)], 1);
        form.on_plan_ready();
        form.selected = 1;
        match form.handle_key(key(KeyCode::Enter)) {
            PlanFormAction::Plugin { plugin, name } => {
                assert_eq!(plugin.as_ref(), "plug");
                assert_eq!(name.as_ref(), "do-it");
            }
            other => panic!("expected Plugin action, got {other:?}"),
        }
    }

    #[test]
    fn plugin_rows_replaced_when_generation_changes() {
        let mut form = PlanForm::new();
        form.set_plugin_rows(vec![plugin_row("a", "A", 500)], 1);
        assert_eq!(form.menu.len(), 4);
        form.set_plugin_rows(vec![], 2);
        assert_eq!(form.menu.len(), BUILTIN_ROWS.len());
    }

    #[test]
    fn selected_clamps_when_plugin_row_removed() {
        let mut form = PlanForm::new();
        form.set_plugin_rows(
            vec![
                plugin_row("a", "A", 500),
                plugin_row("b", "B", 600),
                plugin_row("c", "C", 700),
            ],
            1,
        );
        form.on_plan_ready();
        form.selected = 5;
        form.set_plugin_rows(vec![], 2);
        assert!(form.selected < form.menu.len());
    }

    #[test]
    fn menu_length_drives_form_height() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        let base = form.height();
        form.set_plugin_rows(
            vec![plugin_row("a", "A", 500), plugin_row("b", "B", 550)],
            1,
        );
        assert_eq!(form.height(), base + 2);
    }
}
