use fast_qr::{ECL, QRBuilder};

const QUIET: usize = 2;

/// A scannable QR for terminal transcripts, two pixels per cell using the
/// half-block pair. None when the text outgrows the capacity at level M.
pub(crate) fn block_qr(text: &str) -> Option<String> {
    let qr = QRBuilder::new(text).ecl(ECL::M).build().ok()?;
    let size = qr.size as isize;
    let dark = |x: isize, y: isize| -> bool {
        let (x, y) = (x - QUIET as isize, y - QUIET as isize);
        x >= 0 && y >= 0 && x < size && y < size && qr.data[(y * size + x) as usize].value()
    };
    let side = size + 2 * QUIET as isize;
    let rows = side + side % 2;
    let mut out = String::with_capacity((rows as usize + 1) * 40);
    for row in 0..rows / 2 {
        for col in 0..side {
            let top = dark(col, row * 2);
            let bottom = dark(col, row * 2 + 1);
            out.push(match (top, bottom) {
                (true, true) => ' ',
                (true, false) => '\u{2584}',
                (false, true) => '\u{2580}',
                (false, false) => '\u{2588}',
            });
        }
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_url_becomes_a_square_of_block_characters() {
        let url = "https://maki.example.com/0123456789abcdef0123456789abcdef/";
        let qr = block_qr(url).expect("a url fits");
        let rows: Vec<&str> = qr.lines().collect();
        assert!(rows.len() > 10, "qr too small: {}", rows.len());
        assert!(
            rows.windows(2)
                .all(|w| w[0].chars().count() == w[1].chars().count()),
            "ragged rows"
        );
        assert!(
            rows.iter().any(|r| r.contains('\u{2588}')),
            "no dark modules rendered"
        );
    }

    #[test]
    fn absurd_input_refuses_instead_of_panicking() {
        assert!(block_qr(&"x".repeat(5000)).is_none());
    }
}
