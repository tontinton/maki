use std::path::PathBuf;
use std::thread;

use crate::{clipboard, image};
use flume::Receiver;
use maki_agent::{ImageMediaType, ImageSource};

use crate::repaint::Dirty;

use super::{App, Msg};

const IMAGE_NOT_SUPPORTED_MSG: &str = "Model does not support image input";
const NO_TEXT_NO_VISION_MSG: &str =
    "clipboard has no text, and the model does not support image input";

pub(crate) enum PastedContent {
    Image(ImageSource),
    Text(String),
}

fn read_clipboard(vision: bool) -> Result<PastedContent, String> {
    pick_paste(vision, image::load_clipboard_image, clipboard::read_text)
}

/// Tries an image first for a vision model, keeping its error if text fails too.
fn pick_paste(
    vision: bool,
    read_image: impl FnOnce() -> Result<ImageSource, String>,
    read_text: impl FnOnce() -> Option<String>,
) -> Result<PastedContent, String> {
    let image = if vision {
        read_image()
    } else {
        Err(NO_TEXT_NO_VISION_MSG.into())
    };
    image
        .map(PastedContent::Image)
        .or_else(|e| read_text().map(PastedContent::Text).ok_or(e))
}

/// Runs a blocking read on its own thread: a clipboard owner may be slow or
/// hung, and the UI thread must not wait on it.
pub(super) fn spawn_read<T: Send + 'static>(
    read: impl FnOnce() -> T + Send + 'static,
) -> Receiver<T> {
    let (tx, rx) = flume::bounded(1);
    thread::spawn(move || {
        let _ = tx.send(read());
    });
    rx
}

pub(super) fn take_ready<T>(receivers: &mut Vec<Receiver<T>>) -> Option<T> {
    let (i, value) = receivers
        .iter()
        .enumerate()
        .find_map(|(i, rx)| rx.try_recv().ok().map(|value| (i, value)))?;
    receivers.swap_remove(i);
    Some(value)
}

impl App {
    pub(super) fn start_file_image_paste(&mut self, path: PathBuf, media_type: ImageMediaType) {
        if !self.state.model.supports_vision() {
            self.status_bar.flash(IMAGE_NOT_SUPPORTED_MSG.into());
            return;
        }
        let msg = format!("Reading {}...", path.display());
        self.spawn_image_load(msg, move || image::load_file_image(&path, media_type));
    }

    /// `Ctrl+V` only reaches us when the terminal did not paste itself.
    pub(super) fn start_clipboard_paste(&mut self) {
        let vision = self.state.model.supports_vision();
        self.paste_rx
            .push(spawn_read(move || read_clipboard(vision)));
        self.status_bar.flash("Reading clipboard...".into());
    }

    fn spawn_image_load(
        &mut self,
        flash: String,
        f: impl FnOnce() -> Result<ImageSource, String> + Send + 'static,
    ) {
        self.paste_rx
            .push(spawn_read(move || f().map(PastedContent::Image)));
        self.status_bar.flash(flash);
    }

    pub fn poll_paste(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(result) = take_ready(&mut self.paste_rx) {
            dirty = Dirty::YES;
            match result {
                Ok(PastedContent::Text(text)) => {
                    let actions = self.update(Msg::Paste(text));
                    self.pending_actions.extend(actions);
                }
                Ok(PastedContent::Image(source)) => {
                    if !self.state.model.supports_vision() {
                        self.status_bar.flash(IMAGE_NOT_SUPPORTED_MSG.into());
                    } else {
                        self.input_box.attach_image(source);
                        self.status_bar.flash("Image attached".into());
                    }
                }
                Err(e) => self.status_bar.flash(format!("Paste failed: {e}")),
            }
        }
        dirty
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use test_case::test_case;

    use super::*;

    const IMAGE_ERR: &str = "no image";
    const TEXT: &str = "copied";

    #[derive(Debug, PartialEq)]
    enum Pasted {
        Image,
        Text,
        Error(String),
    }

    #[test_case(true,  true,  true  => Pasted::Image                              ; "vision_prefers_image")]
    #[test_case(true,  false, true  => Pasted::Text                               ; "vision_falls_back_to_text")]
    #[test_case(true,  false, false => Pasted::Error(IMAGE_ERR.into())            ; "vision_keeps_image_error")]
    #[test_case(false, true,  true  => Pasted::Text                               ; "text_model_takes_text")]
    #[test_case(false, true,  false => Pasted::Error(NO_TEXT_NO_VISION_MSG.into()) ; "text_model_says_why_image_failed")]
    fn pick_paste_falls_back(vision: bool, has_image: bool, has_text: bool) -> Pasted {
        let read_image = || {
            if has_image {
                Ok(ImageSource::new(ImageMediaType::Png, Arc::from("")))
            } else {
                Err(IMAGE_ERR.into())
            }
        };
        let read_text = || has_text.then(|| TEXT.into());
        match pick_paste(vision, read_image, read_text) {
            Ok(PastedContent::Image(_)) => Pasted::Image,
            Ok(PastedContent::Text(_)) => Pasted::Text,
            Err(e) => Pasted::Error(e),
        }
    }
}
