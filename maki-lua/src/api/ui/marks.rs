//! The UI draws these marks, but they are built and checked here, on the Lua
//! side. That way a plugin hears about a bad mark right when it sets it, and
//! not later during a draw, where there is nobody to tell.

use std::iter;
use std::ops::Range;
use std::sync::Arc;

use arc_swap::ArcSwap;
use maki_agent::{SnapshotSpan, SpanStyle};
use mlua::{Result as LuaResult, Table, Value};
use regex::{Captures, Regex};

use super::buf::{parse_line, parse_style};
use super::syntax::SyntaxRule;
use crate::api::util::convert::{TableExt, reject_unknown_fields, required};

/// A pushed mark says where it goes, while a rule's mark goes on each match.
const PUSHED_MARK_FIELDS: &[&str] = &[
    "start",
    "stop",
    "hl",
    "conceal",
    "reveal",
    "atomic",
    "virt_text",
    "virt_text_pos",
    "lang",
];
/// `start` and `stop`, which only a pushed mark has.
const PLACEMENT_FIELDS: usize = 2;
const MARK_FIELDS: &[&str] = PUSHED_MARK_FIELDS.split_at(PLACEMENT_FIELDS).1;
const REVEAL_CURSOR: &str = "cursor";
const POS_EOL: &str = "eol";
const POS_INLINE: &str = "inline";
const GROUP_SIGIL: char = '$';
const MARK: &str = "mark";
pub(crate) const CONTROL_CHAR_ERR: &str = "holds a control character";
const BAD_PATTERN_ERR: &str = "bad pattern";
const MISSING_GROUP_ERR: &str = "names a group the pattern does not have";
const FIXED_FIELD_ERR: &str = "`$N` is only filled in inside conceal and virt_text";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VirtTextPos {
    /// After the end of the line, like Neovim's default.
    #[default]
    Eol,
    /// Right before the character at the mark's start.
    Inline,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Reveal {
    #[default]
    Never,
    /// While the cursor is inside the range or on either edge of it. Edges
    /// count, because there the caret would sit right next to hidden text and
    /// the user could not tell which side of it they type on.
    Cursor,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decoration {
    pub hl: Option<SpanStyle>,
    /// Drawn in place of the range, and `""` hides it. A range over line
    /// breaks joins the lines it spans into one. An edit inside the range
    /// drops the mark, because what the text stood for is gone.
    pub conceal: Option<String>,
    pub reveal: Reveal,
    /// The cursor steps over the range and a delete that cuts into it takes
    /// all of it, so an edit only lands inside from a plugin, and that drops
    /// the mark.
    pub atomic: bool,
    pub virt_text: Vec<SnapshotSpan>,
    pub virt_text_pos: VirtTextPos,
    pub lang: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Mark {
    pub start: usize,
    /// `None` makes a point mark.
    pub stop: Option<usize>,
    /// Shared, so a rule stamping one look over many ranges stores it once.
    pub decoration: Arc<Decoration>,
}

impl Mark {
    pub fn end(&self) -> usize {
        self.stop.unwrap_or(self.start)
    }
}

impl Decoration {
    pub fn is_inline_virt(&self) -> bool {
        self.virt_text_pos == VirtTextPos::Inline && !self.virt_text.is_empty()
    }

    /// Conceals and inline virtual text change how many rows a line takes.
    pub fn shapes_rows(&self) -> bool {
        self.conceal.is_some() || self.is_inline_virt()
    }

    /// The layout counts every char of drawn text as a column, and a newline
    /// or an escape would take none or break the row.
    pub fn check_drawn_text(&self) -> Result<(), String> {
        match self
            .drawn_text()
            .find(|(_, text)| text.contains(char::is_control))
        {
            Some((field, _)) => Err(format!("{field} {CONTROL_CHAR_ERR}")),
            None => Ok(()),
        }
    }

    fn drawn_text(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.conceal
            .iter()
            .map(|conceal| ("conceal", conceal.as_str()))
            .chain(
                self.virt_text
                    .iter()
                    .map(|span| ("virt_text", span.text.as_str())),
            )
    }
}

/// `$$` is a plain `$`, and a `$` before anything but a digit stays as it is,
/// so a template like `$skill` that names no group reads as written.
enum Piece<'a> {
    Text(&'a str),
    Group(usize),
}

fn pieces(template: &str) -> impl Iterator<Item = Piece<'_>> {
    let mut rest = template;
    iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let Some(after) = rest.strip_prefix(GROUP_SIGIL) else {
            let end = rest.find(GROUP_SIGIL).unwrap_or(rest.len());
            let (text, tail) = rest.split_at(end);
            rest = tail;
            return Some(Piece::Text(text));
        };
        let digits = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        if digits > 0 {
            rest = &after[digits..];
            return Some(Piece::Group(after[..digits].parse().unwrap_or(usize::MAX)));
        }
        let sigil = &rest[..GROUP_SIGIL.len_utf8()];
        rest = after.strip_prefix(GROUP_SIGIL).unwrap_or(after);
        Some(Piece::Text(sigil))
    })
}

#[derive(Clone)]
pub enum InputRule {
    Pattern(PatternRule),
    Syntax(Arc<SyntaxRule>),
}

#[derive(Clone)]
pub struct PatternRule {
    regex: Regex,
    mark: Arc<Decoration>,
    /// Without a `$` in any template every match shares {mark}, so a plain
    /// highlight rule clones nothing per match.
    substitutes: bool,
}

impl PatternRule {
    /// Everything that could go wrong is refused here, since a rule only runs
    /// later inside a draw, where an error has nowhere to go.
    pub fn new(pattern: &str, mark: Decoration) -> Result<Self, String> {
        let regex = Regex::new(pattern).map_err(|e| format!("{BAD_PATTERN_ERR}: {e}"))?;
        mark.check_drawn_text()?;
        let groups = regex.captures_len() - 1;
        for (field, template) in mark.drawn_text() {
            if let Some(n) = pieces(template).find_map(|piece| match piece {
                Piece::Group(n) if n > groups => Some(n),
                _ => None,
            }) {
                return Err(format!(
                    "`${n}` in {field} {MISSING_GROUP_ERR} (it has {groups})"
                ));
            }
        }
        let style_names = iter::once(&mark.hl)
            .flatten()
            .chain(mark.virt_text.iter().map(|span| &span.style))
            .filter_map(|style| match style {
                SpanStyle::Named(name) => Some(("hl", name.as_str())),
                _ => None,
            });
        let fixed = mark.lang.iter().map(|lang| ("lang", lang.as_str()));
        if let Some((field, _)) = fixed
            .chain(style_names)
            .find(|(_, text)| pieces(text).any(|piece| matches!(piece, Piece::Group(_))))
        {
            return Err(format!("{FIXED_FIELD_ERR}, not {field}"));
        }
        let substitutes = mark
            .drawn_text()
            .any(|(_, text)| text.contains(GROUP_SIGIL));
        Ok(Self {
            regex,
            mark: Arc::new(mark),
            substitutes,
        })
    }

    pub fn mark(&self) -> &Decoration {
        &self.mark
    }

    /// A match never runs past the end of {line}, but the pattern still sees
    /// the text before it. So `\A` is the start of the whole input and only
    /// the first line can match it, while `$` is the end of the line.
    pub fn matches<'a>(
        &'a self,
        text: &'a str,
        line: Range<usize>,
    ) -> impl Iterator<Item = (Range<usize>, Arc<Decoration>)> + 'a {
        let haystack = &text[..line.end];
        let mut at = Some(line.start);
        let mut last_end = None;
        iter::from_fn(move || {
            loop {
                let groups = self.regex.captures_at(haystack, at?)?;
                let range = groups.get(0)?.range();
                at = if range.is_empty() {
                    haystack[range.end..]
                        .chars()
                        .next()
                        .map(|c| range.end + c.len_utf8())
                } else {
                    Some(range.end)
                };
                // Like the regex crate's own iterators, an empty match right
                // where the last one ended is not a match of its own.
                if range.is_empty() && last_end == Some(range.start) {
                    continue;
                }
                last_end = Some(range.end);
                return Some((range, self.mark_for(&groups)));
            }
        })
    }

    fn mark_for(&self, groups: &Captures<'_>) -> Arc<Decoration> {
        if !self.substitutes {
            return Arc::clone(&self.mark);
        }
        let fill = |template: &str| -> String {
            pieces(template)
                .map(|piece| match piece {
                    Piece::Text(text) => text,
                    Piece::Group(n) => groups.get(n).map_or("", |found| found.as_str()),
                })
                .flat_map(str::chars)
                // A tab that came in through `$EDITOR` can sit in a group,
                // and drawn it would throw off every column after it.
                .filter(|c| !c.is_control())
                .collect()
        };
        let mut mark = (*self.mark).clone();
        mark.conceal = mark.conceal.as_deref().map(fill);
        for span in &mut mark.virt_text {
            span.text = fill(&span.text);
        }
        Arc::new(mark)
    }
}

pub(crate) fn parse_mark(tbl: &Table) -> LuaResult<Decoration> {
    reject_unknown_fields(tbl, MARK_FIELDS, MARK)?;
    parse_decoration(tbl)
}

/// Drawn text is checked here, while the offsets can only be checked by the
/// UI against the text they point into.
pub(crate) fn parse_pushed_mark(tbl: &Table) -> LuaResult<Mark> {
    reject_unknown_fields(tbl, PUSHED_MARK_FIELDS, MARK)?;
    let decoration = parse_decoration(tbl)?;
    decoration
        .check_drawn_text()
        .map_err(|msg| mlua::Error::runtime(format!("{MARK}: {msg}")))?;
    Ok(Mark {
        start: required(tbl, MARK, "start")?,
        stop: tbl.opt("stop")?,
        decoration: Arc::new(decoration),
    })
}

fn parse_decoration(tbl: &Table) -> LuaResult<Decoration> {
    let reveal = match tbl.opt::<String>("reveal")?.as_deref() {
        None => Reveal::Never,
        Some(REVEAL_CURSOR) => Reveal::Cursor,
        Some(other) => {
            return Err(mlua::Error::runtime(format!(
                "reveal must be \"{REVEAL_CURSOR}\", got {other:?}"
            )));
        }
    };
    let virt_text_pos = match tbl.opt::<String>("virt_text_pos")?.as_deref() {
        None | Some(POS_EOL) => VirtTextPos::Eol,
        Some(POS_INLINE) => VirtTextPos::Inline,
        Some(other) => {
            return Err(mlua::Error::runtime(format!(
                "virt_text_pos must be \"{POS_EOL}\" or \"{POS_INLINE}\", got {other:?}"
            )));
        }
    };
    let hl = match tbl.get::<Value>("hl")? {
        Value::Nil => None,
        style => Some(parse_style(&style)?),
    };
    let virt_text = match tbl.get::<Value>("virt_text")? {
        Value::Nil => Vec::new(),
        spans => parse_line(&spans)?.spans,
    };
    Ok(Decoration {
        hl,
        conceal: tbl.opt("conceal")?,
        reveal,
        atomic: tbl.opt::<bool>("atomic")?.unwrap_or_default(),
        virt_text,
        virt_text_pos,
        lang: tbl.opt("lang")?,
    })
}

/// Rules are kept in the order their namespaces were first set, because that
/// order decides which rule draws on top.
pub(crate) struct InputRuleStore {
    rules: Vec<(Arc<str>, String, InputRule)>,
    shared: Arc<ArcSwap<Vec<InputRule>>>,
}

impl InputRuleStore {
    pub fn new() -> (Self, InputRulesReader) {
        let shared = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let reader = InputRulesReader(Arc::clone(&shared));
        (
            Self {
                rules: Vec::new(),
                shared,
            },
            reader,
        )
    }

    /// `None` removes the rule. A replaced rule keeps its place, so it still
    /// draws over and under the same rules as before.
    pub fn set(&mut self, plugin: Arc<str>, ns: String, rule: Option<InputRule>) {
        let at = self
            .rules
            .iter()
            .position(|(owner, name, _)| *owner == plugin && *name == ns);
        match (at, rule) {
            (Some(at), Some(rule)) => self.rules[at].2 = rule,
            (Some(at), None) => {
                self.rules.remove(at);
            }
            (None, Some(rule)) => self.rules.push((plugin, ns, rule)),
            (None, None) => return,
        }
        self.publish();
    }

    pub fn clear_plugin(&mut self, plugin: &str) {
        let before = self.rules.len();
        self.rules.retain(|(owner, _, _)| &**owner != plugin);
        if self.rules.len() != before {
            self.publish();
        }
    }

    fn publish(&self) {
        let rules = self.rules.iter().map(|(_, _, rule)| rule.clone()).collect();
        self.shared.store(Arc::new(rules));
    }
}

/// The UI's end of [`InputRuleStore`]. A new `Arc` means the rules changed.
#[derive(Clone)]
pub struct InputRulesReader(Arc<ArcSwap<Vec<InputRule>>>);

impl InputRulesReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(Vec::new())))
    }

    pub fn load_full(&self) -> Arc<Vec<InputRule>> {
        self.0.load_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ui::syntax::{
        BAD_QUERY_ERR, BAD_REGEX_ERR, NO_CAPTURE_ERR, NO_CONTENT_LANG_ERR, NO_TEMPLATE_ERR,
        UNKNOWN_LANG_ERR, UNKNOWN_PREDICATE_ERR, UNKNOWN_SETTING_ERR,
    };
    use crate::api::ui::{RULE_HALVES_ERR, SYNTAX_ALONE_ERR, create_ui_table};
    use crate::api::util::convert::{REQUIRED_ERR, UNKNOWN_FIELD_ERR};
    use crate::plugin_permissions::{PluginPermissions, PluginReach};
    use crate::runtime::mark_loaded;
    use mlua::Lua;
    use test_case::test_case;

    const PLUGIN: &str = "chips";
    const CHIP: &str = r"\$skill:([\w-]+)";

    fn conceal(text: &str) -> Decoration {
        Decoration {
            conceal: Some(text.into()),
            ..Decoration::default()
        }
    }

    fn lua_with_rules() -> (Lua, InputRulesReader) {
        let (lua, reader) = lua_with_rules_unloaded();
        mark_loaded(&lua, PLUGIN);
        (lua, reader)
    }

    fn lua_with_rules_unloaded() -> (Lua, InputRulesReader) {
        let lua = Lua::new();
        let (store, reader) = InputRuleStore::new();
        lua.set_app_data(store);
        let ui = create_ui_table(
            &lua,
            None,
            Arc::from(PLUGIN),
            PluginReach::new(PluginPermissions::trusted()),
        )
        .unwrap();
        lua.globals().set("ui", ui).unwrap();
        (lua, reader)
    }

    #[test_case(r#"{ ns = "a", pattern = "(", mark = {} }"#, BAD_PATTERN_ERR ; "a_pattern_that_does_not_compile")]
    #[test_case(r#"{ ns = "a", pattern = "(a)", mark = { conceal = "$2" } }"#, MISSING_GROUP_ERR ; "a_group_the_pattern_lacks")]
    #[test_case(r#"{ ns = "a", pattern = "(a)", mark = { lang = "$1" } }"#, FIXED_FIELD_ERR ; "a_group_in_lang")]
    #[test_case(r#"{ ns = "a", pattern = "(a)", mark = { hl = "$1" } }"#, FIXED_FIELD_ERR ; "a_group_in_hl")]
    #[test_case(r#"{ ns = "a", pattern = "a", mark = { conceal = "a\tb" } }"#, CONTROL_CHAR_ERR ; "a_control_char_in_drawn_text")]
    #[test_case(r#"{ ns = "a", pattern = "a", mark = { stop = 3 } }"#, UNKNOWN_FIELD_ERR ; "a_field_marks_do_not_have")]
    #[test_case(r#"{ ns = "a", patern = "a" }"#, UNKNOWN_FIELD_ERR ; "a_misspelled_option")]
    #[test_case(r#"{ ns = "a", pattern = "a" }"#, RULE_HALVES_ERR ; "a_pattern_without_a_mark")]
    #[test_case(r#"{ pattern = "a", mark = {} }"#, REQUIRED_ERR ; "no_namespace")]
    #[test_case(r#"{ ns = "a", pattern = "a", mark = {}, treesitter = { lang = "lua", query = "" } }"#, SYNTAX_ALONE_ERR ; "a_pattern_and_a_query")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "klingon", query = "" } }"#, UNKNOWN_LANG_ERR ; "an_unknown_language")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = { klingon = "" } } }"#, UNKNOWN_LANG_ERR ; "a_query_for_an_unknown_language")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = "(nope) @x", captures = { x = {} } } }"#, BAD_QUERY_ERR ; "a_query_that_does_not_compile")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = "(identifier) @id" } }"#, NO_TEMPLATE_ERR ; "a_capture_without_a_template")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = "(identifier) @id", captures = { id = {}, other = {} } } }"#, NO_CAPTURE_ERR ; "a_template_without_a_capture")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = "((identifier) @id (#frob? @id))", captures = { id = {} } } }"#, UNKNOWN_PREDICATE_ERR ; "an_unknown_predicate")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = "(identifier) @id\n((identifier) @id (#not-lua-match? @id \"(\"))", captures = { id = {} } } }"#, BAD_REGEX_ERR ; "a_regex_that_does_not_compile")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = [[((identifier) @id (#set! url "x"))]], captures = { id = {} } } }"#, UNKNOWN_SETTING_ERR ; "an_unknown_setting")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "markdown", query = "(code_fence_content) @injection.content" } }"#, NO_CONTENT_LANG_ERR ; "injected_code_without_a_language")]
    fn input_rules_refuses(opts: &str, expected: &str) {
        let (lua, reader) = lua_with_rules();
        let err = lua
            .load(format!("ui.input_rules({opts})"))
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
        assert!(reader.load_full().is_empty());
    }

    const CONCEALING_PATTERN: &str = r#"{ ns = "a", pattern = "a", mark = { conceal = "" } }"#;
    const CONCEALING_QUERY: &str = r#"{ ns = "a", treesitter = { lang = "lua", query = "(identifier) @id", captures = { id = { conceal = "" } } } }"#;

    #[test_case(CONCEALING_PATTERN ; "a_pattern")]
    #[test_case(CONCEALING_QUERY   ; "a_query")]
    #[test_case(r#"{ ns = "a", treesitter = { lang = "lua", query = "(identifier) @id", captures = { id = { conceal = "$1$$" } } } }"# ; "a_query_template_with_dollars_as_written")]
    fn a_rule_is_set(opts: &str) {
        let (lua, reader) = lua_with_rules();
        lua.load(format!("ui.input_rules({opts})")).exec().unwrap();
        assert_eq!(reader.load_full().len(), 1);
    }

    /// A handler outliving its load would leave a rule the unload already
    /// cleared out, with nothing left to take it away.
    #[test]
    fn a_rule_from_a_load_that_is_gone_is_dropped() {
        let (lua, reader) = lua_with_rules_unloaded();
        lua.load(format!("ui.input_rules({CONCEALING_PATTERN})"))
            .exec()
            .unwrap();
        assert!(reader.load_full().is_empty());
    }

    fn found(bytes: Range<usize>, conceal: &str) -> (Range<usize>, Option<String>) {
        (bytes, Some(conceal.into()))
    }

    #[test_case(r"\A(!)", "!a\n!b", 0 => vec![found(0..1, "<!>")] ; "start_of_input_on_the_first_line")]
    #[test_case(r"\A(!)", "!a\n!b", 1 => Vec::new() ; "start_of_input_never_on_a_later_line")]
    #[test_case(r"(?m)^(!)", "!a\n!b", 1 => vec![found(3..4, "<!>")] ; "start_of_line_on_a_later_line")]
    #[test_case(r"(?s)(b.*)", "ab\nbc", 0 => vec![found(1..2, "<b>")] ; "a_match_stops_at_the_end_of_its_line")]
    #[test_case(r"(\w)$", "ab\ncd", 0 => vec![found(1..2, "<b>")] ; "dollar_is_the_end_of_the_line")]
    #[test_case(r"x(\d)", "x1 x2", 0 => vec![found(0..2, "<1>"), found(3..5, "<2>")] ; "every_match_on_the_line")]
    #[test_case(r"(\d*)", "a", 0 => vec![found(0..0, "<>"), found(1..1, "<>")] ; "an_empty_match_is_a_point")]
    fn a_rule_matches(pattern: &str, text: &str, y: usize) -> Vec<(Range<usize>, Option<String>)> {
        let rule = PatternRule::new(pattern, conceal("<$1>")).unwrap();
        let start: usize = text.split('\n').take(y).map(|line| line.len() + 1).sum();
        let end = start + text.split('\n').nth(y).unwrap().len();
        rule.matches(text, start..end)
            .map(|(bytes, mark)| (bytes, mark.conceal.clone()))
            .collect()
    }

    #[test_case(CHIP, "$skill:go", "$1", "go" ; "a_group")]
    #[test_case(CHIP, "$skill:go", "$0!", "$skill:go!" ; "the_whole_match")]
    #[test_case(CHIP, "$skill:go", "$$1", "$1" ; "an_escaped_sigil")]
    #[test_case(CHIP, "$skill:go", "$x", "$x" ; "a_sigil_before_a_letter")]
    #[test_case(r"<(.*)>", "<a\tb>", "$1", "ab" ; "a_group_without_its_control_chars")]
    fn a_template_fills_in(pattern: &str, text: &str, template: &str, filled: &str) {
        let rule = PatternRule::new(pattern, conceal(template)).unwrap();
        let (_, mark) = rule.matches(text, 0..text.len()).next().unwrap();
        assert_eq!(mark.conceal.as_deref(), Some(filled));
    }

    #[test]
    fn a_namespace_holds_one_rule_until_it_is_removed() {
        let (lua, reader) = lua_with_rules();
        let set = |ns: &str, text: &str| {
            let script = format!(
                r#"ui.input_rules({{ ns = "{ns}", pattern = "x", mark = {{ conceal = "{text}" }} }})"#
            );
            lua.load(script).exec().unwrap();
        };
        let conceals = || -> Vec<Option<String>> {
            reader
                .load_full()
                .iter()
                .map(|rule| match rule {
                    InputRule::Pattern(rule) => rule.mark().conceal.clone(),
                    InputRule::Syntax(_) => None,
                })
                .collect()
        };
        set("a", "1");
        set("b", "2");
        set("a", "3");
        assert_eq!(conceals(), [Some("3".into()), Some("2".into())]);

        lua.load(r#"ui.input_rules({ ns = "a" })"#).exec().unwrap();
        assert_eq!(conceals(), [Some("2".into())]);
    }
}
