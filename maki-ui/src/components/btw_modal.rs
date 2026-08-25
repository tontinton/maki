use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::streaming_content::StreamingContent;
use crate::theme;

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

pub enum BtwEvent {
    TextDelta(String),
    Done,
    Error(String),
}

pub struct BtwModal {
    open: bool,
    question: String,
    answer: StreamingContent,
    scroll: ModalScroll,
    rx: Option<flume::Receiver<BtwEvent>>,
}

impl BtwModal {
    pub fn new(ms_per_char: u64) -> Self {
        let theme = theme::current();
        Self {
            open: false,
            question: String::new(),
            answer: StreamingContent::new("", theme.assistant, theme.assistant, ms_per_char),
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
        Cadence::when(self.open && self.answer.is_animating(), Cadence::SMOOTH)
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

        let theme = theme::current();
        let border_chrome: u16 = 2;
        let padded_width = (area.width as u32 * WIDTH_PERCENT as u32 / 100)
            .saturating_sub((border_chrome + H_PAD * 2) as u32) as u16;

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("Q: {}", self.question),
            theme.tool_dim,
        )));
        lines.push(Line::default());

        let md_lines = self.answer.render_lines(padded_width);
        lines.extend_from_slice(md_lines);

        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(padded_width) as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
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

    #[cfg(test)]
    pub fn answer_eq(&self, expected: &str) -> bool {
        self.answer == expected
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
    use crossterm::event::KeyCode;
    use test_case::test_case;

    fn open_modal(m: &mut BtwModal, question: &str) -> flume::Sender<BtwEvent> {
        let (tx, rx) = flume::bounded(64);
        m.open(question, rx);
        tx
    }

    #[test]
    fn open_sets_question_and_state() {
        let mut m = BtwModal::new(0);
        let _tx = open_modal(&mut m, "why?");
        assert!(m.is_open());
        assert_eq!(m.question, "why?");
        assert!(m.answer.is_empty());
        assert!(m.is_streaming());
    }

    #[test]
    fn close_resets_all_fields() {
        let mut m = BtwModal::new(0);
        let tx = open_modal(&mut m, "q");
        tx.send(BtwEvent::TextDelta("some answer".into())).unwrap();
        let _ = m.poll();
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-5);
        m.close();
        assert!(!m.is_open());
        assert!(m.question.is_empty());
        assert!(m.answer.is_empty());
        assert_eq!(m.scroll.offset(), 0);
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_accumulates_text() {
        let mut m = BtwModal::new(0);
        let tx = open_modal(&mut m, "q");
        tx.send(BtwEvent::TextDelta("hello ".into())).unwrap();
        tx.send(BtwEvent::TextDelta("world".into())).unwrap();
        let _ = m.poll();
        assert!(m.answer_eq("hello world"));
    }

    #[test]
    fn poll_done_sets_done_and_drops_rx() {
        let mut m = BtwModal::new(0);
        let tx = open_modal(&mut m, "q");
        tx.send(BtwEvent::Done).unwrap();
        let _ = m.poll();
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_error_replaces_answer_and_marks_done() {
        let mut m = BtwModal::new(0);
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
        let mut m = BtwModal::new(0);
        let _tx = open_modal(&mut m, "q");
        m.handle_key(key_ev(code));
        assert!(!m.is_open());
        assert!(!m.is_streaming());
    }

    #[test]
    fn other_keys_consumed_but_stay_open() {
        let mut m = BtwModal::new(0);
        let _tx = open_modal(&mut m, "q");
        m.handle_key(key_ev(KeyCode::Char('a')));
        assert!(m.is_open());
    }

    #[test]
    fn scroll_up_down() {
        let mut m = BtwModal::new(0);
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
        let mut m = BtwModal::new(0);
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
        let mut m = BtwModal::new(0);
        let tx = open_modal(&mut m, "q");
        m.close();
        assert!(tx.send(BtwEvent::TextDelta("x".into())).is_err());
    }

    #[test]
    fn poll_noop_when_no_rx() {
        let mut m = BtwModal::new(0);
        let _ = m.poll();
        assert!(!m.is_open());
    }
}
