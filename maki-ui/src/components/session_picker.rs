use std::thread;

use crate::AppSession;
use crate::components::Overlay;
use crate::components::format_relative_time;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_storage::StateDir;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Sessions ";
const NO_SESSIONS_MSG: &str = "No previous sessions";
const FOOTER_HINTS: &[(&str, &str)] = &[("Enter", "open"), (key::DELETE.label, "delete")];

pub enum SessionPickerAction {
    Consumed,
    Select(String),
    ConfirmDelete,
    Delete(String),
    Close,
}

struct SessionEntry {
    id: String,
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
    confirming: Option<(String, u64)>,
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

    pub fn open(&mut self, cwd: &str, current_session_id: &str, dir: &StateDir) {
        self.picker.open_loading(TITLE);
        let cwd = cwd.to_owned();
        let current_session_id = current_session_id.to_owned();
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

    pub fn remove_entry(&mut self, id: &str) {
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
            return SessionPickerAction::Delete(selected.id.clone());
        }

        self.confirming = Some((selected.id.clone(), generation));
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
