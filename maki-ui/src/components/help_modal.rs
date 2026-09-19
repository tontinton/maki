use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::keybindings::{
    ALT_SEP, CANCEL_AGENT_DESC, ESC_ESC_LABEL, KEYBINDS, KeybindContext, ResolvedLabel,
    all_contexts, key,
};
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_config::CancelKey;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = " Keybindings ";
const KEY_COL_GAP: usize = 2;
const PREFIX_TOP: &str = "  ";
const PREFIX_CHILD: &str = "    ";

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

    pub fn view(&mut self, frame: &mut Frame, area: Rect, cancel_key: CancelKey) -> Rect {
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
                let label = if ctx == KeybindContext::Streaming
                    && kb.description == CANCEL_AGENT_DESC
                    && cancel_key == CancelKey::Esc
                {
                    ResolvedLabel::Single(ESC_ESC_LABEL)
                } else {
                    kb.label.resolve()
                };
                let mut spans = key_spans(label, key_col_width, PREFIX_TOP);
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

        let total = lines.len() as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: 50,
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
    use ratatui::layout::Rect;
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

    #[test_case(CancelKey::CtrlC, "Ctrl+C" ; "default_ctrl_c")]
    #[test_case(CancelKey::Esc, "Esc Esc" ; "esc_mode")]
    fn view_shows_the_configured_cancel_key(cancel_key: CancelKey, label: &str) {
        let mut modal = HelpModal::new();
        modal.toggle();
        // Wide enough for the longest multi-key label plus the description.
        let area = Rect::new(0, 0, 140, 60);
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, area, cancel_key);
            })
            .unwrap();

        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .chunks(area.width as usize)
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect())
            .collect();
        let cancel_row = rows
            .iter()
            .find(|row| row.contains(CANCEL_AGENT_DESC))
            .cloned()
            .unwrap_or_default();
        assert!(
            cancel_row.contains(label),
            "cancel row must show {label}, got: {cancel_row}"
        );
    }
}
