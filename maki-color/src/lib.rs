//! The color vocabulary every layer of maki speaks, and the running theme's
//! published style registry.
//!
//! [`SegmentColor`] is the one spelling a color has between crates: the UI
//! converts its ratatui `Color`s into it, the syntax highlighter emits it,
//! and the Lua API parses plugin strings into it. [`UiStyle`] is the style
//! table a theme publishes for plugins.
//!
//! The [`Registry`] holds what the UI installs whenever a theme takes over:
//! the named styles, the theme `[palette]`, and any runtime color overrides
//! a plugin set through `maki.ui.set_theme_color`. It lives in this crate
//! because both `maki-lua` (synchronous reads while a plugin paints) and
//! `maki-ui` (the only writer) must reach it, and neither can depend on the
//! other. Reads are cheap; the UI thread is the only writer.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The one spelling of "default" every layer accepts in a color string.
pub const DEFAULT_COLOR_NAME: &str = "default";

const HEX_RGB_LEN: usize = 6;

type Rgb = (u8, u8, u8);

/// A color exactly as a terminal can paint it. `Default` is "whatever the
/// theme's own color is", the same meaning an absent override carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentColor {
    Rgb(Rgb),
    Ansi(u8),
    Default,
}

impl SegmentColor {
    /// The one reader of the `#rrggbb | index | name | default` grammar, so
    /// themes, syntax scopes and plugin spans cannot drift apart on what
    /// they accept.
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(hex) = s.strip_prefix('#') {
            return parse_hex_rgb(hex).map(Self::Rgb);
        }
        if s == DEFAULT_COLOR_NAME {
            return Some(Self::Default);
        }
        if s.bytes().all(|b| b.is_ascii_digit()) {
            return s.parse().ok().map(Self::Ansi);
        }
        ansi_color_index(s).map(Self::Ansi)
    }
}

/// Rejects the `+4` and `-0` that `u8::from_str` would otherwise accept, and
/// any casing or separator sloppiness, so a spelling either resolves exactly
/// or not at all.
fn parse_hex_rgb(hex: &str) -> Option<Rgb> {
    if hex.len() != HEX_RGB_LEN || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

/// Resolve a terminal color name to its palette index. These are Helix's
/// palette names, so its themes load unchanged. Spelling is exact:
/// `light-gray` resolves, `lightgray` and `LIGHT-GRAY` do not.
pub fn ansi_color_index(name: &str) -> Option<u8> {
    let index = match name {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "gray" => 7,
        "light-gray" => 8,
        "light-red" => 9,
        "light-green" => 10,
        "light-yellow" => 11,
        "light-blue" => 12,
        "light-magenta" => 13,
        "light-cyan" => 14,
        "white" => 15,
        _ => return None,
    };
    Some(index)
}

/// A published named style: [`SegmentColor`]s plus the modifiers a
/// `ratatui::style::Style` can carry, in the shape a plugin reads through
/// `maki.ui.theme_style`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UiStyle {
    pub fg: Option<SegmentColor>,
    pub bg: Option<SegmentColor>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub strikethrough: bool,
    pub reversed: bool,
}

#[derive(Default)]
struct State {
    styles: HashMap<String, UiStyle>,
    /// The published names whose foreground can be overridden. Two published
    /// styles build a `Style` from a `Theme` color instead of naming a field,
    /// so they have no foreground slot to patch.
    settable: HashSet<String>,
    palette: HashMap<String, String>,
    overrides: HashMap<String, SegmentColor>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn lock() -> MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Serializes tests that mutate the registry, mirroring how the theme
/// globals are guarded in the crates above. Held for the whole case: the
/// registry is process-global state. Visible to dependent crates' test
/// suites, which share the same globals.
static TEST_GUARD: Mutex<()> = Mutex::new(());

/// A hold on [`TEST_GUARD`]: drop it when the case ends.
#[doc(hidden)]
pub fn exclusive() -> MutexGuard<'static, ()> {
    TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The running theme's published styles, `[palette]`, and runtime color
/// overrides. The UI thread is the only writer; `maki-lua` reads
/// synchronously while a plugin paints.
pub struct Registry;

impl Registry {
    /// The three mode colors that paint the input box border. Not `Style`
    /// fields on the UI's `Theme`, so the UI publishes them as styles
    /// itself.
    pub const MODE_STYLES: [&'static str; 3] = ["mode_build", "mode_plan", "mode_bash"];

    /// Replace every published style, and the subset whose foreground a
    /// plugin may override.
    pub fn set_styles(styles: HashMap<String, UiStyle>, settable: &[&str]) {
        let mut state = lock();
        state.styles = styles;
        state.settable = settable.iter().map(|name| (*name).to_owned()).collect();
    }

    /// The published style for {name}, with any runtime override already
    /// merged into its foreground.
    pub fn style(name: &str) -> Option<UiStyle> {
        let s = lock();
        let mut style = s.styles.get(name)?.clone();
        if let Some(fg) = s.overrides.get(name) {
            style.fg = Some(*fg);
        }
        Some(style)
    }

    /// The runtime foreground override for {name}, if a plugin set one.
    pub fn override_color(name: &str) -> Option<SegmentColor> {
        lock().overrides.get(name).copied()
    }

    /// Every active override, `{ name: color }`.
    pub fn color_overrides() -> HashMap<String, SegmentColor> {
        lock().overrides.clone()
    }

    /// Whether a plugin may override {name}. The registry answers from the
    /// list the UI published, so a caller cannot pass a stale second list.
    pub fn is_settable(name: &str) -> bool {
        let state = lock();
        state.settable.contains(name) || Self::MODE_STYLES.contains(&name)
    }

    /// Set or clear a runtime foreground override. {color} `None` restores
    /// the published color. A name that is not settable is refused, so a
    /// typo never reads back as a color the screen does not paint.
    pub fn set_override(name: &str, color: Option<SegmentColor>) -> bool {
        let mut state = lock();
        if !state.settable.contains(name) && !Self::MODE_STYLES.contains(&name) {
            return false;
        }
        match color {
            Some(c) => {
                state.overrides.insert(name.to_owned(), c);
            }
            None => {
                state.overrides.remove(name);
            }
        }
        true
    }

    /// Replace the running theme's `[palette]`, color name to `#rrggbb`.
    pub fn set_palette(palette: HashMap<String, String>) {
        lock().palette = palette;
    }

    /// The running theme's `[palette]`. Empty when the theme has none.
    pub fn palette() -> HashMap<String, String> {
        lock().palette.clone()
    }

    /// Resolve {s} the way the running theme would: a `[palette]` name
    /// first, then a literal [`SegmentColor::parse`].
    pub fn resolve_color(s: &str) -> Option<SegmentColor> {
        let state = lock();
        if let Some(c) = state
            .palette
            .get(s)
            .and_then(|hex| SegmentColor::parse(hex))
        {
            return Some(c);
        }
        SegmentColor::parse(s)
    }

    /// Forget every runtime override, leaving the published styles in
    /// place. A case that changes one uses this to start clean.
    #[doc(hidden)]
    pub fn reset_overrides() {
        lock().overrides.clear();
    }

    /// Empty the whole registry. The state is process-global like the
    /// theme, so a suite that shares one process needs a way to start a
    /// case from a blank slate.
    #[doc(hidden)]
    pub fn reset() {
        *lock() = State::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE_BLUE: &str = "#0a0b0c";
    const LITERAL_WHITE: &str = "#ffffff";
    const BLUE: &str = "blue";

    #[test]
    fn hex_index_name_and_default_all_parse() {
        assert_eq!(
            SegmentColor::parse("#0a0b0c"),
            Some(SegmentColor::Rgb((0x0a, 0x0b, 0x0c)))
        );
        assert_eq!(SegmentColor::parse("4"), Some(SegmentColor::Ansi(4)));
        assert_eq!(SegmentColor::parse(BLUE), Some(SegmentColor::Ansi(4)));
        assert_eq!(
            SegmentColor::parse(DEFAULT_COLOR_NAME),
            Some(SegmentColor::Default)
        );
        assert_eq!(SegmentColor::parse("bananas"), None);
        assert_eq!(
            SegmentColor::parse("#0a0b"),
            None,
            "short hex does not resolve"
        );
    }

    #[test]
    fn override_roundtrip_and_clear() {
        let _registry = exclusive();
        const COLOR: SegmentColor = SegmentColor::Rgb((1, 2, 3));

        Registry::set_styles(
            HashMap::from([("input_border".to_owned(), UiStyle::default())]),
            &["input_border"],
        );
        Registry::set_override("input_border", Some(COLOR));
        assert_eq!(Registry::override_color("input_border"), Some(COLOR));

        Registry::set_override("input_border", None);
        assert_eq!(Registry::override_color("input_border"), None);
    }

    /// A typo must not read as "restore the theme color" downstream, so an
    /// unpublished name is refused before anything is painted.
    #[test]
    fn override_rejects_unknown_style_names() {
        let _registry = exclusive();
        Registry::set_styles(
            HashMap::from([("input_border".to_owned(), UiStyle::default())]),
            &["input_border"],
        );
        assert!(!Registry::set_override(
            "input_borders",
            Some(SegmentColor::Default)
        ));
        assert_eq!(Registry::override_color("input_borders"), None);
    }

    #[test]
    fn mode_colors_are_overridable_without_a_published_style() {
        let _registry = exclusive();
        const MODE_STYLE: &str = "mode_build";

        assert!(Registry::set_override(
            MODE_STYLE,
            Some(SegmentColor::Default)
        ));
        assert!(Registry::override_color(MODE_STYLE).is_some());
        Registry::set_override(MODE_STYLE, None);
    }

    #[test]
    fn palette_name_resolves_before_literal() {
        let _registry = exclusive();
        Registry::set_palette(HashMap::from([(BLUE.to_owned(), PALETTE_BLUE.to_owned())]));

        assert_eq!(
            Registry::resolve_color(BLUE),
            SegmentColor::parse(PALETTE_BLUE)
        );
        assert_eq!(
            Registry::resolve_color(LITERAL_WHITE),
            SegmentColor::parse(LITERAL_WHITE),
            "a literal still resolves when a palette is installed"
        );

        Registry::set_palette(HashMap::new());
    }

    /// The override has to be part of the style a plugin reads, or
    /// `get_theme_color` and the border it paints drift apart.
    #[test]
    fn style_read_merges_the_override() {
        let _registry = exclusive();
        const NAME: &str = "input_border";
        Registry::set_styles(
            HashMap::from([(
                NAME.to_owned(),
                UiStyle {
                    fg: Some(SegmentColor::Ansi(4)),
                    bold: true,
                    ..Default::default()
                },
            )]),
            &[NAME],
        );
        Registry::set_override(NAME, Some(SegmentColor::Rgb((9, 9, 9))));

        let style = Registry::style(NAME).unwrap();
        assert_eq!(style.fg, Some(SegmentColor::Rgb((9, 9, 9))));
        assert!(style.bold, "only the foreground changes");

        Registry::set_override(NAME, None);
        assert_eq!(
            Registry::style(NAME).unwrap().fg,
            Some(SegmentColor::Ansi(4))
        );
    }
}
