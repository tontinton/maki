use std::env;
#[cfg(unix)]
use std::io::Write;
use std::io::{Cursor, IsTerminal, stdout};
#[cfg(unix)]
use std::os::fd::AsFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
#[cfg(unix)]
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use color_eyre::eyre::{Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use maki_providers::ImageSource;
use ratatui::layout::Size;
use ratatui_image::picker::cap_parser::Response;
#[cfg(unix)]
use ratatui_image::picker::cap_parser::{Parser, QueryStdioOptions};
use ratatui_image::{
    FontSize,
    picker::{Picker, ProtocolType},
    sliced::SlicedProtocol,
};

use crate::image::{MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS};
use crate::repaint::Dirty;

const MAX_IMAGE_ROWS: u16 = 20;
const FALLBACK_FONT_SIZE: FontSize = FontSize::new(10, 20);
const MULTIPLEXER_VARS: [&str; 3] = ["TMUX", "STY", "ZELLIJ"];
/// The reply ends with a DSR sentinel, so a responsive terminal finishes the
/// poll far sooner; this only caps one that never answers.
#[cfg(unix)]
const QUERY_TIMEOUT: Duration = Duration::from_millis(50);
/// SSH adds a network round trip to the terminal's sub-millisecond answer.
#[cfg(unix)]
const SSH_QUERY_TIMEOUT: Duration = Duration::from_millis(300);
const DECODE_THREAD: &str = "inline-image";
static GENERATION: AtomicU64 = AtomicU64::new(0);
static PICKER: OnceLock<Option<Picker>> = OnceLock::new();
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

/// Call in raw mode and before the input reader spawns: the probe reads replies
/// straight from the tty and a running reader would eat them.
pub(crate) fn init(inline_images: bool) {
    if inline_images {
        PICKER.get_or_init(detect);
    }
}

pub(crate) fn picker(inline_images: bool) -> Option<Picker> {
    if !inline_images {
        return None;
    }
    PICKER.get().cloned().flatten()
}

fn detect() -> Option<Picker> {
    let get = |key: &str| env::var(key).ok();
    if !stdout().is_terminal() || in_multiplexer(&get) {
        return None;
    }
    let probed = if graphics_denied(&get) { None } else { probe() };
    let (protocol, font) = match probed {
        Some((probed, font)) => (env_protocol(&get).unwrap_or(probed), font),
        None => (env_protocol(&get)?, None),
    };
    let font = font.or_else(window_font).unwrap_or(FALLBACK_FONT_SIZE);
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(protocol);
    Some(picker)
}

#[cfg(unix)]
fn query_timeout() -> Duration {
    if env::var_os("SSH_CONNECTION").is_some() || env::var_os("SSH_CLIENT").is_some() {
        SSH_QUERY_TIMEOUT
    } else {
        QUERY_TIMEOUT
    }
}

/// The terminal answers a kitty graphics probe and a DA1 query (attribute 4 is
/// sixel) plus `CSI 16 t` for the cell size. Reading replies with a poll
/// deadline keeps a silent terminal from hanging startup; `ratatui-image`'s
/// `from_query_stdio` blocks a thread on stdin and can eat the next keypress
/// when it times out.
#[cfg(unix)]
fn probe() -> Option<(ProtocolType, Option<FontSize>)> {
    let tty = crate::tty::open()?;
    let query = Parser::query(false, QueryStdioOptions::default());
    let mut out = stdout().lock();
    out.write_all(query.as_bytes()).ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + query_timeout();
    let mut parser = Parser::new();
    let mut responses = Vec::new();
    let mut buf = [0u8; 256];
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if !crate::tty::wait_readable(tty.as_fd(), remaining) {
            break;
        }
        let n = match rustix::io::read(tty.as_fd(), &mut buf) {
            Ok(n) if n > 0 => n,
            _ => break,
        };
        for byte in &buf[..n] {
            for response in parser.push(char::from(*byte)) {
                if matches!(response, Response::Status) {
                    return interpret(responses);
                }
                responses.push(response);
            }
        }
    }
    interpret(responses)
}

#[cfg(not(unix))]
fn probe() -> Option<(ProtocolType, Option<FontSize>)> {
    None
}

fn interpret(responses: Vec<Response>) -> Option<(ProtocolType, Option<FontSize>)> {
    let mut protocol = None;
    let mut font = None;
    for response in responses {
        match response {
            Response::Kitty => protocol = Some(ProtocolType::Kitty),
            Response::Sixel if protocol.is_none() => protocol = Some(ProtocolType::Sixel),
            Response::CellSize(Some((width, height))) if width > 0 && height > 0 => {
                font = Some(FontSize::new(width, height));
            }
            _ => {}
        }
    }
    protocol.map(|protocol| (protocol, font))
}

/// Env hints outrank the probe where the advertised sixel renders worse than
/// the terminal's native protocol. A `TERM_PROGRAM` set by tmux or a nested
/// shell falls through to `TERM`, which kitty forwards over ssh.
fn env_protocol(get: &impl Fn(&str) -> Option<String>) -> Option<ProtocolType> {
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

fn in_multiplexer(get: &impl Fn(&str) -> Option<String>) -> bool {
    MULTIPLEXER_VARS.iter().any(|key| get(key).is_some())
}

/// Terminals that advertise sixel but paint it wrong: `ratatui-image`'s matrix
/// has Konsole leaving stale pixels and Contour failing to clear.
fn graphics_denied(get: &impl Fn(&str) -> Option<String>) -> bool {
    get("KONSOLE_VERSION").is_some()
        || get("TERM_PROGRAM").as_deref() == Some("contour")
        || get("TERM").is_some_and(|term| term.starts_with("contour"))
}

/// Last resort for a terminal that ignores `CSI 16 t`; usually stdout is the
/// tty and the ioctl knows the font cell even when the probe does not.
fn window_font() -> Option<FontSize> {
    crossterm::terminal::window_size()
        .ok()
        .filter(|size| size.columns > 0 && size.rows > 0)
        .map(|size| FontSize::new(size.width / size.columns, size.height / size.rows))
        .filter(|font| font.width > 0 && font.height > 0)
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
    /// The line a terminal without graphics gets instead of the pixels, or
    /// `None` where the text around the image already names it. Both the row
    /// count and the row itself read this, so layout and paint cannot disagree.
    fallback: Option<&'static str>,
}

impl InlineImage {
    pub fn new(source: ImageSource, fallback: Option<&'static str>) -> Self {
        Self {
            source,
            state: ImageState::Idle { height: 0 },
            fallback,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_prepared(source: ImageSource, picker: &Picker, width: u16) -> Result<Self> {
        let protocol = encode(&source, picker, width)?;
        Ok(Self {
            source,
            state: ImageState::Ready { width, protocol },
            fallback: None,
        })
    }

    pub fn source(&self) -> &ImageSource {
        &self.source
    }

    pub fn fallback(&self) -> Option<&'static str> {
        self.fallback
    }

    pub fn height(&self) -> u16 {
        let pixels = match &self.state {
            ImageState::Ready { protocol, .. } => protocol.size().height,
            ImageState::Pending { height, .. } | ImageState::Idle { height } => *height,
            ImageState::Failed => 0,
        };
        pixels.max(u16::from(self.fallback.is_some()))
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
    use maki_providers::{IMAGE_PLACEHOLDER, ImageMediaType, ImageSource};
    use ratatui::layout::Size;
    use ratatui_image::{
        FontSize,
        picker::{Picker, ProtocolType},
    };
    use test_case::test_case;

    use super::{
        ImageState, InlineImage, Response, encode, env_protocol, graphics_denied, in_multiplexer,
        interpret, picker,
    };
    use crate::repaint::Dirty;

    const EXPECTED_IDLE: &str = "nothing to encode with, so the image must stay idle";
    const EXPECTED_PENDING: &str = "image should be pending after prepare";
    const HEIGHT_KEPT: &str = "a released image keeps its height so the scroll does not jump";
    const MALFORMED_IMAGE: &[u8] = b"not an image";
    const IMAGE_WIDTH: u16 = 4;
    const IMAGE_HEIGHT: u16 = 6;
    const TERM_PROGRAM: &str = "TERM_PROGRAM";
    const TERM: &str = "TERM";
    const KITTY_TERM: &str = "xterm-kitty";
    const UNKNOWN_PROGRAM: &str = "unknown";

    fn get<'a>(env: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).into())
        }
    }

    #[test_case(&[(TERM_PROGRAM, "iTerm.app")], Some(ProtocolType::Iterm2); "known_terminal")]
    #[test_case(&[(TERM, KITTY_TERM)], Some(ProtocolType::Kitty); "known_term")]
    #[test_case(&[(TERM_PROGRAM, UNKNOWN_PROGRAM), (TERM, KITTY_TERM)], Some(ProtocolType::Kitty); "unsupported_program_falls_back_to_term")]
    #[test_case(&[(TERM_PROGRAM, UNKNOWN_PROGRAM), (TERM, "xterm-256color")], None; "unsupported_program_and_term")]
    #[test_case(&[], None; "missing_env")]
    fn env_protocol_prefers_the_native_rendering(
        env: &[(&str, &str)],
        expected: Option<ProtocolType>,
    ) {
        assert_eq!(env_protocol(&get(env)), expected);
    }

    #[test_case(&[("TMUX", "mux")]; "tmux")]
    #[test_case(&[("STY", "1")]; "screen")]
    #[test_case(&[("ZELLIJ", "1")]; "zellij")]
    fn multiplexers_disable_graphics(env: &[(&str, &str)]) {
        assert!(in_multiplexer(&get(env)));
    }

    #[test_case(&[("KONSOLE_VERSION", "230800")], true; "konsole")]
    #[test_case(&[(TERM_PROGRAM, "contour")], true; "contour_program")]
    #[test_case(&[(TERM, "contour")], true; "contour_term")]
    #[test_case(&[(TERM, "xterm-256color")], false; "xterm")]
    fn broken_sixel_terminals_are_denied(env: &[(&str, &str)], expected: bool) {
        assert_eq!(graphics_denied(&get(env)), expected);
    }

    #[test_case(&[Response::Sixel, Response::CellSize(Some((9, 18)))], Some((ProtocolType::Sixel, Some((9, 18)))); "sixel_and_cell_size")]
    #[test_case(&[Response::Sixel, Response::Kitty], Some((ProtocolType::Kitty, None)); "kitty_wins")]
    #[test_case(&[Response::CellSize(Some((0, 18)))], None; "cell_size_alone_is_not_graphics")]
    #[test_case(&[], None; "silent_terminal")]
    fn probe_responses_pick_the_protocol(
        responses: &[Response],
        expected: Option<(ProtocolType, Option<(u16, u16)>)>,
    ) {
        let actual = interpret(responses.to_vec())
            .map(|(protocol, font)| (protocol, font.map(|font| (font.width, font.height))));
        assert_eq!(actual, expected);
    }

    #[test]
    fn disabled_inline_images_yields_no_picker() {
        assert!(picker(false).is_none());
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
        let mut image = InlineImage::new(source.clone(), Some(IMAGE_PLACEHOLDER));
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
