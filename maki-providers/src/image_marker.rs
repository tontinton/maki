//! `[Image N]` stands in the prompt where an attached image was pasted, so
//! the text can point at it and the transcript can draw it there. N counts
//! the message's images from 1 in the order they are sent, which is the
//! order the model gets them in.

use std::ops::Range;

use crate::ImageSource;

const PREFIX: &str = "[Image ";
const SUFFIX: char = ']';

pub fn label(number: usize) -> String {
    format!("{PREFIX}{number}{SUFFIX}")
}

pub struct Marker {
    pub range: Range<usize>,
    pub number: usize,
}

pub fn markers(text: &str) -> impl Iterator<Item = Marker> + '_ {
    text.match_indices(PREFIX).filter_map(|(start, _)| {
        let digits_start = start + PREFIX.len();
        let rest = &text[digits_start..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !rest[digits..].starts_with(SUFFIX) {
            return None;
        }
        Some(Marker {
            number: rest[..digits].parse().ok()?,
            range: start..digits_start + digits + SUFFIX.len_utf8(),
        })
    })
}

/// The number of the marker the caret at {byte} sits in or right against.
pub fn number_at(text: &str, byte: usize) -> Option<usize> {
    markers(text)
        .find(|marker| marker.range.contains(&byte) || marker.range.end == byte)
        .map(|marker| marker.number)
}

/// What goes out for {text} written with {attached}: the images its markers
/// name, in the order they appear, and the text renumbered to match. An image
/// whose marker was deleted is not sent, and a marker naming no attached
/// image is left as the user typed it.
pub fn resolve(text: &str, attached: &[ImageSource]) -> (String, Vec<ImageSource>) {
    let mut sent = Vec::new();
    let mut renumbered = vec![None; attached.len()];
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for marker in markers(text) {
        let Some(slot) = marker
            .number
            .checked_sub(1)
            .filter(|&slot| slot < attached.len())
        else {
            continue;
        };
        let number = *renumbered[slot].get_or_insert_with(|| {
            sent.push(attached[slot].clone());
            sent.len()
        });
        out.push_str(&text[copied..marker.range.start]);
        out.push_str(&label(number));
        copied = marker.range.end;
    }
    out.push_str(&text[copied..]);
    (out, sent)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::ImageMediaType;
    use test_case::test_case;

    use super::*;

    fn image(data: &str) -> ImageSource {
        ImageSource::new(ImageMediaType::Png, Arc::from(data))
    }

    fn data(images: &[ImageSource]) -> Vec<&str> {
        images.iter().map(|image| image.data.as_ref()).collect()
    }

    #[test_case("[Image 1]", &[1]                    ; "bare")]
    #[test_case("a [Image 2] b [Image 10]", &[2, 10] ; "several")]
    #[test_case("[Image ] [Image x] [Image 1", &[]   ; "malformed")]
    #[test_case("[Image [Image 3]", &[3]             ; "nested_prefix")]
    fn markers_are_found(text: &str, expected: &[usize]) {
        let numbers: Vec<_> = markers(text).map(|marker| marker.number).collect();
        assert_eq!(numbers, expected);
    }

    #[test_case(0,  None    ; "before")]
    #[test_case(2,  Some(1) ; "start")]
    #[test_case(6,  Some(1) ; "inside")]
    #[test_case(11, Some(1) ; "right_after")]
    #[test_case(12, None    ; "past")]
    fn number_at_caret(byte: usize, expected: Option<usize>) {
        assert_eq!(number_at("a [Image 1] b", byte), expected);
    }

    #[test_case("x [Image 2] y [Image 1]", "x [Image 1] y [Image 2]", &["b", "a"] ; "renumbered_in_order")]
    #[test_case("only [Image 2]",          "only [Image 1]",          &["b"]      ; "deleted_marker_drops_image")]
    #[test_case("[Image 1] and [Image 1]", "[Image 1] and [Image 1]", &["a"]      ; "repeated_marker_sends_once")]
    #[test_case("[Image 7] [Image 1]",     "[Image 7] [Image 1]",     &["a"]      ; "unknown_marker_kept")]
    #[test_case("no markers",              "no markers",              &[]         ; "none")]
    fn resolve_sends_what_markers_name(text: &str, expected_text: &str, expected: &[&str]) {
        let (text, images) = resolve(text, &[image("a"), image("b")]);
        assert_eq!(text, expected_text);
        assert_eq!(data(&images), expected);
    }
}
