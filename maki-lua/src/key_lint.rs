//! Checks the key strings a plugin compares against at load time.
//!
//! `maki.keymap.set` and a window's `keys` parse every key and fail loudly.
//! But in `ev.key == "<Esc>"` the comparison happens inside Lua and the host
//! never sees the string, so a typo there just compares false forever. So we
//! read the source. Only string literals count, found through the Lua grammar,
//! so a key named in a comment is left alone. Two rules, each tuned to fire
//! only on a string that is clearly a key and clearly wrong:
//!
//! - **Misspelling**: a bracketed string that does not parse, but would one
//!   edit away. `"<Escc>"` is a finding. `"<path>"` is not, because nothing
//!   one edit from it is a key, and that is what keeps placeholders quiet.
//! - **Legacy spelling**: what `win:recv` used to deliver, like `"esc"` for
//!   `"<Esc>"`. No shim can help here, because no value equals both. This
//!   rule is temporary: delete [`legacy`], its call in [`check`], the tests
//!   naming [`LEGACY_HINT`] and the migration paragraph under `maki.keymap`.
//!
//! Findings are warnings and never refuse a load, since a false positive
//! should cost a line of output, not a plugin that will not start. User Lua is
//! linted as it enters the VM (`runtime::load_user_source`), modules included.
//! Bundled Lua is linted by a test instead, so users never pay for our typos.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use tree_sitter::Parser;

use crate::key::{Key, candidate_spellings};

/// Two edits already reaches real words, so one it is.
const MAX_EDIT_DISTANCE: usize = 1;

const STRING_CONTENT_KIND: &str = "string_content";
pub const KEY_WARNING: &str = "key spelling";
pub(crate) const LEGACY_HINT: &str = "is the old spelling of";
pub(crate) const MISSPELLED_HINT: &str = "is not a key; did you mean";
pub(crate) const MORE_IN_LOG: &str = "more in the log";

static CANDIDATES: LazyLock<Vec<(String, Key)>> = LazyLock::new(candidate_spellings);

/// Shared by the Lua thread that lints and the host that reports.
#[derive(Clone, Default)]
pub struct KeyLint(Arc<Mutex<KeyLintState>>);

#[derive(Default)]
struct KeyLintState {
    /// A module two plugins `require` is one file, and reporting it twice
    /// would look like two problems.
    linted: HashSet<String>,
    unreported: Vec<String>,
}

impl KeyLint {
    pub(crate) fn check(&self, chunk: &str, source: &str) {
        let first_read = self
            .0
            .lock()
            .is_ok_and(|mut state| state.linted.insert(chunk.to_owned()));
        if !first_read {
            return;
        }
        let findings = lint(chunk, source);
        for finding in &findings {
            tracing::warn!(chunk, finding = %finding, "{KEY_WARNING}");
        }
        if let Ok(mut state) = self.0.lock() {
            state.unreported.extend(findings);
        }
    }

    /// One status bar line for every finding since the last take: the first
    /// in full and a count of the rest, which are in the log.
    pub fn take_summary(&self) -> Option<String> {
        let findings = self
            .0
            .lock()
            .map(|mut state| std::mem::take(&mut state.unreported))
            .unwrap_or_default();
        let (first, rest) = findings.split_first()?;
        Some(match rest.len() {
            0 => format!("{KEY_WARNING}: {first}"),
            more => format!("{KEY_WARNING}: {first} (+{more} {MORE_IN_LOG})"),
        })
    }
}

/// Each wrong key spelling in {source} as `chunk:line: problem`, in source
/// order.
pub(crate) fn lint(chunk: &str, source: &str) -> Vec<String> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_lua::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut findings = Vec::new();
    let mut cursor = tree.walk();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == STRING_CONTENT_KIND {
            if let Ok(text) = node.utf8_text(source.as_bytes())
                && let Some(problem) = check(text)
            {
                findings.push((node.start_position().row + 1, problem));
            }
            continue;
        }
        stack.extend(node.children(&mut cursor));
    }
    findings.sort();
    findings
        .into_iter()
        .map(|(line, problem)| format!("{chunk}:{line}: {problem}"))
        .collect()
}

/// `None` when {text} is a key, or does not look meant to be one.
fn check(text: &str) -> Option<String> {
    if Key::parse(text).is_ok() {
        return None;
    }
    if let Some(canonical) = legacy::notation(text) {
        return Some(format!("{text:?} {LEGACY_HINT} {canonical:?}"));
    }
    let bracketed = text.starts_with('<') && text.ends_with('>') && text.len() > 2;
    let suggestion = bracketed.then(|| nearest(text)).flatten()?;
    Some(format!("{text:?} {MISSPELLED_HINT} {suggestion:?}?"))
}

/// The spellings `win:recv` delivered before keys were unified. Temporary,
/// see the module docs for how to remove it.
mod legacy {
    use crate::key::Key;

    const MODIFIERS: &[(&str, &str)] = &[("ctrl+", "C-"), ("alt+", "M-"), ("shift+", "S-")];

    /// Checked after a `ctrl+`/`alt+`/`shift+` prefix, which already proves
    /// the string is a key.
    const NAMES: &[(&str, &str)] = &[
        ("esc", "Esc"),
        ("enter", "CR"),
        ("tab", "Tab"),
        ("backspace", "BS"),
        ("delete", "Del"),
        ("space", "Space"),
        ("up", "Up"),
        ("down", "Down"),
        ("left", "Left"),
        ("right", "Right"),
        ("home", "Home"),
        ("end", "End"),
        ("pageup", "PageUp"),
        ("pagedown", "PageDown"),
        ("insert", "Insert"),
    ];

    /// The names worth flagging with no modifier in front. The rest of
    /// [`NAMES`] are everyday words, like `split = "left"`, and a lint that
    /// cries on those is one people learn to ignore.
    const BARE_NAMES: &[&str] = &["esc", "enter", "backspace", "pageup", "pagedown"];

    /// The notation {text} is the old spelling of. The answer goes through
    /// [`Key::parse`], so we only ever suggest a string the host accepts.
    ///
    /// Case-sensitive on purpose. The old spellings were always lowercase,
    /// while `"Ctrl+N"` and `"Enter"` are footer labels for humans, and every
    /// bundled plugin has a row of those.
    pub(super) fn notation(text: &str) -> Option<String> {
        let mut rest = text;
        let mut prefix = String::new();
        'strip: loop {
            for (legacy, canonical) in MODIFIERS {
                if let Some(tail) = rest.strip_prefix(legacy) {
                    prefix.push_str(canonical);
                    rest = tail;
                    continue 'strip;
                }
            }
            break;
        }
        if prefix.is_empty() && !BARE_NAMES.contains(&rest) {
            return None;
        }
        let name = NAMES
            .iter()
            .find(|(legacy, _)| *legacy == rest)
            .map(|(_, canonical)| (*canonical).to_owned())
            .or_else(|| (rest.chars().count() == 1).then(|| rest.to_owned()))?;
        Key::parse(&format!("<{prefix}{name}>"))
            .ok()
            .map(|key| key.notation())
    }
}

/// The key {text} was probably meant to be, if exactly one is close enough.
/// A tie between two keys gives `None`, because guessing between `<C-a>` and
/// `<C-b>` is worse than saying nothing. Two spellings of one key are not a
/// tie.
fn nearest(text: &str) -> Option<String> {
    let mut best: Option<Key> = None;
    for (spelling, key) in CANDIDATES.iter() {
        if edit_distance(text, spelling) > MAX_EDIT_DISTANCE {
            continue;
        }
        match best {
            Some(found) if found != *key => return None,
            _ => best = Some(*key),
        }
    }
    best.map(|key| key.notation())
}

/// Levenshtein distance. The length check bails out early on almost every
/// candidate.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > MAX_EDIT_DISTANCE {
        return MAX_EDIT_DISTANCE + 1;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::{KEY_WARNING, KeyLint, LEGACY_HINT, MISSPELLED_HINT, MORE_IN_LOG, check, lint};
    use test_case::test_case;

    const CHUNK: &str = "init.lua";

    /// Real keys, plus the strings that would make a lint too noisy to read.
    #[test_case("<CR>" ; "canonical")]
    #[test_case("<Enter>" ; "alias")]
    #[test_case("<esc>" ; "lowercased_name")]
    #[test_case("a" ; "plain_char")]
    #[test_case("<C-n>" ; "ctrl_letter")]
    #[test_case("<path>" ; "placeholder")]
    #[test_case("<div>" ; "markup")]
    #[test_case("left" ; "split_direction")]
    #[test_case("end" ; "keyword_like")]
    #[test_case("insert" ; "verb")]
    #[test_case("north" ; "unrelated")]
    #[test_case("" ; "empty")]
    #[test_case("Enter" ; "footer_label")]
    #[test_case("Ctrl+N" ; "footer_label_with_modifier")]
    #[test_case("Esc" ; "capitalized_label")]
    #[test_case("<C->" ; "ambiguous_near_miss")]
    fn not_a_finding(text: &str) {
        assert_eq!(check(text), None);
    }

    // The legacy cases go with `super::legacy`.
    #[test_case("ctrl+n", LEGACY_HINT, "<C-n>" ; "legacy_ctrl_letter")]
    #[test_case("alt+x", LEGACY_HINT, "<M-x>" ; "legacy_alt_letter")]
    #[test_case("shift+tab", LEGACY_HINT, "<S-Tab>" ; "legacy_shift_tab")]
    #[test_case("esc", LEGACY_HINT, "<Esc>" ; "legacy_bare_esc")]
    #[test_case("enter", LEGACY_HINT, "<CR>" ; "legacy_bare_enter")]
    #[test_case("pagedown", LEGACY_HINT, "<PageDown>" ; "legacy_bare_pagedown")]
    #[test_case("<Escc>", MISSPELLED_HINT, "<Esc>" ; "extra_letter")]
    #[test_case("<C-nn>", MISSPELLED_HINT, "<C-n>" ; "doubled_modifier_target")]
    #[test_case("<Tabb>", MISSPELLED_HINT, "<Tab>" ; "trailing_letter")]
    #[test_case("<S-Aa>", MISSPELLED_HINT, "A" ; "suggests_notation_not_the_matched_spelling")]
    fn a_finding_suggests_the_canonical_key(text: &str, hint: &str, suggestion: &str) {
        let finding = check(text).expect("a finding");
        assert!(
            finding.contains(hint) && finding.contains(&format!("{suggestion:?}")),
            "{finding}"
        );
    }

    #[test]
    fn a_key_named_in_a_comment_is_not_a_finding() {
        let source = "-- press esc to close, or ctrl+n\nlocal x = 1\n";

        assert!(lint(CHUNK, source).is_empty());
    }

    #[test]
    fn findings_name_the_chunk_and_line_in_source_order() {
        let source = format!("local a = \"esc\"\n{}local b = \"enter\"\n", "\n".repeat(8));

        let findings = lint(CHUNK, &source);

        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].starts_with("init.lua:1: "), "{findings:?}");
        assert!(findings[1].starts_with("init.lua:10: "), "{findings:?}");
    }

    /// The screen gets one line however many there are, and the rest are in
    /// the log, so a file full of old spellings cannot hold the bar hostage.
    #[test]
    fn many_findings_are_one_summary_line() {
        let key_lint = KeyLint::default();
        key_lint.check(CHUNK, "local a, b, c = \"esc\", \"enter\", \"ctrl+n\"\n");

        let summary = key_lint.take_summary().expect("findings summarized");

        assert!(summary.starts_with(KEY_WARNING), "{summary}");
        assert!(summary.contains(&format!("+2 {MORE_IN_LOG}")), "{summary}");
        assert_eq!(key_lint.take_summary(), None, "taken, not read");
    }

    /// A module two plugins `require` is one file with one set of problems.
    #[test]
    fn a_chunk_is_read_once() {
        let key_lint = KeyLint::default();
        let source = "local a = \"esc\"\n";
        key_lint.check(CHUNK, source);
        key_lint.check(CHUNK, source);

        let summary = key_lint.take_summary().expect("finding summarized");

        assert!(!summary.contains(MORE_IN_LOG), "{summary}");
    }
}
