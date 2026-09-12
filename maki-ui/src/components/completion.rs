//! Inline completion popup for the prompt input, fed by Lua completers
//! registered via `maki.api.register_input_completer`.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use maki_lua::{CompleterReader, CompletionItem, EventHandle};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::components::input::is_word_adjacent;
use crate::repaint::Dirty;
use crate::text_buffer::TextBuffer;
use crate::theme;

const MAX_ROWS: u16 = 10;
const NO_MATCHES: &str = "no matches";
const SEARCHING: &str = "searching\u{2026}";

pub(crate) enum CompletionAction {
    Consumed,
    Accept(Replacement),
    Passthrough,
}

/// Char range of the trigger + query in the input buffer, and the text
/// the selected item wants in its place.
pub(crate) struct Replacement {
    pub y: usize,
    pub start_x: usize,
    pub end_x: usize,
    pub insert: String,
}

/// Position of the trigger char in the buffer. A completion stays bound
/// to the trigger it opened on; edits elsewhere close it.
#[derive(Clone, PartialEq, Eq)]
struct Anchor {
    y: usize,
    x: usize,
    trigger: char,
}

struct Active {
    anchor: Anchor,
    plugin: Arc<str>,
    name: Arc<str>,
    query: String,
    items: Vec<CompletionItem>,
    selected: usize,
    pending: Option<flume::Receiver<Option<Vec<CompletionItem>>>>,
}

pub(crate) struct CompletionPopup {
    reader: CompleterReader,
    handle: EventHandle,
    active: Option<Active>,
    /// Esc closes the popup for this anchor until the trigger context
    /// changes, so the literal text can stay.
    dismissed: Option<Anchor>,
}

impl CompletionPopup {
    pub fn new(reader: CompleterReader, handle: EventHandle) -> Self {
        Self {
            reader,
            handle,
            active: None,
            dismissed: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    pub fn close(&mut self) {
        self.active = None;
        self.dismissed = None;
    }

    /// Re-derive the completion state from the buffer. Called after every
    /// input edit or cursor move; opens, re-queries, or closes as needed.
    pub fn sync(&mut self, buffer: &TextBuffer) {
        let Some((anchor, query)) = self.trigger_context(buffer) else {
            self.active = None;
            self.dismissed = None;
            return;
        };
        if self.dismissed.as_ref() == Some(&anchor) {
            self.active = None;
            return;
        }
        self.dismissed = None;
        let snapshot = self.reader.load_full();
        let Some(info) = snapshot
            .completers
            .iter()
            .find(|c| c.trigger == anchor.trigger)
        else {
            self.active = None;
            return;
        };
        if let Some(active) = &mut self.active
            && active.anchor == anchor
            && active.plugin == info.plugin
            && active.name == info.name
        {
            if active.query != query {
                // Replacing the receiver drops the previous one, so a
                // late reply for the outgoing query lands on a closed
                // channel and never reaches `tick`. Items are cleared here
                // so Tab/Enter cannot pick a stale entry belonging to it.
                active.pending = Some(self.handle.query_input_completer(
                    Arc::clone(&info.plugin),
                    Arc::clone(&info.name),
                    query.clone(),
                ));
                active.query = query;
                active.items.clear();
                active.selected = 0;
            }
            return;
        }
        let pending = self.handle.query_input_completer(
            Arc::clone(&info.plugin),
            Arc::clone(&info.name),
            query.clone(),
        );
        self.active = Some(Active {
            anchor,
            plugin: Arc::clone(&info.plugin),
            name: Arc::clone(&info.name),
            query,
            items: Vec::new(),
            selected: 0,
            pending: Some(pending),
        });
    }

    /// The nearest registered trigger before the cursor with no whitespace
    /// between it and the cursor, itself at a word boundary.
    fn trigger_context(&self, buffer: &TextBuffer) -> Option<(Anchor, String)> {
        let y = buffer.y();
        let line = buffer.lines().get(y)?;
        let cursor = buffer.x().min(line.chars().count());
        let chars: Vec<char> = line.chars().take(cursor).collect();
        let triggers = self.reader.load_full();
        for i in (0..chars.len()).rev() {
            let c = chars[i];
            if c.is_whitespace() {
                return None;
            }
            if !triggers.completers.iter().any(|t| t.trigger == c) {
                continue;
            }
            if i > 0 && is_word_adjacent(chars[i - 1]) {
                return None;
            }
            let anchor = Anchor {
                y,
                x: i,
                trigger: c,
            };
            let query: String = chars[i + 1..].iter().collect();
            return Some((anchor, query));
        }
        None
    }

    /// Poll the in-flight query; a handler error closes the popup.
    pub fn tick(&mut self) -> Dirty {
        let Some(active) = &mut self.active else {
            return Dirty::NO;
        };
        let Some(rx) = &active.pending else {
            return Dirty::NO;
        };
        match rx.try_recv() {
            Ok(Some(items)) => {
                active.items = items;
                active.selected = 0;
                active.pending = None;
                Dirty::YES
            }
            Ok(None) | Err(flume::TryRecvError::Disconnected) => {
                self.active = None;
                Dirty::YES
            }
            Err(flume::TryRecvError::Empty) => Dirty::NO,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CompletionAction {
        let Some(active) = &mut self.active else {
            return CompletionAction::Passthrough;
        };
        match key.code {
            KeyCode::Esc => {
                self.dismissed = Some(active.anchor.clone());
                self.active = None;
                CompletionAction::Consumed
            }
            KeyCode::Up if !active.items.is_empty() => {
                active.selected = active
                    .selected
                    .checked_sub(1)
                    .unwrap_or(active.items.len() - 1);
                CompletionAction::Consumed
            }
            KeyCode::Down if !active.items.is_empty() => {
                active.selected = (active.selected + 1) % active.items.len();
                CompletionAction::Consumed
            }
            KeyCode::Tab | KeyCode::Enter if !active.items.is_empty() => {
                let item = &active.items[active.selected];
                let replacement = Replacement {
                    y: active.anchor.y,
                    start_x: active.anchor.x,
                    end_x: active.anchor.x + 1 + active.query.chars().count(),
                    insert: item.insert.clone(),
                };
                self.active = None;
                CompletionAction::Accept(replacement)
            }
            KeyCode::Enter => {
                // Popup open with no items yet: swallow the Enter so it
                // dismisses the popup instead of submitting the half-typed
                // query as a message.
                self.dismissed = Some(active.anchor.clone());
                self.active = None;
                CompletionAction::Consumed
            }
            _ => CompletionAction::Passthrough,
        }
    }

    pub fn view(&self, frame: &mut Frame, input_area: Rect) -> Option<Rect> {
        let active = self.active.as_ref()?;
        // Shown for an empty item list: progress while a query is in
        // flight, a verdict once the source answered.
        let placeholder = if active.pending.is_some() {
            SEARCHING
        } else {
            NO_MATCHES
        };

        let t = theme::current();
        let rows = active.items.len().max(1);
        let popup_height = (rows as u16).min(MAX_ROWS).min(input_area.y);
        if popup_height == 0 {
            return None;
        }

        const PAD: usize = 1;
        const GAP: usize = 2;
        let max_label = active
            .items
            .iter()
            .map(|i| i.label.chars().count())
            .max()
            .unwrap_or_else(|| placeholder.chars().count());
        let max_detail = active
            .items
            .iter()
            .filter_map(|i| i.detail.as_ref())
            .map(|d| d.chars().count())
            .max()
            .unwrap_or(0);
        let gap = if max_detail == 0 { 0 } else { GAP };
        let popup_width = (PAD + max_label + gap + max_detail + PAD) as u16;

        let popup = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(popup_height),
            width: popup_width.min(input_area.width),
            height: popup_height,
        };

        let window = visible_window(active.selected, active.items.len(), popup_height as usize);
        let lines: Vec<Line> = if active.items.is_empty() {
            vec![Line::from(Span::styled(
                format!("{}{}{}", " ".repeat(PAD), placeholder, " ".repeat(PAD)),
                t.item_desc,
            ))]
        } else {
            active.items[window.clone()]
                .iter()
                .zip(window)
                .map(|(item, i)| {
                    let style = if i == active.selected {
                        t.item_selected
                    } else {
                        t.item
                    };
                    let label_pad = max_label - item.label.chars().count() + gap;
                    let mut spans = vec![
                        Span::styled(" ".repeat(PAD), style),
                        Span::styled(item.label.clone(), style),
                    ];
                    if let Some(detail) = &item.detail {
                        spans.push(Span::styled(" ".repeat(label_pad), style));
                        spans.push(Span::styled(
                            detail.clone(),
                            if i == active.selected {
                                style
                            } else {
                                t.item_desc
                            },
                        ));
                    }
                    spans.push(Span::styled(" ".repeat(PAD), style));
                    Line::from(spans)
                })
                .collect()
        };

        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().bg(t.background)),
            popup,
        );
        Some(popup)
    }
}

/// Range of item indices to draw so the selection stays in view.
fn visible_window(selected: usize, len: usize, height: usize) -> std::ops::Range<usize> {
    let start = selected
        .saturating_sub(height.saturating_sub(1))
        .min(len.saturating_sub(height));
    start..(start + height).min(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_lua::CompleterInfo;
    use maki_lua::test_support::{completer_writer_pair, probed_event_handle};

    fn at_completer() -> CompleterInfo {
        CompleterInfo {
            trigger: '@',
            plugin: Arc::from("mention"),
            name: Arc::from("files"),
        }
    }

    fn popup() -> (CompletionPopup, maki_lua::test_support::RequestProbe) {
        let (writer, reader) = completer_writer_pair();
        writer.publish(vec![at_completer()]);
        let (handle, probe) = probed_event_handle();
        (CompletionPopup::new(reader, handle), probe)
    }

    fn buffer_at_end(text: &str) -> TextBuffer {
        let mut buf = TextBuffer::new(text.to_string());
        buf.move_to_end();
        buf
    }

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert: label.to_string(),
            detail: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    #[test]
    fn trigger_at_word_boundary_opens_and_queries() {
        let (mut popup, probe) = popup();
        popup.sync(&buffer_at_end("hello @ma"));
        assert!(popup.is_open());
        let (plugin, name, query) = probe.answer_completer_query(Some(vec![])).unwrap();
        assert_eq!(plugin, "mention");
        assert_eq!(name, "files");
        assert_eq!(query, "ma");
    }

    #[test]
    fn mid_word_trigger_and_broken_query_stay_closed() {
        let (mut popup, _probe) = popup();
        popup.sync(&buffer_at_end("mail@example"));
        assert!(!popup.is_open(), "trigger inside a word must not fire");
        popup.sync(&buffer_at_end("@src and beyond"));
        assert!(!popup.is_open(), "whitespace after the query closes it");
        popup.sync(&buffer_at_end("no trigger here"));
        assert!(!popup.is_open());
    }

    #[test]
    fn unregistered_trigger_stays_closed() {
        let (mut popup, _probe) = popup();
        popup.sync(&buffer_at_end("see #123"));
        assert!(!popup.is_open());
    }

    #[test]
    fn retyping_requeries_only_on_query_change() {
        let (mut popup, probe) = popup();
        let mut buf = buffer_at_end("@m");
        popup.sync(&buf);
        assert!(probe.answer_completer_query(Some(vec![])).is_some());

        popup.sync(&buf);
        assert!(
            probe.answer_completer_query(Some(vec![])).is_none(),
            "unchanged query must not requery"
        );

        buf.push_char('a');
        popup.sync(&buf);
        let (_, _, query) = probe.answer_completer_query(Some(vec![])).unwrap();
        assert_eq!(query, "ma");
    }

    #[test]
    fn accept_replaces_trigger_and_query() {
        let (mut popup, probe) = popup();
        let mut buf = buffer_at_end("read @ma");
        popup.sync(&buf);
        probe.answer_completer_query(Some(vec![item("src/main.rs"), item("src/mail.rs")]));
        assert!(popup.tick().take());

        popup.handle_key(key(KeyCode::Down));
        let CompletionAction::Accept(rep) = popup.handle_key(key(KeyCode::Enter)) else {
            panic!("enter should accept");
        };
        buf.replace_range(rep.y, rep.start_x, rep.end_x, &rep.insert);
        assert_eq!(buf.value(), "read src/mail.rs");
        assert_eq!(buf.x(), "read src/mail.rs".len());
        assert!(!popup.is_open());
    }

    #[test]
    fn esc_dismisses_until_the_anchor_changes() {
        let (mut popup, probe) = popup();
        let buf = buffer_at_end("@ma");
        popup.sync(&buf);
        probe.answer_completer_query(Some(vec![item("src/main.rs")]));
        let _ = popup.tick();

        assert!(matches!(
            popup.handle_key(key(KeyCode::Esc)),
            CompletionAction::Consumed
        ));
        assert!(!popup.is_open());

        popup.sync(&buf);
        assert!(!popup.is_open(), "same anchor stays dismissed");

        popup.sync(&buffer_at_end("x @ma"));
        assert!(popup.is_open(), "new anchor opens again");
    }

    #[test]
    fn handler_failure_closes_the_popup() {
        let (mut popup, probe) = popup();
        popup.sync(&buffer_at_end("@ma"));
        probe.answer_completer_query(None);
        assert!(popup.tick().take());
        assert!(!popup.is_open());
    }

    #[test]
    fn enter_with_open_popup_is_consumed_not_submitted() {
        let (mut popup, _probe) = popup();
        popup.sync(&buffer_at_end("@ma"));
        assert!(matches!(
            popup.handle_key(key(KeyCode::Enter)),
            CompletionAction::Consumed
        ));
        assert!(
            !popup.is_open(),
            "Enter dismisses the popup rather than accepting no-match"
        );
    }

    #[test]
    fn fast_typing_does_not_accept_a_stale_item() {
        let (mut popup, probe) = popup();
        let mut buf = buffer_at_end("@a");
        popup.sync(&buf);
        probe.answer_completer_query(Some(vec![item("apple"), item("avocado")]));
        assert!(popup.tick().take());

        buf.push_char('p');
        popup.sync(&buf);
        // No answer yet for the "ap" query; items must be cleared so Tab
        // cannot accept the previous "apple"/"avocado" reply.
        assert!(matches!(
            popup.handle_key(key(KeyCode::Tab)),
            CompletionAction::Consumed | CompletionAction::Passthrough
        ));
        assert!(
            !matches!(
                popup.handle_key(key(KeyCode::Enter)),
                CompletionAction::Accept(_)
            ),
            "stale items must not be acceptable after query changed"
        );
    }

    #[test]
    fn opens_after_opening_punctuation() {
        for (text, want_open) in [
            ("(@fo", true),
            ("[@fo", true),
            (",@fo", true),
            (">@fo", false),
        ] {
            let (mut popup, _probe) = popup();
            popup.sync(&buffer_at_end(text));
            assert_eq!(
                popup.is_open(),
                want_open,
                "unexpected open state for {text:?}"
            );
        }
    }

    #[test]
    fn visible_window_keeps_selection_in_view() {
        assert_eq!(visible_window(0, 3, 10), 0..3);
        assert_eq!(visible_window(0, 20, 10), 0..10);
        assert_eq!(visible_window(9, 20, 10), 0..10);
        assert_eq!(visible_window(10, 20, 10), 1..11);
        assert_eq!(visible_window(19, 20, 10), 10..20);
    }
}
