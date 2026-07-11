//! `maki.image`: image primitives for plugins. Neovim has no image API to
//! mirror, so these are small blocks (probe, decode, resize, encode) that
//! plugins compose; provider policy stays in Lua. Errors follow the
//! `(nil, err)` convention of `maki.fs`.

use std::io::Cursor;
use std::sync::Arc;

use image::{DynamicImage, ImageFormat, ImageReader};
use mlua::{Lua, Result as LuaResult, Table, UserData, UserDataMethods, Value as LuaValue};

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

impl UserData for LuaImage {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("width", |_, this, ()| Ok(this.0.width()));
        methods.add_method("height", |_, this, ()| Ok(this.0.height()));

        // Aspect-preserving fit inside max_w x max_h; never upscales.
        methods.add_async_method("resize", |_, this, (max_w, max_h): (u32, u32)| {
            let img = Arc::clone(&this.0);
            async move {
                if max_w == 0 || max_h == 0 {
                    return Err(mlua::Error::runtime("resize: dimensions must be positive"));
                }
                if img.width() <= max_w && img.height() <= max_h {
                    return Ok(LuaImage(img));
                }
                let resized = smol::unblock(move || {
                    img.resize(max_w, max_h, image::imageops::FilterType::Triangle)
                })
                .await;
                Ok(LuaImage(Arc::new(resized)))
            }
        });

        methods.add_async_method("encode", |lua, this, format: String| {
            let img = Arc::clone(&this.0);
            async move {
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
        });
    }
}

pub(crate) fn create_image_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "probe",
        lua.create_function(|lua, val: LuaValue| {
            let bytes = bytes_arg(&val, "image.probe")?;
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
        })?,
    )?;

    t.set(
        "decode",
        lua.create_async_function(|lua, val: LuaValue| async move {
            let bytes = bytes_arg(&val, "image.decode")?;
            match smol::unblock(move || decode_bytes(&bytes)).await {
                Ok(img) => Ok((
                    LuaValue::UserData(lua.create_userdata(LuaImage(Arc::new(img)))?),
                    LuaValue::Nil,
                )),
                Err(e) => Ok((LuaValue::Nil, LuaValue::String(lua.create_string(e)?))),
            }
        })?,
    )?;

    Ok(t)
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
