//! Tree-sitter rules for the chat input. A rule is checked whole when a
//! plugin sets it, and runs later on the UI's worker thread, where an error
//! would have nobody to reach.
//!
//! A query capture becomes a mark built from the capture's template. Code
//! under `@injection.content` is parsed again with its own grammar when the
//! rule has a query for that language, and drawn as a `lang` mark otherwise.
//!
//! After an edit the queries run again only where the old and the new tree
//! differ. A match whose node lies outside that span is the same in both
//! trees, so the marks it made stay. The ones inside are found in the old
//! tree and taken out, then found in the new tree and put back in.

mod index;

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use mlua::{Result as LuaResult, Table, Value};
use regex::Regex;
use tree_sitter::{
    CaptureQuantifier, InputEdit, Node, Parser, Point, Query, QueryCursor, QueryMatch,
    Range as TsRange, StreamingIterator, Tree,
};

pub use self::index::SyntaxIndex;
use self::index::mark_key;
use super::marks::{Decoration, parse_mark};
use crate::api::treesitter::query::{broken_regex, is_known_predicate, predicates_hold};
use crate::api::util::convert::{TableExt, reject_unknown_fields};
use crate::language::Language;

const SYNTAX_FIELDS: &[&str] = &["lang", "query", "captures"];
const CONTENT: &str = "injection.content";
const CONTENT_LANG: &str = "injection.language";
const INJECTION_PREFIX: &str = "injection.";
/// Like in Neovim, a capture named `@_x` only feeds a predicate.
const HELPER_PREFIX: char = '_';
/// Markdown parses blocks only and leaves the text inside them to its inline
/// grammar, the way its own parser does. A rule with a query for that
/// grammar gets it run there without having to spell this out.
const MARKDOWN_INLINE_SPOTS: &str = r#"([(inline) (pipe_table_cell)] @injection.content
  (#set! injection.language "markdown_inline"))"#;
/// Markdown inside markdown could go on forever, and nothing real nests this
/// deep.
const MAX_INJECTION_DEPTH: usize = 4;
const WILDCARD_PARENT: &str = "(_ ";

pub(crate) const UNKNOWN_LANG_ERR: &str = "unknown language";
pub(crate) const BAD_QUERY_ERR: &str = "bad query";
pub(crate) const UNKNOWN_PREDICATE_ERR: &str = "unknown predicate";
pub(crate) const BAD_REGEX_ERR: &str = "regex does not compile";
pub(crate) const UNKNOWN_SETTING_ERR: &str = "#set! only takes injection.language";
pub(crate) const NO_TEMPLATE_ERR: &str = "has no template in captures";
pub(crate) const NO_CAPTURE_ERR: &str = "names no capture of any query";
pub(crate) const NO_CONTENT_LANG_ERR: &str =
    "@injection.content needs @injection.language or #set! injection.language in its pattern";

enum Capture {
    Mark(Arc<Decoration>),
    /// Code in another language. The template's own fields still apply.
    Content(Decoration),
    /// `@injection.language` and helper captures, which only feed others.
    Skip,
}

struct RuleQuery {
    lang: Language,
    query: Query,
    first_order: usize,
    captures: Vec<Capture>,
    language_capture: Option<u32>,
    /// The `#set! injection.language` of each pattern.
    languages: Vec<Option<String>>,
}

pub struct SyntaxRule {
    lang: Language,
    /// The root language's first, the rest in name order, so a later query
    /// draws over an earlier one and an injected tree over the one around it.
    queries: Vec<RuleQuery>,
    inline_spots: Option<RuleQuery>,
    shapes_rows: bool,
    orders: usize,
}

/// What one input keeps from one parse to the next: its own copy of the
/// text and a tree per rule.
#[derive(Default)]
pub struct SyntaxDoc {
    text: String,
    /// {text} as it was before the update running now. A tree finds what an
    /// edit changed by looking at the text it was parsed from, and editing
    /// a second copy costs less than copying the whole text every time.
    old_text: String,
    trees: Vec<SyntaxTree>,
}

pub enum SyntaxInput {
    /// The whole text, for a first parse or one the change log no longer
    /// reaches back to.
    Whole(String),
    /// {inserted} went over the bytes {edit} replaced. {start_row} is the
    /// line the edit starts on, which the input knows without counting.
    Edit {
        edit: SyntaxEdit,
        inserted: String,
        start_row: usize,
    },
}

/// The root tree of one rule, which the next parse edits and reuses, the
/// marks of its matches, and the marks of each region of code they inject,
/// like a paragraph's inline markdown.
#[derive(Default)]
struct SyntaxTree {
    root: Option<Tree>,
    /// By [`region_key`].
    regions: HashMap<u64, Region>,
    marks: SyntaxIndex,
}

/// Tree-sitter cannot reuse an injected tree whose ranges moved, so a region
/// keeps its marks for as long as a match injects the same text, and a
/// keystroke parses one paragraph again, not all of them.
struct Region {
    /// Counted from where the region starts.
    marks: Arc<[SyntaxMark]>,
    /// The matches that inject it.
    users: usize,
}

/// One change from the text a tree was parsed from to the text now:
/// `start..old_end` of the old text became `start..new_end`.
pub struct SyntaxEdit {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
}

impl SyntaxDoc {
    /// Brings the text and the marks of every rule up to date with {input},
    /// each rule's marks ranked from its own number up. Returns where the
    /// marks changed in the text now, `None` when they all may have.
    pub fn update(
        &mut self,
        rules: &[(usize, Arc<SyntaxRule>)],
        input: &SyntaxInput,
    ) -> Option<Vec<Range<usize>>> {
        self.trees.resize_with(rules.len(), SyntaxTree::default);
        let edit = input.apply(&mut self.text);
        let mut changed = Some(Vec::new());
        for ((rank, rule), tree) in rules.iter().zip(&mut self.trees) {
            let more = rule.update(tree, &self.old_text, &self.text, edit, *rank);
            changed = changed.zip(more).map(|(mut all, more)| {
                all.extend(more);
                all
            });
        }
        input.apply(&mut self.old_text);
        changed
    }

    /// The marks of every rule. Copies share all but the bits an update
    /// changes, so they are cheap to hand out after each one.
    pub fn marks(&self) -> Vec<SyntaxIndex> {
        self.trees.iter().map(|tree| tree.marks.clone()).collect()
    }
}

impl SyntaxInput {
    /// Brings {text} up to date, and returns the edit as a tree takes it,
    /// `None` when the parse has to start over.
    fn apply(&self, text: &mut String) -> Option<InputEdit> {
        match self {
            Self::Whole(whole) => {
                text.clone_from(whole);
                None
            }
            Self::Edit {
                edit,
                inserted,
                start_row,
            } => {
                let line_start = text[..edit.start].rfind('\n').map_or(0, |at| at + 1);
                let start_position = Point::new(*start_row, edit.start - line_start);
                let tree_edit = InputEdit {
                    start_byte: edit.start,
                    old_end_byte: edit.old_end,
                    new_end_byte: edit.start + inserted.len(),
                    start_position,
                    old_end_position: point_after(start_position, &text[edit.start..edit.old_end]),
                    new_end_position: point_after(start_position, inserted),
                };
                text.replace_range(edit.start..edit.old_end, inserted);
                Some(tree_edit)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxMark {
    pub bytes: Range<usize>,
    pub decoration: Arc<Decoration>,
    /// Where the mark composes among the others: the higher draws on top.
    pub rank: usize,
}

pub(crate) fn parse_syntax(tbl: &Table) -> LuaResult<Result<SyntaxRule, String>> {
    reject_unknown_fields(tbl, SYNTAX_FIELDS, "treesitter")?;
    let lang: String = tbl
        .opt::<String>("lang")?
        .ok_or_else(|| mlua::Error::runtime("treesitter: 'lang' is required"))?;
    let queries = match tbl.get::<Value>("query")? {
        Value::String(query) => vec![(lang.clone(), query.to_string_lossy())],
        Value::Table(per_lang) => per_lang
            .pairs::<String, String>()
            .collect::<LuaResult<_>>()?,
        _ => {
            return Err(mlua::Error::runtime(
                "treesitter: 'query' must be a string or a table of language = query",
            ));
        }
    };
    let mut captures = Vec::new();
    if let Some(templates) = tbl.opt::<Table>("captures")? {
        for pair in templates.pairs::<String, Table>() {
            let (name, mark) = pair?;
            let mark = parse_mark(&mark)
                .map_err(|e| mlua::Error::runtime(format!("captures.{name}: {e}")))?;
            captures.push((name, mark));
        }
    }
    Ok(SyntaxRule::new(&lang, queries, captures))
}

fn language(name: &str) -> Result<Language, String> {
    Language::from_name(name).ok_or_else(|| format!("{UNKNOWN_LANG_ERR} {name:?}"))
}

impl SyntaxRule {
    pub fn new(
        lang: &str,
        mut queries: Vec<(String, String)>,
        captures: Vec<(String, Decoration)>,
    ) -> Result<Self, String> {
        let root = language(lang)?;
        let mut templates = HashMap::new();
        for (name, mark) in captures {
            mark.check_drawn_text()
                .map_err(|e| format!("captures.{name}: {e}"))?;
            templates.insert(name, mark);
        }
        queries.sort_by(|(a, _), (b, _)| (a.as_str() != lang, a).cmp(&(b.as_str() != lang, b)));
        let mut used = Vec::new();
        let mut built = Vec::with_capacity(queries.len());
        let mut orders = 0;
        for (name, source) in &queries {
            let query = RuleQuery::new(language(name)?, source, orders, &templates, &mut used)
                .map_err(|e| format!("query for {name}: {e}"))?;
            orders += query.query.pattern_count() * query.captures.len();
            built.push(query);
        }
        if let Some(name) = templates.keys().find(|name| !used.contains(name)) {
            return Err(format!("captures.{name} {NO_CAPTURE_ERR}"));
        }
        let inline_spots = (root == Language::Markdown
            && built.iter().any(|q| q.lang == Language::MarkdownInline))
        .then(|| {
            RuleQuery::new(
                root,
                MARKDOWN_INLINE_SPOTS,
                0,
                &HashMap::new(),
                &mut Vec::new(),
            )
        })
        .transpose()?;
        let shapes_rows = built.iter().any(RuleQuery::shapes_rows);
        Ok(Self {
            lang: root,
            queries: built,
            inline_spots,
            shapes_rows,
            orders,
        })
    }

    /// How many ranks the rule takes, one per capture of each pattern of
    /// every query.
    pub fn orders(&self) -> usize {
        self.orders
    }

    pub fn shapes_rows(&self) -> bool {
        self.shapes_rows
    }

    /// Parses {text} and brings the marks in {tree} up to date with it,
    /// each ranked from {rank} up. {edit} turns {old}, the text the tree
    /// was parsed from, into {text}, and without one the parse starts over.
    /// Returns where the marks changed in {text}, `None` when they all may
    /// have.
    fn update(
        &self,
        tree: &mut SyntaxTree,
        old: &str,
        text: &str,
        edit: Option<InputEdit>,
        rank: usize,
    ) -> Option<Vec<Range<usize>>> {
        let last = edit.zip(tree.root.take()).map(|(edit, last)| {
            let mut edited = last.clone();
            edited.edit(&edit);
            (edit, last, edited)
        });
        let mut parser = Parser::new();
        let parsed = parser
            .set_language(&self.lang.ts_language())
            .ok()
            .and_then(|()| parser.parse(text, last.as_ref().map(|(_, _, edited)| edited)));
        let Some(parsed) = parsed else {
            *tree = SyntaxTree::default();
            return None;
        };
        let mut run = Run {
            rule: self,
            rank,
            parser,
            regex_cache: HashMap::new(),
            regions: &mut tree.regions,
        };
        let changed = match last {
            Some((edit, last, edited)) => {
                let dirty = dirty_span(edited.changed_ranges(&parsed), &edit);
                let old_end = dirty.end() - edit.new_end_byte + edit.old_end_byte;
                let old_dirty = *dirty.start()..=old_end;
                let removed = run.root_marks(last.root_node(), old, Some(&old_dirty), -1);
                let added = run.root_marks(parsed.root_node(), text, Some(&dirty), 1);
                for mark in &removed {
                    tree.marks.remove(mark);
                }
                tree.marks.edit(edit.old_end_byte, edit.new_end_byte);
                for mark in &added {
                    tree.marks.add(mark);
                }
                Some(changed(removed, added, &edit))
            }
            None => {
                for region in run.regions.values_mut() {
                    region.users = 0;
                }
                tree.marks = SyntaxIndex::new(run.root_marks(parsed.root_node(), text, None, 1));
                None
            }
        };
        tree.regions.retain(|_, region| region.users > 0);
        tree.root = Some(parsed);
        changed
    }

    fn query_for(&self, lang: Language) -> Option<&RuleQuery> {
        self.queries.iter().find(|query| query.lang == lang)
    }
}

impl RuleQuery {
    fn new(
        lang: Language,
        source: &str,
        first_order: usize,
        templates: &HashMap<String, Decoration>,
        used: &mut Vec<String>,
    ) -> Result<Self, String> {
        let compile = |source: &str| {
            Query::new(&lang.ts_language(), source).map_err(|e| format!("{BAD_QUERY_ERR}: {e}"))
        };
        let mut query = compile(source)?;
        let rooted = rooted_source(&query, source);
        if let Some(rooted) = &rooted {
            query = compile(rooted)?;
        }
        // Rooting adds no newline, so its lines are the ones the plugin wrote.
        let compiled = rooted.as_deref().unwrap_or(source);
        let mut captures = Vec::with_capacity(query.capture_names().len());
        let mut language_capture = None;
        for (index, name) in query.capture_names().iter().enumerate() {
            let capture = match *name {
                CONTENT => Capture::Content(templates.get(CONTENT).cloned().unwrap_or_default()),
                CONTENT_LANG => {
                    language_capture = Some(index as u32);
                    Capture::Skip
                }
                name if name.starts_with(INJECTION_PREFIX) => {
                    return Err(format!("@{name} is not an injection capture maki knows"));
                }
                name if name.starts_with(HELPER_PREFIX) => Capture::Skip,
                name => {
                    let template = templates
                        .get(name)
                        .ok_or_else(|| format!("@{name} {NO_TEMPLATE_ERR}"))?;
                    Capture::Mark(Arc::new(template.clone()))
                }
            };
            if !matches!(capture, Capture::Skip) {
                used.push((*name).to_owned());
            }
            captures.push(capture);
        }
        let check = |pattern: usize| {
            if let Some((property, _)) = query.property_predicates(pattern).first() {
                return Err(format!("#is? {} is not supported", property.key));
            }
            if let Some(predicate) = query
                .general_predicates(pattern)
                .iter()
                .find(|predicate| !is_known_predicate(&predicate.operator))
            {
                return Err(format!("{UNKNOWN_PREDICATE_ERR} #{}", predicate.operator));
            }
            if let Some((regex, e)) = broken_regex(&query, pattern) {
                return Err(format!("{BAD_REGEX_ERR}: {regex:?}: {e}"));
            }
            let language = set_language(&query, pattern)?;
            let has_content = captures.iter().enumerate().any(|(index, capture)| {
                matches!(capture, Capture::Content(_)) && in_pattern(&query, pattern, index)
            });
            let has_lang = language.is_some()
                || language_capture
                    .is_some_and(|index| in_pattern(&query, pattern, index as usize));
            if has_content && !has_lang {
                return Err(NO_CONTENT_LANG_ERR.into());
            }
            Ok(language)
        };
        let languages = (0..query.pattern_count())
            .map(|pattern| {
                check(pattern).map_err(|e| {
                    let start = query.start_byte_for_pattern(pattern);
                    let line = compiled[..start].matches('\n').count() + 1;
                    format!("{e} (pattern on line {line})")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            lang,
            query,
            first_order,
            captures,
            language_capture,
            languages,
        })
    }

    /// The rank of {capture} in {pattern}, for a rule ranked from {first}
    /// up. Patterns are in the order they are written and captures in the
    /// order their names first appear in the query, so a later one draws on
    /// top, whichever of their marks a lookup finds first.
    fn rank(&self, first: usize, pattern: usize, capture: u32) -> usize {
        first + self.first_order + pattern * self.captures.len() + capture as usize
    }

    fn shapes_rows(&self) -> bool {
        self.captures.iter().any(|capture| match capture {
            Capture::Mark(mark) => mark.shapes_rows(),
            Capture::Content(mark) => mark.shapes_rows(),
            Capture::Skip => false,
        })
    }

    /// The text `@injection.language` caught in {m}, or the pattern's
    /// `#set! injection.language`.
    fn content_lang<'a>(&'a self, m: &QueryMatch<'_, '_>, text: &'a str) -> Option<&'a str> {
        let caught = self.language_capture.and_then(|index| {
            m.captures()
                .iter()
                .find(|capture| capture.index == index)
                .map(|capture| text[capture.node.byte_range()].trim())
        });
        caught.or(self.languages[m.pattern_index].as_deref())
    }
}

/// The `#set! injection.language` of {pattern}, the one setting a rule
/// takes.
fn set_language(query: &Query, pattern: usize) -> Result<Option<String>, String> {
    let mut language = None;
    for property in query.property_settings(pattern) {
        let key = &*property.key;
        if key != CONTENT_LANG {
            return Err(format!("{UNKNOWN_SETTING_ERR}, not {key}"));
        }
        let value = property
            .value
            .as_deref()
            .ok_or_else(|| format!("#set! {key} needs a value"))?;
        language = Some(value.to_owned());
    }
    Ok(language)
}

struct Run<'a> {
    rule: &'a SyntaxRule,
    rank: usize,
    parser: Parser,
    regex_cache: HashMap<String, Option<Regex>>,
    regions: &'a mut HashMap<u64, Region>,
}

#[derive(Default)]
struct Found {
    marks: Vec<SyntaxMark>,
    injections: Vec<(Language, Vec<TsRange>)>,
}

impl Run<'_> {
    /// The marks of the matches under {root}, with the marks of the code
    /// they inject. With {dirty}, only the matches whose node touches it.
    /// Every region they inject gets {users} added to its count.
    fn root_marks(
        &mut self,
        root: Node,
        text: &str,
        dirty: Option<&RangeInclusive<usize>>,
        users: isize,
    ) -> Vec<SyntaxMark> {
        let mut found = Found::default();
        self.find(root, text, self.rule.lang, 0, dirty, &mut found);
        let mut marks = found.marks;
        for (lang, ranges) in found.injections {
            let key = region_key(text, lang, &ranges);
            if !self.regions.contains_key(&key) {
                let marks = self.region(text, lang, &ranges, 0).into();
                self.regions.insert(key, Region { marks, users: 0 });
            }
            let region = self.regions.get_mut(&key).expect("inserted above");
            region.users = region.users.saturating_add_signed(users);
            let start = ranges[0].start_byte;
            marks.extend(region.marks.iter().map(|mark| moved(mark, start)));
        }
        marks
    }

    /// Adds what the matches of {lang}'s queries under {root} make to
    /// {found}. With {dirty}, only the matches whose node touches it.
    ///
    /// Each node touching {dirty} runs the queries for the patterns that
    /// start at it, and a node inside {dirty} runs them for its whole
    /// subtree at once. A byte range on the cursor would be simpler, but
    /// tree-sitter can then finish a match on a node of the next subtree, as
    /// if it were a sibling.
    fn find(
        &mut self,
        root: Node,
        text: &str,
        lang: Language,
        depth: usize,
        dirty: Option<&RangeInclusive<usize>>,
        found: &mut Found,
    ) {
        let Some(dirty) = dirty else {
            return self.find_at(root, text, lang, depth, false, found);
        };
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            let inside = dirty.contains(&node.start_byte()) && dirty.contains(&node.end_byte());
            self.find_at(node, text, lang, depth, !inside, found);
            if !inside {
                pending.extend(children_touching(node, dirty));
            }
        }
    }

    /// Runs {lang}'s queries on {node}, for the patterns that start at it
    /// when {only_here}, and anywhere under it otherwise.
    fn find_at(
        &mut self,
        node: Node,
        text: &str,
        lang: Language,
        depth: usize,
        only_here: bool,
        found: &mut Found,
    ) {
        let rule = self.rule;
        let spots = rule
            .inline_spots
            .as_ref()
            .filter(|_| lang == Language::Markdown);
        let mut cursor = QueryCursor::new();
        cursor.set_max_start_depth(only_here.then_some(0));
        for query in rule.query_for(lang).into_iter().chain(spots) {
            let mut matches = cursor.matches(&query.query, node, text.as_bytes());
            while let Some(m) = matches.next() {
                self.add_match(query, m, text, depth, found);
            }
        }
    }

    fn add_match(
        &mut self,
        query: &RuleQuery,
        m: &QueryMatch<'_, '_>,
        text: &str,
        depth: usize,
        found: &mut Found,
    ) {
        let source = text.as_bytes();
        if !predicates_hold(
            &query.query,
            m.pattern_index,
            m.captures(),
            source,
            &mut self.regex_cache,
        ) {
            return;
        }
        for capture in m.captures() {
            let rank = query.rank(self.rank, m.pattern_index, capture.index);
            let bytes = capture.node.byte_range();
            match &query.captures[capture.index as usize] {
                Capture::Skip => {}
                Capture::Mark(template) => found.marks.push(SyntaxMark {
                    bytes,
                    decoration: Arc::clone(template),
                    rank,
                }),
                Capture::Content(template) => {
                    let Some(name) = query.content_lang(m, text) else {
                        continue;
                    };
                    let parsed = Language::from_name(name)
                        .filter(|lang| self.rule.query_for(*lang).is_some())
                        .filter(|_| depth < MAX_INJECTION_DEPTH);
                    let decoration = match parsed {
                        Some(lang) => {
                            let ranges = content_ranges(capture.node);
                            if !ranges.is_empty() {
                                found.injections.push((lang, ranges));
                            }
                            if *template == Decoration::default() {
                                continue;
                            }
                            template.clone()
                        }
                        None => Decoration {
                            lang: Some(name.to_owned()),
                            ..template.clone()
                        },
                    };
                    found.marks.push(SyntaxMark {
                        bytes,
                        decoration: Arc::new(decoration),
                        rank,
                    });
                }
            }
        }
    }

    /// The marks of code in {lang} at {ranges}, injected into a tree
    /// {depth} levels down, counted from where it starts, so they hold
    /// wherever the same code moves.
    fn region(
        &mut self,
        text: &str,
        lang: Language,
        ranges: &[TsRange],
        depth: usize,
    ) -> Vec<SyntaxMark> {
        let ready = self.parser.set_language(&lang.ts_language()).is_ok()
            && self.parser.set_included_ranges(ranges).is_ok();
        let tree = ready.then(|| self.parser.parse(text, None)).flatten();
        let _ = self.parser.set_included_ranges(&[]);
        let mut found = Found::default();
        if let Some(tree) = tree {
            self.find(tree.root_node(), text, lang, depth + 1, None, &mut found);
        }
        let mut marks = found.marks;
        for (lang, ranges) in found.injections {
            let start = ranges[0].start_byte;
            let nested = self.region(text, lang, &ranges, depth + 1);
            marks.extend(nested.iter().map(|mark| moved(mark, start)));
        }
        let start = ranges[0].start_byte;
        for mark in &mut marks {
            mark.bytes = mark.bytes.start - start..mark.bytes.end - start;
        }
        marks
    }
}

fn in_pattern(query: &Query, pattern: usize, index: usize) -> bool {
    query.capture_quantifiers(pattern)[index] != CaptureQuantifier::Zero
}

fn children_touching<'t>(node: Node<'t>, dirty: &RangeInclusive<usize>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    // Going to the child by byte skips the ones before it, but it fails
    // when the byte sits in a gap between two children, like the blank line
    // between two blocks, and then every child is looked at.
    let entered = dirty
        .start()
        .checked_sub(1)
        .is_some_and(|before| cursor.goto_first_child_for_byte(before).is_some())
        || cursor.goto_first_child();
    let mut children = Vec::new();
    if !entered {
        return children;
    }
    loop {
        let child = cursor.node();
        if child.start_byte() > *dirty.end() {
            break;
        }
        if child.end_byte() >= *dirty.start() {
            children.push(child);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    children
}

/// {source} with every pattern that has no single root, like `((a) . (b))`,
/// put under a wildcard parent, which matches the same siblings. After an
/// edit a match is found again from the node its pattern starts at, and
/// such a pattern starts at a node but looks at its siblings too. `None`
/// when every pattern has a root already.
fn rooted_source(query: &Query, source: &str) -> Option<String> {
    let unrooted: Vec<usize> = (0..query.pattern_count())
        .filter(|&pattern| !query.is_pattern_rooted(pattern))
        .collect();
    if unrooted.is_empty() {
        return None;
    }
    let mut rooted = String::with_capacity(source.len() + unrooted.len() * WILDCARD_PARENT.len());
    let mut copied = 0;
    for pattern in unrooted {
        let (start, end) = (
            query.start_byte_for_pattern(pattern),
            query.end_byte_for_pattern(pattern),
        );
        rooted.push_str(&source[copied..start]);
        rooted.push_str(WILDCARD_PARENT);
        rooted.push_str(&source[start..end]);
        rooted.push(')');
        copied = end;
    }
    rooted.push_str(&source[copied..]);
    Some(rooted)
}

fn moved(mark: &SyntaxMark, by: usize) -> SyntaxMark {
    SyntaxMark {
        bytes: mark.bytes.start + by..mark.bytes.end + by,
        decoration: Arc::clone(&mark.decoration),
        rank: mark.rank,
    }
}

/// Where the matches have to be found again, in the text after {edit}:
/// where the trees differ and what the edit put in. Its ends count, since a
/// node that only touches the edit may have changed too.
fn dirty_span(changed: impl Iterator<Item = TsRange>, edit: &InputEdit) -> RangeInclusive<usize> {
    let (start, end) = changed.fold(
        (edit.start_byte, edit.new_end_byte),
        |(start, end), range| (start.min(range.start_byte), end.max(range.end_byte)),
    );
    start..=end
}

/// Where the marks {added} differ from the ones {removed}, which were in the
/// text before {edit}, moved through it. The edited text is always counted,
/// and so are marks the edit touched, where they are now and wherever the
/// input may have drawn them: until this result is in, the input moves
/// those by its own rules, which can stretch them over the edited text or
/// drop them.
fn changed(
    removed: Vec<SyntaxMark>,
    mut added: Vec<SyntaxMark>,
    edit: &InputEdit,
) -> Vec<Range<usize>> {
    let shift = |pos: usize| {
        if pos > edit.old_end_byte {
            pos - edit.old_end_byte + edit.new_end_byte
        } else {
            pos
        }
    };
    let (untouched, touched): (Vec<SyntaxMark>, Vec<SyntaxMark>) = removed
        .into_iter()
        .partition(|mark| mark.bytes.start > edit.old_end_byte || mark.bytes.end < edit.start_byte);
    let mut changed: Vec<Range<usize>> = touched
        .iter()
        .map(|mark| {
            let end = if mark.bytes.end > edit.old_end_byte {
                shift(mark.bytes.end)
            } else {
                edit.new_end_byte
            };
            mark.bytes.start.min(edit.start_byte)..end
        })
        .collect();
    let mut moved: Vec<SyntaxMark> = untouched
        .into_iter()
        .map(|mark| SyntaxMark {
            bytes: shift(mark.bytes.start)..shift(mark.bytes.end),
            ..mark
        })
        .collect();
    moved.sort_by_key(mark_key);
    added.sort_by_key(mark_key);
    let mut moved = moved.into_iter().peekable();
    let mut added = added.into_iter().peekable();
    loop {
        let bytes = match (moved.peek(), added.peek()) {
            (None, None) => break,
            (Some(was), Some(now)) if was == now => {
                moved.next();
                added.next();
                continue;
            }
            (Some(was), Some(now)) if mark_key(was) > mark_key(now) => {
                added.next().map(|now| now.bytes)
            }
            (Some(_), _) => moved.next().map(|was| was.bytes),
            (None, Some(_)) => added.next().map(|now| now.bytes),
        };
        changed.extend(bytes);
    }
    changed.push(edit.start_byte..edit.new_end_byte);
    changed.sort_by_key(|bytes| bytes.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(changed.len());
    for bytes in changed {
        match merged.last_mut() {
            Some(last) if bytes.start <= last.end => last.end = last.end.max(bytes.end),
            _ => merged.push(bytes),
        }
    }
    merged
}

/// {node} without its named children, like Neovim and markdown's own parser
/// inject: a quote's `>` markers between the lines of a paragraph are not
/// part of the paragraph's text.
fn content_ranges(node: Node) -> Vec<TsRange> {
    let mut ranges = Vec::new();
    let mut rest = node.range();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        ranges.push(TsRange {
            end_byte: child.start_byte(),
            end_point: child.start_position(),
            ..rest
        });
        rest.start_byte = child.end_byte();
        rest.start_point = child.end_position();
    }
    ranges.push(rest);
    ranges.retain(|range| range.start_byte < range.end_byte);
    ranges
}

/// Everything the marks of an injected region depend on: its language and
/// its text with the holes the outer tree cut into it.
fn region_key(text: &str, lang: Language, ranges: &[TsRange]) -> u64 {
    let start = ranges[0].start_byte;
    let mut hasher = DefaultHasher::new();
    (lang as usize).hash(&mut hasher);
    for range in ranges {
        (
            range.start_byte - start,
            &text[range.start_byte..range.end_byte],
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn point_after(start: Point, text: &str) -> Point {
    let bytes = text.as_bytes();
    let rows = bytes.iter().filter(|&&b| b == b'\n').count();
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(newline) => Point::new(start.row + rows, bytes.len() - newline - 1),
        None => Point::new(start.row, start.column + bytes.len()),
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const INLINE: &str = r#"
        (strong_emphasis (emphasis_delimiter) @hidden) @strong
        (emphasis (emphasis_delimiter) @hidden) @em
        (code_span (code_span_delimiter) @hidden) @code
    "#;
    const BLOCK: &str = r#"
        (atx_heading) @heading
        (fenced_code_block
          (info_string (language) @injection.language)
          (code_fence_content) @injection.content)
    "#;
    const DOC: &str = "# Title\n\nsay **hi** to *you* and `x`\n\n```rust\nfn main() {}\n```\n";
    /// The patterns a re-query gets wrong first. A match whose node is not
    /// captured, or whose siblings an anchor ties together, makes marks
    /// that depend on more than the text under them.
    const MIXED_BLOCK: &str = r#"
        (atx_heading) @heading
        (atx_heading (atx_h1_marker) (inline) @title)
        (list (list_item) . (list_item (paragraph) @second))
        ((paragraph) @after_heading . (atx_heading))
        (block_quote (paragraph) @quoted)
        (fenced_code_block
          (info_string (language) @injection.language)
          (code_fence_content) @injection.content)
    "#;
    const MIXED_INLINE: &str = r#"
        (strong_emphasis (emphasis_delimiter) @hidden) @strong
        (emphasis (emphasis_delimiter) @hidden) @em
        (code_span (code_span_delimiter) @hidden) @code
        (inline_link (link_text) @link)
    "#;
    const MIXED_DOC: &str = "# Title\n\nsay **hi** to *you*\n\n- one\n- two `x`\n  - [link](u) and _em_\n\n```rust\nfn main() {}\n```\n\n> quoted **bold**\n> more\n\n## Next\n\n| a | b |\n|---|---|\n| *c* | d |\n\ntext\n# Last\n";
    const MIXED_CAPTURES: &[&str] = &[
        "heading",
        "title",
        "second",
        "after_heading",
        "quoted",
        "strong",
        "em",
        "code",
        "link",
        "hidden",
    ];
    const SNIPPETS: &[&str] = &[
        "*",
        "**",
        "`",
        "_",
        "\n",
        "\n\n",
        "# ",
        "- ",
        "> ",
        "```\n",
        "```rust\n",
        "|",
        "[a](b)",
        "word ",
        "  ",
        "## x\n",
    ];
    const SEED: u64 = 7;
    const EDITS: usize = 300;
    const MAX_DELETE: usize = 12;

    type Ranked = Vec<(usize, Arc<SyntaxRule>)>;

    fn hl(name: &str) -> Decoration {
        Decoration {
            hl: Some(maki_agent::SpanStyle::Named(name.into())),
            ..Decoration::default()
        }
    }

    fn highlighted<'a>(names: &[&'a str]) -> Vec<(&'a str, Decoration)> {
        names.iter().map(|&name| (name, hl(name))).collect()
    }

    fn rule(lang: &str, queries: &[(&str, &str)], captures: Vec<(&str, Decoration)>) -> Ranked {
        let queries = queries
            .iter()
            .map(|&(lang, query)| (lang.into(), query.into()))
            .collect();
        let captures = captures
            .into_iter()
            .map(|(name, mark)| (name.into(), mark))
            .collect();
        vec![(
            0,
            Arc::new(SyntaxRule::new(lang, queries, captures).unwrap()),
        )]
    }

    fn preview() -> Ranked {
        let hidden = Decoration {
            conceal: Some(String::new()),
            ..Decoration::default()
        };
        let mut captures = highlighted(&["strong", "em", "code", "heading"]);
        captures.push(("hidden", hidden));
        let queries = [("markdown", BLOCK), ("markdown_inline", INLINE)];
        rule("markdown", &queries, captures)
    }

    fn mixed() -> Ranked {
        let queries = [("markdown", MIXED_BLOCK), ("markdown_inline", MIXED_INLINE)];
        rule("markdown", &queries, highlighted(MIXED_CAPTURES))
    }

    fn parsed(rules: &Ranked, text: &str) -> SyntaxDoc {
        let mut doc = SyntaxDoc::default();
        doc.update(rules, &SyntaxInput::Whole(text.into()));
        doc
    }

    fn marks(doc: &SyntaxDoc) -> Vec<SyntaxMark> {
        doc.trees[0].marks.marks()
    }

    fn fresh(rules: &Ranked, text: &str) -> Vec<SyntaxMark> {
        marks(&parsed(rules, text))
    }

    /// Shows the marks only one side has as the text they cover, which is
    /// much easier to read than two long lists of byte ranges.
    fn assert_same(marks: &[SyntaxMark], expected: &[SyntaxMark], text: &str, context: &str) {
        let only_in = |a: &[SyntaxMark], b: &[SyntaxMark]| -> Vec<_> {
            a.iter()
                .filter(|mark| !b.contains(mark))
                .map(|mark| (&text[mark.bytes.clone()], mark.rank))
                .collect()
        };
        let (extra, missing) = (only_in(marks, expected), only_in(expected, marks));
        assert!(
            extra.is_empty() && missing.is_empty(),
            "extra {extra:?} missing {missing:?} {context}"
        );
    }

    /// Puts {inserted} over {bytes} and checks the result two ways. The
    /// marks must match a fresh parse. And a mark that came or went, instead
    /// of just moving with the text, must sit in a range the update
    /// reported, or the input would never recount the line it is on. So must
    /// everywhere the input may have stretched a mark the edit touched.
    fn assert_edit(rules: &Ranked, doc: &mut SyntaxDoc, bytes: Range<usize>, inserted: &str) {
        let before = marks(doc);
        let context = format!("{inserted:?} over {bytes:?} of {:?}", doc.text);
        let input = SyntaxInput::Edit {
            edit: SyntaxEdit {
                start: bytes.start,
                old_end: bytes.end,
                new_end: bytes.start + inserted.len(),
            },
            inserted: inserted.into(),
            start_row: doc.text[..bytes.start].matches('\n').count(),
        };
        let changed = doc.update(rules, &input).unwrap();
        let after = marks(doc);
        assert_same(&after, &fresh(rules, &doc.text), &doc.text, &context);

        let shift = |pos: usize| {
            if pos > bytes.end {
                pos - bytes.end + bytes.start + inserted.len()
            } else {
                pos
            }
        };
        let (untouched, touched): (Vec<_>, Vec<_>) = before
            .iter()
            .partition(|mark| mark.bytes.start > bytes.end || mark.bytes.end < bytes.start);
        let kept: Vec<_> = untouched
            .into_iter()
            .map(|mark| SyntaxMark {
                bytes: shift(mark.bytes.start)..shift(mark.bytes.end),
                ..mark.clone()
            })
            .collect();
        let covered = |mark: &SyntaxMark| {
            changed
                .iter()
                .any(|span| span.start <= mark.bytes.start && mark.bytes.end <= span.end)
        };
        for mark in &after {
            assert!(
                kept.contains(mark) || covered(mark),
                "{mark:?} new, {context}"
            );
        }
        for mark in &kept {
            assert!(
                after.contains(mark) || covered(mark),
                "{mark:?} gone, {context}"
            );
        }
        for mark in touched {
            let end = if mark.bytes.end > bytes.end {
                shift(mark.bytes.end)
            } else {
                bytes.start + inserted.len()
            };
            let stretched = SyntaxMark {
                bytes: mark.bytes.start.min(bytes.start)..end,
                ..mark.clone()
            };
            assert!(covered(&stretched), "{mark:?} touched, {context}");
        }
        let len = doc.text.len();
        assert!(
            changed.iter().all(|span| span.end <= len),
            "past the end, {context}"
        );
    }

    fn described(text: &str, marks: &[SyntaxMark]) -> Vec<(String, String)> {
        marks
            .iter()
            .map(|mark| {
                let what = match &*mark.decoration {
                    Decoration {
                        lang: Some(lang), ..
                    } => format!("lang {lang}"),
                    Decoration {
                        conceal: Some(_), ..
                    } => "hidden".into(),
                    Decoration {
                        hl: Some(maki_agent::SpanStyle::Named(name)),
                        ..
                    } => name.clone(),
                    other => format!("{other:?}"),
                };
                (text[mark.bytes.clone()].to_owned(), what)
            })
            .collect()
    }

    fn pair(text: &str, what: &str) -> (String, String) {
        (text.into(), what.into())
    }

    #[test]
    fn a_markdown_preview_marks_both_grammars_and_injected_code() {
        let marks = fresh(&preview(), DOC);
        assert_eq!(
            described(DOC, &marks),
            [
                pair("# Title\n", "heading"),
                pair("*", "hidden"),
                pair("**hi**", "strong"),
                pair("*", "hidden"),
                pair("*", "hidden"),
                pair("*", "hidden"),
                pair("*", "hidden"),
                pair("*you*", "em"),
                pair("*", "hidden"),
                pair("`", "hidden"),
                pair("`x`", "code"),
                pair("`", "hidden"),
                pair("fn main() {}\n", "lang rust"),
            ]
        );
    }

    #[test_case("say", "tell" ; "a_word_swapped")]
    #[test_case("```rust\n", "" ; "a_fence_opener_removed")]
    #[test_case("hi**", "hi" ; "a_closing_delimiter_removed")]
    #[test_case("# ", "" ; "a_heading_marker_removed")]
    fn an_edit_parses_like_a_fresh_one(from: &str, to: &str) {
        let rules = preview();
        let start = DOC.find(from).unwrap();
        assert_edit(
            &rules,
            &mut parsed(&rules, DOC),
            start..start + from.len(),
            to,
        );
    }

    #[test]
    fn random_edits_parse_like_a_fresh_one() {
        let rules = mixed();
        let mut random = fastrand::Rng::with_seed(SEED);
        let mut doc = parsed(&rules, &MIXED_DOC.repeat(3));
        for _ in 0..EDITS {
            let len = doc.text.len();
            let start = random.usize(..=len);
            let (old_end, inserted) = if random.bool() {
                (start, *random.choice(SNIPPETS).unwrap())
            } else {
                (random.usize(start..=len.min(start + MAX_DELETE)), "")
            };
            assert_edit(&rules, &mut doc, start..old_end, inserted);
        }
    }

    #[test]
    fn a_predicate_filters_matches() {
        let query = r#"((atx_heading (inline) @title) (#contains? @title "keep"))"#;
        let rules = rule("markdown", &[("markdown", query)], highlighted(&["title"]));
        let text = "# keep me\n# drop me\n";
        assert_eq!(
            described(text, &fresh(&rules, text)),
            [pair("keep me", "title")]
        );
    }
}
