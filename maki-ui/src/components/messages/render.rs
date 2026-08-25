use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

pub(super) struct RenderCursor {
    skip: u16,
    y: u16,
    bottom: u16,
    viewport: Rect,
}

impl RenderCursor {
    /// `skip` is the number of rows to drop from the first segment drawn, not
    /// a document offset.
    pub fn new(skip: u16, viewport: Rect) -> Self {
        Self {
            skip,
            y: viewport.y,
            bottom: viewport.y + viewport.height,
            viewport,
        }
    }

    pub fn past_bottom(&self) -> bool {
        self.y >= self.bottom
    }

    pub fn render(
        &mut self,
        lines: &[Line<'static>],
        h: u16,
        style: Option<Style>,
        highlight: bool,
        frame: &mut Frame,
    ) {
        if self.skip >= h {
            self.skip -= h;
            return;
        }
        if self.y >= self.bottom {
            return;
        }
        let visible_h = h
            .saturating_sub(self.skip)
            .min(self.bottom.saturating_sub(self.y));
        let seg_area = Rect::new(self.viewport.x, self.y, self.viewport.width, visible_h);
        let mut p = Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false });
        let mut base = style.unwrap_or_default();
        if highlight {
            base = base.add_modifier(Modifier::REVERSED);
        }
        p = p.style(base);
        if self.skip > 0 {
            p = p.scroll((self.skip, 0));
            self.skip = 0;
        }
        frame.render_widget(p, seg_area);
        self.y += visible_h;
    }
}
