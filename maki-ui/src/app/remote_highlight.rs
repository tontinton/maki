//! Syntax highlighting for a fenced code block from a rendered markdown
//! message or file preview — the same syntect-backed highlighter the TUI
//! and the file panel's own file viewer use ([`super::remote_fs`]), just
//! keyed by a fence's language tag instead of a file path, and with no cwd
//! or session state to jail: a chat message is not a file on disk.

use super::remote_fs::push_html_escaped_segment;

/// `lang` is whatever followed the opening ` ``` ` fence (`"rust"`, `"js"`,
/// possibly empty) — an unrecognized or blank token falls back to
/// unhighlighted plain text via the same lookup `Highlighter::for_path`
/// uses for an unknown extension, so this never fails, only degrades.
pub(crate) fn highlight_code_html(lang: &str, code: &str) -> String {
    let mut hl = maki_highlight::Highlighter::for_token(lang);
    let mut html = String::with_capacity(code.len());
    for line in code.split_inclusive('\n') {
        let had_newline = line.ends_with('\n');
        for seg in hl.highlight_line(line) {
            push_html_escaped_segment(&mut html, &seg);
        }
        if had_newline {
            html.push('\n');
        }
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_a_known_language_with_styled_spans() {
        let html = highlight_code_html("rust", "fn main() {}\n");
        assert!(
            html.contains("<span style=\"color:#"),
            "known syntax should carry color: {html}"
        );
    }

    #[test]
    fn falls_back_to_plain_text_for_an_unknown_language() {
        let html = highlight_code_html("not-a-real-language", "hello world\n");
        assert!(html.contains("hello world"), "content preserved: {html}");
    }

    #[test]
    fn escapes_html_in_the_code() {
        let html = highlight_code_html("", "<script>&\n");
        assert!(html.contains("&lt;script&gt;&amp;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn preserves_multiple_lines() {
        let html = highlight_code_html("", "a\nb\nc");
        let plain: String = html
            .split('\n')
            .map(|line| {
                // Strip any span markup this line might carry, to compare
                // just the text content across lines.
                let mut out = String::new();
                let mut in_tag = false;
                for c in line.chars() {
                    match c {
                        '<' => in_tag = true,
                        '>' => in_tag = false,
                        c if !in_tag => out.push(c),
                        _ => {}
                    }
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(plain, "a\nb\nc");
    }
}
