//! Wire safety for image blocks. Providers cap pixel dimensions and payload
//! size, and check the media type we declare against the magic bytes we send.
//! An image that trips any of that poisons the whole session, since it stays
//! in history and every later request fails on it again. Images come in from
//! all over (paste, file attach, tool results, ACP, SDK) and none of those
//! places knows the model, so the check lives where they all meet:
//! [`crate::adapt_images_for_model`].

use std::io::Cursor;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::imageops::{self, FilterType};
use image::{DynamicImage, ImageFormat, ImageReader, RgbImage, Rgba, RgbaImage};
use tracing::warn;

use crate::types::{ImageMediaType, ImageSource};

/// Anthropic rejects any dimension over 2000px as soon as a request carries
/// many images, and downscales past 1568px server side anyway. Staying under
/// the smaller number is both safe everywhere and cheaper in tokens.
pub(crate) const MAX_EDGE: u32 = 1568;
/// Anthropic refuses a request carrying more than 100 images, and no other
/// provider is more generous.
pub(crate) const MAX_IMAGES: usize = 100;
/// Providers reject images over 5MB base64, and 3MB of raw bytes encodes to
/// roughly 4MB.
const MAX_RAW_BYTES: usize = 3 * 1024 * 1024;
/// Decode-bomb guard: a tiny header can declare gigabytes of RGBA.
const MAX_PIXELS: u64 = 50_000_000;
/// How often the long edge may halve before an image is given up on.
const LOSSY_ATTEMPTS: u32 = 4;

#[derive(Clone)]
pub(crate) enum Fix {
    /// Already wire-safe.
    Keep,
    Replace(ImageSource),
    /// Undecodable here, so no provider will take it either.
    Drop,
}

/// The formats a provider will take. Anything else has to be transcoded,
/// however well it decodes here.
fn media_type(format: ImageFormat) -> Option<ImageMediaType> {
    Some(match format {
        ImageFormat::Png => ImageMediaType::Png,
        ImageFormat::Jpeg => ImageMediaType::Jpeg,
        ImageFormat::Gif => ImageMediaType::Gif,
        ImageFormat::WebP => ImageMediaType::Webp,
        _ => return None,
    })
}

/// Approximate: padding makes it off by at most two bytes, which no limit
/// here is tight enough to care about.
fn raw_len(base64_len: usize) -> usize {
    base64_len / 4 * 3
}

fn probe(bytes: &[u8]) -> Result<(ImageFormat, u32, u32), String> {
    let format = image::guess_format(bytes).map_err(|_| "unrecognized image format".to_owned())?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| format!("cannot read image header: {e}"))?;
    Ok((format, width, height))
}

fn fits(width: u32, height: u32, base64_len: usize) -> bool {
    width.max(height) <= MAX_EDGE && raw_len(base64_len) <= MAX_RAW_BYTES
}

/// Re-encode attempts, least destructive first: lossless at the cap, then
/// lossy at the cap, then halvings of the long edge. Shrinking before trying
/// the cheaper encoder would cost a screenshot half its resolution when JPEG
/// alone would have fit it. A photo skips the lossless attempt, where PNG is
/// both slower and larger than the payload that was already too big.
fn plan(decoded: ImageFormat) -> impl Iterator<Item = (ImageMediaType, u32)> {
    let lossless = (decoded != ImageFormat::Jpeg).then_some((ImageMediaType::Png, MAX_EDGE));
    let lossy = (0..LOSSY_ATTEMPTS).map(|halvings| (ImageMediaType::Jpeg, MAX_EDGE >> halvings));
    lossless.into_iter().chain(lossy)
}

/// Composes alpha over white. `to_rgb8` alone drops the channel rather than
/// resolving it, which leaves whatever colour sat under a transparent pixel,
/// usually black.
fn flatten(img: &DynamicImage) -> RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let mut canvas = RgbaImage::from_pixel(img.width(), img.height(), Rgba([u8::MAX; 4]));
    imageops::overlay(&mut canvas, img, 0, 0);
    DynamicImage::ImageRgba8(canvas).into_rgb8()
}

/// The label is chosen before the bytes exist, so an output can never be sent
/// under a media type its own magic bytes disagree with.
fn encode(img: &DynamicImage, media: ImageMediaType) -> Result<Vec<u8>, String> {
    let (format, flattened) = match media {
        ImageMediaType::Png => (ImageFormat::Png, None),
        // JPEG carries no alpha channel, and the encoder refuses RGBA outright.
        ImageMediaType::Jpeg => (
            ImageFormat::Jpeg,
            Some(DynamicImage::ImageRgb8(flatten(img))),
        ),
        ImageMediaType::Gif | ImageMediaType::Webp => {
            return Err(format!("no encoder for {}", media.mime()));
        }
    };
    let mut out = Vec::new();
    flattened
        .as_ref()
        .unwrap_or(img)
        .write_to(&mut Cursor::new(&mut out), format)
        .map_err(|e| format!("cannot encode: {e}"))?;
    Ok(out)
}

/// Decode, judge, and if need be shrink and re-encode. Runs off the async
/// executor, so it may take its time.
fn decide(data: Arc<str>, declared: ImageMediaType) -> Result<Fix, String> {
    let bytes = STANDARD
        .decode(&*data)
        .map_err(|e| format!("bad base64: {e}"))?;
    let (format, width, height) = probe(&bytes)?;
    if let Some(detected) = media_type(format)
        && fits(width, height, data.len())
    {
        // Providers match the magic bytes against the declared media type and
        // answer a mismatch with the same 400 an oversized image gets. A `.png`
        // that is really a JPEG only needs its label corrected, which costs no
        // pixels at all.
        if detected == declared {
            return Ok(Fix::Keep);
        }
        warn!(
            declared = declared.mime(),
            detected = detected.mime(),
            "relabelled an image whose bytes disagree with its media type"
        );
        return Ok(Fix::Replace(ImageSource::new(detected, data)));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!("image too large to decode ({width}x{height})"));
    }
    let mut img = image::load_from_memory_with_format(&bytes, format)
        .map_err(|e| format!("cannot decode: {e}"))?;

    for (media, edge) in plan(format) {
        if img.width().max(img.height()) > edge {
            img = img.resize(edge, edge, FilterType::Triangle);
        }
        // The plan is a list of fallbacks, so one encoder refusing these
        // pixels is no reason to give up on the ones after it.
        let encoded = match encode(&img, media) {
            Ok(encoded) => encoded,
            Err(error) => {
                warn!(%error, attempted = media.mime(), "an encoder refused an image");
                continue;
            }
        };
        if encoded.len() <= MAX_RAW_BYTES {
            warn!(
                declared = declared.mime(),
                rewritten_as = media.mime(),
                original_bytes = raw_len(data.len()),
                rewritten_bytes = encoded.len(),
                width = img.width(),
                height = img.height(),
                "rewrote an image that no provider would accept"
            );
            return Ok(Fix::Replace(ImageSource::new(
                media,
                Arc::from(STANDARD.encode(&encoded)),
            )));
        }
    }
    Err(format!(
        "no encoding got the image under {MAX_RAW_BYTES} bytes"
    ))
}

/// What has to happen to `source` before it can go on the wire. The verdict
/// rides on the image itself, so every clone shares it and history hands it
/// back for free on every later request of this process. It is not persisted
/// with the pixels, so a session reloaded from disk decides once more.
pub(crate) async fn fix_for_wire(source: &ImageSource) -> Fix {
    if let Some(fix) = source.verdict.get() {
        return fix.clone();
    }
    let declared = source.media_type;
    let data = Arc::clone(&source.data);
    let fix = smol::unblock(move || decide(data, declared))
        .await
        .unwrap_or_else(|error| {
            warn!(
                %error,
                media_type = declared.mime(),
                "dropping an unusable image so the request can still go through"
            );
            Fix::Drop
        });
    // A concurrent request may have settled it first. Either verdict is sound,
    // and taking the winner keeps what a request sends stable across turns.
    source.verdict.get_or_init(|| fix).clone()
}

/// Shared with the [`crate::adapt_images_for_model`] tests next door.
#[cfg(test)]
pub(crate) fn png_base64(width: u32, height: u32) -> String {
    let img = DynamicImage::new_rgb8(width, height);
    STANDARD.encode(encode(&img, ImageMediaType::Png).unwrap())
}

#[cfg(test)]
pub(crate) fn dimensions(source: &ImageSource) -> (u32, u32) {
    let bytes = STANDARD.decode(source.data.as_bytes()).unwrap();
    let (_, width, height) = probe(&bytes).unwrap();
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;
    use test_case::test_case;

    const NOT_AN_IMAGE: &str = "abc123";
    /// Not base64, and not even ASCII.
    const NOT_TEXT: &str = "€€";
    /// Incompressible pixels: PNG cannot get this under the byte cap at the
    /// full edge, so the plan has to reach for the lossy encoder.
    const NOISE_HEIGHT: u32 = 800;

    fn png(width: u32, height: u32) -> ImageSource {
        ImageSource::new(ImageMediaType::Png, Arc::from(png_base64(width, height)))
    }

    /// A deterministic xorshift beats a real photo: no fixture, and PNG has
    /// nothing to compress.
    fn noise_png() -> ImageSource {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(MAX_EDGE, NOISE_HEIGHT, |_, _| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
        }));
        let data = STANDARD.encode(encode(&img, ImageMediaType::Png).unwrap());
        assert!(
            raw_len(data.len()) > MAX_RAW_BYTES,
            "the fixture must not fit"
        );
        ImageSource::new(ImageMediaType::Png, Arc::from(data))
    }

    fn rewrite(source: &ImageSource) -> ImageSource {
        let Fix::Replace(fixed) = smol::block_on(fix_for_wire(source)) else {
            panic!("expected a rewrite");
        };
        fixed
    }

    #[test_case(3000, 100 ; "wide")]
    #[test_case(100, 3000 ; "tall")]
    #[test_case(2001, 2001 ; "just_over_the_many_image_cap")]
    fn oversized_images_are_shrunk_to_fit(width: u32, height: u32) {
        let (w, h) = dimensions(&rewrite(&png(width, height)));
        assert_eq!(w.max(h), MAX_EDGE, "the long edge must land on the cap");
        assert_eq!(w > h, width > height, "orientation must survive");
    }

    /// The lossy encoder at the full edge comes before any shrink, or an
    /// image JPEG alone would have fit loses half its resolution for nothing.
    #[test]
    fn a_payload_too_heavy_for_lossless_keeps_its_resolution() {
        let fixed = rewrite(&noise_png());
        assert_eq!(fixed.media_type, ImageMediaType::Jpeg);
        assert_eq!(dimensions(&fixed), (MAX_EDGE, NOISE_HEIGHT));
    }

    #[test]
    fn images_within_the_limits_are_left_alone() {
        let source = png(MAX_EDGE, 10);
        assert!(matches!(smol::block_on(fix_for_wire(&source)), Fix::Keep));
    }

    /// Extensions lie (a JPEG saved as `.png`), and ACP hands us whatever mime
    /// the client claimed. The bytes are fine, so only the label moves.
    #[test]
    fn a_mislabelled_payload_is_relabelled_rather_than_re_encoded() {
        let pixels = png(32, 32).data;
        let source = ImageSource::new(ImageMediaType::Jpeg, Arc::clone(&pixels));
        let fixed = rewrite(&source);
        assert_eq!(fixed.media_type, ImageMediaType::Png);
        assert!(
            Arc::ptr_eq(&fixed.data, &pixels),
            "the pixels must not be touched"
        );
    }

    #[test_case(NOT_AN_IMAGE ; "not_an_image")]
    #[test_case(NOT_TEXT ; "not_even_text")]
    fn undecodable_payloads_are_dropped(data: &str) {
        let source = ImageSource::new(ImageMediaType::Png, Arc::from(data));
        assert!(matches!(smol::block_on(fix_for_wire(&source)), Fix::Drop));
    }

    /// Every request walks the whole history, and the copy it carries is a
    /// clone of the one history holds, so a verdict either one reaches has to
    /// be visible to the other or a repair is paid for again on every turn.
    #[test]
    fn a_verdict_is_reached_once_and_shared_with_every_clone() {
        let source = png(MAX_EDGE + 1, 4);
        let first = rewrite(&source);
        assert!(Arc::ptr_eq(&rewrite(&source).data, &first.data));
        assert!(Arc::ptr_eq(&rewrite(&source.clone()).data, &first.data));
    }
}
