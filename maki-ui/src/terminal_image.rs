use std::env;
use std::io::{Cursor, IsTerminal, stdout};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use base64::{Engine, engine::general_purpose::STANDARD};
use color_eyre::eyre::{Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use maki_providers::ImageSource;
use ratatui::layout::Size;
use ratatui_image::{
    FontSize,
    picker::{Picker, ProtocolType},
    sliced::SlicedProtocol,
};

use crate::image::{MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS};
use crate::repaint::Dirty;

const MAX_IMAGE_ROWS: u16 = 20;
const FALLBACK_FONT_SIZE: FontSize = FontSize::new(10, 20);
const DECODE_THREAD: &str = "inline-image";
static GENERATION: AtomicU64 = AtomicU64::new(0);
static DECODE_JOBS: OnceLock<flume::Sender<DecodeJob>> = OnceLock::new();

type DecodeJob = Box<dyn FnOnce() + Send + 'static>;

/// Decoding gets its own thread rather than `maki_highlight::pool`: that one is
/// single threaded to bound syntect's regex cache, and a multi-megabyte decode
/// queued ahead of the transcript's highlight jobs stalls the whole UI.
fn decode_jobs() -> &'static flume::Sender<DecodeJob> {
    DECODE_JOBS.get_or_init(|| {
        let (tx, rx) = flume::unbounded::<DecodeJob>();
        thread::Builder::new()
            .name(DECODE_THREAD.into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    if catch_unwind(AssertUnwindSafe(job)).is_err() {
                        tracing::error!("inline image decode panicked");
                    }
                }
            })
            .expect("failed to spawn the inline image thread");
        tx
    })
}

pub(crate) fn invalidate() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub(crate) fn picker(inline_images: bool) -> Option<Picker> {
    if !stdout().is_terminal() {
        return None;
    }
    let protocol = protocol_from_env(inline_images, |key| env::var(key).ok())?;
    let font = crossterm::terminal::window_size()
        .ok()
        .filter(|size| size.columns > 0 && size.rows > 0)
        .map(|size| FontSize::new(size.width / size.columns, size.height / size.rows))
        .filter(|font| font.width > 0 && font.height > 0)
        .unwrap_or(FALLBACK_FONT_SIZE);
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(protocol);
    Some(picker)
}

/// An allowlist rather than trusting what the terminal advertises: one that
/// claims graphics it does not have paints escape bytes all over the UI. When
/// the guess is wrong anyway, `ui.inline_images = false` is the way out. An
/// unrecognized `TERM_PROGRAM` (tmux and some shell configs set it) falls
/// through to `TERM`, which kitty sets itself and forwards over ssh.
fn protocol_from_env(
    inline_images: bool,
    get: impl Fn(&str) -> Option<String>,
) -> Option<ProtocolType> {
    if !inline_images {
        return None;
    }
    if ["TMUX", "STY", "ZELLIJ"]
        .iter()
        .any(|key| get(key).is_some())
    {
        return None;
    }
    match get("TERM_PROGRAM").as_deref() {
        Some("ghostty" | "kitty") => return Some(ProtocolType::Kitty),
        Some("iTerm.app" | "WezTerm") => return Some(ProtocolType::Iterm2),
        _ => {}
    }
    match get("TERM").as_deref() {
        Some("xterm-kitty" | "xterm-ghostty") => Some(ProtocolType::Kitty),
        _ => None,
    }
}

enum ImageState {
    Idle {
        height: u16,
    },
    Pending {
        width: u16,
        height: u16,
        result: flume::Receiver<Result<SlicedProtocol>>,
    },
    Ready {
        width: u16,
        protocol: SlicedProtocol,
    },
    Failed,
}

pub(crate) struct InlineImage {
    source: ImageSource,
    state: ImageState,
}

impl InlineImage {
    pub fn new(source: ImageSource) -> Self {
        Self {
            source,
            state: ImageState::Idle { height: 1 },
        }
    }

    #[cfg(test)]
    pub(crate) fn new_prepared(source: ImageSource, picker: &Picker, width: u16) -> Result<Self> {
        let protocol = encode(&source, picker, width)?;
        Ok(Self {
            source,
            state: ImageState::Ready { width, protocol },
        })
    }

    pub fn source(&self) -> &ImageSource {
        &self.source
    }

    pub fn height(&self) -> u16 {
        match &self.state {
            ImageState::Ready { protocol, .. } => protocol.size().height,
            ImageState::Pending { height, .. } | ImageState::Idle { height } => *height,
            ImageState::Failed => 1,
        }
    }

    pub fn release(&mut self) {
        if !matches!(self.state, ImageState::Failed) {
            self.state = ImageState::Idle {
                height: self.height(),
            };
        }
    }

    pub fn prepare(&mut self, picker: &Picker, width: u16) {
        if width == 0 {
            return;
        }
        match &self.state {
            ImageState::Pending { .. } | ImageState::Failed => return,
            ImageState::Ready {
                width: encoded_width,
                ..
            } if *encoded_width == width => return,
            _ => {}
        }
        let (tx, result) = flume::bounded(1);
        let source = self.source.clone();
        let picker = picker.clone();
        self.state = ImageState::Pending {
            width,
            height: self.height(),
            result,
        };
        let _ = decode_jobs().send(Box::new(move || {
            if !tx.is_disconnected() {
                let _ = tx.send(encode(&source, &picker, width));
            }
        }));
    }

    pub fn poll(&mut self) -> Dirty {
        let ImageState::Pending { width, result, .. } = &self.state else {
            return Dirty::NO;
        };
        let result = match result.try_recv() {
            Ok(result) => result,
            Err(flume::TryRecvError::Empty) => return Dirty::NO,
            Err(flume::TryRecvError::Disconnected) => {
                self.state = ImageState::Failed;
                return Dirty::YES;
            }
        };
        self.state = match result {
            Ok(protocol) => ImageState::Ready {
                width: *width,
                protocol,
            },
            Err(error) => {
                tracing::warn!(%error, "inline image unavailable");
                ImageState::Failed
            }
        };
        Dirty::YES
    }

    pub fn protocol(&self, width: u16) -> Option<&SlicedProtocol> {
        match &self.state {
            ImageState::Ready {
                width: encoded_width,
                protocol,
            } if *encoded_width == width => Some(protocol),
            _ => None,
        }
    }
}

fn encode(source: &ImageSource, picker: &Picker, width: u16) -> Result<SlicedProtocol> {
    ensure!(
        source.data.len() <= MAX_IMAGE_BYTES.div_ceil(3) * 4,
        "Image exceeds 20MB limit"
    );
    let bytes = STANDARD.decode(source.data.as_ref())?;
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some((MAX_IMAGE_PIXELS * 4) as u64);
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    ensure!(
        u64::from(w) * u64::from(h) <= MAX_IMAGE_PIXELS as u64,
        "Image exceeds pixel limit"
    );
    let image = DynamicImage::from_decoder(decoder)?;
    let font = picker.font_size();
    let size = Size::new(
        width.min(u16::MAX / font.width),
        MAX_IMAGE_ROWS.min(u16::MAX / font.height),
    );
    Ok(SlicedProtocol::new(picker, image, Some(size))?)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use base64::{Engine, engine::general_purpose::STANDARD};
    use color_eyre::eyre::Result;
    use image::{DynamicImage, ImageFormat};
    use maki_providers::{ImageMediaType, ImageSource};
    use ratatui::layout::Size;
    use ratatui_image::{
        FontSize,
        picker::{Picker, ProtocolType},
    };
    use test_case::test_case;

    use super::{ImageState, InlineImage, encode, protocol_from_env};
    use crate::repaint::Dirty;

    const EXPECTED_IDLE: &str = "nothing to encode with, so the image must stay idle";
    const EXPECTED_PENDING: &str = "image should be pending after prepare";
    const HEIGHT_KEPT: &str = "a released image keeps its height so the scroll does not jump";
    const MALFORMED_IMAGE: &[u8] = b"not an image";
    const IMAGE_WIDTH: u16 = 4;
    const IMAGE_HEIGHT: u16 = 6;
    const TERM_PROGRAM: &str = "TERM_PROGRAM";
    const TERM: &str = "TERM";
    const KITTY_PROGRAM: &str = "kitty";
    const KITTY_TERM: &str = "xterm-kitty";
    const UNKNOWN_PROGRAM: &str = "unknown";

    fn protocol(inline_images: bool, env: &[(&str, &str)]) -> Option<ProtocolType> {
        protocol_from_env(inline_images, |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).into())
        })
    }

    #[test_case(&[(TERM_PROGRAM, "iTerm.app")], Some(ProtocolType::Iterm2); "known_terminal")]
    #[test_case(&[(TERM, KITTY_TERM)], Some(ProtocolType::Kitty); "known_term")]
    #[test_case(&[(TERM_PROGRAM, UNKNOWN_PROGRAM), (TERM, KITTY_TERM)], Some(ProtocolType::Kitty); "unsupported_program_falls_back_to_term")]
    #[test_case(&[(TERM_PROGRAM, UNKNOWN_PROGRAM), (TERM, "xterm-256color")], None; "unsupported_program_and_term")]
    #[test_case(&[], None; "missing_env")]
    #[test_case(&[(TERM_PROGRAM, KITTY_PROGRAM), ("TMUX", "mux")], None; "multiplexer")]
    fn protocol_env_uses_safe_fallback(env: &[(&str, &str)], expected: Option<ProtocolType>) {
        assert_eq!(protocol(true, env), expected);
    }

    #[test_case(&[(TERM_PROGRAM, KITTY_PROGRAM)]; "term_program")]
    #[test_case(&[(TERM, KITTY_TERM)]; "term")]
    fn disabled_inline_images_yields_no_protocol(env: &[(&str, &str)]) {
        assert_eq!(protocol(false, env), None);
    }

    #[test_case(false; "valid_png")]
    #[test_case(true; "malformed_image")]
    fn decode_result_drives_the_state_machine(malformed: bool) -> Result<()> {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(IMAGE_WIDTH.into(), IMAGE_HEIGHT.into())
            .write_to(&mut png, ImageFormat::Png)?;
        let source = ImageSource::new(
            ImageMediaType::Png,
            STANDARD
                .encode(if malformed {
                    MALFORMED_IMAGE
                } else {
                    png.get_ref()
                })
                .into(),
        );
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(FontSize::new(1, 1));
        picker.set_protocol_type(ProtocolType::Halfblocks);
        let mut image = InlineImage::new(source.clone());
        image.prepare(&picker, 0);
        assert!(
            matches!(image.state, ImageState::Idle { .. }),
            "{EXPECTED_IDLE}"
        );
        assert_eq!(image.height(), 1);
        image.prepare(&picker, IMAGE_WIDTH);
        assert!(
            matches!(image.state, ImageState::Pending { .. }),
            "{EXPECTED_PENDING}"
        );

        // Encode here and hand the answer over instead of waiting on the real
        // decode thread, which is how this test would start failing at 3am on a
        // loaded machine.
        let (tx, result) = flume::bounded(1);
        tx.send(encode(&source, &picker, IMAGE_WIDTH))?;
        image.state = ImageState::Pending {
            width: IMAGE_WIDTH,
            height: 1,
            result,
        };
        assert_eq!(image.poll(), Dirty::YES);
        assert_eq!(image.poll(), Dirty::NO);
        if malformed {
            assert!(matches!(image.state, ImageState::Failed));
            assert_eq!(image.height(), 1);
            assert!(image.protocol(IMAGE_WIDTH).is_none());
            return Ok(());
        }
        assert_eq!(image.height(), IMAGE_HEIGHT);
        assert_eq!(
            image.protocol(IMAGE_WIDTH).map(|protocol| protocol.size()),
            Some(Size::new(IMAGE_WIDTH, IMAGE_HEIGHT))
        );
        image.release();
        assert_eq!(image.height(), IMAGE_HEIGHT, "{HEIGHT_KEPT}");
        assert!(image.protocol(IMAGE_WIDTH).is_none());
        image.prepare(&picker, IMAGE_WIDTH);
        assert!(
            matches!(image.state, ImageState::Pending { .. }),
            "{EXPECTED_PENDING}"
        );
        Ok(())
    }
}
