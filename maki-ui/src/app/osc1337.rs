//! Images a terminal sends as iTerm2's OSC 1337 file sequences.
//!
//! Maki normally reads images from the clipboard on its own machine. A
//! multiplexer or remote-aware terminal can instead wrap the sequence in a
//! bracketed paste, which crossterm already delivers as `Msg::Paste`; this
//! module pulls the image out and leaves any surrounding text intact.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::GenericImageView as _;
use maki_agent::{ImageMediaType, ImageSource};

const MAX_BYTES: usize = 20 * 1024 * 1024;
const MAX_PIXELS: u64 = 8_000_000;

const FILE: &str = "\x1b]1337;File=";
const MULTIPART: &str = "\x1b]1337;MultipartFile=";
const PART: &str = "\x1b]1337;FilePart=";
const END: &str = "\x1b]1337;FileEnd";

/// Pull OSC 1337 image payloads out of a paste, returning the text that was
/// not part of a sequence.
pub(super) fn parse(text: &str) -> (Vec<ImageSource>, String) {
    let mut images = Vec::new();
    let mut rest = String::new();
    let mut bytes = text.as_bytes();
    let mut parts: Option<Vec<u8>> = None;

    while !bytes.is_empty() {
        let found = [
            (FILE, Kind::File),
            (MULTIPART, Kind::Multipart),
            (PART, Kind::Part),
            (END, Kind::End),
        ]
        .into_iter()
        .filter_map(|(prefix, kind)| {
            find(bytes, prefix.as_bytes()).map(|at| (at, prefix.len(), kind))
        })
        .min_by_key(|(at, _, _)| *at);
        let Some((at, prefix_len, kind)) = found else {
            rest.push_str(std::str::from_utf8(bytes).unwrap_or("\u{fffd}"));
            break;
        };
        rest.push_str(std::str::from_utf8(&bytes[..at]).unwrap_or("\u{fffd}"));
        let after = &bytes[at + prefix_len..];
        let (end, skip) = terminator(after).unwrap_or((after.len(), 0));
        let body = std::str::from_utf8(&after[..end]).unwrap_or("");
        bytes = &after[(end + skip).min(after.len())..];
        if let Some(data) = after_body(body, kind, &mut parts)
            && let Some(source) = source(&data)
        {
            images.push(source);
        }
    }
    (images, rest)
}

#[derive(Clone, Copy)]
enum Kind {
    File,
    Multipart,
    Part,
    End,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|part| part == needle)
}

fn terminator(bytes: &[u8]) -> Option<(usize, usize)> {
    bytes
        .iter()
        .position(|&byte| byte == b'\x07')
        .map(|at| (at, 1))
        .or_else(|| find(bytes, b"\x1b\\").map(|at| (at, 2)))
}

fn after_body(body: &str, kind: Kind, parts: &mut Option<Vec<u8>>) -> Option<Vec<u8>> {
    match kind {
        Kind::Multipart => {
            *parts = Some(Vec::new());
            None
        }
        Kind::Part => {
            let chunk = BASE64.decode(body.as_bytes()).ok()?;
            let buffer = parts.get_or_insert_with(Vec::new);
            if buffer.len().saturating_add(chunk.len()) > MAX_BYTES {
                *parts = None;
                return None;
            }
            buffer.extend_from_slice(&chunk);
            None
        }
        Kind::End => parts.take().filter(|data| !data.is_empty()),
        Kind::File => {
            let payload = body.rsplit_once(':').map(|(_, data)| data).unwrap_or(body);
            let data = BASE64.decode(payload.as_bytes()).ok()?;
            (data.len() <= MAX_BYTES).then_some(data)
        }
    }
}

fn source(data: &[u8]) -> Option<ImageSource> {
    let image = image::load_from_memory(data).ok()?;
    let (width, height) = image.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    let format = image::guess_format(data).ok()?;
    let media_type = ImageMediaType::from_mime(format.to_mime_type())?;
    Some(ImageSource::new(media_type, Arc::from(BASE64.encode(data))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(body: &str) -> String {
        format!("\x1b]1337;{body}\x07")
    }

    #[test]
    fn a_single_file_becomes_an_image_and_leaves_surrounding_text() {
        let png = include_bytes!("../../../tests/fixtures/tiny.png");
        let paste = format!(
            "before\n{}after",
            seq(&format!(
                "File=name=aW1hZ2UucG5n;inline=1:{}",
                BASE64.encode(png)
            ))
        );
        let (images, rest) = parse(&paste);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, ImageMediaType::Png);
        assert_eq!(rest, "before\nafter");
    }

    #[test]
    fn multipart_parts_are_assembled_until_file_end() {
        let png = include_bytes!("../../../tests/fixtures/tiny.png");
        let encoded = BASE64.encode(png);
        let (first, second) = encoded.split_at(encoded.len() / 2);
        let paste = format!(
            "{}{}{}{}",
            seq("MultipartFile=name=aW1hZ2UucG5n;inline=1"),
            seq(&format!("FilePart={first}")),
            seq(&format!("FilePart={second}")),
            seq("FileEnd")
        );
        let (images, rest) = parse(&paste);
        assert_eq!(images.len(), 1);
        assert!(rest.is_empty());
    }

    #[test]
    fn malformed_payloads_are_dropped_without_text() {
        let (images, rest) = parse(&format!("x{}y", seq("File=!!!not-base64")));
        assert!(images.is_empty());
        assert_eq!(rest, "xy");
    }
}
