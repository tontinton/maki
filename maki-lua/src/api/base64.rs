//! Mirrors Neovim's `vim.base64`: `encode(str)` / `decode(str)`.
//! Accepts Luau buffers as well as strings so `maki.fs.read_bytes` output
//! feeds straight in.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use mlua::{Lua, Result as LuaResult, Table, Value as LuaValue};

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

pub(crate) fn create_base64_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "encode",
        lua.create_function(|_, val: LuaValue| {
            let bytes = bytes_arg(&val, "base64.encode")?;
            Ok(BASE64.encode(bytes))
        })?,
    )?;

    t.set(
        "decode",
        lua.create_function(|lua, val: LuaValue| {
            let encoded = bytes_arg(&val, "base64.decode")?;
            let decoded = BASE64
                .decode(encoded)
                .map_err(|e| mlua::Error::runtime(format!("base64.decode: {e}")))?;
            lua.create_string(decoded)
        })?,
    )?;

    Ok(t)
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
