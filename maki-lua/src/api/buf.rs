use std::collections::HashMap;

use maki_agent::types::InlineStyle;
use maki_agent::{BufferSnapshot, SnapshotLine, SnapshotSpan, SpanStyle};
use mlua::{Result as LuaResult, UserData, UserDataMethods, Value as LuaValue};

pub(crate) struct BufferStore {
    buffers: HashMap<u32, Vec<SnapshotLine>>,
    next_id: u32,
}

impl BufferStore {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn create(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.buffers.insert(id, Vec::new());
        id
    }

    pub fn append_line(&mut self, id: u32, line: SnapshotLine) {
        if let Some(lines) = self.buffers.get_mut(&id) {
            lines.push(line);
        }
    }

    pub fn len(&self, id: u32) -> usize {
        self.buffers.get(&id).map_or(0, |l| l.len())
    }

    pub fn take(&mut self, id: u32) -> Option<BufferSnapshot> {
        self.buffers
            .remove(&id)
            .map(|lines| BufferSnapshot { lines })
    }

    pub fn clear(&mut self) {
        self.buffers.clear();
    }
}

#[derive(Clone, Copy)]
pub(crate) struct BufHandle(pub u32);

impl UserData for BufHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("line", |lua, this, arg: LuaValue| {
            let line = parse_line(&arg)?;
            let mut store = lua
                .app_data_mut::<BufferStore>()
                .ok_or_else(|| mlua::Error::runtime("buffer store not initialized"))?;
            store.append_line(this.0, line);
            Ok(())
        });

        methods.add_method("lines", |lua, this, tbl: mlua::Table| {
            let mut parsed = Vec::with_capacity(tbl.raw_len());
            for i in 1..=tbl.raw_len() {
                let val: LuaValue = tbl.raw_get(i)?;
                parsed.push(parse_line(&val)?);
            }
            let mut store = lua
                .app_data_mut::<BufferStore>()
                .ok_or_else(|| mlua::Error::runtime("buffer store not initialized"))?;
            for line in parsed {
                store.append_line(this.0, line);
            }
            Ok(())
        });

        methods.add_method("len", |lua, this, ()| {
            let store = lua
                .app_data_ref::<BufferStore>()
                .ok_or_else(|| mlua::Error::runtime("buffer store not initialized"))?;
            Ok(store.len(this.0))
        });
    }
}

pub(crate) fn parse_line(arg: &LuaValue) -> LuaResult<SnapshotLine> {
    match arg {
        LuaValue::String(s) => {
            let text = s.to_str().map_err(mlua::Error::external)?.to_owned();
            Ok(SnapshotLine {
                spans: vec![SnapshotSpan {
                    text,
                    style: SpanStyle::Default,
                }],
            })
        }
        LuaValue::Table(t) => {
            let mut spans = Vec::new();
            for i in 1..=t.raw_len() {
                let entry: LuaValue = t.raw_get(i)?;
                spans.push(parse_span(&entry)?);
            }
            Ok(SnapshotLine { spans })
        }
        _ => Err(mlua::Error::runtime(
            "line argument must be a string or table of spans",
        )),
    }
}

fn parse_span(val: &LuaValue) -> LuaResult<SnapshotSpan> {
    let LuaValue::Table(t) = val else {
        return Err(mlua::Error::runtime("span must be a table {text, style?}"));
    };
    let text_val: LuaValue = t.raw_get(1)?;
    let text = match &text_val {
        LuaValue::String(s) => s.to_str().map_err(mlua::Error::external)?.to_owned(),
        _ => return Err(mlua::Error::runtime("span[1] must be a string")),
    };
    let style_val: LuaValue = t.raw_get(2)?;
    let style = parse_style(&style_val)?;
    Ok(SnapshotSpan { text, style })
}

fn parse_style(val: &LuaValue) -> LuaResult<SpanStyle> {
    match val {
        LuaValue::Nil => Ok(SpanStyle::Default),
        v if v.is_null() => Ok(SpanStyle::Default),
        LuaValue::String(s) => {
            let name = s.to_str().map_err(mlua::Error::external)?.to_owned();
            Ok(SpanStyle::Named(name))
        }
        LuaValue::Table(t) => {
            let mut inline = InlineStyle::default();
            if let Ok(LuaValue::String(s)) = t.raw_get::<LuaValue>("fg") {
                inline.fg = parse_hex_color(&s.to_str().map_err(mlua::Error::external)?);
            }
            if let Ok(LuaValue::String(s)) = t.raw_get::<LuaValue>("bg") {
                inline.bg = parse_hex_color(&s.to_str().map_err(mlua::Error::external)?);
            }
            inline.bold = t.raw_get::<bool>("bold").unwrap_or(false);
            inline.italic = t.raw_get::<bool>("italic").unwrap_or(false);
            inline.underline = t.raw_get::<bool>("underline").unwrap_or(false);
            inline.dim = t.raw_get::<bool>("dim").unwrap_or(false);
            inline.strikethrough = t.raw_get::<bool>("strikethrough").unwrap_or(false);
            inline.reversed = t.raw_get::<bool>("reversed").unwrap_or(false);
            Ok(SpanStyle::Inline(inline))
        }
        _ => Err(mlua::Error::runtime(
            "style must be nil, a string name, or a table {fg?, bg?, bold?, ...}",
        )),
    }
}

fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn take_removes_buffer_from_store() {
        let mut store = BufferStore::new();
        let id = store.create();
        store.append_line(
            id,
            SnapshotLine {
                spans: vec![SnapshotSpan {
                    text: "hello".into(),
                    style: SpanStyle::Default,
                }],
            },
        );
        let snap = store.take(id);
        assert!(snap.is_some());
        assert_eq!(snap.unwrap().lines.len(), 1);
        assert!(store.take(id).is_none(), "second take should return None");
    }

    #[test]
    fn take_nonexistent_id_returns_none() {
        let mut store = BufferStore::new();
        assert!(store.take(999).is_none());
    }

    #[test]
    fn nonexistent_id_is_safe() {
        let mut store = BufferStore::new();
        store.append_line(42, SnapshotLine { spans: vec![] });
        assert_eq!(store.len(42), 0);
        assert_eq!(store.len(999), 0);
    }

    #[test]
    fn clear_frees_all_buffers() {
        let mut store = BufferStore::new();
        let a = store.create();
        let b = store.create();
        store.append_line(a, SnapshotLine { spans: vec![] });
        store.append_line(b, SnapshotLine { spans: vec![] });
        store.clear();
        assert!(store.take(a).is_none());
        assert!(store.take(b).is_none());
    }

    #[test]
    fn clear_does_not_reset_next_id() {
        let mut store = BufferStore::new();
        store.create();
        store.create();
        store.clear();
        assert_eq!(store.create(), 3, "id counter should not reset after clear");
    }

    #[test_case("#ff0000", Some((255, 0, 0))   ; "red")]
    #[test_case("#00ff00", Some((0, 255, 0))    ; "green")]
    #[test_case("#0000ff", Some((0, 0, 255))    ; "blue")]
    #[test_case("#AABBCC", Some((0xAA, 0xBB, 0xCC)) ; "uppercase_hex")]
    #[test_case("ff0000",  None                 ; "missing_hash_prefix")]
    #[test_case("#fff",    None                 ; "short_3_digit_hex")]
    #[test_case("#gggggg", None                 ; "invalid_hex_digits")]
    #[test_case("#ff00",   None                 ; "too_short")]
    #[test_case("#ff000000", None               ; "too_long_8_digits")]
    #[test_case("",        None                 ; "empty_string")]
    fn hex_color_parsing(input: &str, expected: Option<(u8, u8, u8)>) {
        assert_eq!(parse_hex_color(input), expected);
    }

    fn test_lua() -> mlua::Lua {
        let lua = mlua::Lua::new();
        lua.set_app_data(BufferStore::new());
        lua
    }

    #[test]
    fn parse_line_plain_string() {
        let lua = test_lua();
        let val = lua.create_string("hello world").unwrap();
        let line = parse_line(&LuaValue::String(val)).unwrap();
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "hello world");
        assert_eq!(line.spans[0].style, SpanStyle::Default);
    }

    #[test]
    fn parse_line_rejects_non_string_non_table() {
        let result = parse_line(&LuaValue::Integer(42));
        assert!(result.is_err());
    }

    #[test]
    fn parse_line_styled_spans() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        let span1 = lua.create_table().unwrap();
        span1.raw_set(1, "fn ").unwrap();
        span1.raw_set(2, "keyword").unwrap();
        let span2 = lua.create_table().unwrap();
        span2.raw_set(1, "main()").unwrap();
        t.raw_set(1, span1).unwrap();
        t.raw_set(2, span2).unwrap();

        let line = parse_line(&LuaValue::Table(t)).unwrap();
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].text, "fn ");
        assert_eq!(line.spans[0].style, SpanStyle::Named("keyword".into()));
        assert_eq!(line.spans[1].text, "main()");
        assert_eq!(line.spans[1].style, SpanStyle::Default);
    }

    #[test_case(LuaValue::Boolean(true) ; "rejects_non_table")]
    fn parse_span_rejects_invalid(val: LuaValue) {
        assert!(parse_span(&val).is_err());
    }

    #[test]
    fn parse_span_rejects_non_string_text() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        t.raw_set(1, 42).unwrap();
        assert!(parse_span(&LuaValue::Table(t)).is_err());
    }

    #[test]
    fn parse_style_inline_table() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        t.raw_set("fg", "#ff8000").unwrap();
        t.raw_set("bold", true).unwrap();
        t.raw_set("dim", true).unwrap();
        let style = parse_style(&LuaValue::Table(t)).unwrap();
        match style {
            SpanStyle::Inline(ref i) => {
                assert_eq!(i.fg, Some((255, 128, 0)));
                assert!(i.bold);
                assert!(i.dim);
                assert!(!i.italic);
                assert!(i.bg.is_none());
            }
            _ => panic!("expected inline style"),
        }
    }

    #[test]
    fn parse_style_invalid_hex_color_treated_as_none() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        t.raw_set("fg", "not_a_color").unwrap();
        let style = parse_style(&LuaValue::Table(t)).unwrap();
        match style {
            SpanStyle::Inline(ref i) => assert!(i.fg.is_none()),
            _ => panic!("expected inline style"),
        }
    }

    #[test]
    fn parse_style_rejects_integer() {
        let result = parse_style(&LuaValue::Integer(99));
        assert!(result.is_err());
    }

    #[test]
    fn parse_line_empty_table_produces_empty_spans() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        let line = parse_line(&LuaValue::Table(t)).unwrap();
        assert!(line.spans.is_empty());
    }

    #[test]
    fn parse_style_empty_table_produces_default_inline() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        let style = parse_style(&LuaValue::Table(t)).unwrap();
        assert_eq!(style, SpanStyle::Inline(InlineStyle::default()));
    }

    #[test]
    fn buf_handle_line_and_len_via_lua() {
        let lua = test_lua();
        let mut store = lua.app_data_mut::<BufferStore>().unwrap();
        let id = store.create();
        drop(store);

        let handle = lua.create_userdata(BufHandle(id)).unwrap();
        lua.globals().set("buf", handle).unwrap();

        lua.load(r#"buf:line("hello")"#).exec().unwrap();
        lua.load(r#"buf:line({ { "styled", "dim" } })"#)
            .exec()
            .unwrap();

        let len: usize = lua.load("return buf:len()").eval().unwrap();
        assert_eq!(len, 2);

        let store = lua.app_data_ref::<BufferStore>().unwrap();
        assert_eq!(store.len(id), 2);
    }

    #[test]
    fn buf_handle_lines_adds_multiple() {
        let lua = test_lua();
        let mut store = lua.app_data_mut::<BufferStore>().unwrap();
        let id = store.create();
        drop(store);

        let handle = lua.create_userdata(BufHandle(id)).unwrap();
        lua.globals().set("buf", handle).unwrap();

        lua.load(r#"buf:lines({ "a", "b", "c" })"#).exec().unwrap();
        let len: usize = lua.load("return buf:len()").eval().unwrap();
        assert_eq!(len, 3);
    }

    #[test]
    fn buf_handle_line_with_inline_style_via_lua() {
        let lua = test_lua();
        let mut store = lua.app_data_mut::<BufferStore>().unwrap();
        let id = store.create();
        drop(store);

        let handle = lua.create_userdata(BufHandle(id)).unwrap();
        lua.globals().set("buf", handle).unwrap();

        lua.load(r##"buf:line({ { "ERROR", { fg = "#ff0000", bold = true } } })"##)
            .exec()
            .unwrap();

        let mut store = lua.app_data_mut::<BufferStore>().unwrap();
        let snap = store.take(id).unwrap();
        assert_eq!(snap.lines.len(), 1);
        assert_eq!(snap.lines[0].spans[0].text, "ERROR");
        match &snap.lines[0].spans[0].style {
            SpanStyle::Inline(i) => {
                assert_eq!(i.fg, Some((255, 0, 0)));
                assert!(i.bold);
            }
            other => panic!("expected inline style, got {other:?}"),
        }
    }
}
