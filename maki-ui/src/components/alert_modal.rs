use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::{Overlay, hint_line};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

const TITLE: &str = " Action Required ";
const HINTS: &[(&str, &str)] = &[("Enter/Esc", "Dismiss")];
const H_PAD: u16 = 2;
const BORDER_WIDTH: u16 = 2;
const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 80;

/// A message too important for a flash: it stays up until dismissed.
pub struct AlertModal {
    message: Option<String>,
}

impl AlertModal {
    pub fn new() -> Self {
        Self { message: None }
    }

    pub fn open(&mut self, message: String) {
        self.message = Some(message);
    }

    pub fn is_open(&self) -> bool {
        self.message.is_some()
    }

    pub fn close(&mut self) {
        self.message = None;
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        if matches!(key_event.code, KeyCode::Esc | KeyCode::Enter) || key::QUIT.matches(key_event) {
            self.close();
        }
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) -> Rect {
        let Some(message) = &self.message else {
            return Rect::default();
        };
        let lines: Vec<Line> = [Line::default()]
            .into_iter()
            .chain(message.lines().map(Line::raw))
            .chain([Line::default(), hint_line(HINTS), Line::default()])
            .collect();
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let padded_width = (area.width as u32 * WIDTH_PERCENT as u32 / 100)
            .saturating_sub((BORDER_WIDTH + H_PAD * 2) as u32) as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, paragraph.line_count(padded_width) as u16);
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        frame.render_widget(paragraph, padded);
        popup
    }
}

impl Overlay for AlertModal {
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
    use crate::components::key as key_ev;
    use test_case::test_case;

    const MESSAGE: &str = "provider scripts no longer run";

    fn opened() -> AlertModal {
        let mut modal = AlertModal::new();
        modal.open(MESSAGE.to_owned());
        modal
    }

    #[test_case(key_ev(KeyCode::Esc)     ; "esc_dismisses")]
    #[test_case(key_ev(KeyCode::Enter)   ; "enter_dismisses")]
    #[test_case(key::QUIT.to_key_event() ; "ctrl_c_dismisses")]
    fn handle_key_dismisses(k: KeyEvent) {
        let mut modal = opened();
        modal.handle_key(k);
        assert!(!modal.is_open());
    }

    #[test]
    fn other_keys_keep_it_open() {
        let mut modal = opened();
        modal.handle_key(key_ev(KeyCode::Char('a')));
        assert!(modal.is_open());
    }
}
