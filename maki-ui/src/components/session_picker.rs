use std::thread;

use crate::AppSession;
use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::Timestamp;
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Sessions ";
const NO_SESSIONS_MSG: &str = "No previous sessions";
const FOOTER_HINTS: &[(&str, &str)] = &[("Enter", "open"), (key::DELETE.label, "delete")];

pub enum SessionPickerAction {
    Consumed,
    Select(MakiId),
    ConfirmDelete,
    Delete(MakiId),
    Close,
}

struct SessionEntry {
    id: MakiId,
    title: String,
    relative_time: String,
}

impl PickerItem for SessionEntry {
    fn label(&self) -> &str {
        &self.title
    }
    fn detail(&self) -> Option<&str> {
        Some(&self.relative_time)
    }
}

pub struct SessionPicker {
    picker: ListPicker<SessionEntry>,
    confirming: Option<(MakiId, u64)>,
    pending_rx: Option<flume::Receiver<Result<Vec<SessionEntry>, String>>>,
    flash: Option<String>,
}

impl SessionPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_footer(FOOTER_HINTS),
            confirming: None,
            pending_rx: None,
            flash: None,
        }
    }

    pub fn open(&mut self, cwd: &str, current_session_id: MakiId, dir: &StateDir) {
        self.picker.open_loading(TITLE);
        let cwd = cwd.to_owned();
        let dir = dir.clone();
        let (tx, rx) = flume::bounded(1);
        thread::spawn(move || {
            let result = AppSession::list(&cwd, &dir)
                .map(|summaries| {
                    summaries
                        .into_iter()
                        .filter(|s| s.id != current_session_id)
                        .map(|s| SessionEntry {
                            id: s.id,
                            title: s.title,
                            relative_time: format_relative_time(s.updated_at),
                        })
                        .collect()
                })
                .map_err(|e| format!("Failed to list sessions: {e}"));
            let _ = tx.send(result);
        });
        self.pending_rx = Some(rx);
    }

    fn try_resolve(&mut self) {
        let Some(ref rx) = self.pending_rx else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.pending_rx = None;
        match result {
            Ok(entries) if entries.is_empty() => {
                self.picker.close();
                self.flash = Some(NO_SESSIONS_MSG.into());
            }
            Ok(entries) => {
                self.picker.resolve(entries);
            }
            Err(e) => {
                self.picker.close();
                self.flash = Some(e);
            }
        }
    }

    pub fn take_flash(&mut self) -> Option<String> {
        self.flash.take()
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn is_loading(&self) -> bool {
        self.picker.is_loading()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.pending_rx = None;
    }

    pub fn remove_entry(&mut self, id: MakiId) {
        self.picker.retain(|e| e.id != id);
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SessionPickerAction {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && key.code == KeyCode::Char('d')
        {
            return self.handle_delete_key();
        }

        match self.picker.handle_key(key) {
            PickerAction::Consumed => SessionPickerAction::Consumed,
            PickerAction::Select(_, entry) => SessionPickerAction::Select(entry.id),
            PickerAction::Close => {
                self.pending_rx = None;
                SessionPickerAction::Close
            }
            PickerAction::Toggle(..) => SessionPickerAction::Consumed,
        }
    }

    fn handle_delete_key(&mut self) -> SessionPickerAction {
        let Some(selected) = self.picker.selected_item() else {
            return SessionPickerAction::Consumed;
        };

        let generation = self.picker.generation();
        if self
            .confirming
            .as_ref()
            .is_some_and(|(id, g)| id == &selected.id && *g == generation)
        {
            return SessionPickerAction::Delete(selected.id);
        }

        self.confirming = Some((selected.id, generation));
        SessionPickerAction::ConfirmDelete
    }

    pub fn tick(&mut self) {
        self.try_resolve();
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for SessionPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn format_relative_time(epoch_secs: u64) -> String {
    let ts = Timestamp::from_second(epoch_secs as i64).unwrap_or(Timestamp::UNIX_EPOCH);
    let now = Timestamp::now();
    let secs = now.as_second().saturating_sub(ts.as_second()).max(0) as u64;
    humanize_secs(secs)
}

fn humanize_secs(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 3600;
    const DAY: u64 = 86400;
    const WEEK: u64 = 604800;
    const MONTH: u64 = 2592000;
    const YEAR: u64 = 31536000;

    match secs {
        0..MINUTE => "just now".into(),
        MINUTE..HOUR => format!("{}m ago", secs / MINUTE),
        HOUR..DAY => format!("{}h ago", secs / HOUR),
        DAY..WEEK => format!("{}d ago", secs / DAY),
        WEEK..MONTH => format!("{}w ago", secs / WEEK),
        MONTH..YEAR => format!("{}mo ago", secs / MONTH),
        _ => format!("{}y ago", secs / YEAR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(0, "just now" ; "below_minute")]
    #[test_case(60, "1m ago" ; "minute_boundary")]
    #[test_case(3600, "1h ago" ; "hour_boundary")]
    #[test_case(86400, "1d ago" ; "day_boundary")]
    #[test_case(604800, "1w ago" ; "week_boundary")]
    #[test_case(2592000, "1mo ago" ; "month_boundary")]
    #[test_case(31536000, "1y ago" ; "year_boundary")]
    fn relative_time_formatting(secs: u64, expected: &str) {
        assert_eq!(humanize_secs(secs), expected);
    }
}
