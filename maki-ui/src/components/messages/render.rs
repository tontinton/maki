use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui_image::{
    Image,
    picker::Picker,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

use crate::terminal_image::InlineImage;

use super::segment::Segment;

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

    /// `visible` is false while an overlay covers the transcript. The encoded
    /// protocol is kept either way: releasing it here would re-decode and
    /// re-transmit every image each time a permission prompt opens and closes.
    pub fn render_image(
        &mut self,
        image: &mut InlineImage,
        picker: Option<&Picker>,
        visible: bool,
        frame: &mut Frame,
    ) {
        if self.past_bottom() {
            return;
        }
        // Ask for the pixels before measuring: an image with no fallback row is
        // zero rows tall until its protocol lands, and the check below reads
        // zero rows as scrolled past, so it would never get around to asking.
        if let Some(picker) = picker.filter(|_| visible) {
            image.prepare(picker, self.viewport.width);
        }
        let height = image.height();
        if self.skip >= height {
            self.skip -= height;
            return;
        }
        let Some(protocol) = image.protocol(self.viewport.width) else {
            let fallback = image.fallback().map(Line::from);
            self.render(fallback.as_slice(), height, None, false, (0, None), frame);
            return;
        };
        let visible_rows = height
            .saturating_sub(self.skip)
            .min(self.bottom.saturating_sub(self.y));
        let area = Rect::new(self.viewport.x, self.y, self.viewport.width, visible_rows);
        if let SlicedProtocol::Sliced(rows) = protocol {
            // Not `SlicedImage::new`: upstream renders `.skip(skip).take(len - drop)`
            // rows into `area`, which is `skip` rows too many when an image is
            // clipped at the top and the bottom at once, so it draws past `area`
            // into the segments below. Placing each row ourselves cannot overdraw.
            for (offset, row) in rows
                .iter()
                .skip(self.skip as usize)
                .take(visible_rows as usize)
                .enumerate()
            {
                frame.render_widget(
                    Image::new(row),
                    Rect::new(area.x, area.y + offset as u16, area.width, 1),
                );
            }
        } else {
            let position = SignedPosition::from((0, -(self.skip as i16)));
            frame.render_widget(SlicedImage::new(protocol, position), area);
        }
        self.skip = 0;
        self.y += visible_rows;
    }

    pub fn render(
        &mut self,
        lines: &[Line<'static>],
        h: u16,
        style: Option<Style>,
        highlight: bool,
        (gutter_cols, marker): (u16, Option<&Span<'static>>),
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
        let gutter_area = Rect::new(self.viewport.x, self.y, gutter_cols, visible_h);
        let seg_area = Rect::new(
            self.viewport.x + gutter_cols,
            self.y,
            self.viewport.width - gutter_cols,
            visible_h,
        );
        let mut p = Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false });
        let mut base = style.unwrap_or_default();
        if highlight {
            base = base.add_modifier(Modifier::REVERSED);
        }
        if gutter_cols > 0 {
            let marker_line = marker.map_or_else(Line::default, |m| Line::from(m.clone()));
            frame.render_widget(Paragraph::new(marker_line).style(base), gutter_area);
        }
        p = p.style(base);
        if self.skip > 0 {
            p = p.scroll((self.skip, 0));
            self.skip = 0;
        }
        frame.render_widget(p, seg_area);
        self.y += visible_h;
    }

    /// A scrolled `Paragraph` wraps every row above the viewport before it
    /// draws anything, so a tall segment used to cost its full height on every
    /// frame. Walking to the first visible row and drawing only what fits
    /// keeps the cost at one screen.
    pub fn render_segment(
        &mut self,
        seg: &Segment,
        style: Option<Style>,
        highlight: bool,
        frame: &mut Frame,
    ) {
        let h = seg.text_height(self.viewport.width);
        if self.skip >= h {
            self.skip -= h;
            return;
        }
        let cols = seg.gutter_cols(self.viewport.width);
        let mut walk = seg.rows_from(self.skip, self.viewport.width);
        while self.y < self.bottom
            && let Some((lines, rows)) = walk.next_chunk(self.bottom - self.y)
        {
            // The first chunk starts at the top of the line `skip` lands in, so
            // only the rows above `skip` inside that line are left to drop.
            // `render` zeroes `skip`, so later chunks drop nothing.
            self.skip = self.skip.saturating_sub(rows.start);
            let marker = seg
                .gutter
                .as_ref()
                .filter(|_| rows.start == 0 && self.skip == 0);
            self.render(
                lines,
                rows.end - rows.start,
                style,
                highlight,
                (cols, marker),
                frame,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use base64::{Engine, engine::general_purpose::STANDARD};
    use color_eyre::eyre::Result;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use maki_providers::{ImageMediaType, ImageSource};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect, style::Style};
    use ratatui_image::{
        FontSize,
        picker::{Picker, ProtocolType},
    };
    use test_case::test_case;

    use ratatui::{
        style::Color,
        text::{Line, Span},
    };

    use super::RenderCursor;
    use crate::components::messages::segment::{GUTTER_COLS, Segment};
    use crate::terminal_image::InlineImage;

    const VIEWPORT: Rect = Rect::new(2, 2, 4, 2);
    const IMAGE_HEIGHT: u16 = 6;
    const TOP_SKIP: u16 = 2;
    const KITTY_RESTORE_CURSOR: &str = "\x1b[u";
    const OVERDRAW: &str = "a clipped image must not paint outside the viewport";
    const BLANK_ROW: &str = "every viewport row must get some of the image";
    const WRONG_ROW: &str = "the clipped rows must be the ones the full draw put there";

    #[test_case(ProtocolType::Iterm2; "iterm2")]
    #[test_case(ProtocolType::Kitty; "kitty")]
    #[test_case(ProtocolType::Halfblocks; "halfblocks")]
    fn image_clips_top_and_bottom(protocol: ProtocolType) -> Result<()> {
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(FontSize::new(2, 2));
        picker.set_protocol_type(protocol);
        let pixels = RgbaImage::from_fn(8, 12, |_, y| Rgba([y as u8 * 20, 80, 160, 255]));
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels).write_to(&mut png, ImageFormat::Png)?;
        let source = ImageSource::new(
            ImageMediaType::Png,
            STANDARD.encode(png.into_inner()).into(),
        );
        let mut image = InlineImage::new_prepared(source, &picker, VIEWPORT.width)?;
        assert_eq!(image.height(), IMAGE_HEIGHT);
        let mut terminal = Terminal::new(TestBackend::new(8, 8))?;
        let full_viewport = Rect {
            height: IMAGE_HEIGHT,
            ..VIEWPORT
        };
        let full = terminal
            .draw(|frame| {
                RenderCursor::new(0, full_viewport).render_image(
                    &mut image,
                    Some(&picker),
                    true,
                    frame,
                );
            })?
            .buffer
            .clone();
        let mut cursor = RenderCursor::new(TOP_SKIP, VIEWPORT);
        terminal.draw(|frame| {
            let before = frame.buffer_mut().clone();
            cursor.render_image(&mut image, Some(&picker), true, frame);
            let after = frame.buffer_mut();
            for y in before.area.y..before.area.bottom() {
                for x in before.area.x..before.area.right() {
                    if !VIEWPORT.contains((x, y).into()) {
                        assert_eq!(after[(x, y)], before[(x, y)], "{OVERDRAW}");
                    }
                }
            }
            for y in VIEWPORT.y..VIEWPORT.bottom() {
                assert!(
                    (VIEWPORT.x..VIEWPORT.right()).any(|x| after[(x, y)] != before[(x, y)]),
                    "{BLANK_ROW}"
                );
                for x in VIEWPORT.x..VIEWPORT.right() {
                    // Kitty packs a row into one cell and ends it by restoring
                    // the cursor to the far corner of the area it drew into, so
                    // that tail differs between a 6 row draw and a 2 row one.
                    // Everything before it describes the image.
                    if protocol == ProtocolType::Kitty && x == VIEWPORT.x {
                        assert_eq!(
                            after[(x, y)].symbol().split(KITTY_RESTORE_CURSOR).next(),
                            full[(x, y + TOP_SKIP)]
                                .symbol()
                                .split(KITTY_RESTORE_CURSOR)
                                .next(),
                            "{WRONG_ROW}"
                        );
                    } else {
                        assert_eq!(after[(x, y)], full[(x, y + TOP_SKIP)], "{WRONG_ROW}");
                    }
                }
            }
        })?;
        assert_eq!(cursor.skip, 0);
        assert_eq!(cursor.y, VIEWPORT.bottom());
        assert!(cursor.past_bottom());
        Ok(())
    }

    const MARKER: &str = "● ";
    const GUTTER_BG: Color = Color::Blue;
    const GUTTER_WIDTH: u16 = 8;
    const BODY: &str = "abcdefghijkl";

    fn gutter_segment() -> Segment {
        let mut seg = Segment::with_lines(vec![Line::raw(BODY)], None);
        seg.gutter = Some(Span::raw(MARKER));
        seg
    }

    fn draw_rows(seg: &Segment, skip: u16, style: Option<Style>) -> Result<Buffer> {
        let viewport = Rect::new(0, 0, GUTTER_WIDTH, 4);
        let mut terminal = Terminal::new(TestBackend::new(GUTTER_WIDTH, 4))?;
        let frame = terminal.draw(|frame| {
            RenderCursor::new(skip, viewport).render_segment(seg, style, false, frame);
        })?;
        Ok(frame.buffer.clone())
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn marker_drawn_in_gutter_and_body_indented() -> Result<()> {
        let buf = draw_rows(&gutter_segment(), 0, None)?;
        assert_eq!(row_text(&buf, 0), "● abcdef");
        assert_eq!(
            row_text(&buf, 1),
            format!("{}ghijkl", " ".repeat(GUTTER_COLS.into()))
        );
        Ok(())
    }

    #[test]
    fn marker_hidden_when_first_row_scrolled_off() -> Result<()> {
        let buf = draw_rows(&gutter_segment(), 1, None)?;
        assert!(!(0..4).any(|y| row_text(&buf, y).contains('●')));
        assert_eq!(row_text(&buf, 0).trim_end(), "  ghijkl");
        Ok(())
    }

    #[test]
    fn segment_style_fills_gutter() -> Result<()> {
        let buf = draw_rows(&gutter_segment(), 0, Some(Style::new().bg(GUTTER_BG)))?;
        for y in 0..2 {
            for x in 0..GUTTER_COLS {
                assert_eq!(buf[(x, y)].bg, GUTTER_BG);
            }
        }
        Ok(())
    }
}
