//! The attached image whose `[Image N]` marker the input caret is on, drawn
//! over the bottom of the transcript. That is how a wrong screenshot gets
//! caught before it is sent: right after a paste the caret sits on the marker.

use std::sync::Arc;

use maki_providers::ImageSource;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear};
use ratatui_image::picker::Picker;

use super::messages;
use crate::repaint::Dirty;
use crate::terminal_image::{self, InlineImage};
use crate::theme;

#[derive(Default)]
pub(crate) struct ImagePreview {
    image: Option<InlineImage>,
    generation: u64,
}

impl ImagePreview {
    /// Keeps the decoded image while the caret stays on the same one.
    pub fn show(&mut self, source: Option<&ImageSource>) {
        let same = match (source, &self.image) {
            (Some(source), Some(image)) => Arc::ptr_eq(&image.source().data, &source.data),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.image = source.map(|source| InlineImage::new(source.clone(), None));
        }
    }

    /// A font size change makes the transcript rebuild its picker, so the
    /// protocol encoded against the old one is dropped here too.
    pub fn poll(&mut self) -> Dirty {
        let Some(image) = &mut self.image else {
            return Dirty::NO;
        };
        let generation = terminal_image::generation();
        if generation != self.generation {
            self.generation = generation;
            image.release();
            return Dirty::YES;
        }
        image.poll()
    }

    /// Draws at the bottom of {room}. Nothing shows until the image is
    /// decoded, or at all on a terminal that draws no images.
    pub fn view(&mut self, frame: &mut Frame, room: Rect, picker: Option<&Picker>) {
        let (Some(image), Some(picker)) = (&mut self.image, picker) else {
            return;
        };
        image.prepare(picker, room.width);
        let height = image.height().min(room.height);
        if height == 0 || image.protocol(room.width).is_none() {
            return;
        }
        let area = Rect::new(room.x, room.bottom() - height, room.width, height);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(Style::new().bg(theme::current().background)),
            area,
        );
        messages::render_image(image, picker, area, frame);
    }
}
