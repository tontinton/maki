//! Draws one frame and parks the hidden cursor on the input caret.
//!
//! macOS puts IME preedit text at the hardware cursor, so the cursor must sit
//! on the caret. It stays hidden, because a visible one would flip the already
//! reversed caret cell back. While the diff writes cells, the cursor walks over
//! every one of them and the IME follows. So the diff and the park share one
//! synchronized update, and on terminals that honor it the IME only sees the
//! final position. If a frame dies midway, teardown closes the update.
//!
//! The cursor is hidden right after the update begins. The previous frame may
//! have left it shown, and ratatui writes the diff before it hides it, so on
//! terminals that ignore the update a visible cursor would run over every
//! cell.
//!
//! `Frame::set_cursor_position` cannot do the park, since ratatui shows the
//! cursor whenever a frame sets one. So we move it after the draw.
//!
//! Where nothing paints a caret, the chat input, the parked cursor is shown
//! instead, in the shape a plugin asked for if one did.

use std::io::{self, Write};

use crossterm::cursor::{Hide, SetCursorStyle, Show};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{execute, queue};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::{Frame, Terminal};

use crate::terminal::mark_cursor_styled;

/// Where the cursor goes after a frame. Not `shown` where the caret is
/// painted, so the cursor stays parked out of sight. `style` is None until a
/// plugin sets a shape, so the terminal keeps its own.
pub(crate) struct Caret {
    pub position: Position,
    pub shown: bool,
    pub style: Option<SetCursorStyle>,
}

pub(crate) fn present<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut Frame) -> Option<Caret>,
) -> io::Result<()> {
    queue!(terminal.backend_mut(), BeginSynchronizedUpdate, Hide)?;
    let mut caret = None;
    terminal.draw(|frame| caret = render(frame))?;
    if let Some(caret) = caret {
        terminal.set_cursor_position(caret.position)?;
        if caret.shown {
            if let Some(style) = caret.style {
                queue!(terminal.backend_mut(), style)?;
                mark_cursor_styled();
            }
            queue!(terminal.backend_mut(), Show)?;
        }
    }
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::rc::Rc;

    use super::{Caret, present};
    use crossterm::cursor::{Hide, MoveTo, SetCursorStyle, Show};
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    use crossterm::{Command, queue};
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Position, Rect};
    use ratatui::{Terminal, TerminalOptions, Viewport};
    use test_case::test_case;

    const AREA: Rect = Rect::new(0, 0, 8, 1);
    const TEXT: &str = "a漢b";
    const CARET: Position = Position::new(1, 0);
    const ENCODES: &str = "terminal commands encode into a Vec";

    type Bytes = Rc<RefCell<Vec<u8>>>;

    /// The backend only lends its writer behind an unstable ratatui feature,
    /// and dropping the terminal writes `Show`, so the bytes are read through a
    /// shared handle. Only flushed bytes count: stdout holds the rest back from
    /// the terminal.
    struct Sink {
        pending: Vec<u8>,
        delivered: Bytes,
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.pending.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.delivered.borrow_mut().append(&mut self.pending);
            Ok(())
        }
    }

    fn encode(command: impl Command) -> Vec<u8> {
        let mut out = Vec::new();
        queue!(out, command).expect(ENCODES);
        out
    }

    fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    #[test_case(Some(CARET), false, None ; "the_caret_is_parked_last_inside_the_update")]
    #[test_case(Some(CARET), true, None ; "a_shown_caret_keeps_the_terminal_shape")]
    #[test_case(Some(CARET), true, Some(SetCursorStyle::SteadyBar) ; "a_shown_caret_gets_its_shape_after_the_park")]
    #[test_case(None, false, None ; "no_caret_adds_nothing_but_the_update")]
    fn frame_is_the_diff_and_the_park_in_one_update(
        caret: Option<Position>,
        shown: bool,
        style: Option<SetCursorStyle>,
    ) {
        let delivered = Bytes::default();
        let sink = Sink {
            pending: Vec::new(),
            delivered: Rc::clone(&delivered),
        };
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(sink),
            TerminalOptions {
                viewport: Viewport::Fixed(AREA),
            },
        )
        .expect("a fixed viewport needs no tty");

        present(&mut terminal, |frame| {
            frame.render_widget(TEXT, AREA);
            caret.map(|position| Caret {
                position,
                shown,
                style,
            })
        })
        .expect("the frame presents");

        let mut begin = encode(BeginSynchronizedUpdate);
        begin.extend(encode(Hide));
        let end = encode(EndSynchronizedUpdate);
        let mut tail = caret.map_or_else(Vec::new, |pos| encode(MoveTo(pos.x, pos.y)));
        if shown {
            tail.extend(style.map_or_else(Vec::new, encode));
            tail.extend(encode(Show));
        }
        tail.extend_from_slice(&end);
        let delivered = delivered.borrow();
        assert!(delivered.starts_with(&begin));
        assert!(delivered.ends_with(&tail));
        assert_eq!(occurrences(&delivered, &begin), 1);
        assert_eq!(occurrences(&delivered, &end), 1);
    }
}
