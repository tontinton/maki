use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use strum::EnumIter;
use unicode_width::UnicodeWidthStr;

macro_rules! mod_key {
    ($suffix:expr) => {
        if cfg!(target_os = "macos") {
            concat!("⌘", $suffix)
        } else {
            concat!("Ctrl+", $suffix)
        }
    };
}

macro_rules! upper {
    ('a') => {
        "A"
    };
    ('b') => {
        "B"
    };
    ('c') => {
        "C"
    };
    ('d') => {
        "D"
    };
    ('e') => {
        "E"
    };
    ('f') => {
        "F"
    };
    ('g') => {
        "G"
    };
    ('h') => {
        "H"
    };
    ('i') => {
        "I"
    };
    ('j') => {
        "J"
    };
    ('k') => {
        "K"
    };
    ('l') => {
        "L"
    };
    ('m') => {
        "M"
    };
    ('n') => {
        "N"
    };
    ('o') => {
        "O"
    };
    ('p') => {
        "P"
    };
    ('q') => {
        "Q"
    };
    ('r') => {
        "R"
    };
    ('s') => {
        "S"
    };
    ('t') => {
        "T"
    };
    ('u') => {
        "U"
    };
    ('v') => {
        "V"
    };
    ('w') => {
        "W"
    };
    ('x') => {
        "X"
    };
    ('y') => {
        "Y"
    };
    ('z') => {
        "Z"
    };
}

macro_rules! ctrl_bind {
    ($char:tt) => {
        Bind {
            code: KeyCode::Char($char),
            modifiers: KeyModifiers::CONTROL,
            label: mod_key!(upper!($char)),
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

    pub const QUIT: Bind = ctrl_bind!('c');
    pub const HELP: Bind = ctrl_bind!('h');
    pub const PREV_CHAT: Bind = ctrl_bind!('p');
    pub const NEXT_CHAT: Bind = ctrl_bind!('n');
    pub const SCROLL_HALF_UP: Bind = ctrl_bind!('u');
    pub const SCROLL_HALF_DOWN: Bind = ctrl_bind!('d');
    pub const SCROLL_LINE_UP: Bind = ctrl_bind!('y');
    pub const SCROLL_LINE_DOWN: Bind = ctrl_bind!('e');
    pub const SCROLL_TOP: Bind = ctrl_bind!('g');
    pub const SCROLL_BOTTOM: Bind = ctrl_bind!('b');
    pub const POP_QUEUE: Bind = ctrl_bind!('q');
    pub const DELETE_WORD: Bind = ctrl_bind!('w');
    pub const SEARCH: Bind = ctrl_bind!('f');
    pub const OPEN_EDITOR: Bind = ctrl_bind!('o');
    pub const TODO_PANEL: Bind = ctrl_bind!('t');
    pub const TASKS: Bind = ctrl_bind!('x');
    pub const DELETE: Bind = ctrl_bind!('d');
    pub const KILL_LINE: Bind = ctrl_bind!('k');
    pub const LINE_START: Bind = ctrl_bind!('a');
    pub const EDIT_INPUT: Bind = Bind {
        code: KeyCode::Char('o'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+O",
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum KeybindContext {
    General,
    Editing,
    Streaming,
    Picker,
    FormInput,
    TaskPicker,
    SessionPicker,
    RewindPicker,
    ThemePicker,
    ModelPicker,
    QueueFocus,
    CommandPalette,
    Search,
}

impl KeybindContext {
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::Streaming => "While Streaming",
            Self::Picker => "Pickers",
            Self::FormInput => "Form",
            Self::TaskPicker => "Task Picker",
            Self::SessionPicker => "Session Picker",
            Self::RewindPicker => "Rewind Picker",
            Self::ThemePicker => "Theme Picker",
            Self::ModelPicker => "Model Picker",
            Self::QueueFocus => "Queue",
            Self::CommandPalette => "Commands",
            Self::Search => "Search",
        }
    }

    pub const fn parent(self) -> Option<KeybindContext> {
        match self {
            Self::TaskPicker
            | Self::SessionPicker
            | Self::RewindPicker
            | Self::ThemePicker
            | Self::ModelPicker
            | Self::QueueFocus
            | Self::CommandPalette
            | Self::Search => Some(Self::Picker),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    All,
    MacOnly,
}

impl Platform {
    pub const fn is_visible(self) -> bool {
        match self {
            Self::All => true,
            Self::MacOnly => cfg!(target_os = "macos"),
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

pub const KEYBINDS: &[Keybind] = &[
    Keybind {
        label: KeyLabel::Single(key::QUIT.label),
        description: "Quit / clear input",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::HELP.label),
        description: "Show keybindings",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::NEXT_CHAT.label, key::PREV_CHAT.label),
        description: "Next / previous task chat",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SEARCH.label),
        description: "Search messages",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::OPEN_EDITOR.label),
        description: "Open plan in editor",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::TODO_PANEL.label),
        description: "Toggle todo panel",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::TASKS.label),
        description: "Open tasks",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Submit prompt",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::MacMulti(&["\\+Enter", "Ctrl+J", "Alt+Enter"], &["⇧↵", "⌃J", "⌥↵"]),
        description: "Newline",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Tab"),
        description: "Toggle mode",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("/command"),
        description: "Open command palette",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::MacAlt(key::DELETE_WORD.label, "⌥⌫"),
        description: "Delete word backward",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(mod_key!("←"), mod_key!("→")),
        description: "Move word left / right",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        label: KeyLabel::Alt(mod_key!("Del"), "⌥Del"),
        description: "Delete word forward",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        label: KeyLabel::Single(key::KILL_LINE.label),
        description: "Delete to end of line",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        label: KeyLabel::Single(key::LINE_START.label),
        description: "Jump to start of line",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        label: KeyLabel::Alt("⌘←", "⌘→"),
        description: "Jump to start/end of line",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        label: KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        description: "Scroll half page up / down",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::SCROLL_LINE_UP.label, key::SCROLL_LINE_DOWN.label),
        description: "Scroll one line up / down",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SCROLL_TOP.label),
        description: "Scroll to top",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SCROLL_BOTTOM.label),
        description: "Scroll to bottom",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::POP_QUEUE.label),
        description: "Pop queue",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc Esc"),
        description: "Rewind",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::EDIT_INPUT.label),
        description: "Edit input in external editor",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate input history",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc Esc"),
        description: "Cancel agent",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate options",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Select option",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Select",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Type"),
        description: "Filter",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::DELETE.label),
        description: "Delete session",
        context: KeybindContext::SessionPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Remove item",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Tab"),
        description: "Toggle mode",
        context: KeybindContext::CommandPalette,
        platform: Platform::All,
    },
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
