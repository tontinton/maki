use crate::theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear};

/// The border, in cells, on each axis.
pub const BORDER: u16 = 2;

fn percent_of(total: u16, percent: u16) -> u16 {
    (u32::from(total) * u32::from(percent) / 100) as u16
}

/// The rect inside `rect` after `pad` on each side.
pub fn inset(rect: Rect, pad: u16) -> Rect {
    Rect {
        x: rect.x + pad,
        width: rect.width.saturating_sub(pad * 2),
        ..rect
    }
}

pub struct Modal<'a> {
    pub title: &'a str,
    /// A floor, not the size: the content and `min_width` can widen the popup.
    pub width_percent: u16,
    pub min_width: u16,
    /// A ceiling, not the size: `render` shrinks the popup to the content.
    pub max_height_percent: u16,
}

impl Modal<'_> {
    pub fn width(&self, screen: u16) -> u16 {
        percent_of(screen, self.width_percent)
            .max(self.min_width)
            .min(screen)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, content_height: u16) -> (Rect, Rect) {
        self.render_with_footer(frame, area, content_height, None)
    }

    /// Keybind hints go on the bottom border, like the Lua floats draw them,
    /// so they cost no content row.
    pub fn render_with_footer(
        &self,
        frame: &mut Frame,
        area: Rect,
        content_height: u16,
        footer: Option<Line<'_>>,
    ) -> (Rect, Rect) {
        let total_h = (content_height + BORDER)
            .min(percent_of(area.height, self.max_height_percent))
            .max(BORDER + 1);

        let [popup] = Layout::vertical([Constraint::Length(total_h)])
            .flex(Flex::Center)
            .areas(area);
        let [popup] = Layout::horizontal([Constraint::Length(self.width(area.width))])
            .flex(Flex::Center)
            .areas(popup);

        frame.render_widget(Clear, popup);

        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::current().panel_border)
            .title(self.title)
            .title_style(theme::current().panel_title)
            .style(Style::new().bg(theme::current().background));
        if let Some(footer) = footer {
            block = block.title_bottom(footer);
        }

        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        (popup, inner)
    }
}
