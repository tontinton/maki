//! `maki.image`: image primitives for plugins. Neovim has no image API to
//! mirror, so these are small blocks (probe, decode, resize, encode) that
//! plugins compose; provider policy stays in Lua. Errors follow the
//! `(nil, err)` convention of `maki.fs`.

use std::io::Cursor;
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader};
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use maki_providers::{ImageMediaType, ImageSource};
use mlua::{
    AnyUserData, Lua, LuaString, Result as LuaResult, Table, UserDataRef, Value as LuaValue,
};

use super::base64::bytes_arg;
use super::util::pair::{Pair, try_pair};

/// Decode-bomb guard: a tiny file can declare huge dimensions and balloon
/// into gigabytes of RGBA. Host-fixed so no plugin can disable it; 50MP
/// still covers any real camera photo.
const MAX_PIXELS: u64 = 50_000_000;
/// How much base64 a header probe reads before it falls back to the whole
/// payload. A multiple of four, so the cut never lands inside a group, and
/// big enough for a JPEG with a maxed-out EXIF thumbnail before its size.
const HEADER_PREFIX: usize = 192 * 1024;

fn format_name(format: ImageFormat) -> &'static str {
    match format {
        // Only jpeg deviates from its primary extension ("jpg").
        ImageFormat::Jpeg => "jpeg",
        other => other.extensions_str().first().copied().unwrap_or("unknown"),
    }
}

fn image_format(media_type: ImageMediaType) -> ImageFormat {
    match media_type {
        ImageMediaType::Png => ImageFormat::Png,
        ImageMediaType::Jpeg => ImageFormat::Jpeg,
        ImageMediaType::Gif => ImageFormat::Gif,
        ImageMediaType::Webp => ImageFormat::WebP,
    }
}

/// gif/webp have no encoder here, so a resize can only keep JPEG as it is.
fn resized_media_type(source: ImageMediaType) -> ImageMediaType {
    match source {
        ImageMediaType::Jpeg => ImageMediaType::Jpeg,
        _ => ImageMediaType::Png,
    }
}

fn probe_bytes(bytes: &[u8]) -> Result<(ImageFormat, u32, u32), String> {
    let format =
        image::guess_format(bytes).map_err(|_| "not an image (unrecognized format)".to_owned())?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| format!("cannot read image header: {e}"))?;
    Ok((format, width, height))
}

fn decode_bytes(bytes: &[u8]) -> Result<(ImageFormat, DynamicImage), String> {
    let (format, width, height) = probe_bytes(bytes)?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "image too large to decode ({width}x{height}; limit {MAX_PIXELS} pixels)"
        ));
    }
    let pixels = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| format!("cannot decode: {e}"))?;
    Ok((format, pixels))
}

fn decode_payload(data: &str) -> Result<Vec<u8>, String> {
    BASE64
        .decode(data.as_bytes())
        .map_err(|e| format!("image payload is not valid base64: {e}"))
}

/// Dimensions off a bounded prefix, so asking for the size of a stored image
/// never base64-decodes megabytes. Some payloads need more than the prefix,
/// and those fall back to the whole thing.
fn probe_payload(data: &str) -> Result<(ImageFormat, u32, u32), String> {
    let prefix = &data.as_bytes()[..data.len().min(HEADER_PREFIX)];
    if prefix.len() < data.len()
        && let Some(probed) = BASE64
            .decode(prefix)
            .ok()
            .and_then(|b| probe_bytes(&b).ok())
    {
        return Ok(probed);
    }
    probe_bytes(&decode_payload(data)?)
}

fn encode_pixels(pixels: &DynamicImage, media_type: ImageMediaType) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    pixels
        .write_to(&mut Cursor::new(&mut out), image_format(media_type))
        .map_err(|e| format!("cannot encode: {e}"))?;
    Ok(out)
}

fn source_from_bytes(media_type: ImageMediaType, bytes: &[u8]) -> ImageSource {
    ImageSource::new(media_type, Arc::from(BASE64.encode(bytes)))
}

/// Always holds an encoded payload in a message media type. That way placing
/// an image into a message or JSON never encodes on the Lua thread: every
/// producer that has to encode (resize, a decode that transcodes) does it
/// off-thread. `pixels` is only a cache, decoded at most once.
///
/// `canonical` is true when this build's encoder wrote `source` at default
/// settings, so re-encoding to the same format could only reproduce it.
/// Bytes from outside are not: they may be oversized or oddly compressed,
/// and `encode` re-encodes them so a plugin can always shrink a file.
struct ImageState {
    source: ImageSource,
    canonical: bool,
    pixels: OnceLock<DynamicImage>,
}

impl ImageState {
    /// Decodes in full: a file with a valid header and a corrupt body would
    /// otherwise land in history and fail every later request. The pixels are
    /// kept, since the work is already done. A format no message can carry
    /// is transcoded to PNG.
    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let (format, pixels) = decode_bytes(bytes)?;
        let Some(media_type) = ImageMediaType::from_mime(format.to_mime_type()) else {
            return Self::from_pixels(ImageMediaType::Png, pixels);
        };
        Ok(Self {
            source: source_from_bytes(media_type, bytes),
            canonical: false,
            pixels: OnceLock::from(pixels),
        })
    }

    fn from_pixels(media_type: ImageMediaType, pixels: DynamicImage) -> Result<Self, String> {
        let bytes = encode_pixels(&pixels, media_type)?;
        Ok(Self {
            source: source_from_bytes(media_type, &bytes),
            canonical: true,
            pixels: OnceLock::from(pixels),
        })
    }

    fn pixels(&self) -> Result<&DynamicImage, String> {
        if let Some(pixels) = self.pixels.get() {
            return Ok(pixels);
        }
        let (_, decoded) = decode_bytes(&decode_payload(&self.source.data)?)?;
        Ok(self.pixels.get_or_init(|| decoded))
    }

    fn dimensions(&self) -> Result<(u32, u32), String> {
        if let Some(pixels) = self.pixels.get() {
            return Ok((pixels.width(), pixels.height()));
        }
        let (_, width, height) = probe_payload(&self.source.data)?;
        Ok((width, height))
    }

    fn encode(&self, media_type: ImageMediaType) -> Result<Vec<u8>, String> {
        if self.canonical && media_type == self.source.media_type {
            return decode_payload(&self.source.data);
        }
        encode_pixels(self.pixels()?, media_type)
    }

    /// An image that already fits comes back as itself, checked off the
    /// header so a no-op resize never decodes.
    fn resized(self: &Arc<Self>, max_w: u32, max_h: u32) -> Result<Arc<Self>, String> {
        let (width, height) = self.dimensions()?;
        if width <= max_w && height <= max_h {
            return Ok(Arc::clone(self));
        }
        let pixels = self.pixels()?.resize(max_w, max_h, FilterType::Triangle);
        Self::from_pixels(resized_media_type(self.source.media_type), pixels).map(Arc::new)
    }
}

/// `Arc` so resize/encode can hop to a blocking thread without copying, and
/// so a payload placed into a message is shared rather than copied.
pub(crate) struct LuaImage(Arc<ImageState>);

impl LuaImage {
    /// O(1): the payload is shared and nothing is decoded until asked for.
    pub(crate) fn from_source(source: ImageSource) -> Self {
        Self(Arc::new(ImageState {
            source,
            canonical: false,
            pixels: OnceLock::new(),
        }))
    }

    /// O(1): shares the payload.
    pub(crate) fn to_source(&self) -> ImageSource {
        self.0.source.clone()
    }
}

/// Get the width of the image in pixels. Reads only the header when the
/// pixels are not decoded yet.
///
/// @return (integer) Width in pixels.
#[lua_fn]
fn width(_lua: &Lua, this: &LuaImage) -> LuaResult<u32> {
    Ok(this.0.dimensions().map_err(mlua::Error::runtime)?.0)
}

/// Get the height of the image in pixels. Reads only the header when the
/// pixels are not decoded yet.
///
/// @return (integer) Height in pixels.
#[lua_fn]
fn height(_lua: &Lua, this: &LuaImage) -> LuaResult<u32> {
    Ok(this.0.dimensions().map_err(mlua::Error::runtime)?.1)
}

/// Shrink the image to fit inside {max_w} x {max_h}, keeping the aspect
/// ratio. If the image already fits, it is returned as-is. Never upscales.
/// A resized image is JPEG if the source was JPEG and PNG otherwise, which
/// decides its `media_type` when placed into a message.
///
/// @param max_w integer Maximum width in pixels. Must be positive.
/// @param max_h integer Maximum height in pixels. Must be positive.
/// @return (maki.image.Image) A new image handle (or the same one if no resize was needed).
/// @example
/// local img = maki.image.decode(raw_bytes)
/// local small = img:resize(800, 600)
/// local encoded = small:encode("jpeg")
#[lua_fn]
async fn resize(
    _lua: Lua,
    this: UserDataRef<LuaImage>,
    max_w: u32,
    max_h: u32,
) -> LuaResult<LuaImage> {
    let state = Arc::clone(&this.0);
    drop(this);
    if max_w == 0 || max_h == 0 {
        return Err(mlua::Error::runtime("resize: dimensions must be positive"));
    }
    let resized = smol::unblock(move || state.resized(max_w, max_h))
        .await
        .map_err(|e| mlua::Error::runtime(format!("resize: {e}")))?;
    Ok(LuaImage(resized))
}

/// Encode the image into raw bytes in the given format. Use this to prepare
/// images for sending over the network or writing to disk. Bytes that came
/// from outside are always re-encoded, so this can shrink an oversized file.
/// An image from `resize` asked for its own format comes back as is.
///
/// @param format string Output format: `"png"`, `"jpeg"`, or `"jpg"`.
/// @return (string) Encoded image bytes.
/// @example
/// local bytes = img:encode("png")
/// -- bytes is a Lua string containing the raw PNG data
#[lua_fn]
async fn encode(lua: Lua, this: UserDataRef<LuaImage>, format: String) -> LuaResult<LuaString> {
    let state = Arc::clone(&this.0);
    drop(this);
    let media_type = match format.as_str() {
        "png" => ImageMediaType::Png,
        "jpeg" | "jpg" => ImageMediaType::Jpeg,
        other => {
            return Err(mlua::Error::runtime(format!(
                "encode: unsupported format '{other}' (png, jpeg)"
            )));
        }
    };
    let encoded = smol::unblock(move || state.encode(media_type))
        .await
        .map_err(|e| mlua::Error::runtime(format!("encode: {e}")))?;
    lua.create_string(encoded)
}

lua_class! {
    /// An image you can inspect, resize, and re-encode.
    ///
    /// Get one from `maki.image.decode()`. It always holds encoded bytes as
    /// png, jpeg, gif, or webp, and decodes the pixels on first use. As a
    /// message image source or through `maki.json.encode` it becomes
    /// `{ type = "base64", media_type, data }`. The data lives in memory
    /// until the handle is garbage collected.
    "maki.image.Image" => LuaImage, IMAGE_DOCS [width, height, resize, encode]
}

/// Read image metadata (format, dimensions) from raw bytes without fully
/// decoding the pixels. Much faster than `decode` when you only need to
/// check the size or format.
///
/// Returns a table with `format` (string), `width` (integer), `height`
/// (integer), or `(nil, err)` if the bytes are not a recognized image.
///
/// @param data string|buffer Raw image bytes.
/// @return (table?, string?) Info table, or `(nil, err)` on failure.
/// @example
/// local info, err = maki.image.probe(raw_bytes)
/// if err then error(err) end
/// print(info.format, info.width, info.height)
#[lua_fn]
fn probe(lua: &Lua, data: LuaValue) -> LuaResult<Pair<Table>> {
    let bytes = bytes_arg(&data, "image.probe")?;
    let (format, width, height) = try_pair!(probe_bytes(&bytes));
    let info = lua.create_table()?;
    info.set("format", format_name(format))?;
    info.set("width", width)?;
    info.set("height", height)?;
    Ok((Some(info), None))
}

/// Decode raw image bytes into an Image handle you can resize and re-encode.
/// The whole image is decoded up front, so corrupt input fails here rather
/// than later. Images larger than 50 megapixels are rejected to prevent
/// memory bombs. Formats other than png, jpeg, gif, and webp (like bmp) are
/// converted to png, so the image can go into a message.
///
/// @param data string|buffer Raw image bytes.
/// @return (maki.image.Image?, string?) Decoded image, or `(nil, err)` on failure.
/// @example
/// local img, err = maki.image.decode(raw_bytes)
/// if err then error(err) end
/// print(img:width() .. "x" .. img:height())
#[lua_fn]
async fn decode(lua: Lua, data: LuaValue) -> LuaResult<Pair<AnyUserData>> {
    let bytes = bytes_arg(&data, "image.decode")?;
    let state = try_pair!(smol::unblock(move || ImageState::from_bytes(&bytes)).await);
    Ok((Some(lua.create_userdata(LuaImage(Arc::new(state)))?), None))
}

lua_table! {
    /// Small building blocks for working with images: probe metadata, decode
    /// pixels, resize, and encode back to bytes. Plugins compose these freely.
    ///
    /// Decoding is guarded against pixel-bomb attacks (50 MP limit).
    ///
    /// ```lua
    /// local img = maki.image.decode(raw_bytes)
    /// local small = img:resize(1024, 768)
    /// local png = small:encode("png")
    /// ```
    "maki.image" => pub(crate) fn create_image_table(), DOCS [
        probe, decode,
    ]
}

#[cfg(test)]
mod tests {
    use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
    use image::{Rgb, RgbImage};
    use test_case::test_case;

    use super::*;

    const GRADIENT_EDGE: u32 = 64;

    fn gradient() -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(GRADIENT_EDGE, GRADIENT_EDGE, |x, y| {
            Rgb([(x * 4) as u8, (y * 4) as u8, (x ^ y) as u8])
        }))
    }

    fn encoded(img: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), format).unwrap();
        out
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        encoded(&DynamicImage::new_rgb8(width, height), ImageFormat::Png)
    }

    /// Bytes no default re-encode reproduces, so getting them back proves
    /// the original was returned.
    fn uncompressed_png() -> Vec<u8> {
        let mut out = Vec::new();
        gradient()
            .write_with_encoder(PngEncoder::new_with_quality(
                &mut out,
                CompressionType::Uncompressed,
                PngFilter::NoFilter,
            ))
            .unwrap();
        out
    }

    /// A valid header in front of a body cut in half: probes, never decodes.
    fn truncated_png() -> Vec<u8> {
        let mut bytes = encoded(&gradient(), ImageFormat::Png);
        bytes.truncate(bytes.len() / 2);
        bytes
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ ((crc & 1) * 0xEDB8_8320);
            }
        }
        !crc
    }

    #[test]
    fn decode_rejects_pixel_bomb_before_allocating() {
        // Patch a real 1x1 PNG's IHDR to claim 10000x10000 and fix the CRC.
        // The cap must trip on the header alone, before any allocation.
        let mut bytes = png_bytes(1, 1);
        bytes[16..20].copy_from_slice(&10_000_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&10_000_u32.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());

        let (_, w, h) = probe_bytes(&bytes).expect("probe reads only the header");
        assert_eq!((w, h), (10_000, 10_000));
        let err = decode_bytes(&bytes).unwrap_err();
        assert!(err.contains("too large to decode"), "got: {err}");
    }

    #[test_case(truncated_png() ; "truncated_body")]
    #[test_case(b"definitely not an image".to_vec() ; "garbage")]
    #[test_case(Vec::new() ; "empty")]
    fn decode_rejects_corrupt_bytes(bytes: Vec<u8>) {
        assert!(ImageState::from_bytes(&bytes).is_err());
    }

    #[test]
    fn dimensions_read_the_header_without_decoding_pixels() {
        let state =
            LuaImage::from_source(source_from_bytes(ImageMediaType::Png, &truncated_png())).0;
        assert!(state.pixels().is_err(), "the body must be undecodable");
        assert_eq!(state.dimensions(), Ok((GRADIENT_EDGE, GRADIENT_EDGE)));
    }

    #[test]
    fn resize_that_fits_returns_the_same_image_undecoded() {
        let state =
            LuaImage::from_source(source_from_bytes(ImageMediaType::Png, &truncated_png())).0;
        let resized = state.resized(GRADIENT_EDGE, GRADIENT_EDGE).unwrap();
        assert!(Arc::ptr_eq(&resized, &state));
    }

    /// Bytes from outside may be oversized, so asking for their own format
    /// still re-encodes at defaults. That is how `view_image` shrinks them.
    #[test_case(true ; "decoded")]
    #[test_case(false ; "from_source")]
    fn same_format_encode_reencodes_external_bytes(decoded: bool) {
        let original = uncompressed_png();
        let image = if decoded {
            LuaImage(Arc::new(ImageState::from_bytes(&original).unwrap()))
        } else {
            LuaImage::from_source(source_from_bytes(ImageMediaType::Png, &original))
        };
        let reencoded = image.0.encode(ImageMediaType::Png).unwrap();
        assert_ne!(reencoded, original);
        assert_eq!(reencoded, encoded(&gradient(), ImageFormat::Png));
    }

    #[test]
    fn encode_after_resize_returns_the_resized_bytes() {
        let state = Arc::new(ImageState::from_bytes(&png_bytes(20, 10)).unwrap());
        let resized = state.resized(5, 5).unwrap();
        assert_eq!(
            resized.encode(ImageMediaType::Png).unwrap(),
            decode_payload(&resized.source.data).unwrap()
        );
    }

    #[test_case(ImageFormat::Png, ImageMediaType::Png ; "png_stays_png")]
    #[test_case(ImageFormat::Jpeg, ImageMediaType::Jpeg ; "jpeg_stays_jpeg")]
    #[test_case(ImageFormat::Gif, ImageMediaType::Png ; "gif_becomes_png")]
    fn resize_format_rule(source: ImageFormat, expected: ImageMediaType) {
        let state = Arc::new(ImageState::from_bytes(&encoded(&gradient(), source)).unwrap());
        let resized = state.resized(8, 8).unwrap();
        assert_eq!(resized.source.media_type, expected);
        let (format, _, _) = probe_payload(&resized.source.data).unwrap();
        assert_eq!(format, image_format(expected));
    }

    #[test]
    fn decode_transcodes_a_format_no_message_carries_to_png() {
        let state = ImageState::from_bytes(&encoded(&gradient(), ImageFormat::Bmp)).unwrap();
        assert_eq!(state.source.media_type, ImageMediaType::Png);
        assert_eq!(
            probe_payload(&state.source.data).unwrap(),
            (ImageFormat::Png, GRADIENT_EDGE, GRADIENT_EDGE)
        );
    }
}
