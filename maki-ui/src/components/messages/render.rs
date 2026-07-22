use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};

use super::COPY_LABEL_WIDTH;
use super::segment::Surface;
use crate::theme;

const COPY_LABEL: &str = "[copy]";

pub(super) struct RenderCursor {
    skip: u16,
    y: u16,
    bottom: u16,
    viewport: Rect,
}

impl RenderCursor {
    pub fn new(scroll_top: u16, viewport: Rect) -> Self {
        Self {
            skip: scroll_top,
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
        surface: Surface,
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
        let segment_skip = self.skip;
        let visible_h = h
            .saturating_sub(segment_skip)
            .min(self.bottom.saturating_sub(self.y));
        let seg_area = Rect::new(self.viewport.x, self.y, self.viewport.width, visible_h);
        let mut base = style.unwrap_or_default();
        if highlight {
            base = base.add_modifier(Modifier::REVERSED);
        }
        let framed = surface.is_framed();
        let borders = if framed {
            Borders::LEFT | Borders::RIGHT
        } else {
            Borders::NONE
        };
        let block = match surface {
            Surface::Plain | Surface::Assistant => None,
            Surface::User => Some(
                Block::default()
                    .borders(borders)
                    .border_type(BorderType::Rounded)
                    .border_style(theme::current().user)
                    .style(theme::current().tool_bg)
                    .padding(Padding::horizontal(1)),
            ),
            Surface::Tool => Some(
                Block::default()
                    .borders(borders)
                    .border_type(BorderType::Rounded)
                    .border_style(theme::current().tool_dim)
                    .style(theme::current().tool_bg)
                    .padding(Padding::horizontal(1)),
            ),
        };
        let mut p = Paragraph::new(lines.to_vec())
            .style(base)
            .wrap(Wrap { trim: false });
        if let Some(block) = block {
            p = p.block(block);
        }
        if segment_skip > 0 {
            p = p.scroll((segment_skip, 0));
            self.skip = 0;
        }
        frame.render_widget(p, seg_area);
        if surface == Surface::Assistant
            && segment_skip == 0
            && visible_h > 0
            && seg_area.width >= COPY_LABEL_WIDTH
            && lines
                .first()
                .is_none_or(|line| line.width() <= usize::from(seg_area.width - COPY_LABEL_WIDTH))
        {
            let copy_width = COPY_LABEL_WIDTH.min(seg_area.width);
            let copy_area = Rect::new(
                seg_area.right().saturating_sub(copy_width),
                seg_area.y,
                copy_width,
                1,
            );
            frame.render_widget(
                Paragraph::new(COPY_LABEL).style(theme::current().tool_dim),
                copy_area,
            );
        }
        self.y += visible_h;
    }

    pub fn render_image(
        &mut self,
        protocol: &SlicedProtocol,
        h: u16,
        surface: Surface,
        frame: &mut Frame,
    ) {
        if self.skip >= h {
            self.skip -= h;
            return;
        }
        if self.y >= self.bottom {
            return;
        }
        let segment_skip = self.skip;
        let visible_h = h
            .saturating_sub(segment_skip)
            .min(self.bottom.saturating_sub(self.y));
        let seg_area = Rect::new(self.viewport.x, self.y, self.viewport.width, visible_h);
        let content_area = if surface.is_framed() {
            let block = frame_block(surface);
            let inner = block.inner(seg_area);
            frame.render_widget(block, seg_area);
            inner
        } else {
            seg_area
        };
        if content_area.width > 0 && content_area.height > 0 {
            let position = SignedPosition::from((0, -(segment_skip as i16)));
            let image = SlicedImage::new(protocol, position);
            frame.render_widget(image, content_area);
        }
        self.skip = 0;
        self.y += visible_h;
    }
}

fn frame_block(surface: Surface) -> Block<'static> {
    let borders = Borders::LEFT | Borders::RIGHT;
    match surface {
        Surface::User => Block::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(theme::current().user)
            .style(theme::current().tool_bg)
            .padding(Padding::horizontal(1)),
        Surface::Tool => Block::default()
            .borders(borders)
            .border_type(BorderType::Rounded)
            .border_style(theme::current().tool_dim)
            .style(theme::current().tool_bg)
            .padding(Padding::horizontal(1)),
        _ => Block::default(),
    }
}
