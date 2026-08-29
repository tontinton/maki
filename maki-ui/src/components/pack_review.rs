use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_lua::PackPlan;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::components::Overlay;
use crate::components::form::render_form;
use crate::components::hint_line;
use crate::components::is_ctrl;
use crate::theme;

const REVIEW_HINTS: &[(&str, &str)] = &[("Enter / y", "Apply"), ("Esc / n", "Decline")];
const PACK_REVIEW_TITLE: &str = " Package Changes ";

pub(crate) enum PackReviewAction {
    Accept(PackPlan),
    Decline,
}

pub(crate) enum PackReview {
    Closed,
    Open { prompt: String, plan: PackPlan },
}

impl Overlay for PackReview {
    fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    fn is_modal(&self) -> bool {
        false
    }

    fn close(&mut self) {
        *self = Self::Closed;
    }
}

impl PackReview {
    pub fn new() -> Self {
        Self::Closed
    }

    pub fn open(&mut self, prompt: String, plan: PackPlan) {
        *self = Self::Open { prompt, plan };
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PackReviewAction> {
        if !self.is_open() {
            return None;
        }
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            self.close();
            return Some(PackReviewAction::Decline);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => match std::mem::replace(self, Self::Closed) {
                Self::Open { plan, .. } => Some(PackReviewAction::Accept(plan)),
                Self::Closed => None,
            },
            KeyCode::Char('n') | KeyCode::Esc => {
                self.close();
                Some(PackReviewAction::Decline)
            }
            _ => None,
        }
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let Self::Open { prompt, .. } = self else {
            return Vec::new();
        };
        let mut lines = vec![Line::raw("")];
        lines.extend(prompt.lines().map(|line| Line::raw(format!("  {line}"))));
        lines.push(Line::raw(""));
        lines.push(hint_line(REVIEW_HINTS));
        lines.push(Line::raw(""));
        lines
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_open() {
            return;
        }
        render_form(
            &theme::current(),
            PACK_REVIEW_TITLE,
            frame,
            area,
            self.lines(),
            (0, 0),
        );
    }

    pub fn height(&self, width: u16) -> u16 {
        let paragraph = Paragraph::new(self.lines()).wrap(Wrap { trim: false });
        paragraph.line_count(width.saturating_sub(2)) as u16 + 2
    }
}
