use crate::components::Overlay;
use crate::components::is_ctrl;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crossterm::event::{KeyCode, KeyEvent};
use maki_lua::{SelectEvent, SelectItem, SelectOpts};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const DEFAULT_TITLE: &str = " Select ";
const FOOTER_WITH_DELETE: &[(&str, &str)] = &[("Enter", "select"), (key::DELETE.label, "delete")];
const FOOTER_NO_DELETE: &[(&str, &str)] = &[("Enter", "select")];
const FLASH_CONFIRM_DELETE: &str = "Press Ctrl+D again to confirm delete";

pub enum LuaSelectAction {
    Consumed,
    Flash(&'static str),
}

impl PickerItem for SelectItem {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

pub struct LuaSelectModal {
    picker: ListPicker<SelectItem>,
    reply_tx: Option<flume::Sender<SelectEvent>>,
    has_on_delete: bool,
    confirming_idx: Option<usize>,
}

impl LuaSelectModal {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new(),
            reply_tx: None,
            has_on_delete: false,
            confirming_idx: None,
        }
    }

    pub fn open(
        &mut self,
        items: Vec<SelectItem>,
        opts: SelectOpts,
        reply_tx: flume::Sender<SelectEvent>,
    ) {
        self.close();

        self.has_on_delete = opts.has_on_delete;
        self.reply_tx = Some(reply_tx);
        self.confirming_idx = None;

        let picker = if !opts.footer.is_empty() {
            ListPicker::new().with_footer_owned(opts.footer)
        } else if opts.has_on_delete {
            ListPicker::new().with_footer(FOOTER_WITH_DELETE)
        } else {
            ListPicker::new().with_footer(FOOTER_NO_DELETE)
        };
        self.picker = picker;

        let title = if opts.title.is_empty() {
            DEFAULT_TITLE.to_string()
        } else {
            opts.title
        };
        self.picker.open(items, title);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(SelectEvent::Close);
        }
        self.picker.close();
        self.confirming_idx = None;
        self.has_on_delete = false;
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

    pub fn handle_key(&mut self, key: KeyEvent) -> LuaSelectAction {
        if self.has_on_delete && is_ctrl(&key) && key.code == KeyCode::Char('d') {
            return self.handle_delete_key();
        }

        if is_ctrl(&key) && key.code == KeyCode::Char('o') {
            return self.handle_open_editor();
        }

        self.confirming_idx = None;

        match self.picker.handle_key(key) {
            PickerAction::Select(idx, _) => self.send_choice(idx),
            PickerAction::Close => self.close(),
            PickerAction::Consumed | PickerAction::Toggle(..) => {}
        }
        LuaSelectAction::Consumed
    }

    fn send_choice(&mut self, index: usize) {
        self.send_and_close(SelectEvent::Choice { index });
    }

    fn send_and_close(&mut self, event: SelectEvent) {
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(event);
        }
        self.picker.close();
    }

    fn handle_delete_key(&mut self) -> LuaSelectAction {
        let Some(idx) = self.picker.selected_index() else {
            return LuaSelectAction::Consumed;
        };

        if self.confirming_idx == Some(idx) {
            self.send_and_close(SelectEvent::Delete { index: idx });
            self.confirming_idx = None;
            return LuaSelectAction::Consumed;
        }

        self.confirming_idx = Some(idx);
        LuaSelectAction::Flash(FLASH_CONFIRM_DELETE)
    }

    fn handle_open_editor(&mut self) -> LuaSelectAction {
        let Some(idx) = self.picker.selected_index() else {
            return LuaSelectAction::Consumed;
        };
        self.send_and_close(SelectEvent::OpenEditor { index: idx });
        LuaSelectAction::Consumed
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Drop for LuaSelectModal {
    fn drop(&mut self) {
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(SelectEvent::Close);
        }
    }
}

impl Overlay for LuaSelectModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_ev;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_lua::{SelectEvent, SelectItem, SelectOpts};
    use test_case::test_case;

    fn ctrl_d() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
    }

    fn ctrl_o() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)
    }

    fn sample_items() -> Vec<SelectItem> {
        vec![
            SelectItem {
                label: "alpha".into(),
                detail: Some("first".into()),
            },
            SelectItem {
                label: "beta".into(),
                detail: None,
            },
        ]
    }

    fn opts_with_delete() -> SelectOpts {
        SelectOpts {
            title: " Test ".into(),
            has_on_delete: true,
            footer: vec![],
        }
    }

    fn opts_no_delete() -> SelectOpts {
        SelectOpts {
            title: " Test ".into(),
            has_on_delete: false,
            footer: vec![],
        }
    }

    fn open_modal(
        modal: &mut LuaSelectModal,
        items: Vec<SelectItem>,
        opts: SelectOpts,
    ) -> flume::Receiver<SelectEvent> {
        let (tx, rx) = flume::bounded(8);
        modal.open(items, opts, tx);
        rx
    }

    #[test]
    fn enter_sends_choice_event() {
        let mut modal = LuaSelectModal::new();
        let rx = open_modal(&mut modal, sample_items(), opts_no_delete());

        modal.handle_key(key_ev(KeyCode::Down));
        modal.handle_key(key_ev(KeyCode::Enter));

        assert!(!modal.is_open());
        match rx.try_recv().unwrap() {
            SelectEvent::Choice { index } => assert_eq!(index, 1),
            other => panic!("expected Choice, got {}", select_event_name(&other)),
        }
    }

    #[test]
    fn ctrl_d_confirm_flow_sends_delete() {
        let mut modal = LuaSelectModal::new();
        let rx = open_modal(&mut modal, sample_items(), opts_with_delete());

        let action = modal.handle_key(ctrl_d());
        assert!(modal.is_open());
        assert!(matches!(action, LuaSelectAction::Flash(_)));

        modal.handle_key(ctrl_d());
        assert!(!modal.is_open());

        match rx.try_recv().unwrap() {
            SelectEvent::Delete { index } => assert_eq!(index, 0),
            other => panic!("expected Delete, got {}", select_event_name(&other)),
        }
    }

    #[test]
    fn ctrl_d_ignored_without_on_delete() {
        let mut modal = LuaSelectModal::new();
        let _rx = open_modal(&mut modal, sample_items(), opts_no_delete());

        modal.handle_key(ctrl_d());
        assert!(modal.is_open());
    }

    #[test]
    fn drop_sends_close_event() {
        let rx;
        {
            let mut modal = LuaSelectModal::new();
            rx = open_modal(&mut modal, sample_items(), opts_no_delete());
        }
        match rx.try_recv().unwrap() {
            SelectEvent::Close => {}
            other => panic!("expected Close, got {}", select_event_name(&other)),
        }
    }

    #[test]
    fn reopen_sends_close_on_old_reply() {
        let mut modal = LuaSelectModal::new();
        let rx1 = open_modal(&mut modal, sample_items(), opts_no_delete());
        let _rx2 = open_modal(&mut modal, sample_items(), opts_no_delete());

        match rx1.try_recv().unwrap() {
            SelectEvent::Close => {}
            other => panic!(
                "expected Close on old rx, got {}",
                select_event_name(&other)
            ),
        }
        assert!(modal.is_open());
    }

    #[test_case(key_ev(KeyCode::Enter), false ; "enter_on_empty")]
    #[test_case(ctrl_d(), true ; "ctrl_d_on_empty")]
    #[test_case(ctrl_o(), false ; "ctrl_o_on_empty")]
    fn action_on_empty_list_is_consumed(key: KeyEvent, with_delete: bool) {
        let mut modal = LuaSelectModal::new();
        let opts = if with_delete {
            opts_with_delete()
        } else {
            opts_no_delete()
        };
        let _rx = open_modal(&mut modal, vec![], opts);

        modal.handle_key(key);
        assert!(modal.is_open());
    }

    #[test]
    fn moving_selection_resets_confirm() {
        let mut modal = LuaSelectModal::new();
        let _rx = open_modal(&mut modal, sample_items(), opts_with_delete());

        modal.handle_key(ctrl_d());
        modal.handle_key(key_ev(KeyCode::Down));
        let action = modal.handle_key(ctrl_d());
        assert!(matches!(action, LuaSelectAction::Flash(_)));
    }

    fn select_event_name(event: &SelectEvent) -> &'static str {
        match event {
            SelectEvent::Choice { .. } => "Choice",
            SelectEvent::Delete { .. } => "Delete",
            SelectEvent::OpenEditor { .. } => "OpenEditor",
            SelectEvent::Close => "Close",
        }
    }

    #[test]
    fn choice_after_close_is_noop() {
        let mut modal = LuaSelectModal::new();
        let rx = open_modal(&mut modal, sample_items(), opts_no_delete());

        modal.close();
        match rx.try_recv().unwrap() {
            SelectEvent::Close => {}
            other => panic!("expected Close, got {}", select_event_name(&other)),
        }

        modal.handle_key(key_ev(KeyCode::Enter));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn esc_closes_modal_sends_close_event() {
        let mut modal = LuaSelectModal::new();
        let rx = open_modal(&mut modal, sample_items(), opts_no_delete());

        modal.handle_key(key_ev(KeyCode::Esc));

        assert!(!modal.is_open());
        match rx.try_recv().unwrap() {
            SelectEvent::Close => {}
            other => panic!("expected Close, got {}", select_event_name(&other)),
        }
    }

    #[test]
    fn ctrl_o_sends_open_editor_event() {
        let mut modal = LuaSelectModal::new();
        let rx = open_modal(&mut modal, sample_items(), opts_no_delete());

        modal.handle_key(ctrl_o());

        assert!(!modal.is_open());
        match rx.try_recv().unwrap() {
            SelectEvent::OpenEditor { index } => assert_eq!(index, 0),
            other => panic!("expected OpenEditor, got {}", select_event_name(&other)),
        }
    }
}
