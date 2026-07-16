//! `maki.image`: image primitives for plugins. Neovim has no image API to
//! mirror, so these are small blocks (probe, decode, resize, encode) that
//! plugins compose; provider policy stays in Lua. Errors follow the
//! `(nil, err)` convention of `maki.fs`.

use std::io::Cursor;
use std::sync::Arc;

use image::{DynamicImage, ImageFormat, ImageReader};
use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Value as LuaValue};

use super::base64::bytes_arg;

/// Decode-bomb guard: a tiny file can declare huge dimensions and balloon
/// into gigabytes of RGBA. Host-fixed so no plugin can disable it; 50MP
/// still covers any real camera photo.
const MAX_PIXELS: u64 = 50_000_000;

fn format_name(format: ImageFormat) -> &'static str {
    match format {
        // Only jpeg deviates from its primary extension ("jpg").
        ImageFormat::Jpeg => "jpeg",
        other => other.extensions_str().first().copied().unwrap_or("unknown"),
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

fn decode_bytes(bytes: &[u8]) -> Result<DynamicImage, String> {
    let (format, width, height) = probe_bytes(bytes)?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(format!(
            "image too large to decode ({width}x{height}; limit {MAX_PIXELS} pixels)"
        ));
    }
    image::load_from_memory_with_format(bytes, format).map_err(|e| format!("cannot decode: {e}"))
}

/// Opaque decoded image. `Arc` so resize/encode can hop to a blocking thread
/// without copying pixels.
struct LuaImage(Arc<DynamicImage>);

/// Get the width of the image in pixels.
///
/// @return (integer) Width in pixels.
#[lua_fn]
fn width(_lua: &Lua, this: &LuaImage) -> LuaResult<u32> {
    Ok(this.0.width())
}

/// Get the height of the image in pixels.
///
/// @return (integer) Height in pixels.
#[lua_fn]
fn height(_lua: &Lua, this: &LuaImage) -> LuaResult<u32> {
    Ok(this.0.height())
}

/// Shrink the image to fit inside {max_w} x {max_h}, keeping the aspect
/// ratio. If the image already fits, it is returned as-is. Never upscales.
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
    this: mlua::UserDataRef<LuaImage>,
    max_w: u32,
    max_h: u32,
) -> LuaResult<LuaImage> {
    let img = Arc::clone(&this.0);
    drop(this);
    if max_w == 0 || max_h == 0 {
        return Err(mlua::Error::runtime("resize: dimensions must be positive"));
    }
    if img.width() <= max_w && img.height() <= max_h {
        return Ok(LuaImage(img));
    }
    let resized =
        smol::unblock(move || img.resize(max_w, max_h, image::imageops::FilterType::Triangle))
            .await;
    Ok(LuaImage(Arc::new(resized)))
}

/// Encode the image into raw bytes in the given format. Use this to prepare
/// images for sending over the network or writing to disk.
///
/// @param format string Output format: `"png"`, `"jpeg"`, or `"jpg"`.
/// @return (string) Encoded image bytes.
/// @example
/// local bytes = img:encode("png")
/// -- bytes is a Lua string containing the raw PNG data
#[lua_fn]
async fn encode(
    lua: Lua,
    this: mlua::UserDataRef<LuaImage>,
    format: String,
) -> LuaResult<mlua::String> {
    let img = Arc::clone(&this.0);
    drop(this);
    let out_format = match format.as_str() {
        "png" => ImageFormat::Png,
        "jpeg" | "jpg" => ImageFormat::Jpeg,
        other => {
            return Err(mlua::Error::runtime(format!(
                "encode: unsupported format '{other}' (png, jpeg)"
            )));
        }
    };
    let encoded = smol::unblock(move || {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), out_format)
            .map(|()| out)
    })
    .await
    .map_err(|e| mlua::Error::runtime(format!("encode: {e}")))?;
    lua.create_string(encoded)
}

lua_class! {
    /// A decoded image you can inspect, resize, and re-encode.
    ///
    /// Get one from `maki.image.decode()`. The image data lives in memory
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
fn probe(lua: &Lua, data: LuaValue) -> LuaResult<(LuaValue, LuaValue)> {
    let bytes = bytes_arg(&data, "image.probe")?;
    match probe_bytes(&bytes) {
        Ok((format, width, height)) => {
            let info = lua.create_table()?;
            info.set("format", format_name(format))?;
            info.set("width", width)?;
            info.set("height", height)?;
            Ok((LuaValue::Table(info), LuaValue::Nil))
        }
        Err(e) => Ok((LuaValue::Nil, LuaValue::String(lua.create_string(e)?))),
    }
}

/// Decode raw image bytes into an Image handle you can resize and re-encode.
/// Images larger than 50 megapixels are rejected to prevent memory bombs.
///
/// @param data string|buffer Raw image bytes.
/// @return (maki.image.Image?, string?) Decoded image, or `(nil, err)` on failure.
/// @example
/// local img, err = maki.image.decode(raw_bytes)
/// if err then error(err) end
/// print(img:width() .. "x" .. img:height())
#[lua_fn]
async fn decode(lua: Lua, data: LuaValue) -> LuaResult<(LuaValue, LuaValue)> {
    let bytes = bytes_arg(&data, "image.decode")?;
    match smol::unblock(move || decode_bytes(&bytes)).await {
        Ok(img) => Ok((
            LuaValue::UserData(lua.create_userdata(LuaImage(Arc::new(img)))?),
            LuaValue::Nil,
        )),
        Err(e) => Ok((LuaValue::Nil, LuaValue::String(lua.create_string(e)?))),
    }
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
    use super::*;

    // Lua-facing behavior is covered by plugins/view_image/tests/spec.lua
    // and the view_image tests in plugin_host.rs; only the host-side
    // decode-bomb guard is tested here.

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let img = DynamicImage::new_rgb8(width, height);
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
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
}
