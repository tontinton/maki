use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

use crossterm::event::KeyEvent;
use maki_providers::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Rewind ";
const PREVIEW_MAX_LEN: usize = 80;
pub(crate) const NO_TURNS_MSG: &str = "No user turns to rewind to";

pub enum RewindPickerAction {
    Consumed,
    Select(RewindEntry),
    Close,
}

pub struct RewindEntry {
    pub turn_index: usize,
    pub prompt_preview: String,
    pub prompt_text: String,
}

impl PickerItem for RewindEntry {
    fn label(&self) -> &str {
        &self.prompt_preview
    }
}

pub struct RewindPicker {
    picker: ListPicker<RewindEntry>,
}

impl RewindPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new(),
        }
    }

    pub fn open(&mut self, messages: &[Message]) -> Result<(), String> {
        let mut turn_num = 0usize;
        let mut entries: Vec<RewindEntry> = Vec::new();
        for (msg_idx, msg) in messages.iter().enumerate() {
            if !matches!(msg.role, Role::User) {
                continue;
            }
            let Some(full_text) = msg.user_text() else {
                continue;
            };
            turn_num += 1;
            let first_line = full_text.lines().next().unwrap_or("");
            let preview = if first_line.len() > PREVIEW_MAX_LEN {
                format!(
                    "{turn_num}: {}...",
                    &first_line[..first_line.floor_char_boundary(PREVIEW_MAX_LEN)]
                )
            } else {
                format!("{turn_num}: {first_line}")
            };
            entries.push(RewindEntry {
                turn_index: msg_idx,
                prompt_preview: preview,
                prompt_text: full_text.to_owned(),
            });
        }
        if entries.is_empty() {
            return Err(NO_TURNS_MSG.into());
        }
        entries.reverse();
        self.picker.open(entries, TITLE);
        Ok(())
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

    pub fn handle_key(&mut self, key: KeyEvent) -> RewindPickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Consumed => RewindPickerAction::Consumed,
            PickerAction::Select(entry) => RewindPickerAction::Select(entry),
            PickerAction::Close => RewindPickerAction::Close,
            PickerAction::Toggle(..) => RewindPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for RewindPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_providers::ContentBlock;
    use test_case::test_case;

    fn user_msg(text: &str) -> Message {
        Message::user(text.into())
    }

    fn assistant_msg() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response".into(),
            }],
            ..Default::default()
        }
    }

    #[test_case(&[]                                          ; "empty_messages")]
    #[test_case(&[assistant_msg()]                            ; "no_user_turns")]
    #[test_case(&[Message::synthetic("continue".into())]     ; "only_synthetic")]
    fn open_without_user_turns_returns_error(msgs: &[Message]) {
        let mut picker = RewindPicker::new();
        assert_eq!(picker.open(msgs), Err(NO_TURNS_MSG.into()));
    }

    #[test]
    fn entries_are_in_reverse_order() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            user_msg("first"),
            assistant_msg(),
            user_msg("second"),
            assistant_msg(),
            user_msg("third"),
        ];
        picker.open(&msgs).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("third"));
        assert_eq!(item.turn_index, 4);
    }

    #[test]
    fn long_prompt_is_truncated_in_preview() {
        let mut picker = RewindPicker::new();
        let long_text = "a".repeat(120);
        picker.open(&[user_msg(&long_text)]).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().ends_with("..."));
        assert!(item.label().len() < 90);
        assert_eq!(item.prompt_text, long_text);
    }

    #[test]
    fn multiline_prompt_uses_first_line_for_preview() {
        let mut picker = RewindPicker::new();
        picker.open(&[user_msg("first line\nsecond line")]).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("first line"));
        assert!(!item.label().contains("second"));
        assert_eq!(item.prompt_text, "first line\nsecond line");
    }

    #[test]
    fn display_text_overrides_content() {
        let mut picker = RewindPicker::new();
        let msg = Message::user_display("ai sees this".into(), "user typed this".into());
        picker.open(&[msg]).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("user typed this"));
        assert_eq!(item.prompt_text, "user typed this");
    }

    #[test]
    fn synthetic_messages_are_excluded() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            user_msg("real prompt"),
            assistant_msg(),
            Message::synthetic("[Cancelled by user]".into()),
        ];
        picker.open(&msgs).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("real prompt"));
        assert_eq!(item.turn_index, 0);
    }

    #[test]
    fn turn_numbers_skip_synthetic() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            user_msg("first"),
            assistant_msg(),
            Message::synthetic("continue".into()),
            assistant_msg(),
            user_msg("second"),
        ];
        picker.open(&msgs).unwrap();
        let top = picker.picker.selected_item().unwrap();
        assert!(top.label().starts_with("2: second"));
    }
}
