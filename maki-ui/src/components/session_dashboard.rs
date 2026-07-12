use std::thread;

use crate::AppSession;
use crate::components::format_relative_time;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_storage::StateDir;
use maki_storage::sessions::SessionStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};

const TITLE: &str = " Agents ";
const NO_SESSIONS_MSG: &str = "No sessions yet in this directory";
const FOOTER_HINTS: &[(&str, &str)] = &[
    ("↑/↓", "navigate"),
    ("→", "open"),
    ("Enter", "open/spawn"),
    (key::DELETE.label, "delete"),
];

const SECTION_NEEDS_INPUT: &str = "Needs input";
const SECTION_WORKING: &str = "Working";
const SECTION_COMPLETED: &str = "Completed";

/// The event loop ticks roughly per frame; refreshing the board about once a
/// second keeps background status changes visible without hammering storage.
const REFRESH_INTERVAL_TICKS: u16 = 60;


#[derive(Debug)]
pub enum DashboardAction {
    Consumed,
    Open(String),
    NewSession,
    ConfirmDelete,
    Delete(String),
    /// Not a dashboard navigation key: the caller should route it to the
    /// shared input box (typing the new-session task, file picker, etc.).
    Passthrough(KeyEvent),
    None,
}

struct DashboardEntry {
    id: String,
    title: String,
    detail: String,
    section: &'static str,
    spinning: bool,
}

impl PickerItem for DashboardEntry {
    fn label(&self) -> &str {
        &self.title
    }
    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }
    fn section(&self) -> Option<&str> {
        Some(self.section)
    }
    fn is_spinning(&self) -> bool {
        self.spinning
    }
}

/// Full-screen multi-session overview shown by `maki agents`. Lists the current
/// directory's sessions grouped into Needs input / Working / Completed sections.
pub struct SessionDashboard {
    picker: ListPicker<DashboardEntry>,
    confirming: Option<(String, u64)>,
    pending_rx: Option<flume::Receiver<Result<Vec<DashboardEntry>, String>>>,
    flash: Option<String>,
    source: Option<(String, StateDir)>,
    refreshing: bool,
    ticks_since_refresh: u16,
}

impl SessionDashboard {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_footer(FOOTER_HINTS),
            confirming: None,
            pending_rx: None,
            flash: None,
            source: None,
            refreshing: false,
            ticks_since_refresh: 0,
        }
    }

    pub fn open(&mut self, cwd: &str, dir: &StateDir) {
        self.picker.open_loading(TITLE);
        self.source = Some((cwd.to_owned(), dir.clone()));
        self.refreshing = false;
        self.ticks_since_refresh = 0;
        self.pending_rx = Some(spawn_scan(cwd.to_owned(), dir.clone()));
    }

    fn try_resolve(&mut self) {
        let Some(ref rx) = self.pending_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.pending_rx = None;
        let was_refresh = self.refreshing;
        self.refreshing = false;
        match result {
            Ok(entries) if entries.is_empty() => {
                self.picker.resolve(entries);
                self.picker.set_error_text(Some(NO_SESSIONS_MSG.into()));
            }
            // A live refresh replaces items in place so the user's current
            // selection and scroll position survive the status update.
            Ok(entries) if was_refresh => {
                self.picker.set_error_text(None);
                self.picker.replace_items(entries);
            }
            Ok(entries) => self.picker.resolve(entries),
            Err(e) => {
                if !was_refresh {
                    self.picker.resolve(Vec::new());
                    self.picker.set_error_text(Some(e));
                }
            }
        }
    }

    /// Kick off a background re-scan to reflect status changes from other
    /// sessions without disturbing the current selection. No-op while an
    /// initial load or another refresh is in flight.
    fn refresh(&mut self) {
        if self.pending_rx.is_some() || self.picker.is_loading() {
            return;
        }
        let Some((cwd, dir)) = self.source.clone() else {
            return;
        };
        self.refreshing = true;
        self.pending_rx = Some(spawn_scan(cwd, dir));
    }

    pub fn take_flash(&mut self) -> Option<String> {
        self.flash.take()
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.pending_rx = None;
        self.source = None;
        self.refreshing = false;
    }

    pub fn remove_entry(&mut self, id: &str) {
        self.picker.retain(|e| e.id != id);
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DashboardAction {
        if is_delete_key(&key) {
            return self.handle_delete_key();
        }

        // Ctrl-N spawns a brand-new empty session.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('n')
        {
            return DashboardAction::NewSession;
        }

        // Navigation keys drive the session list. Everything else (typing the
        // task, `/` commands, file picker, paste) is passed through to the
        // shared input box owned by the App.
        match key.code {
            KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() => {
                self.picker.handle_key(key);
                DashboardAction::Consumed
            }
            KeyCode::Right if key.modifiers.is_empty() => match self.picker.selected_item() {
                Some(item) => DashboardAction::Open(item.id.clone()),
                None => DashboardAction::Consumed,
            },
            KeyCode::Esc => match self.picker.handle_key(key) {
                PickerAction::Close => DashboardAction::None,
                _ => DashboardAction::Consumed,
            },
            _ => DashboardAction::Passthrough(key),
        }
    }

    pub fn selected_id(&self) -> Option<String> {
        self.picker.selected_item().map(|item| item.id.clone())
    }

    fn handle_delete_key(&mut self) -> DashboardAction {
        let Some(selected) = self.picker.selected_item() else {
            return DashboardAction::Consumed;
        };

        let generation = self.picker.generation();
        if self
            .confirming
            .as_ref()
            .is_some_and(|(id, g)| id == &selected.id && *g == generation)
        {
            return DashboardAction::Delete(selected.id.clone());
        }

        self.confirming = Some((selected.id.clone(), generation));
        DashboardAction::ConfirmDelete
    }

    pub fn tick(&mut self) {
        self.try_resolve();

        self.ticks_since_refresh = self.ticks_since_refresh.saturating_add(1);
        if self.ticks_since_refresh >= REFRESH_INTERVAL_TICKS {
            self.ticks_since_refresh = 0;
            self.refresh();
        }
    }

    /// Renders the session list and reserves the bottom of the panel for the
    /// shared new-session input box. Returns the `Rect` the caller renders the
    /// real input box into (so it gets horizontal scroll, multiline, cursor,
    /// and image display for free).
    pub fn view(&mut self, frame: &mut Frame, area: Rect, input_height: u16) -> DashboardLayout {
        // The input box draws its own top+bottom border, so it needs at least 3
        // rows (border, one content line, border). Give it what the caller asks.
        let reserved = input_height.max(3);
        let [list_area, bottom] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(reserved)]).areas(area);

        let popup = self.picker.view(frame, list_area);

        let input_area = Rect {
            x: popup.x,
            y: bottom.y,
            width: popup.width.max(1),
            height: reserved.min(bottom.height),
        };

        DashboardLayout { popup, input_area }
    }
}

/// Where the dashboard drew itself: `popup` is the modal rect (for overlay
/// bookkeeping), `input_area` is where the caller renders the shared input box.
pub struct DashboardLayout {
    pub popup: Rect,
    pub input_area: Rect,
}

impl crate::components::Overlay for SessionDashboard {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn is_delete_key(key: &KeyEvent) -> bool {
    key::DELETE.matches(*key)
}

fn section_rank(status: SessionStatus) -> u8 {
    match status {
        SessionStatus::NeedsInput => 0,
        SessionStatus::Working => 1,
        SessionStatus::Completed | SessionStatus::Idle | SessionStatus::Error => 2,
    }
}

fn section_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::NeedsInput => SECTION_NEEDS_INPUT,
        SessionStatus::Working => SECTION_WORKING,
        SessionStatus::Completed | SessionStatus::Idle | SessionStatus::Error => SECTION_COMPLETED,
    }
}

fn spawn_scan(cwd: String, dir: StateDir) -> flume::Receiver<Result<Vec<DashboardEntry>, String>> {
    let (tx, rx) = flume::bounded(1);
    thread::spawn(move || {
        let result = AppSession::list(&cwd, &dir)
            .map(|mut summaries| {
                summaries.sort_by(|a, b| {
                    section_rank(a.status)
                        .cmp(&section_rank(b.status))
                        .then(b.updated_at.cmp(&a.updated_at))
                });
                summaries.into_iter().map(entry_from_summary).collect()
            })
            .map_err(|e| format!("Failed to list sessions: {e}"));
        let _ = tx.send(result);
    });
    rx
}

fn entry_from_summary(s: maki_storage::sessions::SessionSummary) -> DashboardEntry {
    let time = format_relative_time(s.updated_at);
    let detail = match &s.summary {
        Some(summary) if !summary.is_empty() => format!("{summary}  ·  {time}"),
        _ => time,
    };
    DashboardEntry {
        id: s.id,
        title: s.title,
        detail,
        section: section_label(s.status),
        spinning: matches!(s.status, SessionStatus::Working),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(SessionStatus::NeedsInput, 0 ; "needs_input_first")]
    #[test_case(SessionStatus::Working, 1 ; "working_second")]
    #[test_case(SessionStatus::Completed, 2 ; "completed_last")]
    #[test_case(SessionStatus::Idle, 2 ; "idle_with_completed")]
    #[test_case(SessionStatus::Error, 2 ; "error_with_completed")]
    fn status_orders_into_sections(status: SessionStatus, expected_rank: u8) {
        assert_eq!(section_rank(status), expected_rank);
    }

    #[test_case(SessionStatus::NeedsInput, SECTION_NEEDS_INPUT ; "needs_input_label")]
    #[test_case(SessionStatus::Working, SECTION_WORKING ; "working_label")]
    #[test_case(SessionStatus::Completed, SECTION_COMPLETED ; "completed_label")]
    fn status_maps_to_section_label(status: SessionStatus, expected: &str) {
        assert_eq!(section_label(status), expected);
    }

    fn press(dash: &mut SessionDashboard, code: KeyCode) -> DashboardAction {
        dash.handle_key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    #[test]
    fn typing_and_enter_pass_through_to_shared_input() {
        let mut dash = SessionDashboard::new();
        // Printable keys and Enter are not dashboard-navigation, so they are
        // passed through to the App's shared input box.
        assert!(matches!(
            press(&mut dash, KeyCode::Char('x')),
            DashboardAction::Passthrough(_)
        ));
        assert!(matches!(
            press(&mut dash, KeyCode::Enter),
            DashboardAction::Passthrough(_)
        ));
    }

    #[test]
    fn navigation_keys_stay_in_dashboard() {
        let mut dash = SessionDashboard::new();
        assert!(matches!(
            press(&mut dash, KeyCode::Up),
            DashboardAction::Consumed
        ));
        assert!(matches!(
            press(&mut dash, KeyCode::Down),
            DashboardAction::Consumed
        ));
        // Right with no selection is consumed (nothing to open).
        assert!(matches!(
            press(&mut dash, KeyCode::Right),
            DashboardAction::Consumed
        ));
    }
}
