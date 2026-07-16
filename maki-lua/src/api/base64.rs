//! Base64 encoding and decoding, modelled after `vim.base64`.
//! Accepts both strings and Luau buffers, so you can pipe
//! `maki.fs.read_bytes` output straight into `encode`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Value as LuaValue};

pub(crate) fn bytes_arg(val: &LuaValue, what: &str) -> LuaResult<Vec<u8>> {
    match val {
        LuaValue::String(s) => Ok(s.as_bytes().to_vec()),
        LuaValue::Buffer(b) => Ok(b.to_vec()),
        _ => Err(mlua::Error::runtime(format!(
            "{what}: expected string or buffer, got {}",
            val.type_name()
        ))),
    }
}

/// Encode {data} to standard Base64. Like `vim.base64.encode`.
/// Accepts both strings and Luau buffers.
///
/// @param data string|buffer Data to encode.
/// @return (string) Base64-encoded string.
/// @example
/// maki.base64.encode("hello") -- "aGVsbG8="
#[lua_fn]
fn encode(_lua: &Lua, data: LuaValue) -> LuaResult<String> {
    let bytes = bytes_arg(&data, "base64.encode")?;
    Ok(BASE64.encode(bytes))
}

/// Decode a Base64-encoded {str} back to its original bytes. Like `vim.base64.decode`.
/// Throws if {str} is not valid Base64.
///
/// @param str string|buffer Base64-encoded text.
/// @return (string) Decoded bytes as a string.
/// @example
/// maki.base64.decode("aGVsbG8=") -- "hello"
#[lua_fn]
fn decode(lua: &Lua, str: LuaValue) -> LuaResult<mlua::String> {
    let encoded = bytes_arg(&str, "base64.decode")?;
    let decoded = BASE64
        .decode(encoded)
        .map_err(|e| mlua::Error::runtime(format!("base64.decode: {e}")))?;
    lua.create_string(decoded)
}

lua_table! {
    /// Base64 encoding and decoding, modelled after `vim.base64`.
    ///
    /// Both functions accept strings and Luau buffers, so you can round-trip
    /// binary data read with `maki.fs.read_bytes`.
    ///
    /// ```lua
    /// local encoded = maki.base64.encode("hello")
    /// local decoded = maki.base64.decode(encoded)
    /// ```
    "maki.base64" => pub(crate) fn create_base64_table(), DOCS [
        encode, decode,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_binary_is_byte_safe() {
        let lua = Lua::new();
        let t = create_base64_table(&lua).unwrap();
        let encode: mlua::Function = t.get("encode").unwrap();
        let decode: mlua::Function = t.get("decode").unwrap();

        // Non-UTF8 bytes; "AJ+Slg==" pins the standard (not url-safe) alphabet.
        let bytes = [0u8, 159, 146, 150];
        let encoded: String = encode.call(lua.create_string(bytes).unwrap()).unwrap();
        assert_eq!(encoded, "AJ+Slg==");
        let decoded: mlua::String = decode.call(encoded).unwrap();
        assert_eq!(&*decoded.as_bytes(), &bytes);
    }

    #[test]
    fn decode_invalid_errors() {
        let lua = Lua::new();
        let t = create_base64_table(&lua).unwrap();
        let decode: mlua::Function = t.get("decode").unwrap();
        assert!(decode.call::<mlua::String>("!!!not base64!!!").is_err());
    }
}
