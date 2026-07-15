use crate::theme;

use maki_highlight::StyledSegment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

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
    maki_highlight::set_ui_colors(
        [
            ("diff_old", theme.diff_old.bg),
            ("diff_new", theme.diff_new.bg),
        ]
        .into_iter()
        .filter_map(|(name, color)| match color {
            Some(Color::Rgb(r, g, b)) => Some((name.to_owned(), (r, g, b))),
            _ => None,
        })
        .collect(),
    );
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

pub fn fallback_span(text: &str) -> Span<'static> {
    Span::styled(
        maki_highlight::normalize_text(text),
        theme::current().code_block,
    )
}

pub fn highlight_ansi(lang: &str, code: &str) -> String {
    let theme = theme::current();
    maki_highlight::set_theme(theme.syntax.clone());
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
}
