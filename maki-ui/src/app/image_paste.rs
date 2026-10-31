use std::path::PathBuf;
use std::thread;

use crate::image;
use flume::Receiver;
use maki_agent::{ImageMediaType, ImageSource};

use crate::repaint::Dirty;

use super::App;

const IMAGE_NOT_SUPPORTED_MSG: &str = "Model does not support image input";

/// Runs a blocking read on its own thread: a clipboard owner may be slow or
/// hung, and the UI thread must not wait on it.
pub(crate) fn spawn_read<T: Send + 'static>(
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

    pub(super) fn start_image_paste(&mut self) {
        if !self.state.model.supports_vision() {
            self.status_bar.flash(IMAGE_NOT_SUPPORTED_MSG.into());
            return;
        }
        self.spawn_image_load("Reading clipboard...".into(), image::load_clipboard_image);
    }

    fn spawn_image_load(
        &mut self,
        flash: String,
        f: impl FnOnce() -> Result<ImageSource, String> + Send + 'static,
    ) {
        self.image_paste_rx.push(spawn_read(f));
        self.status_bar.flash(flash);
    }

    pub fn poll_image_paste(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        while let Some(result) = take_ready(&mut self.image_paste_rx) {
            dirty = Dirty::YES;
            match result {
                Ok(source) => {
                    if !self.state.model.supports_vision() {
                        self.status_bar.flash(IMAGE_NOT_SUPPORTED_MSG.into());
                    } else {
                        self.input_box.attach_image(source);
                        self.status_bar.flash("Image attached".into());
                    }
                }
                Err(e) => self.status_bar.flash(format!("Image paste failed: {e}")),
            }
        }
        dirty
    }
}
