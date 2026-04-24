use std::fmt::Write;
use std::sync::{Arc, OnceLock, RwLock};

use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as SynHighlighter, Style as SynStyle,
    Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const TOKEN_ALIASES: &[(&str, &str)] = &[("jsx", "js")];
pub const TAB_SPACES: &str = "  ";

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<RwLock<Arc<Theme>>> = OnceLock::new();

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
}

pub fn theme() -> Arc<Theme> {
    theme_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
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

pub fn highlight_code(lang: &str, code: &str) -> Vec<Vec<StyledSegment>> {
    let mut hl = Highlighter::for_token(lang);
    LinesWithEndings::from(code)
        .map(|raw| hl.highlight_line(raw))
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
    use super::*;
    use test_case::test_case;

    fn segments_text(segs: &[StyledSegment]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    fn lines_text(lines: &[Vec<StyledSegment>]) -> Vec<String> {
        lines.iter().map(|l| segments_text(l)).collect()
    }

    #[test]
    fn highlight_code_line_handling() {
        warmup();
        let single = highlight_code("rust", "fn main() {}\n");
        assert_eq!(single.len(), 1);
        assert_eq!(segments_text(&single[0]), "fn main() {}");

        let no_newline = highlight_code("rust", "let x = 1;");
        assert_eq!(no_newline.len(), 1);
        assert_eq!(segments_text(&no_newline[0]), "let x = 1;");

        let trailing = highlight_code("rust", "let x = 1;\n\n\n");
        assert_eq!(trailing.len(), 3);
        assert_eq!(segments_text(&trailing[1]), "");
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
        let full = highlight_code("rust", full_code);

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

    #[test_case("jsx", "js"; "jsx_alias")]
    fn token_alias_resolves(alias: &str, canonical: &str) {
        warmup();
        let aliased = syntax_for_token(alias);
        let canonical_syntax = syntax_set().find_syntax_by_token(canonical).unwrap();
        assert_eq!(aliased.name, canonical_syntax.name);
    }
}
