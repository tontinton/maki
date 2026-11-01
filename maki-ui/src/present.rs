//! One frame, as the terminal presents it to an IME.
//!
//! The cell diff moves the hardware cursor onto every changed cell, and the
//! caret is parked with a flush of its own. macOS anchors preedit to that
//! cursor, so an intermediate position that reaches the terminal is where the
//! preedit jumps. A synchronized update holds the frame until it closes, and
//! the caret park is inside that update: it is the last position in it.

use std::io::{self, Write};

use crossterm::queue;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};

#[cfg(test)]
use crossterm::Command;
#[cfg(test)]
use crossterm::cursor::{Hide, MoveTo};
#[cfg(test)]
use ratatui::layout::Position;

pub(crate) fn begin<W: Write>(writer: &mut W) -> io::Result<()> {
    queue!(writer, BeginSynchronizedUpdate)
}

#[cfg(test)]
fn park<W: Write>(writer: &mut W, pos: Position) -> io::Result<()> {
    queue!(writer, Hide, MoveTo(pos.x, pos.y))
}

pub(crate) fn end<W: Write>(writer: &mut W) -> io::Result<()> {
    queue!(writer, EndSynchronizedUpdate)?;
    writer.flush()
}

/// Runs `draw`, then parks `cursor` when that draw succeeded. The synchronized
/// update is closed either way.
#[cfg(test)]
pub(crate) fn present<W: Write>(
    writer: &mut W,
    draw: impl FnOnce(&mut W) -> io::Result<()>,
    cursor: Option<Position>,
) -> io::Result<()> {
    begin(writer)?;
    let drawn = draw(writer);
    let parked = match (&drawn, cursor) {
        (Ok(()), Some(pos)) => park(writer, pos),
        _ => Ok(()),
    };
    let closed = end(writer);
    drawn.and(parked).and(closed)
}

#[cfg(test)]
pub(crate) fn cursor_positions_outside(bytes: &[u8]) -> Vec<Position> {
    let bsu = command_bytes(&BeginSynchronizedUpdate);
    let esu = command_bytes(&EndSynchronizedUpdate);
    let mut outside = Vec::new();
    let mut rest = bytes;
    let Some(open) = find_slice(rest, &bsu) else {
        return cursor_positions(bytes);
    };
    outside.extend(cursor_positions(&rest[..open]));
    rest = &rest[open + bsu.len()..];
    while let Some(close) = find_slice(rest, &esu) {
        rest = &rest[close + esu.len()..];
        match find_slice(rest, &bsu) {
            Some(next) => {
                outside.extend(cursor_positions(&rest[..next]));
                rest = &rest[next + bsu.len()..];
            }
            None => {
                outside.extend(cursor_positions(rest));
                break;
            }
        }
    }
    outside
}

#[cfg(test)]
pub(crate) fn last_cursor_inside(bytes: &[u8]) -> Option<Position> {
    cursor_positions_inside(bytes).last().copied()
}

#[cfg(test)]
fn cursor_positions_inside(bytes: &[u8]) -> Vec<Position> {
    let bsu = command_bytes(&BeginSynchronizedUpdate);
    let esu = command_bytes(&EndSynchronizedUpdate);
    let mut rest = bytes;
    let mut found = Vec::new();
    while let Some(open) = find_slice(rest, &bsu) {
        rest = &rest[open + bsu.len()..];
        let Some(close) = find_slice(rest, &esu) else {
            break;
        };
        found.extend(cursor_positions(&rest[..close]));
        rest = &rest[close + esu.len()..];
    }
    found
}

#[cfg(test)]
fn command_bytes(command: &impl Command) -> Vec<u8> {
    let mut out = Vec::new();
    write_command(&mut out, command).expect("a cursor command encodes");
    out
}

#[cfg(test)]
fn write_command(out: &mut Vec<u8>, command: &impl Command) -> io::Result<()> {
    let mut encoded = String::new();
    command
        .write_ansi(&mut encoded)
        .map_err(|_| io::Error::other("cursor command did not encode"))?;
    out.extend_from_slice(encoded.as_bytes());
    Ok(())
}

#[cfg(test)]
fn find_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
fn cursor_positions(bytes: &[u8]) -> Vec<Position> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] != 0x1b || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        if bytes[i + 2] == b'?' {
            i += 2;
            continue;
        }
        let row_start = i + 2;
        let mut j = row_start;
        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j == row_start || bytes.get(j) != Some(&b';') {
            i += 2;
            continue;
        }
        let row_end = j;
        j += 1;
        let col_start = j;
        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j == col_start || bytes.get(j) != Some(&b'H') {
            i += 2;
            continue;
        }
        let Ok(row) = std::str::from_utf8(&bytes[row_start..row_end]) else {
            i += 2;
            continue;
        };
        let Ok(col) = std::str::from_utf8(&bytes[col_start..j]) else {
            i += 2;
            continue;
        };
        let (Ok(row), Ok(col)) = (row.parse::<u16>(), col.parse::<u16>()) else {
            i += 2;
            continue;
        };
        if row == 0 || col == 0 {
            i = j + 1;
            continue;
        }
        found.push(Position::new(col - 1, row - 1));
        i = j + 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{
        command_bytes, cursor_positions, cursor_positions_inside, cursor_positions_outside,
        last_cursor_inside, present,
    };
    use crossterm::cursor::{Hide, MoveTo};
    use crossterm::execute;
    use crossterm::queue;
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    use ratatui::layout::Position;
    use std::io::{self, Write};

    /// The cell `terminal_cursor_tracks_the_software_cursor` reports for `a漢b`
    /// with the caret on `漢`.
    const ON_THE_WIDE_CHAR: Position = Position::new(3, 1);
    const IME_SEES_ONLY_THE_CARET: &str =
        "a cursor position outside the synchronized update is where the IME anchors";
    const CARET_IS_LAST: &str = "the last cursor position inside the update is the input caret";
    const NO_PARK_WITHOUT_CARET: &str =
        "an overlay that owns the keyboard must not append a cursor move";
    const SYNC_CLOSES: &str = "a failed draw must still end the synchronized update";
    const ONE_UPDATE: &str = "the frame is one synchronized update, closed after the caret";
    const PARSER_READS_MOVE_TO: &str =
        "the cursor parser has to read the command the backend sends";

    fn diff_that_walks_the_cursor() -> Vec<u8> {
        let mut out = Vec::new();
        queue!(&mut out, MoveTo(0, 0), MoveTo(40, 2)).expect("the diff encodes");
        out.extend_from_slice(b"*");
        out
    }

    fn assert_one_closed_update(frame: &[u8]) {
        let bsu = command_bytes(&BeginSynchronizedUpdate);
        let esu = command_bytes(&EndSynchronizedUpdate);
        assert!(frame.starts_with(&bsu), "{ONE_UPDATE}");
        assert!(frame.ends_with(&esu), "{ONE_UPDATE}");
        assert_eq!(count(frame, &bsu), 1, "{ONE_UPDATE}");
        assert_eq!(count(frame, &esu), 1, "{ONE_UPDATE}");
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    #[test]
    fn parser_reads_the_crossterm_cursor_command() {
        let mut bytes = Vec::new();
        queue!(&mut bytes, MoveTo(ON_THE_WIDE_CHAR.x, ON_THE_WIDE_CHAR.y)).unwrap();
        assert_eq!(
            cursor_positions(&bytes),
            vec![ON_THE_WIDE_CHAR],
            "{PARSER_READS_MOVE_TO}"
        );
    }

    #[test]
    fn ime_sees_the_wide_char_caret_and_nothing_outside_the_frame() {
        let draw = diff_that_walks_the_cursor();
        let mut frame = Vec::new();
        present(
            &mut frame,
            |writer| writer.write_all(&draw),
            Some(ON_THE_WIDE_CHAR),
        )
        .expect("the frame presents");
        assert_one_closed_update(&frame);
        assert!(
            cursor_positions_outside(&frame).is_empty(),
            "{IME_SEES_ONLY_THE_CARET}"
        );
        assert_eq!(
            last_cursor_inside(&frame),
            Some(ON_THE_WIDE_CHAR),
            "{CARET_IS_LAST}"
        );
        let inside = cursor_positions_inside(&frame);
        assert!(inside.contains(&Position::new(0, 0)), "{CARET_IS_LAST}");
        assert!(inside.contains(&Position::new(40, 2)), "{CARET_IS_LAST}");
    }

    /// `paint` parks with the backend's hide and move. Those bytes are what
    /// has to sit inside the update, immediately before it closes.
    #[test]
    fn the_park_matches_the_backend_commands_paint_sends() {
        let mut via_backend = Vec::new();
        execute!(
            via_backend,
            Hide,
            MoveTo(ON_THE_WIDE_CHAR.x, ON_THE_WIDE_CHAR.y)
        )
        .expect("hide and the caret move encode");

        let mut frame = Vec::new();
        present(&mut frame, |_| Ok(()), Some(ON_THE_WIDE_CHAR)).expect("the frame presents");
        let bsu = command_bytes(&BeginSynchronizedUpdate);
        let esu = command_bytes(&EndSynchronizedUpdate);
        assert_eq!(
            &frame[bsu.len()..frame.len() - esu.len()],
            via_backend.as_slice(),
            "{CARET_IS_LAST}"
        );
    }

    #[test]
    fn an_overlay_adds_no_cursor_move_after_the_diff() {
        let draw = diff_that_walks_the_cursor();
        let mut frame = Vec::new();
        present(&mut frame, |writer| writer.write_all(&draw), None).expect("the frame presents");
        assert_one_closed_update(&frame);
        assert!(
            cursor_positions_outside(&frame).is_empty(),
            "{IME_SEES_ONLY_THE_CARET}"
        );
        let bsu = command_bytes(&BeginSynchronizedUpdate);
        let esu = command_bytes(&EndSynchronizedUpdate);
        let open = bsu.len();
        let close = frame.len() - esu.len();
        assert_eq!(
            &frame[open..close],
            draw.as_slice(),
            "{NO_PARK_WITHOUT_CARET}"
        );
    }

    #[test]
    fn a_failed_draw_still_ends_the_synchronized_update() {
        let mut frame = Vec::new();
        let err = present(
            &mut frame,
            |writer| {
                queue!(writer, MoveTo(0, 0))?;
                Err(io::Error::other("draw failed"))
            },
            Some(ON_THE_WIDE_CHAR),
        );
        assert!(err.is_err(), "{SYNC_CLOSES}");
        assert_one_closed_update(&frame);
        assert!(
            cursor_positions_outside(&frame).is_empty(),
            "{IME_SEES_ONLY_THE_CARET}"
        );
        assert_eq!(
            last_cursor_inside(&frame),
            Some(Position::new(0, 0)),
            "{SYNC_CLOSES}"
        );
    }
}
