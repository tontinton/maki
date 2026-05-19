use crate::theme::Theme;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

pub(crate) fn render_form(
    t: &Theme,
    title: &str,
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    scroll: (u16, u16),
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.panel_border)
        .title_top(Line::from(title.to_string()).left_aligned())
        .title_style(t.panel_title);

    let paragraph = Paragraph::new(lines)
        .style(Style::new().fg(t.foreground))
        .wrap(Wrap { trim: false })
        .block(block)
        .scroll(scroll);

    frame.render_widget(paragraph, area);
}

pub(crate) fn selected_prefix(t: &Theme, is_selected: bool) -> (&'static str, Style) {
    if is_selected {
        ("▸ ", t.form_active)
    } else {
        ("  ", Style::new().fg(t.foreground))
    }
}
