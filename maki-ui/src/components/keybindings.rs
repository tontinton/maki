use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use strum::EnumIter;
use unicode_width::UnicodeWidthStr;

use self::KeybindContext as Ctx;

macro_rules! mod_key {
    ($suffix:expr) => {
        concat!("Ctrl+", $suffix)
    };
}

macro_rules! ctrl_bind {
    ($upper:literal) => {
        Bind {
            code: KeyCode::Char($upper.as_bytes()[0].to_ascii_lowercase() as char),
            modifiers: KeyModifiers::CONTROL,
            label: mod_key!($upper),
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub label: &'static str,
}

impl Bind {
    pub fn matches(&self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }

    #[cfg(test)]
    pub const fn to_key_event(self) -> KeyEvent {
        KeyEvent {
            code: self.code,
            modifiers: self.modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }
}

pub mod key {
    use super::Bind;
    use crossterm::event::{KeyCode, KeyModifiers};

    pub const QUIT: Bind = ctrl_bind!("C");
    pub const HELP: Bind = ctrl_bind!("H");
    pub const PASTE: Bind = ctrl_bind!("V");
    pub const SESSIONS: Bind = ctrl_bind!("P");
    pub const TASKS: Bind = ctrl_bind!("X");
    pub const BACKGROUND: Bind = ctrl_bind!("B");
    pub const SCROLL_HALF_UP: Bind = ctrl_bind!("U");
    pub const SCROLL_HALF_DOWN: Bind = ctrl_bind!("D");
    pub const SCROLL_PAGE_UP: Bind = Bind {
        code: KeyCode::PageUp,
        modifiers: KeyModifiers::NONE,
        label: "PageUp",
    };
    pub const SCROLL_PAGE_DOWN: Bind = Bind {
        code: KeyCode::PageDown,
        modifiers: KeyModifiers::NONE,
        label: "PageDown",
    };
    pub const SCROLL_LINE_UP: Bind = ctrl_bind!("Y");
    pub const SCROLL_LINE_DOWN: Bind = ctrl_bind!("E");
    pub const SCROLL_TOP: Bind = ctrl_bind!("G");
    pub const SCROLL_BOTTOM: Bind = Bind {
        code: KeyCode::Char('g'),
        modifiers: KeyModifiers::CONTROL.union(KeyModifiers::ALT),
        label: mod_key!("Alt+G"),
    };
    pub const POP_QUEUE: Bind = ctrl_bind!("Q");
    pub const DELETE_WORD: Bind = ctrl_bind!("W");
    pub const SEARCH: Bind = ctrl_bind!("F");
    pub const FORK: Bind = ctrl_bind!("F");
    pub const FILE_PICKER: Bind = ctrl_bind!("S");
    pub const OPEN_EDITOR: Bind = ctrl_bind!("O");
    pub const PLAN_TOGGLE: Bind = ctrl_bind!("T");
    pub const MODEL_PICKER: Bind = ctrl_bind!("L");
    pub const REFRESH: Bind = ctrl_bind!("R");
    pub const SUSPEND: Bind = ctrl_bind!("Z");
    pub const DELETE: Bind = ctrl_bind!("D");
    pub const KILL_LINE: Bind = ctrl_bind!("K");
    pub const LINE_START: Bind = ctrl_bind!("A");
    pub const LINE_END: Bind = ctrl_bind!("E");
    pub const EDIT_INPUT: Bind = Bind {
        code: KeyCode::Char('o'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+O",
    };
    pub const UNDO: Bind = Bind {
        code: KeyCode::Char('z'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+Z",
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum KeybindContext {
    General,
    Editing,
    Streaming,
    Picker,
    FormInput,
    RewindPicker,
    ThemePicker,
    ModelPicker,
    QueueFocus,
    CommandPalette,
    Search,
    FilePicker,
}

impl KeybindContext {
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::Streaming => "While Streaming",
            Self::Picker => "Pickers",
            Self::FormInput => "Form",
            Self::RewindPicker => "Rewind Picker",
            Self::ThemePicker => "Theme Picker",
            Self::ModelPicker => "Model Picker",
            Self::QueueFocus => "Queue",
            Self::CommandPalette => "Commands",
            Self::Search => "Search",
            Self::FilePicker => "File Picker",
        }
    }

    pub const fn parent(self) -> Option<KeybindContext> {
        match self {
            Self::RewindPicker
            | Self::ThemePicker
            | Self::ModelPicker
            | Self::QueueFocus
            | Self::CommandPalette
            | Self::Search
            | Self::FilePicker => Some(Self::Picker),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    All,
    MacOnly,
    UnixOnly,
}

impl Platform {
    pub const fn is_visible(self) -> bool {
        match self {
            Self::All => true,
            Self::MacOnly => cfg!(target_os = "macos"),
            Self::UnixOnly => cfg!(unix),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum KeyLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    /// Alt on Mac, Single (first) on other platforms
    MacAlt(&'static str, &'static str),
    /// Multi on Mac, Multi (first slice) on other platforms
    MacMulti(&'static [&'static str], &'static [&'static str]),
}

pub const ALT_SEP: &str = " / ";

#[derive(Debug, Clone, Copy)]
pub enum ResolvedLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    Multi(&'static [&'static str]),
}

impl ResolvedLabel {
    pub fn display_width(self) -> usize {
        match self {
            Self::Single(s) => UnicodeWidthStr::width(s),
            Self::Alt(a, b) => {
                let sep_w = UnicodeWidthStr::width(ALT_SEP);
                UnicodeWidthStr::width(a) + sep_w + UnicodeWidthStr::width(b)
            }
            Self::Multi(keys) => {
                let sep_w = UnicodeWidthStr::width(ALT_SEP);
                keys.iter()
                    .map(|k| UnicodeWidthStr::width(*k))
                    .sum::<usize>()
                    + sep_w * keys.len().saturating_sub(1)
            }
        }
    }
}

impl KeyLabel {
    pub fn resolve(self) -> ResolvedLabel {
        match self {
            Self::Single(s) => ResolvedLabel::Single(s),
            Self::Alt(a, b) => ResolvedLabel::Alt(a, b),
            Self::MacAlt(a, b) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Alt(a, b)
                } else {
                    ResolvedLabel::Single(a)
                }
            }
            Self::MacMulti(normal, mac) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Multi(mac)
                } else {
                    ResolvedLabel::Multi(normal)
                }
            }
        }
    }

    #[cfg(test)]
    fn flat_str(&self) -> String {
        match self.resolve() {
            ResolvedLabel::Single(s) => s.to_string(),
            ResolvedLabel::Alt(a, b) => format!("{a}/{b}"),
            ResolvedLabel::Multi(keys) => keys.join("/"),
        }
    }
}

pub struct Keybind {
    pub label: KeyLabel,
    pub description: &'static str,
    pub context: KeybindContext,
    pub platform: Platform,
}

impl Keybind {
    const fn on(self, platform: Platform) -> Self {
        Self { platform, ..self }
    }
}

const fn kb(label: KeyLabel, description: &'static str, context: Ctx) -> Keybind {
    Keybind {
        label,
        description,
        context,
        platform: Platform::All,
    }
}

const fn single(label: &'static str, description: &'static str, context: Ctx) -> Keybind {
    kb(KeyLabel::Single(label), description, context)
}

pub const KEYBINDS: &[Keybind] = &[
    single(
        key::QUIT.label,
        "Quit / clear input / copy selection",
        Ctx::General,
    ),
    single(key::HELP.label, "Show keybindings", Ctx::General),
    single(key::SEARCH.label, "Search messages", Ctx::General),
    single(key::FILE_PICKER.label, "File picker", Ctx::General),
    single(key::OPEN_EDITOR.label, "Open plan in editor", Ctx::General),
    single(key::PLAN_TOGGLE.label, "Toggle plan panel", Ctx::General),
    single(key::MODEL_PICKER.label, "Model picker", Ctx::General),
    single(key::SUSPEND.label, "Suspend process", Ctx::General).on(Platform::UnixOnly),
    single("Enter", "Submit prompt", Ctx::Editing),
    kb(
        KeyLabel::MacMulti(
            &["Shift+Enter", "Ctrl+Enter", "Ctrl+J", "Alt+Enter"],
            &["⇧↵", "⌃↵", "⌃J", "⌥↵"],
        ),
        "Newline",
        Ctx::Editing,
    ),
    single("Tab", "Switch between plan and build", Ctx::Editing),
    single("/command", "Open command palette", Ctx::Editing),
    kb(
        KeyLabel::MacAlt(key::DELETE_WORD.label, "⌥⌫"),
        "Delete word backward",
        Ctx::Editing,
    ),
    kb(
        KeyLabel::MacMulti(&["Alt+←", "Alt+→"], &["⌥←", "⌥→"]),
        "Move word left / right",
        Ctx::Editing,
    ),
    single("Shift+motion", "Select text", Ctx::Editing),
    kb(
        KeyLabel::Alt(mod_key!("Del"), "⌥Del"),
        "Delete word forward",
        Ctx::Editing,
    )
    .on(Platform::MacOnly),
    single(key::KILL_LINE.label, "Delete to end of line", Ctx::Editing).on(Platform::MacOnly),
    single(key::LINE_START.label, "Jump to start of line", Ctx::Editing),
    kb(
        KeyLabel::Alt("Home", "End"),
        "Jump to start/end of line",
        Ctx::Editing,
    ),
    kb(
        KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        "Scroll half page up / down",
        Ctx::Editing,
    ),
    kb(
        KeyLabel::Alt(key::SCROLL_PAGE_UP.label, key::SCROLL_PAGE_DOWN.label),
        "Scroll page up / down",
        Ctx::Editing,
    ),
    single(key::LINE_END.label, "Jump to end of line", Ctx::Editing),
    single(key::SCROLL_TOP.label, "Scroll to top", Ctx::Editing),
    single(key::SCROLL_BOTTOM.label, "Scroll to bottom", Ctx::Editing),
    single(key::POP_QUEUE.label, "Pop queue", Ctx::Editing),
    single("Esc Esc", "Rewind", Ctx::Editing),
    single(
        key::EDIT_INPUT.label,
        "Edit input in external editor",
        Ctx::Editing,
    ),
    single(
        key::PASTE.label,
        "Paste clipboard image or text",
        Ctx::Editing,
    ),
    single(key::UNDO.label, "Undo typing / paste", Ctx::Editing),
    kb(
        KeyLabel::Alt("↑", "↓"),
        "Navigate input history",
        Ctx::Streaming,
    ),
    single("Esc Esc", "Cancel agent", Ctx::Streaming),
    single(
        key::BACKGROUND.label,
        "Move running calls to the background (tmux: press twice)",
        Ctx::Streaming,
    ),
    kb(KeyLabel::Alt("↑", "↓"), "Navigate options", Ctx::FormInput),
    single("Enter", "Select option", Ctx::FormInput),
    single("Esc", "Close", Ctx::FormInput),
    kb(KeyLabel::Alt("↑", "↓"), "Navigate", Ctx::Picker),
    single("Enter", "Select", Ctx::Picker),
    single("Esc", "Close", Ctx::Picker),
    single("Type", "Filter", Ctx::Picker),
    kb(
        KeyLabel::Alt("PageUp", "PageDown"),
        "Scroll page up / down",
        Ctx::Picker,
    ),
    kb(
        KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        "Scroll half page up / down",
        Ctx::Picker,
    ),
    single(
        key::FORK.label,
        "Fork up to the selected prompt and switch to it",
        Ctx::RewindPicker,
    ),
    single("Enter", "Remove item", Ctx::QueueFocus),
    single("Tab", "Complete command", Ctx::CommandPalette),
    single(
        "!/@/#/$",
        "Set tier (strong/medium/weak/compaction)",
        Ctx::ModelPicker,
    ),
    single("%", "Fold or unfold provider", Ctx::ModelPicker),
];

pub fn all_contexts() -> impl Iterator<Item = KeybindContext> {
    use strum::IntoEnumIterator;
    KeybindContext::iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn bind_requires_exact_modifiers() {
        let bind = key::OPEN_EDITOR; // Ctrl+O
        let exact = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let extra = KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let wrong = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT);

        assert!(bind.matches(exact));
        assert!(!bind.matches(extra), "extra modifiers should not match");
        assert!(!bind.matches(wrong), "wrong modifier should not match");
    }

    #[test]
    fn every_context_has_at_least_one_keybind() {
        for ctx in all_contexts() {
            let has_own = KEYBINDS.iter().any(|kb| kb.context == ctx);
            let has_parent = ctx
                .parent()
                .is_some_and(|p| KEYBINDS.iter().any(|kb| kb.context == p));
            assert!(
                has_own || has_parent,
                "context {:?} has no keybinds and no parent with keybinds",
                ctx,
            );
        }
    }

    #[test]
    fn no_duplicate_entries() {
        for (i, a) in KEYBINDS.iter().enumerate() {
            for (j, b) in KEYBINDS.iter().enumerate() {
                if i != j && a.context == b.context {
                    assert!(
                        a.label.flat_str() != b.label.flat_str() || a.description != b.description,
                        "duplicate keybind: {} - {} in {:?}",
                        a.label.flat_str(),
                        a.description,
                        a.context,
                    );
                }
            }
        }
    }
}
