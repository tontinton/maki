use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::{BORDER, Modal, inset};
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::streaming_content::StreamingContent;
use crate::components::thinking;
use crate::components::tool_display::{assistant_style, thinking_style};
use crate::theme;

use maki_config::ThinkingLines;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::repaint::{Cadence, Dirty};

const TITLE: &str = " /btw ";
const H_PAD: u16 = 2;
const WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 80;
/// Clicks never reach the modal, so its thinking stays the way the config
/// draws it and never promises to open.
const CLICK_EXPANDED: bool = false;
const CLICKABLE: bool = false;

pub enum BtwEvent {
    TextDelta(String),
    ThinkingDelta(String),
    Done,
    Error(String),
}

pub struct BtwModal {
    open: bool,
    question: String,
    answer: StreamingContent,
    thinking: StreamingContent,
    /// Same setting as the transcript, so thinking looks the same in both.
    thinking_lines: ThinkingLines,
    /// `StreamingContent` keeps its styles by value and the modal outlives any
    /// number of theme switches, so this is what tells us to hand it new ones.
    theme_generation: u64,
    scroll: ModalScroll,
    rx: Option<flume::Receiver<BtwEvent>>,
}

impl BtwModal {
    pub fn new(ms_per_char: u64, thinking_lines: ThinkingLines) -> Self {
        let thinking = thinking_style();
        let answer = assistant_style();
        Self {
            open: false,
            question: String::new(),
            answer: StreamingContent::new("", answer.text_style, answer.prefix_style, ms_per_char),
            thinking: StreamingContent::new(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
                ms_per_char,
            ),
            thinking_lines,
            theme_generation: theme::generation(),
            scroll: ModalScroll::new(),
            rx: None,
        }
    }

    pub fn open(&mut self, question: &str, rx: flume::Receiver<BtwEvent>) {
        self.close();
        self.open = true;
        self.question = question.to_string();
        self.rx = Some(rx);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.question.clear();
        self.answer.clear();
        self.thinking.clear();
        self.scroll.reset();
        self.rx = None;
    }

    #[cfg(test)]
    pub fn is_streaming(&self) -> bool {
        self.rx.is_some()
    }

    /// Only the typewriter moves on its own, and only while it is on screen.
    /// A pending stream is drained by [`Self::poll`], which reports its own
    /// [`Dirty`].
    pub fn cadence(&self) -> Cadence {
        let smooth = self.answer.is_animating()
            || (thinking::reveals_text(self.thinking_lines, CLICK_EXPANDED)
                && self.thinking.is_animating());
        Cadence::when(self.open && smooth, Cadence::SMOOTH)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn poll(&mut self) -> Dirty {
        let Some(ref rx) = self.rx else {
            return Dirty::NO;
        };
        let mut dirty = Dirty::NO;
        while let Ok(event) = rx.try_recv() {
            dirty = Dirty::YES;
            match event {
                BtwEvent::TextDelta(text) => self.answer.push(&text),
                BtwEvent::ThinkingDelta(text) => self.thinking.push(&text),
                BtwEvent::Done => {
                    self.rx = None;
                    break;
                }
                BtwEvent::Error(msg) => {
                    self.answer.clear();
                    self.answer.push(&msg);
                    self.rx = None;
                    break;
                }
            }
        }
        dirty
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => {
                self.close();
            }
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            min_width: 0,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let padded_width = modal.width(area.width).saturating_sub(BORDER + H_PAD * 2);

        let lines = self.body_lines(padded_width);
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(padded_width) as u16;
        let (popup, inner) = modal.render(frame, area, total);
        let padded = inset(inner, H_PAD);
        let viewport_h = padded.height;
        self.scroll.update_dimensions(total, viewport_h);
        let scroll = self.scroll.offset();

        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, padded);

        if total > viewport_h {
            render_vertical_scrollbar(frame, inner, u32::from(total), u32::from(scroll));
        }

        popup
    }

    /// Question, then the reasoning, then the answer. Kept out of [`Self::view`]
    /// because the height pass and the paint pass have to agree on the lines.
    fn body_lines(&mut self, width: u16) -> Vec<Line<'static>> {
        self.restyle_on_theme_change();
        let theme = theme::current();
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Q: {}", self.question),
                theme.tool_dim,
            )),
            Line::default(),
        ];
        if !self.thinking.is_empty() {
            let collapsed = thinking::collapsed_view(
                thinking::tail_rows(self.thinking_lines, CLICK_EXPANDED),
                self.thinking.line_count(),
                CLICKABLE,
                |rows| self.thinking.render_tail(rows, width).map(<[_]>::to_vec),
            );
            match collapsed {
                Some(collapsed) => lines.extend(collapsed),
                None => lines.extend_from_slice(self.thinking.render_lines(width)),
            }
            lines.push(Line::default());
        }
        lines.extend_from_slice(self.answer.render_lines(width));
        lines
    }

    fn restyle_on_theme_change(&mut self) {
        let generation = theme::generation();
        if self.theme_generation == generation {
            return;
        }
        self.theme_generation = generation;
        let thinking = thinking_style();
        let answer = assistant_style();
        self.thinking
            .set_style(thinking.prefix, thinking.text_style, thinking.prefix_style);
        self.answer
            .set_style("", answer.text_style, answer.prefix_style);
    }

    #[cfg(test)]
    pub fn answer_eq(&self, expected: &str) -> bool {
        self.answer == expected
    }

    #[cfg(test)]
    pub fn thinking_eq(&self, expected: &str) -> bool {
        self.thinking == expected
    }
}

impl Overlay for BtwModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        self.cadence()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_ev;
    use crate::markdown::EXPAND_HINT;
    use crossterm::event::KeyCode;
    use ratatui::style::Style;
    use test_case::test_case;

    const INSTANT: u64 = 0;
    const WIDTH: u16 = 80;
    const TAIL_ROWS: usize = 2;
    const IDEA_COUNT: usize = 6;
    const THEME_A: &str = "dracula";
    const THEME_B: &str = "tokyonight";
    const REASONING: &str = "hmm, sqlite";
    const ANSWER: &str = "Because it is embedded.";
    const HIDDEN_COUNT: &str = "(1 lines)";

    fn modal() -> BtwModal {
        BtwModal::new(INSTANT, ThinkingLines::Never)
    }

    fn idea(n: usize) -> String {
        format!("idea {n:02}")
    }

    fn ideas() -> String {
        (1..=IDEA_COUNT)
            .map(|n| format!("- {}", idea(n)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn open_modal(m: &mut BtwModal, question: &str) -> flume::Sender<BtwEvent> {
        let (tx, rx) = flume::bounded(64);
        m.open(question, rx);
        tx
    }

    fn answer_a_question(m: &mut BtwModal) {
        let tx = open_modal(m, "q");
        tx.send(BtwEvent::ThinkingDelta(REASONING.into())).unwrap();
        tx.send(BtwEvent::TextDelta(ANSWER.into())).unwrap();
        let _ = m.poll();
    }

    fn body_text(m: &mut BtwModal) -> String {
        m.body_lines(WIDTH)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect()
    }

    /// The chrome around the body already reads the live theme, so looking at
    /// the two streams on their own is what proves they were restyled too.
    fn stream_styles(m: &mut BtwModal) -> Vec<Style> {
        m.body_lines(WIDTH);
        m.thinking
            .cached_lines()
            .iter()
            .chain(m.answer.cached_lines())
            .flat_map(|l| l.spans.iter().map(|s| s.style))
            .collect()
    }

    #[test]
    fn open_sets_question_and_state() {
        let mut m = modal();
        let _tx = open_modal(&mut m, "why?");
        assert!(m.is_open());
        assert_eq!(m.question, "why?");
        assert!(m.answer.is_empty());
        assert!(m.is_streaming());
    }

    #[test]
    fn close_resets_all_fields() {
        let mut m = modal();
        answer_a_question(&mut m);
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-5);
        m.close();
        assert!(!m.is_open());
        assert!(m.question.is_empty());
        assert!(m.answer.is_empty());
        assert!(m.thinking.is_empty());
        assert_eq!(m.scroll.offset(), 0);
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_accumulates_text() {
        let mut m = modal();
        let tx = open_modal(&mut m, "q");
        tx.send(BtwEvent::TextDelta("hello ".into())).unwrap();
        tx.send(BtwEvent::TextDelta("world".into())).unwrap();
        let _ = m.poll();
        assert!(m.answer_eq("hello world"));
    }

    #[test]
    fn poll_done_sets_done_and_drops_rx() {
        let mut m = modal();
        let tx = open_modal(&mut m, "q");
        tx.send(BtwEvent::Done).unwrap();
        let _ = m.poll();
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_error_replaces_answer_and_marks_done() {
        let mut m = modal();
        let tx = open_modal(&mut m, "q");
        tx.send(BtwEvent::TextDelta("partial".into())).unwrap();
        tx.send(BtwEvent::Error("oops".into())).unwrap();
        let _ = m.poll();
        assert!(m.answer_eq("oops"));
        assert!(!m.is_streaming());
    }

    #[test_case(KeyCode::Esc   ; "esc_closes")]
    #[test_case(KeyCode::Enter ; "enter_closes")]
    #[test_case(KeyCode::Char(' ') ; "space_closes")]
    fn dismiss_keys_close(code: KeyCode) {
        let mut m = modal();
        let _tx = open_modal(&mut m, "q");
        m.handle_key(key_ev(code));
        assert!(!m.is_open());
        assert!(!m.is_streaming());
    }

    #[test]
    fn other_keys_consumed_but_stay_open() {
        let mut m = modal();
        let _tx = open_modal(&mut m, "q");
        m.handle_key(key_ev(KeyCode::Char('a')));
        assert!(m.is_open());
    }

    #[test]
    fn scroll_up_down() {
        let mut m = modal();
        let _tx = open_modal(&mut m, "q");
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-5);
        assert_eq!(m.scroll.offset(), 90);
        m.handle_key(key_ev(KeyCode::Up));
        assert_eq!(m.scroll.offset(), 89);
        m.handle_key(key_ev(KeyCode::Down));
        assert_eq!(m.scroll.offset(), 90);
        m.scroll.scroll(200);
        assert_eq!(m.scroll.offset(), 0);
    }

    #[test]
    fn double_open_resets_first() {
        let mut m = modal();
        let tx1 = open_modal(&mut m, "first");
        tx1.send(BtwEvent::TextDelta("leftover".into())).unwrap();
        let _ = m.poll();
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-10);
        let _tx2 = open_modal(&mut m, "second");
        assert!(m.is_open());
        assert_eq!(m.question, "second");
        assert!(m.answer.is_empty());
        assert_eq!(m.scroll.offset(), 0);
    }

    #[test]
    fn close_drops_rx_signaling_sender() {
        let mut m = modal();
        let tx = open_modal(&mut m, "q");
        m.close();
        assert!(tx.send(BtwEvent::TextDelta("x".into())).is_err());
    }

    #[test]
    fn poll_noop_when_no_rx() {
        let mut m = modal();
        let _ = m.poll();
        assert!(!m.is_open());
    }

    /// Both deltas used to be pushed into the answer, which left the reasoning
    /// indistinguishable from the response. They need separate streams because
    /// each carries its own colour and prefix.
    #[test]
    fn thinking_deltas_are_kept_apart_from_the_answer() {
        let mut m = modal();
        answer_a_question(&mut m);
        assert!(m.answer_eq(ANSWER));
        assert!(m.thinking_eq(REASONING));
    }

    /// The count comes off the full buffer, not the typewriter's visible slice,
    /// which never advances here because nothing draws it.
    #[test]
    fn hidden_thinking_is_summarised_not_printed() {
        let mut m = BtwModal::new(INSTANT, ThinkingLines::Tail(0));
        answer_a_question(&mut m);
        let body = body_text(&mut m);
        assert!(!body.contains(REASONING), "reasoning leaked: {body}");
        assert!(body.contains(HIDDEN_COUNT), "no line count: {body}");
        assert!(!body.contains(EXPAND_HINT), "nothing to click: {body}");
        assert!(body.contains(ANSWER), "answer missing: {body}");
    }

    /// Nothing in the modal can be clicked open, so it must never offer to.
    #[test_case(ThinkingLines::Never, 1 ; "never_shows_it_all")]
    #[test_case(ThinkingLines::Tail(TAIL_ROWS), IDEA_COUNT - TAIL_ROWS + 1 ; "tail_shows_the_last_rows")]
    fn thinking_is_shown_from(thinking_lines: ThinkingLines, first_shown: usize) {
        let mut m = BtwModal::new(INSTANT, thinking_lines);
        let tx = open_modal(&mut m, "q");
        tx.send(BtwEvent::ThinkingDelta(ideas())).unwrap();
        let _ = m.poll();
        let body = body_text(&mut m);
        assert!(!body.contains(EXPAND_HINT), "nothing to click: {body}");
        for n in 1..=IDEA_COUNT {
            assert_eq!(body.contains(&idea(n)), n >= first_shown, "{n}: {body}");
        }
    }

    /// The modal is built once at startup and lives through every theme switch,
    /// so the streams have to be handed the new colours before they repaint.
    #[test]
    fn theme_switch_repaints_the_streams() {
        theme::set(theme::load_by_name(THEME_A).expect(THEME_A));
        let mut m = modal();
        answer_a_question(&mut m);
        let before = stream_styles(&mut m);
        assert!(!before.is_empty(), "nothing rendered to compare");

        theme::set(theme::load_by_name(THEME_B).expect(THEME_B));

        assert_ne!(
            before,
            stream_styles(&mut m),
            "a theme switch must restyle the body, not keep the startup palette"
        );
    }
}
