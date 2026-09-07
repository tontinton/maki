//! Mirrors a plugin's `open_win()` float (task/session/memory pickers, the
//! `question` tool's form, ...) out to the remote web UI as a generic
//! overlay, and routes the browser's key/paste input back into the same
//! `WinHandle` channel a local keypress already uses.
//!
//! A window's chrome (title, footer hints, border) and its content
//! (`SharedBuf` lines) are two separate things in the local renderer; this
//! module flattens both into one JSON snapshot per window; see
//! [`crate::components::lua_float::WindowRemoteParts`].

use ratatui::style::{Color, Modifier};
use serde_json::{Value, json};

use super::App;
use super::remote_fs::html_escape_into;
use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::tool_display::{
    SPINNER_STYLE_NAME, SPINNER_STYLE_PREFIX, resolve_span_style,
};
use crate::theme;
use maki_agent::{SnapshotLine, SnapshotSpan, SpanStyle};

impl App {
    /// Ids of windows touched since the last drain — content, chrome, or
    /// closed — for the event loop to mirror out after every tick. See
    /// [`crate::components::lua_float::FloatManager::take_tick_changes`].
    pub(crate) fn take_window_changes(&mut self) -> (Vec<u32>, Vec<u32>) {
        self.float_mgr.take_tick_changes()
    }

    /// The full snapshot for `window_open`/`window_update` SSE frames, or
    /// `None` if the window has already closed. `focus` tells the browser
    /// whether to actually show this as its modal overlay — a background
    /// panel or toast (`todo_write`, the memory toast) is tracked but never
    /// rendered, since nothing would be able to dismiss a modal that never
    /// took local focus either.
    pub(crate) fn remote_window_snapshot(&self, id: u32) -> Option<Value> {
        let parts = self.float_mgr.window_remote_parts(id)?;
        Some(json!({
            "id": id,
            "title": parts.title,
            "footer": parts.footer,
            "html": window_lines_to_html(&parts.lines, parts.cursor),
            "visible": parts.visible,
            "focus": parts.focused,
        }))
    }

    /// The focused window's snapshot, for `remote_snapshot()` to include so
    /// a reconnecting tab (a phone screen waking up, a hard refresh) shows
    /// an already-open window again instead of nothing — without this, a
    /// blocking one like the `question` tool's form (which has no redraw
    /// loop of its own to self-heal on the next content change) would hang
    /// the agent with no way for the reconnected browser to ever see it.
    pub(crate) fn remote_focused_window_snapshot(&self) -> Option<Value> {
        let id = self.float_mgr.focused_window_id()?;
        self.remote_window_snapshot(id)
    }

    /// Forwards a browser key or paste event to the focused window, exactly
    /// as a local keypress/paste would — `key` must already be in
    /// [`crate::components::keybindings::key_event_to_string`]'s format
    /// (e.g. `"enter"`, `"ctrl+c"`, `"a"`). Errors when there is no focused
    /// window to receive it, so the caller can tell the browser its input
    /// landed nowhere instead of silently dropping it.
    pub(crate) fn send_remote_window_input(
        &self,
        key: Option<&str>,
        paste: Option<&str>,
    ) -> Result<(), String> {
        if let Some(key) = key
            && self.float_mgr.forward_key_str(key)
        {
            return Ok(());
        }
        if let Some(text) = paste
            && self.float_mgr.handle_paste(text)
        {
            return Ok(());
        }
        Err("no focused window".to_owned())
    }
}

/// One `<div>` per content line so the cursor row can be targeted with a
/// class; a line's own spans carry their resolved color/weight inline,
/// mirroring the file panel's `highlight_html` (`remote_fs.rs`).
///
/// The cursor row is always marked, regardless of `cursor_line` (the local
/// terminal's own opt-in row highlight): the browser needs a stable element
/// to scroll to on every update, or arrow-key navigation runs the selection
/// off-screen with no way to see where it landed. Most pickers already bake
/// their own selection color into the row's spans (see `picker.lua`), so
/// the marker class carries a scroll target rather than a visible
/// highlight, which would otherwise double up against the plugin's own.
fn window_lines_to_html(lines: &[SnapshotLine], cursor: usize) -> String {
    let mut html = String::with_capacity(lines.len() * 24);
    for (i, line) in lines.iter().enumerate() {
        if i == cursor {
            html.push_str("<div class=\"win-cursor\">");
        } else {
            html.push_str("<div>");
        }
        if line.spans.is_empty() {
            html.push_str("&nbsp;");
        }
        for span in &line.spans {
            push_span_html(&mut html, span);
        }
        html.push_str("</div>");
    }
    html
}

fn push_span_html(html: &mut String, span: &SnapshotSpan) {
    // A spinner placeholder is meaningless to resolve_span_style (its name
    // is a convention `bake_spans`/`snapshot_to_line` consume, not a real
    // theme key) — mirrors lua_float.rs's own snapshot_to_line exactly, so
    // a running row gets the same glyph and color the terminal would show,
    // just not live-animated between pushes the way the terminal repaints.
    if let SpanStyle::Named(name) = &span.style
        && (name == SPINNER_STYLE_NAME || name.starts_with(SPINNER_STYLE_PREFIX))
    {
        let resolved =
            theme::style_by_name(name.strip_prefix(SPINNER_STYLE_PREFIX).unwrap_or(name));
        return push_styled_text(html, spinner_str(animation_elapsed_ms()), resolved);
    }
    push_styled_text(html, &span.text, resolve_span_style(&span.style));
}

fn push_styled_text(html: &mut String, text: &str, style: ratatui::style::Style) {
    let mut css = String::new();
    if let Some(Color::Rgb(r, g, b)) = style.fg {
        css.push_str(&format!("color:#{r:02x}{g:02x}{b:02x}"));
    }
    if let Some(Color::Rgb(r, g, b)) = style.bg {
        if !css.is_empty() {
            css.push(';');
        }
        css.push_str(&format!("background:#{r:02x}{g:02x}{b:02x}"));
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        if !css.is_empty() {
            css.push(';');
        }
        css.push_str("font-weight:bold");
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        if !css.is_empty() {
            css.push(';');
        }
        css.push_str("font-style:italic");
    }
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        if !css.is_empty() {
            css.push(';');
        }
        css.push_str("text-decoration:underline");
    }
    if css.is_empty() {
        html_escape_into(html, text);
    } else {
        html.push_str("<span style=\"");
        html.push_str(&css);
        html.push_str("\">");
        html_escape_into(html, text);
        html.push_str("</span>");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> SnapshotLine {
        SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.to_owned(),
                style: SpanStyle::Default,
            }],
        }
    }

    #[test]
    fn window_lines_to_html_wraps_each_line_and_marks_the_cursor_row() {
        let lines = vec![plain("a"), plain("b"), plain("c")];
        let html = window_lines_to_html(&lines, 1);
        // Rows: plain, cursor, plain — content itself still carries the
        // resolved theme color (SpanStyle::Default is not "no style").
        assert!(html.starts_with("<div>"), "html: {html}");
        assert!(html.contains("<div class=\"win-cursor\">"), "html: {html}");
        assert!(html.contains(">a<"), "html: {html}");
        assert!(html.contains(">b<"), "html: {html}");
        assert!(html.contains(">c<"), "html: {html}");
    }

    #[test]
    fn window_lines_to_html_marks_the_cursor_row_even_without_cursor_line() {
        // Unlike the local terminal's opt-in row highlight, the remote
        // bridge always marks the cursor row: the browser needs a stable
        // scroll target regardless of whether the plugin also asked for a
        // visible highlight (most pickers bake their own selection color
        // into spans instead of setting cursor_line at all).
        let lines = vec![plain("a"), plain("b")];
        let html = window_lines_to_html(&lines, 1);
        assert!(html.contains("win-cursor"), "html: {html}");
    }

    #[test]
    fn window_lines_to_html_escapes_span_text() {
        let lines = vec![plain("<b>&\"'")];
        let html = window_lines_to_html(&lines, 0);
        assert!(html.contains("&lt;b&gt;&amp;&quot;&#39;"), "html: {html}");
    }

    #[test]
    fn window_lines_to_html_keeps_empty_lines_visible() {
        let lines = vec![SnapshotLine { spans: vec![] }];
        // cursor is out of range so this line's own markup, not the
        // cursor-row wrapper, is what's under test here.
        let html = window_lines_to_html(&lines, 99);
        assert_eq!(html, "<div>&nbsp;</div>");
    }

    fn spinner(name: &str) -> SnapshotLine {
        SnapshotLine {
            spans: vec![SnapshotSpan {
                text: "· ".to_owned(),
                style: SpanStyle::Named(name.to_owned()),
            }],
        }
    }

    #[test]
    fn spinner_span_gets_a_glyph_and_does_not_fall_back_to_unstyled() {
        // A raw "spinner:selected" is not a real theme key (style_by_name
        // would return Style::default() for it, silently dropping the
        // row's color) — push_span_html must strip the prefix first, the
        // same convention lua_float.rs's own snapshot_to_line follows.
        let lines = vec![spinner("spinner:selected")];
        let html = window_lines_to_html(&lines, 99);
        assert!(!html.contains("· "), "placeholder must be replaced: {html}");
        assert!(html.contains("<span style=\"color:"), "html: {html}");
    }

    #[test]
    fn bare_spinner_name_also_resolves() {
        let lines = vec![spinner("spinner")];
        let html = window_lines_to_html(&lines, 99);
        assert!(!html.contains("· "), "placeholder must be replaced: {html}");
    }
}
