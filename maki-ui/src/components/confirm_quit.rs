use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::{Overlay, hint_line};

const TITLE: &str = " Quit Maki? ";
const HINTS: &[(&str, &str)] = &[("Enter / y", "Quit"), ("Esc / n", "Stay")];
const WIDTH_PERCENT: u16 = 50;
const MAX_HEIGHT_PERCENT: u16 = 50;

#[derive(Default)]
pub struct ConfirmQuit {
    open: bool,
}

impl ConfirmQuit {
    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key::CANCEL.matches(key) {
            self.open = false;
            return false;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            return false;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
                self.open = false;
                true
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.open = false;
                false
            }
            _ => false,
        }
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        let (popup, inner) = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        }
        .render(frame, area, 4);
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::raw("  Exit the application?"),
                Line::raw(""),
                hint_line(HINTS),
            ]),
            inner,
        );
        popup
    }
}

impl Overlay for ConfirmQuit {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.open = false;
    }
}
