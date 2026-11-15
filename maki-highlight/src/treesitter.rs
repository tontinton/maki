//! Highlighting for tree-sitter grammars a user loads at runtime. Languages
//! without one stay on syntect.

use std::borrow::Cow;
use std::collections::HashMap;
use std::iter;
use std::sync::{Arc, LazyLock, Mutex};

use arc_swap::ArcSwap;
use regex::Regex;
use syntect::highlighting::{Highlighter as SynHighlighter, Style as SynStyle};
use syntect::parsing::Scope;
use syntect::util::LinesWithEndings;
use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use crate::predicates::predicates_hold;
use crate::scopes::helix_to_textmate_scope;
use crate::{StyledSegment, invalidate_highlights, normalize_text, theme};

const UNSTYLED_CAPTURES: &[&str] = &["spell", "nospell", "conceal"];
const PLAIN_CAPTURE: &str = "none";
const PRIVATE_CAPTURE_PREFIX: char = '_';
const SCOPE_SEPARATOR: char = '.';
const UNPAINTED: u16 = 0;

/// nvim-treesitter capture names that Helix themes spell differently. Matched
/// on whole segments, so `number.float` becomes `constant.numeric.float`.
const NVIM_TO_HELIX: &[(&str, &str)] = &[
    ("number", "constant.numeric"),
    ("boolean", "constant.builtin.boolean"),
    ("character", "constant.character"),
    ("string.escape", "constant.character.escape"),
    ("keyword.conditional", "keyword.control.conditional"),
    ("keyword.repeat", "keyword.control.repeat"),
    ("keyword.import", "keyword.control.import"),
    ("keyword.exception", "keyword.control.exception"),
    ("keyword.modifier", "keyword.storage.modifier"),
    ("keyword.type", "keyword.storage.type"),
    ("module", "namespace"),
    ("property", "variable.other.member"),
    ("variable.member", "variable.other.member"),
];

pub struct Grammar {
    owner: String,
    language: Language,
    highlights: Arc<Query>,
    regex_cache: Mutex<HashMap<String, Option<Regex>>>,
}

#[derive(Clone)]
struct Alias {
    owner: String,
    lang: String,
}

/// The last plugin to register a language or filetype wins it.
#[derive(Clone, Default)]
struct Registry {
    grammars: HashMap<String, Arc<Grammar>>,
    aliases: HashMap<String, Alias>,
}

static REGISTRY: LazyLock<ArcSwap<Registry>> = LazyLock::new(ArcSwap::default);

pub fn register(owner: &str, lang: &str, language: Language, highlights: Arc<Query>) {
    let grammar = Arc::new(Grammar {
        owner: owner.to_owned(),
        language,
        highlights,
        regex_cache: Mutex::default(),
    });
    update(|registry| {
        registry
            .grammars
            .insert(lang.to_owned(), Arc::clone(&grammar));
    });
}

pub fn alias(owner: &str, filetype: &str, lang: &str) {
    let alias = Alias {
        owner: owner.to_owned(),
        lang: lang.to_owned(),
    };
    update(|registry| {
        registry.aliases.insert(filetype.to_owned(), alias.clone());
    });
}

pub fn forget(owner: &str) {
    let registry = REGISTRY.load();
    if !registry.grammars.values().any(|g| g.owner == owner)
        && !registry.aliases.values().any(|a| a.owner == owner)
    {
        return;
    }
    update(|registry| {
        registry.grammars.retain(|_, g| g.owner != owner);
        registry.aliases.retain(|_, a| a.owner != owner);
    });
}

fn update(change: impl Fn(&mut Registry)) {
    REGISTRY.rcu(|registry| {
        let mut registry = Registry::clone(registry);
        change(&mut registry);
        registry
    });
    invalidate_highlights();
}

pub(crate) fn grammar_for_token(token: &str) -> Option<Arc<Grammar>> {
    let registry = REGISTRY.load();
    if registry.grammars.is_empty() {
        return None;
    }
    let lang = registry
        .aliases
        .get(token)
        .map_or(token, |alias| alias.lang.as_str());
    registry.grammars.get(lang).cloned()
}

impl Grammar {
    pub(crate) fn highlight(&self, code: &str) -> Vec<Vec<StyledSegment>> {
        let theme = theme();
        let syn_hl = SynHighlighter::new(&theme);
        let base = syn_hl.style_for_stack(&[]);
        let palette: Vec<Option<SynStyle>> = iter::once(Some(base))
            .chain(
                self.highlights
                    .capture_names()
                    .iter()
                    .map(|name| capture_style(&syn_hl, name)),
            )
            .collect();

        let mut painted = vec![UNPAINTED; code.len()];
        let mut parser = Parser::new();
        if parser.set_language(&self.language).is_ok()
            && let Some(tree) = parser.parse(code, None)
        {
            let mut regex_cache = self.regex_cache.lock().unwrap_or_else(|e| e.into_inner());
            let mut cursor = QueryCursor::new();
            let mut captures = cursor.captures(&self.highlights, tree.root_node(), code.as_bytes());
            while let Some((matched, index)) = captures.next() {
                let capture = matched.captures()[*index];
                let slot = capture.index as usize + 1;
                if palette[slot].is_some()
                    && predicates_hold(
                        &self.highlights,
                        matched.pattern_index,
                        matched.captures(),
                        code.as_bytes(),
                        &mut regex_cache,
                    )
                {
                    painted[capture.node.byte_range()].fill(slot as u16);
                }
            }
        }

        let mut line_start = 0;
        LinesWithEndings::from(code)
            .map(|line| {
                let painted = &painted[line_start..];
                line_start += line.len();
                split_line(line.trim_end_matches('\n'), painted, &palette, base)
            })
            .collect()
    }

    /// `prefix` only gives `code` its context, its lines are not returned.
    pub(crate) fn highlight_after(&self, prefix: &str, code: &str) -> Vec<Vec<StyledSegment>> {
        if prefix.is_empty() {
            return self.highlight(code);
        }
        let separator = if prefix.ends_with('\n') { "" } else { "\n" };
        let mut lines = self.highlight(&format!("{prefix}{separator}{code}"));
        lines.split_off(LinesWithEndings::from(prefix).count().min(lines.len()))
    }
}

fn split_line(
    line: &str,
    painted: &[u16],
    palette: &[Option<SynStyle>],
    base: SynStyle,
) -> Vec<StyledSegment> {
    let segment = |start: usize, end: usize| {
        let style = palette[painted[start] as usize].unwrap_or(base);
        StyledSegment::from_syntect(style, normalize_text(&line[start..end]))
    };
    if line.is_empty() {
        return vec![StyledSegment::from_syntect(base, String::new())];
    }
    let mut segments = Vec::new();
    let mut start = 0;
    for (i, _) in line.char_indices().skip(1) {
        if painted[i] != painted[start] {
            segments.push(segment(start, i));
            start = i;
        }
    }
    segments.push(segment(start, line.len()));
    segments
}

/// Every parent of the capture joins the stack, so `type.definition` falls
/// back to the color of `type` the way Helix resolves it.
fn capture_style(syn_hl: &SynHighlighter, capture: &str) -> Option<SynStyle> {
    if capture.starts_with(PRIVATE_CAPTURE_PREFIX) || UNSTYLED_CAPTURES.contains(&capture) {
        return None;
    }
    if capture == PLAIN_CAPTURE {
        return Some(syn_hl.style_for_stack(&[]));
    }
    let name = helix_name(capture);
    let stack: Vec<Scope> = name
        .match_indices(SCOPE_SEPARATOR)
        .map(|(end, _)| &name[..end])
        .chain(iter::once(name.as_ref()))
        .filter_map(|helix| {
            let textmate = helix_to_textmate_scope(helix).split([',', ' ']).next()?;
            Scope::new(textmate).ok()
        })
        .collect();
    Some(syn_hl.style_for_stack(&stack))
}

fn helix_name(capture: &str) -> Cow<'_, str> {
    NVIM_TO_HELIX
        .iter()
        .find_map(|(nvim, helix)| {
            let rest = capture.strip_prefix(nvim)?;
            (rest.is_empty() || rest.starts_with(SCOPE_SEPARATOR))
                .then(|| Cow::Owned(format!("{helix}{rest}")))
        })
        .unwrap_or(Cow::Borrowed(capture))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use syntect::highlighting::{
        Color as SynColor, ScopeSelectors, StyleModifier, Theme, ThemeItem,
    };
    use test_case::test_case;

    use super::*;
    use crate::tests::exclusive_globals;
    use crate::{SegmentColor, set_theme, theme_generation};

    const OWNER: &str = "bash_plugin";
    const LANG: &str = "bash_ts";
    const FILETYPE: &str = "bash_ts_alias";
    const FORGOTTEN_OWNER: &str = "forgotten_plugin";
    const FORGOTTEN_LANG: &str = "bash_ts_forgotten";
    const FORGOTTEN_FILETYPE: &str = "bash_ts_forgotten_alias";
    const CONTESTED_WINNER: &str = "later_plugin";
    const CONTESTED_LANG: &str = "bash_ts_contested";
    const CONTESTED_FILETYPE: &str = "bash_ts_contested_alias";
    const QUERY: &str = "(comment) @comment (command_name) @function.call";
    const PREDICATE_LANG: &str = "bash_ts_predicate";
    const PREDICATE_QUERY: &str =
        r#"((command_name) @function.call (#lua-match? @function.call "^l"))"#;
    const COMMENT: SynColor = SynColor {
        r: 1,
        g: 2,
        b: 3,
        a: 0xFF,
    };
    const FUNCTION: SynColor = SynColor {
        r: 4,
        g: 5,
        b: 6,
        a: 0xFF,
    };

    fn theme_of(items: &[(&str, SynColor)]) -> Theme {
        Theme {
            scopes: items
                .iter()
                .map(|(selector, color)| ThemeItem {
                    scope: ScopeSelectors::from_str(selector).unwrap(),
                    style: StyleModifier {
                        foreground: Some(*color),
                        ..StyleModifier::default()
                    },
                })
                .collect(),
            ..Theme::default()
        }
    }

    fn register_bash(lang: &str, query: &str) {
        let language = Language::new(tree_sitter_bash::LANGUAGE);
        let query = Query::new(&language, query).unwrap();
        register(OWNER, lang, language, Arc::new(query));
    }

    fn fg_of(lines: &[Vec<StyledSegment>], text: &str) -> Option<SegmentColor> {
        lines
            .iter()
            .flatten()
            .find(|s| s.text == text)
            .map(|s| s.fg)
    }

    #[test]
    fn a_registered_grammar_colors_its_captures_line_by_line() {
        let _guard = exclusive_globals();
        set_theme(theme_of(&[
            ("comment", COMMENT),
            ("entity.name.function", FUNCTION),
        ]));
        register_bash(LANG, QUERY);
        alias(OWNER, FILETYPE, LANG);

        let lines = crate::highlight_code(FILETYPE, "echo hi # note\n\n\tls", "");

        let texts: Vec<Vec<&str>> = lines
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(
            texts,
            [
                vec!["echo", " hi ", "# note"],
                vec![""],
                vec![crate::TAB_SPACES, "ls"]
            ]
        );
        assert_eq!(
            fg_of(&lines, "# note"),
            Some(SegmentColor::from_syntect(COMMENT))
        );
        assert_eq!(
            fg_of(&lines, "ls"),
            Some(SegmentColor::from_syntect(FUNCTION))
        );
    }

    #[test]
    fn a_capture_whose_predicate_fails_paints_nothing() {
        let _guard = exclusive_globals();
        set_theme(theme_of(&[("entity.name.function", FUNCTION)]));
        register_bash(PREDICATE_LANG, PREDICATE_QUERY);

        let lines = crate::highlight_code(PREDICATE_LANG, "echo\nls", "");

        let painted = Some(SegmentColor::from_syntect(FUNCTION));
        assert_ne!(fg_of(&lines, "echo"), painted);
        assert_eq!(fg_of(&lines, "ls"), painted);
    }

    #[test]
    fn the_prefix_gives_context_but_is_not_returned() {
        let _guard = exclusive_globals();
        register_bash(LANG, QUERY);
        let lines = crate::highlight_code(LANG, "ls", "echo one\necho two");
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn forgetting_the_owner_falls_back_to_syntect() {
        let _guard = exclusive_globals();
        let language = Language::new(tree_sitter_bash::LANGUAGE);
        let query = Query::new(&language, QUERY).unwrap();
        register(FORGOTTEN_OWNER, FORGOTTEN_LANG, language, Arc::new(query));
        alias(FORGOTTEN_OWNER, FORGOTTEN_FILETYPE, FORGOTTEN_LANG);
        assert!(grammar_for_token(FORGOTTEN_FILETYPE).is_some());

        let before = theme_generation();
        forget(FORGOTTEN_OWNER);

        assert!(grammar_for_token(FORGOTTEN_FILETYPE).is_none());
        assert!(grammar_for_token(FORGOTTEN_LANG).is_none());
        assert_ne!(theme_generation(), before);
    }

    #[test]
    fn the_last_plugin_to_register_a_language_wins_it() {
        let _guard = exclusive_globals();
        for owner in [OWNER, CONTESTED_WINNER] {
            let language = Language::new(tree_sitter_bash::LANGUAGE);
            let query = Query::new(&language, QUERY).unwrap();
            register(owner, CONTESTED_LANG, language, Arc::new(query));
            alias(owner, CONTESTED_FILETYPE, CONTESTED_LANG);
        }
        let grammar = grammar_for_token(CONTESTED_FILETYPE).unwrap();
        assert_eq!(grammar.owner, CONTESTED_WINNER);
    }

    #[test]
    fn registering_a_grammar_invalidates_painted_code() {
        let _guard = exclusive_globals();
        let before = theme_generation();
        register_bash(LANG, QUERY);
        assert_ne!(theme_generation(), before);
    }

    #[test_case("type.definition", "entity.name.type"; "falls back to the parent")]
    #[test_case("number.float", "constant.numeric"; "nvim name in helix spelling")]
    #[test_case("constant.numeric", "constant.numeric"; "child beats mapped parent")]
    fn capture_resolves_to_theme_scope(capture: &str, selector: &str) {
        let theme = theme_of(&[(selector, COMMENT), ("constant", FUNCTION)]);
        let style = capture_style(&SynHighlighter::new(&theme), capture).unwrap();
        assert_eq!(style.foreground, COMMENT);
    }

    #[test_case("spell")]
    #[test_case("_cmd")]
    fn non_highlight_captures_paint_nothing(capture: &str) {
        let theme = Theme::default();
        assert_eq!(capture_style(&SynHighlighter::new(&theme), capture), None);
    }
}
