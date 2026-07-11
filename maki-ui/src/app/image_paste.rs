use std::path::PathBuf;
use std::thread;

use crate::image;
use maki_agent::{ImageMediaType, ImageSource};

use super::App;

const IMAGE_NOT_SUPPORTED_MSG: &str = "Model does not support image input";

impl App {
    pub(super) fn start_file_image_paste(&mut self, path: PathBuf, media_type: ImageMediaType) {
        if !self.state.model.vision {
            self.status_bar.flash(IMAGE_NOT_SUPPORTED_MSG.into());
            return;
        }
        let msg = format!("Reading {}...", path.display());
        self.spawn_image_load(msg, move || image::load_file_image(&path, media_type));
    }

    pub(super) fn start_image_paste(&mut self) {
        if !self.state.model.vision {
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
        let (tx, rx) = flume::bounded(1);
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        self.image_paste_rx.push(rx);
        self.status_bar.flash(flash);
    }

    pub fn poll_image_paste(&mut self) {
        let mut i = 0;
        while i < self.image_paste_rx.len() {
            let Ok(result) = self.image_paste_rx[i].try_recv() else {
                i += 1;
                continue;
            };
            self.image_paste_rx.swap_remove(i);
            match result {
                Ok(source) => {
                    if !self.state.model.vision {
                        self.status_bar.flash(IMAGE_NOT_SUPPORTED_MSG.into());
                    } else {
                        self.input_box.attach_image(source);
                        self.status_bar.flash("Image attached".into());
                    }
                }
                Err(e) => self.status_bar.flash(format!("Image paste failed: {e}")),
            }
        }
    }
}
