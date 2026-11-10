//! Byte and line window reads shared by the host and sandbox backends.

use std::fs::File;
use std::io::{
    BufRead, BufReader, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom,
};
use std::path::Path;

/// Chunk size used when scanning back from a file's end for a line window.
pub const TAIL_SCAN_CHUNK: usize = 64 * 1024;

fn too_large(max_bytes: u64) -> IoError {
    IoError::new(
        ErrorKind::FileTooLarge,
        format!("file exceeds the {max_bytes}-byte read limit"),
    )
}

/// Read a window of bytes from the file at `path`.
///
/// A negative `offset` counts back from the end of the file. `len` caps how many
/// bytes are read; omit it to read to the end of the file.
pub fn read_byte_window(
    path: &Path,
    offset: i64,
    len: Option<u64>,
    max_bytes: u64,
) -> IoResult<Vec<u8>> {
    let mut file = File::open(path)?;
    let start = if offset < 0 {
        file.metadata()?.len().saturating_sub(offset.unsigned_abs())
    } else {
        offset as u64
    };
    file.seek(SeekFrom::Start(start))?;

    let mut buf = Vec::new();
    file.take(len.unwrap_or(u64::MAX).min(max_bytes + 1))
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(too_large(max_bytes));
    }
    Ok(buf)
}

/// Byte offset where the last `lines` lines of `file` start, found by scanning
/// back from the end in `chunk_size` reads. A trailing newline does not open a
/// new line, so the last line of `a\nb\n` is `b\n`.
fn tail_start(file: &mut File, size: u64, lines: u64, chunk_size: usize) -> IoResult<u64> {
    let mut buf = vec![0; chunk_size];
    let mut remaining = lines;
    let mut end = size;
    while end > 0 {
        let len = end.min(chunk_size as u64) as usize;
        let pos = end - len as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut buf[..len])?;
        let mut chunk = &buf[..len];
        if end == size {
            chunk = chunk.strip_suffix(b"\n").unwrap_or(chunk);
        }
        while let Some(newline) = chunk.iter().rposition(|&b| b == b'\n') {
            remaining -= 1;
            if remaining == 0 {
                return Ok(pos + newline as u64 + 1);
            }
            chunk = &chunk[..newline];
        }
        end = pos;
    }
    Ok(0)
}

/// Read up to `limit` lines, failing once the returned bytes pass `max_bytes`.
fn read_lines(mut reader: impl BufRead, limit: Option<u64>, max_bytes: u64) -> IoResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut taken = 0u64;
    while limit.is_none_or(|l| taken < l) {
        let budget = max_bytes + 1 - out.len() as u64;
        if reader.by_ref().take(budget).read_until(b'\n', &mut out)? == 0 {
            break;
        }
        if out.len() as u64 > max_bytes {
            return Err(too_large(max_bytes));
        }
        taken += 1;
    }
    Ok(out)
}

/// Read a window of lines from the file at `path`.
///
/// `offset` is a 1-based line number; a negative `offset` counts back from the last
/// line, so `-1` starts at the last line. `limit` caps how many lines are read;
/// omit it to read every line from `offset`. The result holds the file's own text
/// for those lines, so a window over the whole file equals a plain read. Skipped
/// lines never count toward the read limit.
pub fn read_line_window(
    path: &Path,
    offset: i64,
    limit: Option<u64>,
    max_bytes: u64,
    tail_chunk_size: usize,
) -> IoResult<Vec<u8>> {
    let mut file = File::open(path)?;
    if offset < 0 {
        let size = file.metadata()?.len();
        let start = tail_start(&mut file, size, offset.unsigned_abs(), tail_chunk_size)?;
        file.seek(SeekFrom::Start(start))?;
        return read_lines(BufReader::new(file.take(size - start)), limit, max_bytes);
    }
    let mut reader = BufReader::new(file);
    for _ in 1..offset {
        if reader.skip_until(b'\n')? == 0 {
            break;
        }
    }
    read_lines(reader, limit, max_bytes)
}

/// Appended to a line cut at a length cap.
pub const LINE_CUT_MARKER: &str = "...";

/// A read whose bytes are not valid UTF-8.
pub const NON_UTF8_CONTENT_ERR: &str = "non-utf8 content; use read_bytes";

fn non_utf8() -> IoError {
    IoError::new(ErrorKind::InvalidData, NON_UTF8_CONTENT_ERR)
}

/// One line read from a stream, without its `\n` or `\r\n`.
#[derive(Clone)]
pub struct Line {
    pub text: String,
    /// Its length before any cut, so a caller can say how much was dropped.
    pub bytes: u64,
}

impl From<String> for Line {
    fn from(text: String) -> Self {
        let bytes = text.len() as u64;
        Self { text, bytes }
    }
}

impl From<&str> for Line {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

/// Skips up to {n} lines and returns how many it skipped. A trailing newline
/// ends the last line instead of starting a new one, so `a\nb\n` has 2 lines.
pub fn skip_lines(reader: &mut impl BufRead, n: u64) -> IoResult<u64> {
    let mut skipped = 0;
    while skipped < n && reader.skip_until(b'\n')? > 0 {
        skipped += 1;
    }
    Ok(skipped)
}

/// The next line without its `\n` or `\r\n`, or `None` at the end of the file.
/// A line over {max_line} is cut and gets [`LINE_CUT_MARKER`]. The rest of it
/// is skipped without ever landing in memory, which is how a file that is one
/// 300 MB line still pages fine. Without {max_line}, a line over {max_bytes}
/// fails the read. Bytes that are not UTF-8 fail the read, or become U+FFFD
/// when {lossy}.
pub fn next_line(
    reader: &mut impl BufRead,
    max_line: Option<u64>,
    max_bytes: u64,
    lossy: bool,
) -> IoResult<Option<Line>> {
    let keep = max_line.unwrap_or(max_bytes);
    // The extra 2 bytes let a line of exactly `keep` bytes see its `\r\n`, so it is not cut.
    let window = keep + 2;
    let mut line = Vec::new();
    let read = reader.by_ref().take(window).read_until(b'\n', &mut line)? as u64;
    if read == 0 {
        return Ok(None);
    }
    let bytes = if line.ends_with(b"\n") || read < window {
        line.pop_if(|b| *b == b'\n');
        line.pop_if(|b| *b == b'\r');
        line.len() as u64
    } else {
        let (rest, last) = skip_rest_of_line(reader)?;
        // Stripped like the `\r` of a line that fits: by the last byte before the newline.
        let ends_in_cr = if rest == 0 {
            line.last().copied()
        } else {
            last
        } == Some(b'\r');
        read + rest - u64::from(ends_in_cr)
    };
    if line.len() as u64 > keep {
        if max_line.is_none() {
            return Err(too_large(max_bytes));
        }
        line.truncate(keep as usize);
        // An error with no length is a char the cut split in half, so drop what is left of it.
        let split_char = line
            .utf8_chunks()
            .last()
            .map(|chunk| chunk.invalid())
            .filter(|tail| std::str::from_utf8(tail).is_err_and(|e| e.error_len().is_none()))
            .map_or(0, <[u8]>::len);
        line.truncate(line.len() - split_char);
        line.extend_from_slice(LINE_CUT_MARKER.as_bytes());
    }
    let text = match String::from_utf8(line) {
        Ok(text) => text,
        Err(e) if lossy => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        Err(_) => return Err(non_utf8()),
    };
    Ok(Some(Line { text, bytes }))
}

/// Skips to the next `\n` and returns how many bytes came before it, and the
/// last of them.
fn skip_rest_of_line(reader: &mut impl BufRead) -> IoResult<(u64, Option<u8>)> {
    let (mut skipped, mut last) = (0, None);
    loop {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok((skipped, last));
        }
        let (rest, found) = match buf.iter().position(|b| *b == b'\n') {
            Some(newline) => (&buf[..newline], true),
            None => (buf, false),
        };
        skipped += rest.len() as u64;
        last = rest.last().copied().or(last);
        let consumed = rest.len() + usize::from(found);
        reader.consume(consumed);
        if found {
            return Ok((skipped, last));
        }
    }
}

/// One page of a file plus how much of it there is.
#[derive(Clone, Copy, Debug)]
pub struct PageQuery {
    /// 1-based, so `0` also starts at the first line.
    pub offset: u64,
    pub limit: Option<u64>,
    pub max_line_bytes: Option<u64>,
    /// Reads on past the page to count every line in the file.
    pub count: bool,
}

impl Default for PageQuery {
    fn default() -> Self {
        Self {
            offset: 1,
            limit: None,
            max_line_bytes: None,
            count: true,
        }
    }
}

/// Returns the page and, when the query counts, the total lines in the file.
/// Both come from one pass, so they always agree even on a log that grows
/// while we read it. Only the page itself is ever held in memory.
pub fn read_line_page(
    mut reader: impl BufRead,
    query: &PageQuery,
    max_bytes: u64,
) -> IoResult<(Vec<String>, Option<u64>)> {
    let skipped = skip_lines(&mut reader, query.offset.saturating_sub(1))?;
    let mut lines = Vec::new();
    let mut bytes = 0;
    while query.limit.is_none_or(|l| (lines.len() as u64) < l)
        && let Some(Line { text: line, .. }) =
            next_line(&mut reader, query.max_line_bytes, max_bytes, false)?
    {
        // A line costs its `String` here and a string and a table slot in Lua,
        // so even a file of empty lines has to run into the limit.
        bytes += (line.len() + std::mem::size_of::<String>()) as u64;
        if bytes > max_bytes {
            return Err(too_large(max_bytes));
        }
        lines.push(line);
    }
    let total = if query.count {
        Some(skipped + lines.len() as u64 + skip_lines(&mut reader, u64::MAX)?)
    } else {
        None
    };
    Ok((lines, total))
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Error as IoError, ErrorKind, Read, repeat};

    use super::{
        PageQuery, next_line, read_line_page, LINE_CUT_MARKER, NON_UTF8_CONTENT_ERR,
    };
    use test_case::test_case;

    const CUT: u64 = 5;
    const PAGE_LOG: &str = "l1\nl2\nl3\nl4\nl5\n";
    const PAGE_LOG_LINES: u64 = 5;
    const SMALL_TAIL_CHUNK: usize = 4;
    /// The page size maki applies to a read.
    const TEST_MAX_BYTES: u64 = 67108864;
    const READ_LIMIT_ERROR: &str = "file exceeds the 67108864-byte read limit";

    fn small_reader(content: &[u8]) -> BufReader<&[u8]> {
        BufReader::with_capacity(SMALL_TAIL_CHUNK, content)
    }

    fn cut_query() -> PageQuery {
        PageQuery {
            max_line_bytes: Some(CUT),
            ..PageQuery::default()
        }
    }

    #[test_case("", &[]; "empty")]
    #[test_case("a\nb", &["a", "b"]; "unterminated_last_line")]
    #[test_case("a\nb\n", &["a", "b"]; "trailing_newline")]
    #[test_case("\n\n\n", &["", "", ""]; "blank_lines")]
    #[test_case("a\r\nb\r\n", &["a", "b"]; "crlf")]
    #[test_case("a\r", &["a"]; "unterminated_cr")]
    #[test_case("hello\r\n", &["hello"]; "exact_fit_before_crlf")]
    #[test_case("hello world\nnext", &["hello...", "next"]; "cut_skips_the_rest_of_the_line")]
    #[test_case("caf\u{e9}!", &["caf\u{e9}..."]; "cut_after_a_multibyte_char")]
    #[test_case("ab\u{1f389}", &["ab..."]; "cut_inside_a_multibyte_char")]
    fn read_line_page_splits_and_cuts_lines(content: &str, expected: &[&str]) {
        let (lines, total) = read_line_page(
            small_reader(content.as_bytes()),
            &cut_query(),
            TEST_MAX_BYTES,
        )
        .unwrap();
        assert_eq!(lines, expected);
        assert_eq!(total, Some(expected.len() as u64));
    }

    #[test_case(1, None, &["l1", "l2", "l3", "l4", "l5"]; "whole_file")]
    #[test_case(2, Some(2), &["l2", "l3"]; "window")]
    #[test_case(0, Some(1), &["l1"]; "offset_zero_starts_at_the_first_line")]
    #[test_case(4, Some(10), &["l4", "l5"]; "limit_past_the_end")]
    #[test_case(9, Some(1), &[]; "offset_past_the_end")]
    fn read_line_page_counts_every_line(offset: u64, limit: Option<u64>, expected: &[&str]) {
        let query = PageQuery {
            offset,
            limit,
            ..PageQuery::default()
        };
        let (lines, total) =
            read_line_page(small_reader(PAGE_LOG.as_bytes()), &query, TEST_MAX_BYTES).unwrap();
        assert_eq!(lines, expected);
        assert_eq!(total, Some(PAGE_LOG_LINES));
    }

    #[test]
    fn read_line_page_without_count_stops_after_the_page() {
        let mut reader = small_reader(PAGE_LOG.as_bytes());
        let query = PageQuery {
            offset: 2,
            limit: Some(1),
            count: false,
            ..PageQuery::default()
        };
        let (lines, total) =
            read_line_page(&mut reader, &query, TEST_MAX_BYTES).unwrap();
        assert_eq!(lines, ["l2"]);
        assert_eq!(total, None);
        let next = next_line(&mut reader, None, TEST_MAX_BYTES, false)
            .unwrap()
            .map(|line| line.text);
        assert_eq!(next.as_deref(), Some("l3"));
    }

    /// The length a cut line had leaves out its line ending, the same way
    /// the text of a whole line does.
    #[test_case("abcdefgh\n", 8; "cut_line")]
    #[test_case("abcdefgh\r\n", 8; "cut_crlf_line")]
    #[test_case("abcdefgh", 8; "cut_line_at_eof")]
    #[test_case("abc\r\n", 3; "whole_crlf_line")]
    fn next_line_reports_the_uncut_length(content: &str, expected: u64) {
        let line = next_line(
            &mut small_reader(content.as_bytes()),
            Some(4),
            TEST_MAX_BYTES,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(line.bytes, expected);
    }

    /// Fails its second read with `Interrupted`, which a reader must retry.
    struct InterruptedOnce<R> {
        inner: R,
        reads: usize,
    }

    impl<R: Read> Read for InterruptedOnce<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.reads == 2 {
                return Err(IoError::from(ErrorKind::Interrupted));
            }
            self.inner.read(buf)
        }
    }

    #[test]
    fn next_line_retries_an_interrupted_skip() {
        const CHUNK: usize = 4;
        let inner = InterruptedOnce {
            inner: &b"abcdefgh\nnext\n"[..],
            reads: 0,
        };
        let mut reader = BufReader::with_capacity(CHUNK, inner);
        let cut = next_line(&mut reader, Some(2), TEST_MAX_BYTES, false)
            .unwrap()
            .unwrap();
        assert_eq!((cut.text.as_str(), cut.bytes), ("ab...", 8));
        let next = next_line(&mut reader, Some(2), TEST_MAX_BYTES, false)
            .unwrap()
            .unwrap();
        assert_eq!(next.bytes, 4);
    }

    #[test_case(b"a\xffb\n", "a\u{FFFD}b"; "whole_line")]
    #[test_case(b"\xffa\xc3\xa9b\n", "\u{FFFD}a..."; "cut_through_a_char")]
    fn next_line_lossy_replaces_non_utf8(content: &[u8], expected: &str) {
        let line = next_line(&mut small_reader(content), Some(3), TEST_MAX_BYTES, true)
            .unwrap()
            .unwrap();
        assert_eq!(line.text, expected);
    }

    #[test_case(b"ok\xff\n"; "inside_the_kept_bytes")]
    #[test_case(b"ab\xffcdefgh\n"; "inside_a_cut_line")]
    fn read_line_page_rejects_non_utf8(content: &[u8]) {
        let err =
            read_line_page(small_reader(content), &cut_query(), TEST_MAX_BYTES).unwrap_err();
        assert_eq!(err.to_string(), NON_UTF8_CONTENT_ERR);
    }

    #[test]
    fn read_line_page_charges_empty_lines_against_the_read_limit() {
        let over_the_limit = TEST_MAX_BYTES / size_of::<String>() as u64 + 1;
        let empty_lines = BufReader::new(repeat(b'\n').take(over_the_limit));
        let err = read_line_page(empty_lines, &PageQuery::default(), TEST_MAX_BYTES).unwrap_err();
        assert_eq!(err.to_string(), READ_LIMIT_ERROR);
    }

    #[test]
    fn read_line_page_cuts_a_line_bigger_than_the_read_limit() {
        let huge_line = || BufReader::new(repeat(b'x').take(TEST_MAX_BYTES * 2));

        let (lines, total) =
            read_line_page(huge_line(), &cut_query(), TEST_MAX_BYTES).unwrap();
        assert_eq!(lines, ["x".repeat(CUT as usize) + LINE_CUT_MARKER]);
        assert_eq!(total, Some(1));

        let err =
            read_line_page(huge_line(), &PageQuery::default(), TEST_MAX_BYTES).unwrap_err();
        assert_eq!(err.to_string(), READ_LIMIT_ERROR);
    }
}
