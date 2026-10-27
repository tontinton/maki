use std::collections::HashMap;
use std::sync::Arc;

use crate::app::tasks::count_phrase;
use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::repaint::Cadence;

use crossterm::event::{KeyCode, KeyEvent};
use maki_providers::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Rewind ";
const PREVIEW_MAX_LEN: usize = 80;
pub(crate) const NO_TURNS_MSG: &str = "No user turns to rewind to";
const FOOTER: &[(&str, &str)] = &[("Enter", "rewind"), (key::FORK.label, "fork")];
const STOPS_PREFIX: &str = "This stops ";
const ONE_TASK: &str = "1 background task";
const MANY_TASKS_SUFFIX: &str = " background tasks";
const CONFIRM_SUFFIX: &str = " · enter again to rewind";

/// Of the {running} tasks, by id and name, those a rewind to {turn_index}
/// stops: the ones whose receipt, the call that moved, leaves history with
/// it. A task no call in history names is not a moved call, and stays.
pub(crate) fn stopped_by_rewind<'a>(
    messages: &[Message],
    turn_index: usize,
    running: &'a [(Arc<str>, String)],
) -> Vec<&'a (Arc<str>, String)> {
    running
        .iter()
        .filter(|(id, _)| {
            messages
                .iter()
                .position(|m| m.tool_uses().any(|(tool_id, _, _)| tool_id == &**id))
                .is_some_and(|at| at >= turn_index)
        })
        .collect()
}

fn stop_notice(stopped: &[&(Arc<str>, String)]) -> String {
    let count = count_phrase(stopped.len(), ONE_TASK, MANY_TASKS_SUFFIX);
    let names: Vec<&str> = stopped.iter().map(|(_, name)| name.as_str()).collect();
    format!(
        "{STOPS_PREFIX}{count} ({}){CONFIRM_SUFFIX}",
        names.join(", ")
    )
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

/// A rewind that would stop background tasks says so on the first Enter and
/// acts on the second, the way quitting does.
pub struct RewindPicker {
    picker: ListPicker<RewindEntry>,
    /// What each turn that stops tasks says, keyed by `turn_index`.
    stops: HashMap<usize, String>,
    armed: Option<usize>,
}

impl RewindPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_footer(FOOTER),
            stops: HashMap::new(),
            armed: None,
        }
    }

    pub fn open(
        &mut self,
        messages: &[Message],
        running: &[(Arc<str>, String)],
    ) -> Result<(), String> {
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
        self.stops = entries
            .iter()
            .filter_map(|entry| {
                let stopped = stopped_by_rewind(messages, entry.turn_index, running);
                (!stopped.is_empty()).then(|| (entry.turn_index, stop_notice(&stopped)))
            })
            .collect();
        self.armed = None;
        self.picker.set_error_text(None);
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
        self.disarm_if_moved();
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let handled = self.picker.handle_paste(text);
        self.disarm_if_moved();
        handled
    }

    /// The confirmation is for the entry it was shown on. Coming back to
    /// that entry asks again rather than rewinding on the first Enter.
    fn disarm_if_moved(&mut self) {
        let selected = self.picker.selected_item().map(|entry| entry.turn_index);
        if self.armed.is_some() && self.armed != selected {
            self.armed = None;
            self.picker.set_alert_text(None);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> RewindPickerAction {
        if key::FORK.matches(key) {
            return self
                .picker
                .take_selected()
                .map_or(RewindPickerAction::Consumed, RewindPickerAction::Fork);
        }
        if key.code == KeyCode::Enter
            && let Some(turn) = self.picker.selected_item().map(|entry| entry.turn_index)
            && self.armed != Some(turn)
            && let Some(notice) = self.stops.get(&turn)
        {
            self.armed = Some(turn);
            self.picker.set_alert_text(Some(notice.clone()));
            return RewindPickerAction::Consumed;
        }
        let action = self.picker.handle_key(key);
        self.disarm_if_moved();
        match action {
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
    fn is_prompting(&self) -> bool {
        false
    }

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
    use crossterm::event::KeyModifiers;
    use maki_providers::ContentBlock;
    use test_case::test_case;

    const RECEIPT_ID: &str = "toolu_moved";
    const TASK_NAME: &str = "cargo test";

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
        assert_eq!(picker.open(msgs, &[]), Err(NO_TURNS_MSG.into()));
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
        picker.open(&msgs, &[]).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("third"));
        assert_eq!(item.turn_index, 4);
    }

    #[test]
    fn long_prompt_is_truncated_in_preview() {
        let mut picker = RewindPicker::new();
        let long_text = "a".repeat(120);
        picker.open(&[user_msg(&long_text)], &[]).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().ends_with("..."));
        assert!(item.label().len() < 90);
        assert_eq!(item.prompt_text, long_text);
    }

    #[test]
    fn multiline_prompt_uses_first_line_for_preview() {
        let mut picker = RewindPicker::new();
        picker
            .open(&[user_msg("first line\nsecond line")], &[])
            .unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("first line"));
        assert!(!item.label().contains("second"));
        assert_eq!(item.prompt_text, "first line\nsecond line");
    }

    #[test]
    fn display_text_overrides_content() {
        let mut picker = RewindPicker::new();
        let msg = Message::user_display("ai sees this".into(), "user typed this".into());
        picker.open(&[msg], &[]).unwrap();
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
        picker.open(&msgs, &[]).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("real prompt"));
        assert_eq!(item.turn_index, 1);
    }

    fn context_update() -> Message {
        Message::context_update("plan mode".into(), Default::default())
    }

    /// Left behind, an update would sit in front of whatever is sent next,
    /// and rewinding to the first prompt would not empty the session.
    #[test_case(vec![context_update(), user_msg("first")], 0 ; "first_prompt")]
    #[test_case(vec![user_msg("first"), assistant_msg(), Message::observation("note".into()), context_update(), context_update(), user_msg("second")], 3 ; "after_an_observation")]
    #[test_case(vec![context_update(), user_msg("first"), assistant_msg(), user_msg("second")], 3 ; "earlier_update_stays")]
    fn rewind_takes_the_updates_sent_with_the_prompt(msgs: Vec<Message>, expected: usize) {
        let mut picker = RewindPicker::new();
        picker.open(&msgs, &[]).unwrap();
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
        picker.open(&msgs, &[]).unwrap();
        let top = picker.picker.selected_item().unwrap();
        assert!(top.label().starts_with("2: second"));
    }

    #[test]
    fn fork_key_takes_the_selected_entry_and_closes() {
        let mut picker = RewindPicker::new();
        picker
            .open(
                &[user_msg("first"), assistant_msg(), user_msg("second")],
                &[],
            )
            .unwrap();
        let RewindPickerAction::Fork(entry) = picker.handle_key(key::FORK.to_key_event()) else {
            panic!("the fork key commits the selection as a fork");
        };
        assert_eq!(entry.turn_index, 2);
        assert!(!picker.is_open());
    }

    fn press(picker: &mut RewindPicker, code: KeyCode) -> RewindPickerAction {
        picker.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn picker_with_receipt_in_first_turn() -> RewindPicker {
        let mut picker = RewindPicker::new();
        let receipt = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                RECEIPT_ID,
                "bash",
                serde_json::json!({}),
            )],
            ..Default::default()
        };
        let running = [(Arc::from(RECEIPT_ID), TASK_NAME.to_owned())];
        picker
            .open(&[user_msg("first"), receipt, user_msg("second")], &running)
            .unwrap();
        picker
    }

    /// The receipt sits in the first turn, so only rewinding to that turn
    /// stops the task, and only that one asks first.
    #[test]
    fn a_rewind_that_stops_a_task_asks_before_acting() {
        let mut picker = picker_with_receipt_in_first_turn();
        assert_eq!(
            picker.stops,
            HashMap::from([(
                0,
                format!("{STOPS_PREFIX}{ONE_TASK} ({TASK_NAME}){CONFIRM_SUFFIX}")
            )])
        );

        press(&mut picker, KeyCode::Down);
        assert!(matches!(
            press(&mut picker, KeyCode::Enter),
            RewindPickerAction::Consumed
        ));
        assert!(picker.is_open());
        assert!(matches!(
            press(&mut picker, KeyCode::Enter),
            RewindPickerAction::Select(entry) if entry.turn_index == 0
        ));
    }

    #[test]
    fn moving_off_the_armed_entry_asks_again_on_return() {
        let mut picker = picker_with_receipt_in_first_turn();
        press(&mut picker, KeyCode::Down);
        press(&mut picker, KeyCode::Enter);
        assert_eq!(picker.armed, Some(0));

        press(&mut picker, KeyCode::Up);
        assert_eq!(picker.armed, None);
        assert_eq!(picker.picker.alert_text(), None);

        press(&mut picker, KeyCode::Down);
        assert!(matches!(
            press(&mut picker, KeyCode::Enter),
            RewindPickerAction::Consumed
        ));
        assert!(picker.is_open());
    }
}
