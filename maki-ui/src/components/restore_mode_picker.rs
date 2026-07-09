//! Restore-mode picker (§7, §11): conversation / code / both.
//!
//! Opens after the tree selector commits a node selection. The rewind UI offers
//! three modes: `conversation` (move leaf only — C3 rewind, no code), `code`
//! (restore working tree to the node's snapshot, leave conversation), `both`.
//! `conversation` is the default (§7).

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use strum::EnumIter;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

const TITLE: &str = " Restore mode ";
const LABEL_CONVERSATION: &str = "Conversation (rewind chat only)";
const LABEL_CODE: &str = "Code (restore working tree)";
const LABEL_BOTH: &str = "Both (rewind chat + restore code)";

/// The rewind restore mode (§7). `Conversation` is the default — just the C3
/// leaf move; `Code`/`Both` additionally restore the working-tree snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum RestoreMode {
    Conversation,
    Code,
    Both,
}

impl RestoreMode {
    pub fn restores_code(self) -> bool {
        matches!(self, Self::Code | Self::Both)
    }
}

impl PickerItem for RestoreMode {
    fn label(&self) -> &str {
        match self {
            Self::Conversation => LABEL_CONVERSATION,
            Self::Code => LABEL_CODE,
            Self::Both => LABEL_BOTH,
        }
    }
}

pub enum RestoreModeAction {
    Consumed,
    Select(RestoreMode),
    Close,
}

pub struct RestoreModePicker {
    picker: ListPicker<RestoreMode>,
}

impl RestoreModePicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new(),
        }
    }

    pub fn open(&mut self) {
        use strum::IntoEnumIterator;
        let modes: Vec<RestoreMode> = RestoreMode::iter().collect();
        self.picker.open(modes, TITLE);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
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

    pub fn handle_key(&mut self, key: KeyEvent) -> RestoreModeAction {
        match self.picker.handle_key(key) {
            PickerAction::Consumed | PickerAction::Toggle(..) => RestoreModeAction::Consumed,
            PickerAction::Select(_, mode) => RestoreModeAction::Select(mode),
            PickerAction::Close => RestoreModeAction::Close,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for RestoreModePicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}
