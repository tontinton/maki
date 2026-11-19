//! Checks the key strings a plugin compares against at load time.
//!
//! `maki.keymap.set` and a window's `keys` parse every key and fail loudly.
//! But in `ev.key == "<Esc>"` the comparison happens inside Lua and the host
//! never sees the string, so a typo there just compares false forever. So we
//! read the source. Only string literals count, found through the Lua grammar,
//! so a key named in a comment is left alone. A finding is a bracketed string
//! that does not parse, but would one edit away. `"<Escc>"` is a finding.
//! `"<path>"` is not, because nothing one edit from it is a key, and that is
//! what keeps placeholders quiet.
//!
//! Findings are warnings and never refuse a load, since a false positive
//! should cost a line of output, not a plugin that will not start. User Lua is
//! linted as it enters the VM (`runtime::load_user_source`), modules included.
//! Bundled Lua is linted by a test instead, so users never pay for our typos.
//!
//! The same status bar line carries one check that reads the keymap instead
//! of the source: a plugin whose global bindings are in normal mode only, see
//! `KeymapStore::normal_only_key`.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use tree_sitter::Parser;

use crate::key::{Key, candidate_spellings};

/// Two edits already reaches real words, so one it is.
const MAX_EDIT_DISTANCE: usize = 1;

const STRING_CONTENT_KIND: &str = "string_content";
pub const KEY_WARNING: &str = "key spelling";
pub(crate) const MODE_WARNING: &str = "keymap mode";
const NORMAL_ONLY_HINT: &str = "in \"n\" only, so it does not fire while you type";
pub(crate) const UPGRADE_DOCS: &str = "https://maki.sh/docs/keybindings/#upgrading-old-keymaps";
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
    /// Plugins already told about a normal mode only key, so a reload does
    /// not repeat it.
    mode_warned: HashSet<String>,
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
            let findings = findings
                .iter()
                .map(|finding| format!("{KEY_WARNING}: {finding}"));
            state.unreported.extend(findings);
        }
    }

    /// Once per plugin, for the key `KeymapStore::normal_only_key` found.
    pub(crate) fn check_normal_only(&self, plugin: &str, key: Key) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        if !state.mode_warned.insert(plugin.to_owned()) {
            return;
        }
        let key = key.notation();
        tracing::warn!(plugin, key = %key, "{MODE_WARNING}");
        state.unreported.push(format!(
            "{MODE_WARNING}: {plugin} binds {key} {NORMAL_ONLY_HINT} ({UPGRADE_DOCS})"
        ));
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
            0 => first.clone(),
            more => format!("{first} (+{more} {MORE_IN_LOG})"),
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
    let bracketed = text.starts_with('<') && text.ends_with('>') && text.len() > 2;
    let suggestion = bracketed.then(|| nearest(text)).flatten()?;
    Some(format!("{text:?} {MISSPELLED_HINT} {suggestion:?}?"))
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
    use super::{KEY_WARNING, KeyLint, MISSPELLED_HINT, MORE_IN_LOG, check, lint};
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

    #[test_case("<Escc>", "<Esc>" ; "extra_letter")]
    #[test_case("<C-nn>", "<C-n>" ; "doubled_modifier_target")]
    #[test_case("<Tabb>", "<Tab>" ; "trailing_letter")]
    #[test_case("<S-Aa>", "A" ; "suggests_notation_not_the_matched_spelling")]
    fn a_finding_suggests_the_canonical_key(text: &str, suggestion: &str) {
        let finding = check(text).expect("a finding");
        assert!(
            finding.contains(MISSPELLED_HINT) && finding.contains(&format!("{suggestion:?}")),
            "{finding}"
        );
    }

    #[test]
    fn a_key_named_in_a_comment_is_not_a_finding() {
        let source = "-- press <Escc> to close, or <C-nn>\nlocal x = 1\n";

        assert!(lint(CHUNK, source).is_empty());
    }

    #[test]
    fn findings_name_the_chunk_and_line_in_source_order() {
        let source = format!("local a = \"<Escc>\"\n{}local b = \"<Tabb>\"\n", "\n".repeat(8));

        let findings = lint(CHUNK, &source);

        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].starts_with("init.lua:1: "), "{findings:?}");
        assert!(findings[1].starts_with("init.lua:10: "), "{findings:?}");
    }

    /// The screen gets one line however many there are, and the rest are in
    /// the log, so a file full of typos cannot hold the bar hostage.
    #[test]
    fn many_findings_are_one_summary_line() {
        let key_lint = KeyLint::default();
        key_lint.check(CHUNK, "local a, b, c = \"<Escc>\", \"<Tabb>\", \"<C-nn>\"\n");

        let summary = key_lint.take_summary().expect("findings summarized");

        assert!(summary.starts_with(KEY_WARNING), "{summary}");
        assert!(summary.contains(&format!("+2 {MORE_IN_LOG}")), "{summary}");
        assert_eq!(key_lint.take_summary(), None, "taken, not read");
    }

    /// A module two plugins `require` is one file with one set of problems.
    #[test]
    fn a_chunk_is_read_once() {
        let key_lint = KeyLint::default();
        let source = "local a = \"<Escc>\"\n";
        key_lint.check(CHUNK, source);
        key_lint.check(CHUNK, source);

        let summary = key_lint.take_summary().expect("finding summarized");

        assert!(!summary.contains(MORE_IN_LOG), "{summary}");
    }
}
