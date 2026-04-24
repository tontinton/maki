use crate::theme;

use maki_highlight::StyledSegment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub use maki_highlight::TAB_SPACES;

pub(crate) fn warmup() {
    refresh_syntax_theme();
    maki_highlight::warmup();
}

pub(crate) fn is_ready() -> bool {
    maki_highlight::is_ready()
}

pub(crate) fn refresh_syntax_theme() {
    let theme = theme::current();
    maki_highlight::set_theme(theme.syntax.clone());
}

pub fn highlight_line(hl: &mut maki_highlight::Highlighter, text: &str) -> Vec<Span<'static>> {
    hl.highlight_line(text)
        .into_iter()
        .map(|seg| {
            let style = convert_segment(&seg);
            Span::styled(seg.text, style)
        })
        .collect()
}

pub fn highlight_code_plain(lang: &str, code: &str) -> Vec<Line<'static>> {
    maki_highlight::highlight_code(lang, code)
        .into_iter()
        .map(|segs| segments_to_line(&segs))
        .collect()
}

pub struct CodeHighlighter {
    inner: maki_highlight::CodeHighlighter,
    lines: Vec<Line<'static>>,
    completed: usize,
}

impl CodeHighlighter {
    pub fn new(lang: &str) -> Self {
        Self {
            inner: maki_highlight::CodeHighlighter::new(lang),
            lines: Vec::new(),
            completed: 0,
        }
    }

    pub fn update(&mut self, code: &str) -> &[Line<'static>] {
        let segments = self.inner.update(code);
        let n = segments.len();
        self.lines.truncate(n);
        let stable = n.saturating_sub(1).min(self.completed);
        for (i, segs) in segments[stable..].iter().enumerate() {
            let idx = stable + i;
            let line = segments_to_line(segs);
            if idx < self.lines.len() {
                self.lines[idx] = line;
            } else {
                self.lines.push(line);
            }
        }
        self.completed = n.saturating_sub(1);
        &self.lines
    }
}

fn segments_to_line(segs: &[StyledSegment]) -> Line<'static> {
    Line::from(
        segs.iter()
            .map(|s| Span::styled(s.text.clone(), convert_segment(s)))
            .collect::<Vec<_>>(),
    )
}

pub fn highlight_regex_inline(pattern: &str) -> Vec<Span<'static>> {
    let Some(syntax) = maki_highlight::syntax_set().find_syntax_by_token("re") else {
        return vec![fallback_span(pattern)];
    };
    let mut hl = maki_highlight::Highlighter::for_syntax(syntax);
    highlight_line(&mut hl, pattern)
}

pub fn fallback_span(text: &str) -> Span<'static> {
    Span::styled(
        maki_highlight::normalize_text(text),
        theme::current().code_fallback,
    )
}

pub fn highlight_ansi(lang: &str, code: &str) -> String {
    let theme = theme::current();
    let bg = match theme.background {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    };
    maki_highlight::highlight_ansi(lang, code, bg)
}

fn convert_segment(seg: &StyledSegment) -> Style {
    let mut style = Style::new().fg(Color::Rgb(seg.fg.0, seg.fg.1, seg.fg.2));
    if seg.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if seg.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if seg.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_segment_modifiers() {
        let all_mods = StyledSegment {
            text: "x".into(),
            fg: (255, 0, 128),
            bold: true,
            italic: true,
            underline: true,
        };
        let style = convert_segment(&all_mods);
        assert_eq!(style.fg, Some(Color::Rgb(255, 0, 128)));
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));

        let no_mods = StyledSegment {
            text: "plain".into(),
            fg: (100, 100, 100),
            bold: false,
            italic: false,
            underline: false,
        };
        let style = convert_segment(&no_mods);
        assert_eq!(style.fg, Some(Color::Rgb(100, 100, 100)));
        assert!(style.add_modifier.is_empty());
    }

    #[test]
    fn fallback_span_normalizes() {
        let span = fallback_span("\thello\n");
        let expected = format!("{TAB_SPACES}hello");
        assert_eq!(span.content.as_ref(), expected);
    }

    #[test]
    fn highlight_regex_inline_roundtrips_text() {
        let pattern = "[a-z]+\\d{2,}";
        let spans = highlight_regex_inline(pattern);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, pattern);
    }
}
