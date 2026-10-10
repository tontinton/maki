use crate::components::Overlay;
use crate::components::keybindings::Bind;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_providers::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

const TITLE: &str = " Rewind ";
const PREVIEW_MAX_LEN: usize = 80;
pub(crate) const NO_TURNS_MSG: &str = "No user turns to rewind to";
const FORK_KEY: Bind = Bind {
    code: KeyCode::Char('f'),
    modifiers: KeyModifiers::CONTROL,
    label: "Ctrl+F",
};

fn footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::raw(" "),
        Span::styled("Enter", t.keybind_key),
        Span::styled(" rewind", t.tool_dim),
        Span::raw(" "),
        Span::styled("Ctrl+F", t.keybind_key),
        Span::styled(" fork", t.tool_dim),
        Span::raw(" "),
    ])
    .right_aligned()
}

pub enum RewindPickerAction {
    Consumed,
    Select(RewindEntry),
    Fork(RewindEntry),
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
            picker: ListPicker::new().with_footer_builder(footer_line),
        }
    }

    pub fn open(&mut self, messages: &[Message]) -> Result<(), String> {
        let mut turn_num = 0usize;
        let mut entries: Vec<RewindEntry> = Vec::new();
        // Context updates go out right before the prompt they came with, so
        // rewinding to the prompt takes them too, leaving the transcript as
        // it was before the prompt was sent.
        let mut updates_from = None;
        for (msg_idx, msg) in messages.iter().enumerate() {
            if msg.is_context_update() {
                updates_from.get_or_insert(msg_idx);
                continue;
            }
            let sent_from = updates_from.take().unwrap_or(msg_idx);
            if !matches!(msg.role, Role::User) || msg.is_from_host() {
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
                turn_index: sent_from,
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
        if FORK_KEY.matches(key) {
            return match self.picker.take_selected() {
                Some(entry) => RewindPickerAction::Fork(entry),
                None => RewindPickerAction::Consumed,
            };
        }
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

    fn cadence(&self) -> Cadence {
        self.picker.cadence()
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
    fn synthetic_messages_and_observations_are_excluded() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            Message::observation("build failed".into()),
            user_msg("real prompt"),
            assistant_msg(),
            Message::synthetic("[Cancelled by user]".into()),
        ];
        picker.open(&msgs).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("real prompt"));
        assert_eq!(item.turn_index, 1);
    }

    fn context_update() -> Message {
        Message::context_update("plan mode".into(), "plan mode".into(), Default::default())
    }

    /// Left behind, an update would sit in front of whatever is sent next,
    /// and rewinding to the first prompt would not empty the session.
    #[test_case(vec![context_update(), user_msg("first")], 0 ; "first_prompt")]
    #[test_case(vec![user_msg("first"), assistant_msg(), Message::observation("note".into()), context_update(), context_update(), user_msg("second")], 3 ; "after_an_observation")]
    #[test_case(vec![context_update(), user_msg("first"), assistant_msg(), user_msg("second")], 3 ; "earlier_update_stays")]
    fn rewind_takes_the_updates_sent_with_the_prompt(msgs: Vec<Message>, expected: usize) {
        let mut picker = RewindPicker::new();
        picker.open(&msgs).unwrap();
        assert_eq!(picker.picker.selected_item().unwrap().turn_index, expected);
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

    #[test]
    fn fork_key_takes_the_selected_entry_and_closes() {
        let mut picker = RewindPicker::new();
        picker
            .open(&[user_msg("first"), assistant_msg(), user_msg("second")])
            .unwrap();
        match picker.handle_key(FORK_KEY.to_key_event()) {
            RewindPickerAction::Fork(entry) => assert_eq!(entry.turn_index, 2),
            _ => panic!("Ctrl+F commits the selection as a fork"),
        }
        assert!(!picker.is_open(), "committing a fork closes the picker");
    }
}
