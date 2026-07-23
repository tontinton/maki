use base64::Engine;
use maki_agent::ImageSource;
use ratatui::layout::Size;
use ratatui_image::errors::Errors as ProtocolErrors;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FontSize, Resize};
use std::cell::OnceCell;
use std::sync::Arc;
use thiserror::Error;

const UNBOUNDED_HEIGHT: u16 = u16::MAX;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_IMAGE_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Error)]
pub(crate) enum TerminalImageError {
    #[error("invalid base64 image data")]
    Base64(#[from] base64::DecodeError),
    #[error("could not decode image")]
    Image(#[from] image::ImageError),
    #[error("could not resize image for terminal")]
    Protocol(#[from] ProtocolErrors),
}

pub(crate) fn picker() -> Picker {
    let mut picker = match crossterm::terminal::window_size() {
        Ok(size) => match cell_size(size.width, size.height, size.columns, size.rows) {
            Some(font_size) => picker_from_font_size(font_size),
            None => Picker::halfblocks(),
        },
        Err(error) => {
            tracing::debug!(%error, "terminal pixel size unavailable; using default image sizing");
            Picker::halfblocks()
        }
    };

    let protocol = detect_protocol(
        env_hint("TERM").as_deref(),
        env_hint("TERM_PROGRAM").as_deref(),
        env_hint("LC_TERMINAL").as_deref(),
        |name| env_hint(name).is_some(),
    );
    picker.set_protocol_type(protocol);
    picker
}

fn cell_size(width: u16, height: u16, columns: u16, rows: u16) -> Option<FontSize> {
    if width == 0 || height == 0 || columns == 0 || rows == 0 {
        return None;
    }
    let width = rounded_cell_dimension(width, columns)?;
    let height = rounded_cell_dimension(height, rows)?;
    Some(FontSize::new(width, height))
}

fn rounded_cell_dimension(size: u16, cells: u16) -> Option<u16> {
    let size = u32::from(size);
    let cells = u32::from(cells);
    // Round to the nearest pixel dimension.
    let dim = (size * 2 / cells).div_ceil(2);
    let Ok(dim) = u16::try_from(dim) else {
        return None;
    };
    (dim > 0).then_some(dim)
}

#[allow(
    deprecated,
    reason = "stdio capability queries can outlive their timeout and steal TUI input"
)]
fn picker_from_font_size(font_size: FontSize) -> Picker {
    Picker::from_fontsize(font_size)
}

fn env_hint(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) | Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::debug!(name, "terminal environment hint is not valid UTF-8");
            None
        }
    }
}

pub(crate) fn supports_images() -> bool {
    !matches!(env_hint("TERM").as_deref(), Some("dumb"))
}

fn detect_protocol(
    term: Option<&str>,
    term_program: Option<&str>,
    lc_terminal: Option<&str>,
    env_present: impl Fn(&str) -> bool,
) -> ProtocolType {
    use ProtocolType::{Halfblocks, Iterm2, Kitty, Sixel};

    if env_present("KONSOLE_VERSION") || env_present("ALACRITTY_WINDOW_ID") {
        return Halfblocks;
    }

    if env_present("KITTY_WINDOW_ID") || env_present("GHOSTTY_RESOURCES_DIR") {
        return Kitty;
    }

    if env_present("WEZTERM_PANE")
        || env_present("WEZTERM_EXECUTABLE")
        || env_present("ITERM_SESSION_ID")
    {
        return Iterm2;
    }

    if let Some(tp) = term_program {
        match tp.to_lowercase().as_str() {
            "ghostty" | "xterm-ghostty" => return Kitty,
            "wezterm" | "iterm.app" | "rio" | "tabby" | "hyper" | "bobcat" | "mintty"
            | "vscode" => return Iterm2,
            "warpterminal" | "apple_terminal" | "alacritty" | "contour" | "ctx" | "black box" => {
                return Halfblocks;
            }
            _ => {}
        }
    }

    if lc_terminal.is_some_and(|value| value.to_lowercase().contains("iterm")) {
        return Iterm2;
    }

    if let Some(t) = term {
        let t = t.to_lowercase();
        if t.starts_with("xterm-kitty") || t.starts_with("xterm-ghostty") {
            return Kitty;
        }
        if t.starts_with("foot") || t.starts_with("mlterm") {
            return Sixel;
        }
        if t.starts_with("wezterm") || t.starts_with("rio") {
            return Iterm2;
        }
        if t.starts_with("xterm")
            && env_present("XTERM_VERSION")
            && (t.contains("340") || t.contains("sixel"))
        {
            return Sixel;
        }
        if t.starts_with("alacritty") || t.starts_with("contour") || t.starts_with("konsole") {
            return Halfblocks;
        }
    }

    Halfblocks
}

pub struct TerminalImage {
    pub protocol: SlicedProtocol,
    pub size: Size,
}

impl TerminalImage {
    fn decode(
        source: &ImageSource,
        picker: &Picker,
        max_width: u16,
    ) -> Result<Self, TerminalImageError> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(source.data.as_bytes())?;
        let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(image::ImageError::from)?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
        limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
        limits.max_alloc = Some(MAX_IMAGE_ALLOC_BYTES);
        reader.limits(limits);
        let dyn_img = reader.decode()?;
        let target = Size::new(max_width, UNBOUNDED_HEIGHT);
        let protocol = SlicedProtocol::new_with_resize(picker, dyn_img, target, Resize::Fit(None))?;
        let size = protocol.size();
        Ok(Self { protocol, size })
    }
}

/// Defers the expensive decode (base64 + image decode + resize) from
/// segment construction to the first time the image is actually rendered.
/// Segments that are scrolled out of view never pay the cost.
pub struct LazyImage {
    source: ImageSource,
    picker: Arc<Picker>,
    width: u16,
    decoded: OnceCell<Result<TerminalImage, TerminalImageError>>,
}

impl LazyImage {
    pub fn new(source: ImageSource, picker: Arc<Picker>, width: u16) -> Self {
        Self {
            source,
            picker,
            width,
            decoded: OnceCell::new(),
        }
    }

    pub fn get_or_decode(&self) -> Result<&TerminalImage, &TerminalImageError> {
        self.decoded
            .get_or_init(|| TerminalImage::decode(&self.source, &self.picker, self.width))
            .as_ref()
    }

    pub fn size(&self) -> Result<Size, &TerminalImageError> {
        self.get_or_decode().map(|img| img.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use image::ImageBuffer;
    use maki_agent::{ImageMediaType, ImageSource};
    use test_case::test_case;

    const TEST_IMAGE_SIZE: u32 = 8;
    const TEST_IMAGE_MAX_WIDTH: u16 = 4;
    const TEST_PIXEL_WIDTH: u16 = 1200;
    const TEST_PIXEL_HEIGHT: u16 = 800;
    const TEST_TERM_COLUMNS: u16 = 120;
    const TEST_TERM_ROWS: u16 = 40;
    const EXPECTED_CELL_WIDTH: u16 = 10;
    const EXPECTED_CELL_HEIGHT: u16 = 20;
    const ROUNDED_PIXEL_WIDTH: u16 = 1199;
    const ROUNDED_EXPECTED_CELL_WIDTH: u16 = 10;

    fn env_with<'a>(present: &'a [&str]) -> impl Fn(&str) -> bool + 'a {
        |name| present.contains(&name)
    }

    fn no_env(_: &str) -> bool {
        false
    }

    #[test]
    fn from_source_decodes_png_and_fits_width() {
        let img = ImageBuffer::from_fn(TEST_IMAGE_SIZE, TEST_IMAGE_SIZE, |x, y| {
            image::Rgb([
                u8::try_from(x * 32).unwrap(),
                u8::try_from(y * 32).unwrap(),
                128,
            ])
        });
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let source = ImageSource {
            media_type: ImageMediaType::Png,
            data: base64::engine::general_purpose::STANDARD
                .encode(&png)
                .into(),
        };
        let picker = Picker::halfblocks();
        let term_img = TerminalImage::decode(&source, &picker, TEST_IMAGE_MAX_WIDTH).unwrap();
        assert!(term_img.size.width > 0 && term_img.size.width <= TEST_IMAGE_MAX_WIDTH);
        assert!(term_img.size.height > 0);
    }

    #[test]
    fn cell_size_requires_pixel_and_cell_dimensions() {
        let size = cell_size(
            TEST_PIXEL_WIDTH,
            TEST_PIXEL_HEIGHT,
            TEST_TERM_COLUMNS,
            TEST_TERM_ROWS,
        )
        .unwrap();
        assert_eq!(
            (size.width, size.height),
            (EXPECTED_CELL_WIDTH, EXPECTED_CELL_HEIGHT)
        );
        assert!(cell_size(0, TEST_PIXEL_HEIGHT, TEST_TERM_COLUMNS, TEST_TERM_ROWS).is_none());
        assert!(cell_size(TEST_PIXEL_WIDTH, TEST_PIXEL_HEIGHT, 0, TEST_TERM_ROWS).is_none());
    }

    #[test]
    fn cell_size_rounds_to_nearest_pixel_dimension() {
        let size = cell_size(
            ROUNDED_PIXEL_WIDTH,
            TEST_PIXEL_HEIGHT,
            TEST_TERM_COLUMNS,
            TEST_TERM_ROWS,
        )
        .unwrap();
        assert_eq!(size.width, ROUNDED_EXPECTED_CELL_WIDTH);
    }

    #[test]
    fn kitty_is_detected_from_term_and_env() {
        assert_eq!(
            detect_protocol(Some("xterm-kitty"), None, None, no_env),
            ProtocolType::Kitty
        );
        assert_eq!(
            detect_protocol(Some("xterm-ghostty"), None, None, no_env),
            ProtocolType::Kitty
        );
        assert_eq!(
            detect_protocol(
                Some("xterm-256color"),
                None,
                None,
                env_with(&["KITTY_WINDOW_ID"])
            ),
            ProtocolType::Kitty
        );
        assert_eq!(
            detect_protocol(Some("xterm-256color"), Some("ghostty"), None, no_env),
            ProtocolType::Kitty
        );
        assert_eq!(
            detect_protocol(None, None, None, env_with(&["GHOSTTY_RESOURCES_DIR"])),
            ProtocolType::Kitty
        );
    }

    #[test_case("WezTerm")]
    #[test_case("iTerm.app")]
    #[test_case("Rio")]
    #[test_case("Tabby")]
    #[test_case("Hyper")]
    #[test_case("Bobcat")]
    #[test_case("mintty")]
    #[test_case("vscode")]
    fn iterm2_is_detected_from_term_program(term_program: &str) {
        assert_eq!(
            detect_protocol(Some("xterm-256color"), Some(term_program), None, no_env),
            ProtocolType::Iterm2,
            "expected {term_program} to use iTerm2"
        );
    }

    #[test]
    fn iterm2_is_detected_from_term_and_env() {
        assert_eq!(
            detect_protocol(Some("wezterm"), None, None, no_env),
            ProtocolType::Iterm2
        );
        let iterm_env = |name: &str| {
            matches!(
                name,
                "WEZTERM_PANE" | "WEZTERM_EXECUTABLE" | "ITERM_SESSION_ID"
            )
        };
        assert_eq!(
            detect_protocol(None, None, None, iterm_env),
            ProtocolType::Iterm2
        );
        assert_eq!(
            detect_protocol(None, None, Some("iTerm2"), no_env),
            ProtocolType::Iterm2
        );
    }

    #[test]
    fn sixel_is_detected_for_foot_mlterm_and_xterm_340() {
        assert_eq!(
            detect_protocol(Some("foot"), None, None, no_env),
            ProtocolType::Sixel
        );
        assert_eq!(
            detect_protocol(Some("foot-256color"), None, None, no_env),
            ProtocolType::Sixel
        );
        assert_eq!(
            detect_protocol(Some("mlterm"), None, None, no_env),
            ProtocolType::Sixel
        );
        assert_eq!(
            detect_protocol(
                Some("xterm-340color"),
                None,
                None,
                env_with(&["XTERM_VERSION"])
            ),
            ProtocolType::Sixel
        );
    }

    #[test_case("WarpTerminal")]
    #[test_case("Apple_Terminal")]
    #[test_case("Alacritty")]
    #[test_case("Contour")]
    #[test_case("ctx")]
    #[test_case("Black Box")]
    fn broken_term_programs_fall_back_to_halfblocks(term_program: &str) {
        assert_eq!(
            detect_protocol(Some("xterm-256color"), Some(term_program), None, no_env),
            ProtocolType::Halfblocks,
            "expected {term_program} to fall back to halfblocks"
        );
    }

    #[test]
    fn unknown_terminals_fall_back_to_halfblocks() {
        assert_eq!(
            detect_protocol(Some("xterm-256color"), None, None, no_env),
            ProtocolType::Halfblocks
        );
        assert_eq!(
            detect_protocol(None, None, None, no_env),
            ProtocolType::Halfblocks
        );
        assert_eq!(
            detect_protocol(
                Some("xterm-256color"),
                None,
                None,
                env_with(&["KONSOLE_VERSION"])
            ),
            ProtocolType::Halfblocks
        );
        assert_eq!(
            detect_protocol(
                Some("xterm-256color"),
                None,
                None,
                env_with(&["ALACRITTY_WINDOW_ID"])
            ),
            ProtocolType::Halfblocks
        );
    }

    #[test_case("skitty")]
    #[test_case("superiterm")]
    #[test_case("mywezterm")]
    #[test_case("bigfoot")]
    fn false_positive_term_name_falls_back(term: &str) {
        assert_eq!(
            detect_protocol(Some(term), None, None, no_env),
            ProtocolType::Halfblocks,
            "expected {term} to fall back to halfblocks"
        );
    }

    #[test_case("screen-256color")]
    #[test_case("tmux-256color")]
    fn outer_terminal_env_marker_outranks_tmux_screen(term: &str) {
        assert_eq!(
            detect_protocol(Some(term), None, None, env_with(&["KITTY_WINDOW_ID"])),
            ProtocolType::Kitty,
            "expected {term} with KITTY_WINDOW_ID to use Kitty"
        );
    }
}
