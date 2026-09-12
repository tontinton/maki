use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui_image::{
    Image,
    picker::Picker,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

use crate::components::IMAGE_PLACEHOLDER;
use crate::terminal_image::InlineImage;

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
        let height = image.height();
        if self.skip >= height || self.past_bottom() {
            self.skip = self.skip.saturating_sub(height);
            return;
        }
        let protocol = match picker.filter(|_| visible) {
            Some(picker) => {
                image.prepare(picker, self.viewport.width);
                image.protocol(self.viewport.width)
            }
            None => None,
        };
        let Some(protocol) = protocol else {
            self.render(&[Line::from(IMAGE_PLACEHOLDER)], height, None, false, frame);
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use base64::{Engine, engine::general_purpose::STANDARD};
    use color_eyre::eyre::Result;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use maki_providers::{ImageMediaType, ImageSource};
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use ratatui_image::{
        FontSize,
        picker::{Picker, ProtocolType},
    };
    use test_case::test_case;

    use super::RenderCursor;
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
}
