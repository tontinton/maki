//! One key identity and one spelling for it.
//!
//! A [`Key`] is built from a terminal event or from notation, and both paths
//! normalize. Parsing and printing read the same tables, so
//! `Key::parse(k.notation()) == Ok(k)` holds for every `Key`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Canonical names only. Other spellings users may type go in
/// [`NAME_ALIASES`].
const NAMED_KEYS: &[(&str, KeyCode)] = &[
    ("CR", KeyCode::Enter),
    ("Esc", KeyCode::Esc),
    ("BS", KeyCode::Backspace),
    ("Del", KeyCode::Delete),
    ("Tab", KeyCode::Tab),
    ("Space", KeyCode::Char(' ')),
    ("Up", KeyCode::Up),
    ("Down", KeyCode::Down),
    ("Left", KeyCode::Left),
    ("Right", KeyCode::Right),
    ("Home", KeyCode::Home),
    ("End", KeyCode::End),
    ("PageUp", KeyCode::PageUp),
    ("PageDown", KeyCode::PageDown),
    ("Insert", KeyCode::Insert),
];

/// Read on the way in, never printed. Each target must be a name in
/// [`NAMED_KEYS`].
const NAME_ALIASES: &[(&str, &str)] = &[
    ("Enter", "CR"),
    ("Return", "CR"),
    ("Escape", "Esc"),
    ("Backspace", "BS"),
    ("Delete", "Del"),
];

/// Canonical prefixes, in the order [`Key::notation`] prints them.
const MODIFIERS: &[(&str, KeyModifiers)] = &[
    ("C-", KeyModifiers::CONTROL),
    ("M-", KeyModifiers::ALT),
    ("S-", KeyModifiers::SHIFT),
];

const MODIFIER_ALIASES: &[(&str, KeyModifiers)] = &[
    ("ctrl-", KeyModifiers::CONTROL),
    ("alt-", KeyModifiers::ALT),
    ("a-", KeyModifiers::ALT),
    ("shift-", KeyModifiers::SHIFT),
];

/// A press carrying `SUPER`, `HYPER` or `META` has no spelling, so it is not a
/// [`Key`].
const NAMEABLE_MODIFIERS: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SHIFT);

/// Kitty reports past F12, and `<F13>` is valid vim notation.
const MAX_FUNCTION_KEY: u8 = 24;

/// Keys the host answers before it looks at any binding, because quit and
/// suspend have to work whatever a plugin is doing. A binding on one could
/// never fire, so `maki.keymap.set` and a window's `keys` refuse them. The
/// host, the keymap and the windows all read this one list, so they cannot
/// drift apart.
pub const RESERVED_KEYS: [Key; 2] = [
    Key {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
    },
    Key {
        code: KeyCode::Char('z'),
        modifiers: KeyModifiers::CONTROL,
    },
];

/// Every bracketed spelling the key lint measures a typo against, with the
/// key it names. Several spellings can name one key (`<S-a>` and `<S-A>` are
/// both `A`), so the lint suggests the key's notation, not the spelling that
/// matched.
pub(crate) fn candidate_spellings() -> Vec<(String, Key)> {
    let names: Vec<String> = NAMED_KEYS
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .chain((1..=MAX_FUNCTION_KEY).map(|n| format!("F{n}")))
        .chain(('a'..='z').map(String::from))
        .chain(('0'..='9').map(String::from))
        .collect();
    let prefixes = ["", "C-", "M-", "S-", "C-M-", "C-S-", "M-S-", "C-M-S-"];
    prefixes
        .iter()
        .flat_map(|prefix| names.iter().map(move |name| format!("<{prefix}{name}>")))
        .filter_map(|spelling| Key::parse(&spelling).ok().map(|key| (spelling, key)))
        .collect()
}

/// A keypress as plugins see it.
///
/// Not a wrapped [`KeyEvent`], because crossterm's equality also compares
/// `kind` and `state`, and two presses of the same key should be equal
/// whatever the terminal put there. `Copy` like `KeyEvent` itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Key {
    /// `None` when no notation names {event}: media keys, bare modifiers, and
    /// anything carrying a modifier notation cannot spell.
    pub fn from_event(event: KeyEvent) -> Option<Self> {
        if !NAMEABLE_MODIFIERS.contains(event.modifiers) {
            return None;
        }
        let key = Self::new(event.code, event.modifiers);
        name_of(key.code).is_some().then_some(key)
    }

    /// Reads vim notation: `<C-n>`, `<S-Tab>`, `<Space>`, `<F13>`, `a`.
    pub fn parse(lhs: &str) -> Result<Self, String> {
        let s = lhs.trim();
        if s.is_empty() {
            return Err("empty key notation".into());
        }

        if s.len() > 2 && s.starts_with('<') && s.ends_with('>') {
            let (modifiers, name) = strip_modifiers(&s[1..s.len() - 1]);
            return Ok(Self::new(code_of(name)?, modifiers));
        }

        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(Self::new(KeyCode::Char(c), KeyModifiers::NONE)),
            _ => Err(format!("invalid key notation: {s}")),
        }
    }

    /// The one spelling of this key, which [`Key::parse`] reads back into it.
    pub fn notation(&self) -> String {
        let name = name_of(self.code).unwrap_or_default();
        if self.modifiers.is_empty() && name.chars().count() == 1 {
            return name;
        }
        let mut out = String::from("<");
        for (prefix, bits) in MODIFIERS {
            if self.modifiers.contains(*bits) {
                out.push_str(prefix);
            }
        }
        out.push_str(&name);
        out.push('>');
        out
    }

    pub fn code(&self) -> KeyCode {
        self.code
    }

    pub fn modifiers(&self) -> KeyModifiers {
        self.modifiers
    }

    pub fn is_reserved(&self) -> bool {
        RESERVED_KEYS.contains(self)
    }

    fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        let (code, modifiers) = normalize(code, modifiers);
        Self { code, modifiers }
    }
}

impl From<Key> for KeyEvent {
    /// Lets the UI keep matching on [`KeyEvent`] while still seeing the
    /// normalized key a plugin sees.
    fn from(key: Key) -> Self {
        KeyEvent::new(key.code, key.modifiers)
    }
}

/// Whether the host answers {key} itself, whatever any plugin bound.
pub fn is_reserved(key: KeyEvent) -> bool {
    Key::from_event(key).is_some_and(|k| k.is_reserved())
}

/// Settles the places where terminals disagree with each other and with
/// notation. Both constructors run it, so a spelling and the press it names
/// always end up as the same key. It is idempotent, so running it twice is
/// harmless.
///
/// 1. `(Tab, SHIFT)` and `(BackTab, _)` both become `(BackTab, SHIFT)`. We
///    push the kitty flags, so a kitty terminal reports Shift+Tab as
///    `Tab + SHIFT` while every other one sends `CSI Z`, which is `BackTab`.
/// 2. With `CONTROL`, a letter is lowercase and shift stays a bit, like vim:
///    `<C-N>` is `<C-n>`, but `<C-S-n>` is its own key.
/// 3. Without `CONTROL`, the letter's case is the shift, so the bit goes:
///    `<S-a>`, `<S-A>`, `A` and a terminal's `A + SHIFT` are all `A`. A char
///    with no case, like a digit, keeps the bit: `<S-1>`.
fn normalize(code: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let mut code = match code {
        KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        other => other,
    };
    if code == KeyCode::BackTab {
        modifiers |= KeyModifiers::SHIFT;
    }
    if let KeyCode::Char(c) = code {
        if modifiers.contains(KeyModifiers::CONTROL) {
            code = KeyCode::Char(single(c.to_lowercase()).unwrap_or(c));
        } else if modifiers.contains(KeyModifiers::SHIFT)
            && let Some(upper) = shifted(c)
        {
            code = KeyCode::Char(upper);
            modifiers.remove(KeyModifiers::SHIFT);
        }
    }
    (code, modifiers)
}

/// {c} with shift applied, when it has case. `ß` uppercases to two chars, so
/// it keeps the shift bit instead.
fn shifted(c: char) -> Option<char> {
    if c.is_uppercase() {
        return Some(c);
    }
    c.is_lowercase().then(|| single(c.to_uppercase())).flatten()
}

fn single(mut chars: impl Iterator<Item = char>) -> Option<char> {
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// `BackTab` is named `Tab`, because normalization already gave it the
/// `SHIFT` that prints as `S-`.
fn name_of(code: KeyCode) -> Option<String> {
    let code = if code == KeyCode::BackTab {
        KeyCode::Tab
    } else {
        code
    };
    if let Some((name, _)) = NAMED_KEYS.iter().find(|(_, c)| *c == code) {
        return Some((*name).to_owned());
    }
    match code {
        KeyCode::Char(c) => Some(c.to_string()),
        KeyCode::F(n @ 1..=MAX_FUNCTION_KEY) => Some(format!("F{n}")),
        _ => None,
    }
}

fn strip_modifiers(inner: &str) -> (KeyModifiers, &str) {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = inner;
    'strip: loop {
        for (prefix, bits) in MODIFIERS.iter().chain(MODIFIER_ALIASES) {
            if rest.len() > prefix.len()
                && rest
                    .get(..prefix.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
            {
                modifiers |= *bits;
                rest = &rest[prefix.len()..];
                continue 'strip;
            }
        }
        return (modifiers, rest);
    }
}

fn code_of(name: &str) -> Result<KeyCode, String> {
    let canonical = NAME_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, target)| target);
    if let Some((_, code)) = NAMED_KEYS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(canonical))
    {
        return Ok(*code);
    }

    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Ok(KeyCode::Char(c));
    }

    if let Some(number) = name.strip_prefix(['f', 'F'])
        && let Ok(n) = number.parse::<u8>()
    {
        return match n {
            1..=MAX_FUNCTION_KEY => Ok(KeyCode::F(n)),
            _ => Err(format!("function key out of range: {name}")),
        };
    }

    Err(format!("unknown key: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MediaKeyCode, ModifierKeyCode};
    use test_case::test_case;

    const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const NONE: KeyModifiers = KeyModifiers::NONE;

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn every_named_key_round_trips() {
        for (name, code) in NAMED_KEYS {
            let spelling = format!("<{name}>");
            let key = Key::parse(&spelling).unwrap();
            assert_eq!(key.code(), *code, "{spelling} parses to its table entry");
            assert_eq!(key.notation(), spelling, "and prints back as itself");
        }
    }

    #[test]
    fn every_alias_prints_as_its_canonical_name() {
        for (alias, target) in NAME_ALIASES {
            let canonical = NAMED_KEYS
                .iter()
                .find(|(name, _)| name == target)
                .unwrap_or_else(|| panic!("alias {alias} targets unknown name {target}"));
            let key = Key::parse(&format!("<{alias}>")).unwrap();
            assert_eq!(key.code(), canonical.1);
            assert_eq!(key.notation(), format!("<{target}>"));
        }
    }

    #[test]
    fn every_modifier_prefix_parses_to_its_bit() {
        for (prefix, bits) in MODIFIERS.iter().chain(MODIFIER_ALIASES) {
            let key = Key::parse(&format!("<{prefix}F5>")).unwrap();
            assert_eq!(key.modifiers(), *bits, "{prefix} is {bits:?}");
        }
    }

    const CHAR_SAMPLE: &[char] = &['a', 'z', 'A', 'Z', '1', '!', 'ä', 'Ä', 'ß'];
    const MODIFIER_COMBOS: [KeyModifiers; 8] = [
        NONE,
        CONTROL,
        ALT,
        SHIFT,
        CONTROL.union(ALT),
        CONTROL.union(SHIFT),
        ALT.union(SHIFT),
        CONTROL.union(ALT).union(SHIFT),
    ];

    fn prefix_of(mods: KeyModifiers) -> String {
        MODIFIERS
            .iter()
            .filter(|(_, bits)| mods.contains(*bits))
            .map(|(prefix, _)| *prefix)
            .collect()
    }

    /// A rule applied on one side only is a binding that parses and never
    /// fires, so every sample char is tried with every modifier set.
    #[test]
    fn spelling_a_press_and_pressing_it_are_one_key() {
        for &c in CHAR_SAMPLE {
            for mods in MODIFIER_COMBOS {
                let spelled = Key::parse(&format!("<{}{c}>", prefix_of(mods))).unwrap();
                let pressed = Key::from_event(event(KeyCode::Char(c), mods)).unwrap();
                assert_eq!(spelled, pressed, "{c:?} with {mods:?}");
            }
        }
    }

    /// Terminals report a shifted letter with or without the shift bit, in
    /// either case. All of those have to be the same key.
    #[test_case('a', 'A' ; "ascii")]
    #[test_case('ä', 'Ä' ; "non_ascii")]
    fn a_letter_has_one_identity_however_shift_is_reported(lower: char, upper: char) {
        let key = |c, mods| Key::from_event(event(KeyCode::Char(c), mods)).unwrap();
        for extra in [NONE, ALT] {
            let shifted = key(upper, extra);
            assert_eq!(key(lower, extra | SHIFT), shifted);
            assert_eq!(key(upper, extra | SHIFT), shifted);
            assert!(!shifted.modifiers().contains(SHIFT), "case carries it");

            let ctrl = key(lower, extra | CONTROL);
            assert_eq!(key(upper, extra | CONTROL), ctrl);
            assert_eq!(key(lower, extra | CONTROL | SHIFT).code(), ctrl.code());
        }
    }

    #[test_case(KeyCode::Tab, SHIFT ; "shift_tab_on_a_kitty_terminal")]
    #[test_case(KeyCode::BackTab, NONE ; "shift_tab_everywhere_else")]
    #[test_case(KeyCode::BackTab, SHIFT ; "back_tab_already_carrying_shift")]
    #[test_case(KeyCode::Char('N'), CONTROL ; "ctrl_upper_letter")]
    #[test_case(KeyCode::Char('n'), CONTROL.union(SHIFT) ; "ctrl_shift_letter")]
    #[test_case(KeyCode::Char('A'), SHIFT ; "shift_upper_letter")]
    #[test_case(KeyCode::Char('1'), SHIFT ; "shift_digit")]
    #[test_case(KeyCode::Char(' '), CONTROL ; "ctrl_space")]
    #[test_case(KeyCode::F(24), ALT ; "alt_f24")]
    #[test_case(KeyCode::Enter, NONE ; "plain_enter")]
    fn normalization_is_idempotent_and_notation_round_trips(code: KeyCode, mods: KeyModifiers) {
        let (once, once_mods) = normalize(code, mods);
        assert_eq!(
            normalize(once, once_mods),
            (once, once_mods),
            "applying it twice changes nothing"
        );

        let key = Key::from_event(event(code, mods)).unwrap();
        assert_eq!(Key::parse(&key.notation()), Ok(key), "{}", key.notation());
    }

    #[test_case(KeyCode::Tab, SHIFT, "<S-Tab>" ; "shift_tab_from_kitty")]
    #[test_case(KeyCode::BackTab, NONE, "<S-Tab>" ; "shift_tab_from_csi_z")]
    #[test_case(KeyCode::Char('N'), CONTROL, "<C-n>" ; "ctrl_letter_is_lowercased")]
    #[test_case(KeyCode::Char(' '), CONTROL, "<C-Space>" ; "ctrl_space")]
    #[test_case(KeyCode::Char('A'), SHIFT, "A" ; "shift_is_already_in_the_letter")]
    #[test_case(KeyCode::Char('1'), SHIFT, "<S-1>" ; "shift_digit_keeps_its_bit")]
    #[test_case(KeyCode::Char('x'), ALT, "<M-x>" ; "alt_is_printed_as_m")]
    #[test_case(KeyCode::Home, ALT, "<M-Home>" ; "alt_home")]
    #[test_case(KeyCode::F(13), NONE, "<F13>" ; "f13")]
    #[test_case(KeyCode::Char('a'), NONE, "a" ; "plain_char")]
    #[test_case(KeyCode::Char('n'), CONTROL.union(SHIFT).union(ALT), "<C-M-S-n>" ; "modifier_order")]
    fn notation_cases(code: KeyCode, mods: KeyModifiers, expected: &str) {
        let key = Key::from_event(event(code, mods)).expect("nameable");
        assert_eq!(key.notation(), expected);
    }

    #[test_case("<Shift-Tab>", KeyCode::BackTab, SHIFT ; "long_shift_tab")]
    #[test_case("<C-S-a>", KeyCode::Char('a'), CONTROL.union(SHIFT) ; "ctrl_shift")]
    #[test_case("<C-T>", KeyCode::Char('t'), CONTROL ; "ctrl_upper_is_one_key_with_ctrl_lower")]
    #[test_case("<S-a>", KeyCode::Char('A'), NONE ; "shift_letter_is_the_uppercase_letter")]
    #[test_case("<M-S-a>", KeyCode::Char('A'), ALT ; "alt_shift_letter")]
    #[test_case("<", KeyCode::Char('<'), NONE ; "bare_angle_bracket")]
    fn parse_cases(input: &str, code: KeyCode, mods: KeyModifiers) {
        let key = Key::parse(input).unwrap();
        assert_eq!(key.code(), code);
        assert_eq!(key.modifiers(), mods);
    }

    #[test_case("" ; "empty")]
    #[test_case("<>" ; "empty_brackets")]
    #[test_case("<F0>" ; "function_key_zero")]
    #[test_case("<F25>" ; "function_key_past_the_end")]
    #[test_case("<lt>" ; "vim_escape_we_do_not_need")]
    #[test_case("abc" ; "a_word")]
    fn parse_refuses(input: &str) {
        assert!(Key::parse(input).is_err(), "{input} must not parse");
    }

    #[test_case(KeyCode::Media(MediaKeyCode::Play), NONE ; "media")]
    #[test_case(KeyCode::Modifier(ModifierKeyCode::LeftShift), NONE ; "bare_modifier")]
    #[test_case(KeyCode::CapsLock, NONE ; "caps_lock")]
    #[test_case(KeyCode::Null, NONE ; "null")]
    #[test_case(KeyCode::Char('a'), KeyModifiers::SUPER ; "modifier_notation_cannot_spell")]
    fn from_event_refuses_what_notation_cannot_name(code: KeyCode, mods: KeyModifiers) {
        assert_eq!(Key::from_event(event(code, mods)), None);
    }

    /// A reserved key no terminal can deliver would quietly reserve nothing.
    #[test]
    fn reserved_keys_are_normalized_and_nameable() {
        for key in RESERVED_KEYS {
            assert_eq!(Key::parse(&key.notation()), Ok(key));
            assert!(is_reserved(event(key.code(), key.modifiers())));
        }
    }
}
