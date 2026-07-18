use maki_agent::types::InlineStyle;
use maki_agent::{SnapshotLine, SnapshotSpan, SpanStyle};
use thiserror::Error;
use unicode_width::UnicodeWidthStr;

pub(crate) const DEFAULT_CELL: &str = "▀";

type Rgb = (u8, u8, u8);
type Run = (Rgb, Option<Rgb>, usize);

#[derive(Debug)]
pub(crate) struct FormatSpec {
    name: &'static str,
    bpp: usize,
    rgb_offsets: [usize; 3],
}

const FORMATS: &[FormatSpec] = &[
    FormatSpec {
        name: "rgb",
        bpp: 3,
        rgb_offsets: [0, 1, 2],
    },
    FormatSpec {
        name: "rgba",
        bpp: 4,
        rgb_offsets: [0, 1, 2],
    },
    FormatSpec {
        name: "bgra",
        bpp: 4,
        rgb_offsets: [2, 1, 0],
    },
];

pub(crate) const DEFAULT_FORMAT: &str = "rgb";

#[derive(Debug, PartialEq, Error)]
pub(crate) enum BlitError {
    #[error("blit: width and height must be > 0 (got {w}x{h})")]
    ZeroDimension { w: usize, h: usize },
    #[error("blit: buffer is {got} bytes but {w}x{h} {format} needs exactly {expected}")]
    SizeMismatch {
        got: usize,
        expected: usize,
        w: usize,
        h: usize,
        format: &'static str,
    },
    #[error("blit: unknown format {got:?}, valid formats: {valid}")]
    UnknownFormat { got: String, valid: String },
    #[error("blit: dimensions overflow ({w}x{h})")]
    Overflow { w: usize, h: usize },
    #[error("blit: char must be exactly one column wide, got {got:?}")]
    BadCell { got: String },
}

pub(crate) fn parse_format(name: &str) -> Result<&'static FormatSpec, BlitError> {
    FORMATS
        .iter()
        .find(|f| f.name == name)
        .ok_or_else(|| BlitError::UnknownFormat {
            got: name.to_owned(),
            valid: FORMATS
                .iter()
                .map(|f| f.name)
                .collect::<Vec<_>>()
                .join(", "),
        })
}

pub(crate) fn render(
    bytes: &[u8],
    w: usize,
    h: usize,
    fmt: &FormatSpec,
    cell: &str,
) -> Result<Vec<SnapshotLine>, BlitError> {
    if w == 0 || h == 0 {
        return Err(BlitError::ZeroDimension { w, h });
    }
    if cell.width() != 1 {
        return Err(BlitError::BadCell {
            got: cell.to_owned(),
        });
    }
    let expected = w
        .checked_mul(h)
        .and_then(|px| px.checked_mul(fmt.bpp))
        .ok_or(BlitError::Overflow { w, h })?;
    if bytes.len() != expected {
        return Err(BlitError::SizeMismatch {
            got: bytes.len(),
            expected,
            w,
            h,
            format: fmt.name,
        });
    }

    let pixel = |x: usize, y: usize| -> Rgb {
        let base = (y * w + x) * fmt.bpp;
        let [r, g, b] = fmt.rgb_offsets;
        (bytes[base + r], bytes[base + g], bytes[base + b])
    };

    let mut lines = Vec::with_capacity(h.div_ceil(2));
    for pair in 0..h.div_ceil(2) {
        let (top, bottom) = (pair * 2, pair * 2 + 1);
        let mut spans = Vec::new();
        let mut run: Option<Run> = None;
        for x in 0..w {
            let fg = pixel(x, top);
            let bg = (bottom < h).then(|| pixel(x, bottom));
            match &mut run {
                Some((rf, rb, count)) if (*rf, *rb) == (fg, bg) => *count += 1,
                _ => {
                    spans.extend(run.take().map(|r| run_span(r, cell)));
                    run = Some((fg, bg, 1));
                }
            }
        }
        spans.extend(run.map(|r| run_span(r, cell)));
        lines.push(SnapshotLine { spans });
    }
    Ok(lines)
}

fn run_span((fg, bg, count): Run, cell: &str) -> SnapshotSpan {
    SnapshotSpan {
        text: cell.repeat(count),
        style: SpanStyle::Inline(InlineStyle {
            fg: Some(fg),
            bg,
            ..InlineStyle::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const RED: (u8, u8, u8) = (255, 0, 0);
    const GREEN: (u8, u8, u8) = (0, 255, 0);
    const BLUE: (u8, u8, u8) = (0, 0, 255);
    const WHITE: (u8, u8, u8) = (255, 255, 255);

    fn assert_structure(lines: &[SnapshotLine], w: usize, h: usize) {
        assert_eq!(lines.len(), h.div_ceil(2));
        for line in lines {
            let cells: usize = line
                .spans
                .iter()
                .map(|s| s.text.matches(DEFAULT_CELL).count())
                .sum();
            assert_eq!(cells, w);
        }
    }

    fn style(span: &SnapshotSpan) -> (Option<Rgb>, Option<Rgb>) {
        match &span.style {
            SpanStyle::Inline(i) => (i.fg, i.bg),
            other => panic!("expected inline style, got {other:?}"),
        }
    }

    fn rgb_bytes(pixels: &[(u8, u8, u8)]) -> Vec<u8> {
        pixels.iter().flat_map(|&(r, g, b)| [r, g, b]).collect()
    }

    #[test]
    fn even_height_maps_top_to_fg_bottom_to_bg() {
        let bytes = rgb_bytes(&[RED, GREEN, BLUE, WHITE]);
        let lines = render(&bytes, 2, 2, parse_format("rgb").unwrap(), DEFAULT_CELL).unwrap();
        assert_structure(&lines, 2, 2);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(style(&lines[0].spans[0]), (Some(RED), Some(BLUE)));
        assert_eq!(style(&lines[0].spans[1]), (Some(GREEN), Some(WHITE)));
    }

    #[test]
    fn odd_height_last_line_has_no_bg() {
        let bytes = rgb_bytes(&[RED, RED, GREEN, GREEN, BLUE, WHITE]);
        let lines = render(&bytes, 2, 3, parse_format("rgb").unwrap(), DEFAULT_CELL).unwrap();
        assert_structure(&lines, 2, 3);
        assert_eq!(style(&lines[0].spans[0]), (Some(RED), Some(GREEN)));
        assert_eq!(style(&lines[1].spans[0]), (Some(BLUE), None));
        assert_eq!(style(&lines[1].spans[1]), (Some(WHITE), None));
    }

    #[test_case(DEFAULT_CELL ; "default_cell")]
    #[test_case("█" ; "custom_cell")]
    fn uniform_image_merges_into_one_span(cell: &str) {
        let bytes = rgb_bytes(&[RED; 8]);
        let lines = render(&bytes, 4, 2, parse_format("rgb").unwrap(), cell).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].text, cell.repeat(4));
        assert_eq!(style(&lines[0].spans[0]), (Some(RED), Some(RED)));
    }

    #[test]
    fn bg_change_alone_breaks_run() {
        let top = [RED, RED, RED, RED];
        let bottom = [GREEN, GREEN, BLUE, BLUE];
        let bytes = rgb_bytes(&[top.as_slice(), bottom.as_slice()].concat());
        let lines = render(&bytes, 4, 2, parse_format("rgb").unwrap(), DEFAULT_CELL).unwrap();
        assert_structure(&lines, 4, 2);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(style(&lines[0].spans[0]), (Some(RED), Some(GREEN)));
        assert_eq!(style(&lines[0].spans[1]), (Some(RED), Some(BLUE)));
    }

    #[test]
    fn all_formats_decode_same_logical_image() {
        let pixels = [RED, GREEN, BLUE, WHITE];
        let rgb = rgb_bytes(&pixels);
        let rgba: Vec<u8> = pixels.iter().flat_map(|&(r, g, b)| [r, g, b, 0]).collect();
        let bgra: Vec<u8> = pixels.iter().flat_map(|&(r, g, b)| [b, g, r, 0]).collect();

        let expected = render(&rgb, 2, 2, parse_format("rgb").unwrap(), DEFAULT_CELL).unwrap();
        for (name, bytes) in [("rgba", rgba), ("bgra", bgra)] {
            let lines = render(&bytes, 2, 2, parse_format(name).unwrap(), DEFAULT_CELL).unwrap();
            assert_eq!(lines, expected, "format {name} diverged from rgb");
        }
    }

    #[test_case(0, 2 ; "zero_width")]
    #[test_case(2, 0 ; "zero_height")]
    fn zero_dimension_errors(w: usize, h: usize) {
        let err = render(&[], w, h, parse_format("rgb").unwrap(), DEFAULT_CELL).unwrap_err();
        assert_eq!(err, BlitError::ZeroDimension { w, h });
    }

    #[test_case(11 ; "one_byte_short")]
    #[test_case(13 ; "one_byte_long")]
    fn size_mismatch_errors(len: usize) {
        let err = render(
            &vec![0; len],
            2,
            2,
            parse_format("rgb").unwrap(),
            DEFAULT_CELL,
        )
        .unwrap_err();
        assert_eq!(
            err,
            BlitError::SizeMismatch {
                got: len,
                expected: 12,
                w: 2,
                h: 2,
                format: "rgb",
            }
        );
    }

    #[test]
    fn overflow_errors() {
        let err = render(
            &[],
            usize::MAX,
            2,
            parse_format("rgb").unwrap(),
            DEFAULT_CELL,
        )
        .unwrap_err();
        assert_eq!(
            err,
            BlitError::Overflow {
                w: usize::MAX,
                h: 2
            }
        );
    }

    #[test_case("" ; "empty")]
    #[test_case("ab" ; "two_columns")]
    #[test_case("字" ; "wide_char")]
    fn bad_cell_errors(cell: &str) {
        let bytes = rgb_bytes(&[RED]);
        let err = render(&bytes, 1, 1, parse_format("rgb").unwrap(), cell).unwrap_err();
        assert_eq!(
            err,
            BlitError::BadCell {
                got: cell.to_owned()
            }
        );
    }
}
