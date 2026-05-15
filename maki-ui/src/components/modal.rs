use crate::theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Clear};

pub const CHROME_LINES: u16 = 2;

pub struct Modal<'a> {
    pub title: &'a str,
    pub width_percent: u16,
    pub max_height_percent: u16,
}

impl Modal<'_> {
    pub fn render(&self, frame: &mut Frame, area: Rect, content_height: u16) -> (Rect, Rect) {
        let max_h = (area.height as u32 * self.max_height_percent as u32 / 100) as u16;
        let total_h = (content_height + CHROME_LINES)
            .min(max_h)
            .max(CHROME_LINES + 1);

        let [popup] = Layout::vertical([Constraint::Length(total_h)])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::horizontal([Constraint::Percentage(self.width_percent)])
            .flex(Flex::Center)
            .areas(popup);

        frame.render_widget(Clear, popup);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::current().panel_border)
            .title(self.title)
            .title_style(theme::current().panel_title)
            .style(Style::new().bg(theme::current().background));

        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        (popup, inner)
    }
}
