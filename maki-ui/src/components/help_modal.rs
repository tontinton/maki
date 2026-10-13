use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::keybindings::{
    ALT_SEP, KEYBINDS, KeybindContext, ResolvedLabel, all_contexts, key,
};
use crate::components::modal::{BORDER, Modal};
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_lua::{BuiltinAction, InputMode, KeymapEntry, KeymapReader};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use strum::VariantNames;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = " Keybindings ";
const KEY_COL_GAP: usize = 2;
const PREFIX_TOP: &str = "  ";
const PREFIX_CHILD: &str = "    ";
const PLUGIN_KEYS_TITLE: &str = "  Plugin Keys";
const ACTIONS_TITLE: &str = "  Actions";
const BUILTIN_OWNER: &str = "built-in";
const NO_DESC: &str = "-";
const BOTH_MODES: &str = "n i";
const MODES_WIDTH: usize = BOTH_MODES.len();

const INPUT_PREFIXES: &[(&str, &str)] = &[
    ("!", "Run shell command (visible to agent)"),
    ("!!", "Run shell command (hidden from agent)"),
];

pub struct HelpModal {
    open: bool,
    scroll: ModalScroll,
}

fn key_spans(label: ResolvedLabel, pad: usize, prefix: &str) -> Vec<Span<'static>> {
    let theme = theme::current();
    match label {
        ResolvedLabel::Single(s) => {
            let w = UnicodeWidthStr::width(s);
            let trailing = pad.saturating_sub(w);
            vec![Span::styled(
                format!("{prefix}{s}{:trailing$}", ""),
                theme.keybind_key,
            )]
        }
        ResolvedLabel::Alt(a, b) => multi_key_spans(&[a, b], pad, prefix, &theme),
        ResolvedLabel::Multi(keys) => multi_key_spans(keys, pad, prefix, &theme),
    }
}

fn multi_key_spans(
    keys: &[&'static str],
    pad: usize,
    prefix: &str,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let sep_w = UnicodeWidthStr::width(ALT_SEP);
    let content_w: usize = keys
        .iter()
        .map(|k| UnicodeWidthStr::width(*k))
        .sum::<usize>()
        + sep_w * keys.len().saturating_sub(1);
    let trailing = pad.saturating_sub(content_w);
    let mut spans = Vec::with_capacity(keys.len() * 2);
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(ALT_SEP, theme.keybind_desc));
        }
        let text = if i == 0 && i == keys.len() - 1 {
            format!("{prefix}{k}{:trailing$}", "")
        } else if i == 0 {
            format!("{prefix}{k}")
        } else if i == keys.len() - 1 {
            format!("{k}{:trailing$}", "")
        } else {
            (*k).to_string()
        };
        spans.push(Span::styled(text, theme.keybind_key));
    }
    spans
}

/// A key a plugin holds, in the modes where its binding is the one that fires.
struct PluginKey<'a> {
    entry: &'a KeymapEntry,
    normal: bool,
    insert: bool,
}

impl PluginKey<'_> {
    fn modes(&self) -> &'static str {
        match (self.normal, self.insert) {
            (true, true) => BOTH_MODES,
            (true, false) => InputMode::Normal.letter(),
            _ => InputMode::Insert.letter(),
        }
    }
}

/// The global bindings that win their key in each mode, newest first like
/// dispatch, so a key two plugins bind shows only the one that fires. One
/// binding set in several modes is one row.
fn plugin_keys(entries: &[KeymapEntry]) -> Vec<PluginKey<'_>> {
    let mut keys: Vec<PluginKey> = Vec::new();
    for entry in entries.iter().filter(|e| e.is_global()) {
        let normal = matches!(entry.mode, InputMode::Normal);
        let taken = |k: &PluginKey| if normal { k.normal } else { k.insert };
        if keys.iter().any(|k| k.entry.key == entry.key && taken(k)) {
            continue;
        }
        let same_binding = |k: &PluginKey| {
            k.entry.key == entry.key && k.entry.plugin == entry.plugin && k.entry.desc == entry.desc
        };
        let at = keys.iter().position(same_binding).unwrap_or_else(|| {
            keys.push(PluginKey {
                entry,
                normal: false,
                insert: false,
            });
            keys.len() - 1
        });
        let row = &mut keys[at];
        row.normal |= normal;
        row.insert |= !normal;
    }
    keys
}

/// Who answers each key plugins took, and every action `maki.ui.action` can
/// run, with its owner. Built from the published snapshots on every frame the
/// modal is open, which is a walk over a few dozen bindings.
fn plugin_lines(keymaps: &KeymapReader) -> Vec<Line<'static>> {
    let snapshot = keymaps.load();
    let keys: Vec<(String, String)> = plugin_keys(&snapshot.entries)
        .into_iter()
        .map(|key| {
            let desc = Some(key.entry.desc.as_str())
                .filter(|desc| !desc.is_empty())
                .unwrap_or(NO_DESC);
            let owner = &key.entry.plugin;
            (
                key.entry.key.notation(),
                format!("{:<MODES_WIDTH$} {desc} ({owner})", key.modes()),
            )
        })
        .collect();
    let actions = keymaps.actions();
    let builtins = BuiltinAction::VARIANTS
        .iter()
        .map(|name| (*name, BUILTIN_OWNER));
    let plugins = actions.iter().map(|a| (a.name.as_ref(), a.plugin.as_ref()));
    let actions: Vec<(String, String)> = builtins
        .chain(plugins)
        .map(|(name, owner)| (name.to_owned(), owner.to_owned()))
        .collect();

    let pad = keys
        .iter()
        .chain(&actions)
        .map(|(name, _)| UnicodeWidthStr::width(name.as_str()))
        .max()
        .unwrap_or(0)
        + KEY_COL_GAP;
    let theme = theme::current();
    let mut lines = Vec::new();
    for (title, rows) in [(PLUGIN_KEYS_TITLE, keys), (ACTIONS_TITLE, actions)] {
        if rows.is_empty() {
            continue;
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(title, theme.keybind_section)));
        for (name, rest) in rows {
            let trailing = pad - UnicodeWidthStr::width(name.as_str());
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{PREFIX_CHILD}{name}{:trailing$}", ""),
                    theme.keybind_key,
                ),
                Span::styled(rest, theme.keybind_desc),
            ]));
        }
    }
    lines
}

impl HelpModal {
    pub fn new() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.scroll.reset();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        let close = key_event.code == KeyCode::Esc
            || key::HELP.matches(key_event)
            || key::QUIT.matches(key_event);
        if close {
            self.close();
            return true;
        }
        self.scroll.handle_key(key_event);
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, keymaps: &KeymapReader) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let mut lines: Vec<Line> = Vec::new();
        let theme = theme::current();

        let key_col_width = KEYBINDS
            .iter()
            .filter(|kb| kb.platform.is_visible())
            .map(|kb| kb.label.resolve().display_width())
            .max()
            .unwrap_or(0)
            + KEY_COL_GAP;

        let mut first = true;
        for ctx in all_contexts() {
            if ctx.parent().is_some() {
                continue;
            }
            if !first {
                lines.push(Line::default());
            }
            first = false;

            lines.push(Line::from(Span::styled(
                format!("  {}", ctx.label()),
                theme.keybind_section,
            )));

            for kb in KEYBINDS
                .iter()
                .filter(|kb| kb.context == ctx && kb.platform.is_visible())
            {
                let mut spans = key_spans(kb.label.resolve(), key_col_width, PREFIX_TOP);
                spans.push(Span::styled(kb.description, theme.keybind_desc));
                lines.push(Line::from(spans));
            }

            for child in all_contexts() {
                if child.parent() != Some(ctx) {
                    continue;
                }
                let child_binds: Vec<_> = KEYBINDS
                    .iter()
                    .filter(|kb| kb.context == child && kb.platform.is_visible())
                    .collect();
                if child_binds.is_empty() {
                    continue;
                }
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    format!("    {}", child.label()),
                    theme.keybind_section,
                )));
                for kb in child_binds {
                    let mut spans = key_spans(
                        kb.label.resolve(),
                        key_col_width - KEY_COL_GAP,
                        PREFIX_CHILD,
                    );
                    spans.push(Span::styled(kb.description, theme.keybind_desc));
                    lines.push(Line::from(spans));
                }
            }

            if ctx == KeybindContext::Editing {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    "    Input Prefixes",
                    theme.keybind_section,
                )));
                for &(pfx, desc) in INPUT_PREFIXES {
                    let mut spans = key_spans(
                        ResolvedLabel::Single(pfx),
                        key_col_width - KEY_COL_GAP,
                        PREFIX_CHILD,
                    );
                    spans.push(Span::styled(desc, theme.keybind_desc));
                    lines.push(Line::from(spans));
                }
            }
        }

        lines.extend(plugin_lines(keymaps));

        let total = lines.len() as u16;
        let content_cols = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: 50,
            min_width: content_cols + BORDER,
            max_height_percent: 80,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let viewport_h = inner.height;
        self.scroll.update_dimensions(total, viewport_h);
        let scroll = self.scroll.offset();

        let paragraph = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(paragraph, inner);

        if total > viewport_h {
            render_vertical_scrollbar(frame, inner, u32::from(total), u32::from(scroll));
        }

        popup
    }
}

impl Overlay for HelpModal {
    fn is_prompting(&self) -> bool {
        self.is_open()
    }

    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_ev;
    use crossterm::event::KeyCode;
    use maki_agent::tools::ToolRegistry;
    use maki_lua::PluginHost;
    use std::sync::Arc;
    use test_case::test_case;

    #[test_case(key_ev(KeyCode::Esc)       ; "esc_closes")]
    #[test_case(key::QUIT.to_key_event()    ; "ctrl_c_closes")]
    #[test_case(key::HELP.to_key_event()    ; "ctrl_h_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let mut modal = HelpModal::new();
        modal.toggle();
        assert!(modal.handle_key(k));
        assert!(!modal.is_open());
    }

    #[test]
    fn handle_key_consumes_all() {
        let mut modal = HelpModal::new();
        modal.toggle();
        assert!(modal.handle_key(key_ev(KeyCode::Char('a'))));
        assert!(modal.is_open());
    }

    const HELPER_PLUGIN: &str = r#"
maki.api.register_action("ping", function() end)
maki.keymap.set({ "n", "i" }, "<C-t>", function() end, { desc = "Ping" })
"#;
    const LATER_PLUGIN: &str = r#"
maki.keymap.set("i", "<C-t>", function() end, { desc = "Pong" })
"#;

    /// The later plugin takes `<C-t>` in `"i"` only, so the first one still
    /// answers it in `"n"`.
    #[test]
    fn plugin_lines_name_who_answers_each_key_and_every_action() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source("helper", HELPER_PLUGIN).unwrap();
        host.load_source("later", LATER_PLUGIN).unwrap();

        let text: Vec<String> = plugin_lines(&host.keymap_reader())
            .iter()
            .map(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .collect();

        for row in [
            "<C-t> i Pong (later)",
            "<C-t> n Ping (helper)",
            "undo built-in",
            "helper.ping helper",
        ] {
            assert!(text.iter().any(|line| line == row), "{row} in {text:?}");
        }
    }
}
