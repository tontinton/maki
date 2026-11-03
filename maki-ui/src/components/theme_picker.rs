use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction};
use crate::repaint::Cadence;
use crate::theme;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

const TITLE: &str = " Themes ";
const MAX_VISIBLE: u16 = 15;

pub enum ThemePickerAction {
    Consumed,
    Closed,
}

pub struct ThemePicker {
    picker: ListPicker<String>,
    original_theme_name: Option<String>,
}

impl ThemePicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_max_visible(MAX_VISIBLE),
            original_theme_name: None,
        }
    }

    pub fn open(&mut self) {
        let current_name = theme::current_theme_name();
        let entries = theme::all_theme_names();
        let current_idx = entries
            .iter()
            .position(|name| *name == current_name)
            .unwrap_or(0);
        self.original_theme_name = Some(current_name);
        self.picker.open(entries, TITLE);
        self.picker.select(current_idx);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
        self.original_theme_name = None;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ThemePickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Consumed => {
                self.apply_preview();
                ThemePickerAction::Consumed
            }
            PickerAction::Select(name) => {
                theme::persist_theme(&name);
                self.original_theme_name = None;
                ThemePickerAction::Closed
            }
            PickerAction::Close => {
                self.restore_original();
                self.original_theme_name = None;
                ThemePickerAction::Closed
            }
            PickerAction::Toggle(..) => ThemePickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> (Rect, Option<Position>) {
        self.picker.view(frame, area)
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.picker.handle_paste(text);
        self.apply_preview();
    }

    fn apply_preview(&self) {
        if let Some(name) = self.picker.selected_item()
            && let Ok(t) = theme::load_by_name(name)
        {
            theme::set(t);
        }
    }

    fn restore_original(&self) {
        if let Some(ref name) = self.original_theme_name
            && let Ok(t) = theme::load_by_name(name)
        {
            theme::set(t);
        }
    }
}

impl Overlay for ThemePicker {
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
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::KeyCode;
    use test_case::test_case;

    const THEME_FILTER: &str = "ayu_light";
    const NOT_PREVIEWED: &str = "narrowing the list has to preview a theme";

    #[test_case(|p: &mut ThemePicker| {
        for c in THEME_FILTER.chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
    } ; "typed")]
    #[test_case(|p: &mut ThemePicker| p.handle_paste(THEME_FILTER) ; "pasted")]
    fn narrowing_the_list_previews_a_theme(filter: fn(&mut ThemePicker)) {
        let mut p = ThemePicker::new();
        p.open();
        let generation_before = theme::generation();

        filter(&mut p);

        assert!(theme::generation() > generation_before, "{NOT_PREVIEWED}");
    }

    #[test]
    fn enter_closes() {
        let mut p = ThemePicker::new();
        p.open();
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, ThemePickerAction::Closed));
        assert!(!p.is_open());
    }

    #[test_case(key(KeyCode::Esc) ; "escape_restores_and_closes")]
    #[test_case(kb::QUIT.to_key_event() ; "ctrl_c_restores_and_closes")]
    fn cancel_restores(cancel_key: crossterm::event::KeyEvent) {
        let mut p = ThemePicker::new();
        p.open();
        p.handle_key(key(KeyCode::Down));
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ThemePickerAction::Closed));
        assert!(!p.is_open());
    }
}
