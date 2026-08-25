use std::collections::HashMap;
use std::fmt::Write;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as SynHighlighter, Style as SynStyle,
    Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const TOKEN_ALIASES: &[(&str, &str)] = &[("jsx", "js")];
pub const TAB_SPACES: &str = "  ";
const BLOCK_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;

type Rgb = (u8, u8, u8);
pub type BlockSegments = Arc<Vec<Vec<StyledSegment>>>;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<RwLock<Arc<Theme>>> = OnceLock::new();
static UI_COLORS: OnceLock<RwLock<HashMap<String, Rgb>>> = OnceLock::new();
static BLOCK_CACHE: OnceLock<RwLock<BlockCache>> = OnceLock::new();
static THEME_GEN: AtomicU64 = AtomicU64::new(0);

fn theme_lock() -> &'static RwLock<Arc<Theme>> {
    THEME.get_or_init(|| RwLock::new(Arc::new(Theme::default())))
}

pub fn warmup() {
    syntax_set();
    theme_lock();
    let mut hl = Highlighter::for_token("bash");
    hl.highlight_line("x");
}

pub fn is_ready() -> bool {
    SYNTAX_SET.get().is_some()
}

pub fn set_theme(theme: Theme) {
    *theme_lock().write().unwrap_or_else(|e| e.into_inner()) = Arc::new(theme);
    // Bump first: a highlight already in flight read the old generation, so its
    // insert lands under a key nobody will look up again.
    THEME_GEN.fetch_add(1, Ordering::Release);
    block_cache_lock()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Bumped by every [`set_theme`]. Anything derived from the theme, like an
/// incremental [`CodeHighlighter`] or a bag of painted lines, is stale once
/// this changes.
pub fn theme_generation() -> u64 {
    THEME_GEN.load(Ordering::Acquire)
}

fn block_cache_lock() -> &'static RwLock<BlockCache> {
    BLOCK_CACHE.get_or_init(RwLock::default)
}

/// The byte length rides along with the hash, so an accidental hit needs both
/// to collide. A miss only costs a re-highlight, a false hit costs wrong colors.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct BlockKey {
    hash: u64,
    code_bytes: usize,
    theme_gen: u64,
}

impl BlockKey {
    fn new(lang: &str, code: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        lang.hash(&mut hasher);
        code.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
            code_bytes: code.len(),
            theme_gen: theme_generation(),
        }
    }
}

fn segments_bytes(segments: &[Vec<StyledSegment>]) -> usize {
    segments
        .iter()
        .map(|line| {
            size_of::<Vec<StyledSegment>>()
                + line
                    .iter()
                    .map(|seg| size_of::<StyledSegment>() + seg.text.len())
                    .sum::<usize>()
        })
        .sum()
}

/// Budgeted in bytes, since entries range from a one-liner to a whole file.
///
/// Eviction picks an arbitrary entry on purpose. A resize walks the transcript
/// in order, so an LRU (or dropping a whole generation) always throws out
/// exactly what the walk asks for next and misses every time once the
/// transcript outgrows the budget. Arbitrary eviction keeps a stable subset
/// instead, and the hit rate settles near `budget / working set`.
#[derive(Default)]
struct BlockCache {
    entries: HashMap<BlockKey, (BlockSegments, usize)>,
    bytes: usize,
}

impl BlockCache {
    fn get(&self, key: BlockKey) -> Option<BlockSegments> {
        self.entries.get(&key).map(|(segs, _)| Arc::clone(segs))
    }

    /// Evicts before inserting, so the incoming entry is never its own victim.
    fn insert(&mut self, key: BlockKey, segments: BlockSegments, budget: usize) {
        let bytes = segments_bytes(&segments);
        if bytes > budget {
            return;
        }
        self.remove(key);
        while self.bytes + bytes > budget
            && let Some(&victim) = self.entries.keys().next()
        {
            self.remove(victim);
        }
        self.entries.insert(key, (segments, bytes));
        self.bytes += bytes;
    }

    fn remove(&mut self, key: BlockKey) {
        if let Some((_, bytes)) = self.entries.remove(&key) {
            self.bytes -= bytes;
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

pub fn theme() -> Arc<Theme> {
    theme_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn ui_colors_lock() -> &'static RwLock<HashMap<String, Rgb>> {
    UI_COLORS.get_or_init(RwLock::default)
}

pub fn set_ui_colors(colors: HashMap<String, Rgb>) {
    *ui_colors_lock().write().unwrap_or_else(|e| e.into_inner()) = colors;
}

pub fn theme_color(name: &str) -> Option<Rgb> {
    if let Some(&c) = ui_colors_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
    {
        return Some(c);
    }
    let settings = &theme().settings;
    let map = serde_json::to_value(settings).ok()?;
    let obj = map.as_object()?;
    let val = obj.get(name)?;
    let obj = val.as_object()?;
    let r = obj.get("r")?.as_u64()? as u8;
    let g = obj.get("g")?.as_u64()? as u8;
    let b = obj.get("b")?.as_u64()? as u8;
    Some((r, g, b))
}

pub fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

pub fn normalize_text(text: &str) -> String {
    text.trim_end_matches('\n').replace('\t', TAB_SPACES)
}

pub fn syntax_for_path(path: &str) -> &'static SyntaxReference {
    syntax_set()
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            let ext = path.rsplit('.').next().unwrap_or(path);
            syntax_for_token(ext)
        })
}

pub fn syntax_for_token(lang: &str) -> &'static SyntaxReference {
    let ss = syntax_set();
    ss.find_syntax_by_token(lang)
        .or_else(|| {
            TOKEN_ALIASES
                .iter()
                .find(|(from, _)| *from == lang)
                .and_then(|(_, to)| ss.find_syntax_by_token(to))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

pub struct Highlighter {
    theme: Arc<Theme>,
    parse_state: ParseState,
    highlight_state: HighlightState,
}

impl Highlighter {
    fn new(syntax: &SyntaxReference, theme: Arc<Theme>) -> Self {
        let syn_hl = SynHighlighter::new(&theme);
        Self {
            highlight_state: HighlightState::new(&syn_hl, ScopeStack::new()),
            parse_state: ParseState::new(syntax),
            theme,
        }
    }

    fn from_state(
        theme: Arc<Theme>,
        highlight_state: HighlightState,
        parse_state: ParseState,
    ) -> Self {
        Self {
            theme,
            highlight_state,
            parse_state,
        }
    }

    pub fn for_path(path: &str) -> Self {
        Self::new(syntax_for_path(path), theme())
    }

    pub fn for_syntax(syntax: &'static SyntaxReference) -> Self {
        Self::new(syntax, theme())
    }

    pub fn for_token(lang: &str) -> Self {
        Self::new(syntax_for_token(lang), theme())
    }

    fn raw_highlight_line<'a>(
        &mut self,
        text: &'a str,
    ) -> Result<Vec<(SynStyle, &'a str)>, syntect::Error> {
        let ops = self.parse_state.parse_line(text, syntax_set())?;
        let syn_hl = SynHighlighter::new(&self.theme);
        let iter = HighlightIterator::new(&mut self.highlight_state, &ops, text, &syn_hl);
        Ok(iter.collect())
    }

    pub fn highlight_line(&mut self, text: &str) -> Vec<StyledSegment> {
        match self.raw_highlight_line(text) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| StyledSegment::from_syntect(style, normalize_text(text)))
                .collect(),
            Err(_) => vec![StyledSegment::fallback(normalize_text(text))],
        }
    }

    pub fn advance(&mut self, text: &str) {
        let _ = self.raw_highlight_line(text);
    }

    pub fn state(self) -> (HighlightState, ParseState) {
        (self.highlight_state, self.parse_state)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StyledSegment {
    pub text: String,
    pub fg: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl StyledSegment {
    fn from_syntect(style: SynStyle, text: String) -> Self {
        let f = style.foreground;
        Self {
            text,
            fg: (f.r, f.g, f.b),
            bold: style.font_style.contains(FontStyle::BOLD),
            italic: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        }
    }

    fn fallback(text: String) -> Self {
        Self {
            text,
            fg: (204, 204, 204),
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

pub fn highlight_code(lang: &str, code: &str, prefix: &str) -> Vec<Vec<StyledSegment>> {
    let mut hl = Highlighter::for_token(lang);
    if !prefix.is_empty() {
        for line in LinesWithEndings::from(prefix) {
            hl.advance(line);
        }
    }
    LinesWithEndings::from(code)
        .map(|raw| hl.highlight_line(raw))
        .collect()
}

/// Highlights a whole block, memoized on its content.
///
/// Highlighting is by far the most expensive part of a markdown render (syntect
/// burns ~270us of regex per line) and the result does not depend on terminal
/// width, so laying the same text out again after a resize should never pay for
/// it twice. Callers streaming a growing block stay on [`CodeHighlighter`]:
/// it is incremental, and would miss this cache on every token.
pub fn highlight_block(lang: &str, code: &str) -> BlockSegments {
    let key = BlockKey::new(lang, code);
    if let Some(hit) = block_cache_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
    {
        return hit;
    }
    let segments: BlockSegments = Arc::new(highlight_code(lang, code, ""));
    block_cache_lock()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, Arc::clone(&segments), BLOCK_CACHE_MAX_BYTES);
    segments
}

pub fn highlight_lines_independent(lang: &str, code: &str) -> Vec<Vec<StyledSegment>> {
    let syntax = syntax_for_token(lang);
    LinesWithEndings::from(code)
        .map(|raw| Highlighter::for_syntax(syntax).highlight_line(raw))
        .collect()
}

pub fn highlight_ansi(lang: &str, code: &str, bg: (u8, u8, u8)) -> String {
    let bg_code = format!("\x1b[48;2;{};{};{}m", bg.0, bg.1, bg.2);
    let mut hl = Highlighter::for_token(lang);
    let mut out = String::new();
    for line in LinesWithEndings::from(code) {
        out.push_str(&bg_code);
        for seg in hl.highlight_line(line) {
            let bold = if seg.bold { "1;" } else { "" };
            let _ = write!(
                out,
                "\x1b[{bold}38;2;{};{};{}m{}",
                seg.fg.0, seg.fg.1, seg.fg.2, seg.text
            );
        }
        out.push_str("\x1b[K\x1b[0m\n");
    }
    out
}

pub struct CodeHighlighter {
    checkpoint_parse: ParseState,
    checkpoint_highlight: HighlightState,
    completed_lines: usize,
    cached_segments: Vec<Vec<StyledSegment>>,
}

impl CodeHighlighter {
    pub fn new(lang: &str) -> Self {
        let syntax = syntax_for_token(lang);
        let t = theme();
        let highlighter = SynHighlighter::new(&t);
        Self {
            checkpoint_parse: ParseState::new(syntax),
            checkpoint_highlight: HighlightState::new(&highlighter, ScopeStack::new()),
            completed_lines: 0,
            cached_segments: Vec::new(),
        }
    }

    fn set_or_push(&mut self, index: usize, segments: Vec<StyledSegment>) {
        if index < self.cached_segments.len() {
            self.cached_segments[index] = segments;
        } else {
            self.cached_segments.push(segments);
        }
    }

    pub fn update(&mut self, code: &str) -> &[Vec<StyledSegment>] {
        let raw_lines: Vec<&str> = LinesWithEndings::from(code).collect();
        let total = raw_lines.len();
        if total == 0 {
            self.cached_segments.clear();
            self.completed_lines = 0;
            return &[];
        }

        let new_completed = if code.ends_with('\n') {
            total
        } else {
            total - 1
        };

        if new_completed > self.completed_lines {
            let mut hl = Highlighter::from_state(
                theme(),
                self.checkpoint_highlight.clone(),
                self.checkpoint_parse.clone(),
            );

            for raw in &raw_lines[self.completed_lines..new_completed] {
                self.set_or_push(self.completed_lines, hl.highlight_line(raw));
                self.completed_lines += 1;
            }

            let (hs, ps) = hl.state();
            self.checkpoint_parse = ps;
            self.checkpoint_highlight = hs;
        }

        let line_count = new_completed + usize::from(new_completed < total);
        self.cached_segments.truncate(line_count);

        if new_completed < total {
            let mut hl = Highlighter::from_state(
                theme(),
                self.checkpoint_highlight.clone(),
                self.checkpoint_parse.clone(),
            );
            self.set_or_push(new_completed, hl.highlight_line(raw_lines[new_completed]));
        }

        &self.cached_segments
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};
    use std::thread;

    use super::*;
    use test_case::test_case;

    const BUDGETED_ENTRIES: usize = 32;
    const RUST: &str = "rust";

    /// The theme and the block cache are process globals. Nextest gives every
    /// test its own process, `cargo test` does not, so tests that swap the theme
    /// or read the cache take this first and cannot be tripped up by a sibling.
    fn exclusive_globals() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn segments_text(segs: &[StyledSegment]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    fn lines_text(lines: &[Vec<StyledSegment>]) -> Vec<String> {
        lines.iter().map(|l| segments_text(l)).collect()
    }

    #[test]
    fn highlight_code_line_handling() {
        warmup();
        let single = highlight_code("rust", "fn main() {}\n", "");
        assert_eq!(single.len(), 1);
        assert_eq!(segments_text(&single[0]), "fn main() {}");

        let no_newline = highlight_code("rust", "let x = 1;", "");
        assert_eq!(no_newline.len(), 1);
        assert_eq!(segments_text(&no_newline[0]), "let x = 1;");

        let trailing = highlight_code("rust", "let x = 1;\n\n\n", "");
        assert_eq!(trailing.len(), 3);
        assert_eq!(segments_text(&trailing[1]), "");
    }

    #[test]
    fn highlight_lines_independent_ignores_cross_line_state() {
        warmup();
        let context_line = "/* start of block comment\n";
        let target_line = "let x = 42;\n";
        let combined = format!("{context_line}{target_line}");

        let stateful = highlight_code("rust", &combined, "");
        let independent = highlight_lines_independent("rust", &combined);

        assert_eq!(
            stateful.len(),
            independent.len(),
            "both should produce the same number of lines"
        );
        assert_ne!(
            stateful[1], independent[1],
            "inside a block comment the stateful highlighter should parse \
             `let x = 42;` differently than a fresh independent highlighter"
        );
    }

    #[test]
    fn syntax_for_token_fallback() {
        warmup();
        let plain = syntax_set().find_syntax_plain_text();
        assert_eq!(
            syntax_for_token("nonexistent_language_xyz").name,
            plain.name
        );
    }

    #[test]
    fn set_theme_applies_without_panic() {
        let _globals = exclusive_globals();
        warmup();
        for _ in 0..3 {
            set_theme(Theme::default());
        }
        let mut hl = Highlighter::for_token("rust");
        assert!(!hl.highlight_line("let x = 1;\n").is_empty());
    }

    #[test]
    fn code_highlighter_streaming_consistency() {
        warmup();
        let full_code = "fn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n";
        let full = highlight_code("rust", full_code, "");

        let mut ch = CodeHighlighter::new("rust");
        ch.update("fn main() {\n");
        ch.update("fn main() {\n    let x = 42;\n");
        let result = ch.update(full_code);

        assert_eq!(lines_text(&full), lines_text(result));
    }

    #[test]
    fn code_highlighter_partial_line() {
        warmup();
        let mut ch = CodeHighlighter::new("rust");

        ch.update("let x");
        let text1 = segments_text(&ch.update("let x")[0]);

        let text2 = segments_text(&ch.update("let x = 42")[0]);
        assert_ne!(
            text1, text2,
            "partial line should be re-highlighted as content changes"
        );
    }

    #[test]
    fn code_highlighter_shrinks() {
        warmup();
        let mut ch = CodeHighlighter::new("rust");
        ch.update("let a = 1;\nlet b = 2;\nlet c = 3;\n");
        let segs = ch.update("let a = 1;\n");
        assert_eq!(segs.len(), 1);
    }

    #[test]
    fn normalize_text_tabs_and_newlines() {
        assert_eq!(normalize_text("\t\t"), format!("{TAB_SPACES}{TAB_SPACES}"));
        assert_eq!(normalize_text("hello\n"), "hello");
        assert_eq!(normalize_text("a\tb"), format!("a{TAB_SPACES}b"));
        assert_eq!(normalize_text("hello world"), "hello world");
        assert_eq!(normalize_text(""), "");
    }

    #[test_case("test.rs" => "Rust"; "rust_extension")]
    #[test_case("test.py" => "Python"; "python_extension")]
    #[test_case("test.go" => "Go"; "go_extension")]
    #[test_case("Makefile" => "Makefile"; "makefile_no_ext")]
    fn syntax_for_path_resolves(path: &str) -> String {
        warmup();
        syntax_for_path(path).name.to_string()
    }

    #[test]
    fn syntax_for_path_unknown_falls_back() {
        warmup();
        let plain = syntax_set().find_syntax_plain_text();
        assert_eq!(syntax_for_path("file.totally_unknown_xyz").name, plain.name);
    }

    #[test]
    fn highlight_ansi_formatting() {
        warmup();
        let out = highlight_ansi("rust", "let x = 1;\nlet y = 2;\n", (30, 30, 30));
        let bg_count = out.matches("\x1b[48;2;30;30;30m").count();
        assert_eq!(bg_count, 2, "each line should get its own bg escape");
        assert!(out.ends_with("\x1b[K\x1b[0m\n"));
    }

    #[test]
    fn highlighter_advance_and_state_roundtrip() {
        warmup();
        let mut hl = Highlighter::for_token("rust");
        hl.advance("fn main() {\n");
        let (hs, ps) = hl.state();

        let mut from_state = Highlighter::from_state(theme(), hs, ps);
        let seg_from_state = from_state.highlight_line("    let x = 1;\n");

        let mut fresh = Highlighter::for_token("rust");
        fresh.advance("fn main() {\n");
        let seg_fresh = fresh.highlight_line("    let x = 1;\n");

        assert_eq!(seg_from_state, seg_fresh);
    }

    #[test_case(RUST, "fn main() {\n    let x = 1;\n}\n"; "rust_block")]
    #[test_case(RUST, ""; "empty_code")]
    #[test_case("totally_unknown_xyz", "!!! not a language !!!\n"; "unknown_language")]
    fn highlight_block_matches_an_uncached_highlight(lang: &str, code: &str) {
        let _globals = exclusive_globals();
        warmup();
        assert_eq!(*highlight_block(lang, code), highlight_code(lang, code, ""));
    }

    #[test]
    fn highlight_block_memoizes_per_language() {
        let _globals = exclusive_globals();
        warmup();
        const CODE: &str = "x = 1\n";
        let rust = highlight_block(RUST, CODE);
        assert!(
            Arc::ptr_eq(&rust, &highlight_block(RUST, CODE)),
            "the same block must hand back the same allocation"
        );
        assert!(
            !Arc::ptr_eq(&rust, &highlight_block("python", CODE)),
            "the language is part of the key"
        );
    }

    /// Hashing the language and the code into one stream would make
    /// `("rust", "x")` and `("rus", "tx")` the same key, and a false hit paints
    /// a block with another block's colors.
    #[test_case((RUST, "x"), ("rus", "tx"); "language_code_boundary")]
    #[test_case((RUST, "let a = 1;\n"), (RUST, "let b = 2;\n"); "equal_length_bodies")]
    fn distinct_blocks_get_distinct_keys(left: (&str, &str), right: (&str, &str)) {
        assert!(BlockKey::new(left.0, left.1) != BlockKey::new(right.0, right.1));
    }

    /// `set_theme` bumps the generation before clearing, so a highlight that
    /// started earlier and finishes after the clear lands under a key no later
    /// lookup can mint. Bumping after the clear would leave that insert
    /// reachable, serving the old palette until the next theme change.
    #[test]
    fn a_theme_change_orphans_the_blocks_highlighted_before_it() {
        const CODE: &str = "let themed = 1;\n";
        let _globals = exclusive_globals();
        warmup();
        let stale_key = BlockKey::new(RUST, CODE);
        highlight_block(RUST, CODE);

        set_theme(Theme::default());
        assert_eq!(
            block_cache_lock().read().unwrap().bytes,
            0,
            "set_theme must clear the cache"
        );

        let racing = one_line();
        block_cache_lock().write().unwrap().insert(
            stale_key,
            Arc::clone(&racing),
            BLOCK_CACHE_MAX_BYTES,
        );
        assert!(
            !Arc::ptr_eq(&racing, &highlight_block(RUST, CODE)),
            "an insert under a key minted before the bump must be unreachable"
        );
    }

    fn block_of(lines: usize) -> BlockSegments {
        Arc::new(vec![vec![StyledSegment::fallback("x".into())]; lines])
    }

    fn one_line() -> BlockSegments {
        block_of(1)
    }

    fn assert_bytes_match_entries(cache: &BlockCache) {
        assert_eq!(
            cache.bytes,
            cache
                .entries
                .values()
                .map(|(segs, _)| segments_bytes(segs))
                .sum::<usize>(),
            "the byte tally must track the entries it holds"
        );
    }

    /// A budget that fits exactly `BUDGETED_ENTRIES` of [`one_line`].
    fn tiny_budget() -> usize {
        segments_bytes(&one_line()) * BUDGETED_ENTRIES
    }

    /// Replays what a resize does: walk every block in order, over and over.
    /// Returns hits per pass.
    fn tiny_cache_scan(blocks: usize, passes: usize) -> (Vec<usize>, BlockCache) {
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        let mut hits = Vec::new();
        for _ in 0..passes {
            let mut pass_hits = 0;
            for i in 0..blocks {
                let key = BlockKey::new(RUST, &i.to_string());
                if cache.get(key).is_some() {
                    pass_hits += 1;
                } else {
                    cache.insert(key, one_line(), budget);
                }
            }
            hits.push(pass_hits);
        }
        (hits, cache)
    }

    /// Arbitrary eviction is the whole point: an LRU or a generational rotation
    /// would score a flat zero once the walk outgrows the budget. How far above
    /// zero we land is `HashMap` iteration order talking, so only the floor is
    /// ours to assert.
    #[test_case(BUDGETED_ENTRIES, BUDGETED_ENTRIES; "a_scan_that_fits_stays_fully_warm")]
    #[test_case(BUDGETED_ENTRIES * 4, 1; "a_scan_that_overflows_still_hits")]
    fn a_repeated_scan_keeps_hitting_within_budget(blocks: usize, min_hits: usize) {
        const PASSES: usize = 3;
        let (hits, cache) = tiny_cache_scan(blocks, PASSES);
        assert_eq!(hits[0], 0, "nothing is warm on the first pass");
        assert!(
            hits[1..].iter().all(|&pass| pass >= min_hits),
            "a scan of {blocks} blocks must keep serving at least {min_hits}, got {hits:?}"
        );
        assert!(cache.bytes <= tiny_budget(), "{} over budget", cache.bytes);
        assert_bytes_match_entries(&cache);
    }

    /// An entry too big for even an empty cache gets dropped, rather than spun
    /// through the eviction loop until the cache is empty and it still does not
    /// fit.
    #[test]
    fn block_cache_drops_an_entry_bigger_than_the_budget() {
        const OVERSIZED_LINES: usize = BUDGETED_ENTRIES + 1;
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        let resident = BlockKey::new(RUST, "resident");
        cache.insert(resident, one_line(), budget);
        let bytes_before = cache.bytes;

        cache.insert(
            BlockKey::new(RUST, "oversized"),
            block_of(OVERSIZED_LINES),
            budget,
        );

        assert!(
            cache.get(BlockKey::new(RUST, "oversized")).is_none(),
            "an entry over the whole budget must not be cached"
        );
        assert!(
            cache.get(resident).is_some(),
            "a rejected insert must not evict what already fits"
        );
        assert_eq!(cache.bytes, bytes_before);
    }

    /// Without the `remove` before the insert, the replaced value's bytes stay
    /// on the tally forever. A block streamed at growing lengths would then
    /// drift up to the budget and evict everything on every insert while
    /// holding almost nothing.
    #[test]
    fn reinserting_a_key_does_not_double_count_its_bytes() {
        const GROWN_LINES: usize = 4;
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        let key = BlockKey::new(RUST, "same");

        cache.insert(key, one_line(), budget);
        cache.insert(key, block_of(GROWN_LINES), budget);
        cache.insert(key, one_line(), budget);

        assert_eq!(cache.entries.len(), 1);
        assert_bytes_match_entries(&cache);
    }

    /// The eviction loop picks an arbitrary key, so inserting first would let it
    /// pick the incoming one and hand the caller a value the cache never holds.
    #[test]
    fn eviction_spares_the_entry_being_inserted() {
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        for i in 0..BUDGETED_ENTRIES {
            cache.insert(BlockKey::new(RUST, &i.to_string()), one_line(), budget);
        }
        assert_eq!(cache.bytes, budget, "the cache must start out exactly full");

        let incoming = BlockKey::new(RUST, "incoming");
        cache.insert(incoming, block_of(BUDGETED_ENTRIES), budget);

        assert!(
            cache.get(incoming).is_some(),
            "the entry that forced the eviction must survive it"
        );
        assert_eq!(cache.entries.len(), 1, "it takes the budget on its own");
        assert_bytes_match_entries(&cache);
    }

    /// `highlight_block` drops the read lock before taking the write lock, so a
    /// theme change can slip in between, and whatever comes back must still be
    /// an honest highlight of its own input. The theme swapped in here is the
    /// one already installed, so `highlight_code` stays a valid expectation
    /// while the generation churns underneath.
    #[test]
    fn concurrent_blocks_survive_a_theme_change() {
        const THREADS: usize = 4;
        const ITERATIONS: usize = 8;
        let _globals = exclusive_globals();
        warmup();

        thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    for i in 0..ITERATIONS {
                        let code = format!("let v{i} = {i};\n");
                        assert_eq!(
                            *highlight_block(RUST, &code),
                            highlight_code(RUST, &code, "")
                        );
                    }
                });
            }
            scope.spawn(|| {
                for _ in 0..THREADS * ITERATIONS {
                    set_theme(Theme::default());
                }
            });
        });

        assert_bytes_match_entries(&block_cache_lock().read().unwrap());
    }

    #[test_case("jsx", "js"; "jsx_alias")]
    fn token_alias_resolves(alias: &str, canonical: &str) {
        warmup();
        let aliased = syntax_for_token(alias);
        let canonical_syntax = syntax_set().find_syntax_by_token(canonical).unwrap();
        assert_eq!(aliased.name, canonical_syntax.name);
    }
}
