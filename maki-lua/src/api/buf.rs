use std::collections::HashMap;
use std::sync::Arc;

use maki_agent::types::InlineStyle;
use maki_agent::{SharedBuf, SnapshotLine, SnapshotSpan, SpanStyle};
use mlua::{Function, Result as LuaResult, UserData, UserDataMethods, Value as LuaValue};

use crate::runtime::{with_click_handlers, with_live_ctx};

/// `live_buf` tracks the first buffer a handler creates, which is the one
/// the runtime streams to the UI during execution.
pub(crate) struct BufferStore {
    buffers: HashMap<u32, Arc<SharedBuf>>,
    next_id: u32,
    live_buf: Option<Arc<SharedBuf>>,
}

impl BufferStore {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            next_id: 1,
            live_buf: None,
        }
    }

    pub fn create(&mut self) -> BufHandle {
        let buf = Arc::new(SharedBuf::new());
        let id = self.next_id;
        self.next_id += 1;
        self.buffers.insert(id, Arc::clone(&buf));
        BufHandle { id, buf }
    }

    pub fn create_live(&mut self) -> BufHandle {
        let handle = self.create();
        if self.live_buf.is_none() {
            self.live_buf = Some(Arc::clone(&handle.buf));
        }
        handle
    }

    #[cfg(test)]
    pub fn append_line(&mut self, id: u32, line: SnapshotLine) {
        if let Some(buf) = self.buffers.get(&id) {
            buf.append(line);
        }
    }

    #[cfg(test)]
    pub fn len(&self, id: u32) -> usize {
        self.buffers.get(&id).map_or(0, |b| b.len())
    }

    #[cfg(test)]
    pub fn take(&mut self, id: u32) -> Option<maki_agent::BufferSnapshot> {
        self.buffers.remove(&id).map(|b| b.take())
    }

    pub fn clear(&mut self) {
        self.buffers.clear();
        self.live_buf = None;
    }

    pub fn live_buf(&self) -> Option<&Arc<SharedBuf>> {
        self.live_buf.as_ref()
    }
}

#[derive(Clone)]
pub(crate) struct BufHandle {
    #[cfg_attr(not(test), allow(dead_code))]
    pub id: u32,
    pub buf: Arc<SharedBuf>,
}

impl UserData for BufHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("line", |_lua, this, arg: LuaValue| {
            let line = parse_line(&arg)?;
            this.buf.append(line);
            Ok(())
        });

        methods.add_method("lines", |_lua, this, tbl: mlua::Table| {
            let mut parsed = Vec::with_capacity(tbl.raw_len());
            for i in 1..=tbl.raw_len() {
                let val: LuaValue = tbl.raw_get(i)?;
                parsed.push(parse_line(&val)?);
            }
            for line in parsed {
                this.buf.append(line);
            }
            Ok(())
        });

        methods.add_method("set_lines", |_lua, this, tbl: mlua::Table| {
            let mut parsed = Vec::with_capacity(tbl.raw_len());
            for i in 1..=tbl.raw_len() {
                let val: LuaValue = tbl.raw_get(i)?;
                parsed.push(parse_line(&val)?);
            }
            this.buf.set_lines(parsed);
            Ok(())
        });

        methods.add_method("len", |_lua, this, ()| Ok(this.buf.len()));

        methods.add_method("on", |lua, _this, (event, callback): (String, Function)| {
            if event != "click" {
                return Err(mlua::Error::runtime(format!("unsupported event: {event}")));
            }
            let tool_id = with_live_ctx(lua, |live| live.tool_use_id.clone())
                .ok_or_else(|| mlua::Error::runtime("buf:on() requires a live tool context"))?;
            let key = lua.create_registry_value(callback)?;
            with_click_handlers(lua, |handlers| {
                if let Some(old) = handlers.insert(tool_id, key) {
                    let _ = lua.remove_registry_value(old);
                }
            });
            Ok(())
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
        let id = store.create().id;
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
        assert!(store.take(id).is_none());
    }

    #[test]
    fn take_nonexistent_id_returns_none() {
        let mut store = BufferStore::new();
        assert!(store.take(999).is_none());
    }

    #[test]
    fn append_to_nonexistent_id_is_noop() {
        let mut store = BufferStore::new();
        store.append_line(42, SnapshotLine { spans: vec![] });
        assert_eq!(store.len(42), 0);
    }

    #[test]
    fn clear_frees_all_buffers() {
        let mut store = BufferStore::new();
        let a = store.create().id;
        let b = store.create().id;
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
        assert_eq!(store.create().id, 3);
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
        assert!(parse_line(&LuaValue::Integer(42)).is_err());
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

    #[test]
    fn parse_line_empty_table_produces_empty_spans() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        let line = parse_line(&LuaValue::Table(t)).unwrap();
        assert!(line.spans.is_empty());
    }

    #[test]
    fn parse_span_rejects_non_table() {
        assert!(parse_span(&LuaValue::Boolean(true)).is_err());
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
        assert!(parse_style(&LuaValue::Integer(99)).is_err());
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
        let (handle, id) = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            let handle = store.create();
            let id = handle.id;
            (handle, id)
        };

        let ud = lua.create_userdata(handle).unwrap();
        lua.globals().set("buf", ud).unwrap();

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
        let handle = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            store.create()
        };

        let ud = lua.create_userdata(handle).unwrap();
        lua.globals().set("buf", ud).unwrap();

        lua.load(r#"buf:lines({ "a", "b", "c" })"#).exec().unwrap();
        let len: usize = lua.load("return buf:len()").eval().unwrap();
        assert_eq!(len, 3);
    }

    #[test]
    fn buf_handle_line_with_inline_style_via_lua() {
        let lua = test_lua();
        let (handle, id) = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            let handle = store.create();
            let id = handle.id;
            (handle, id)
        };

        let ud = lua.create_userdata(handle).unwrap();
        lua.globals().set("buf", ud).unwrap();

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

    #[test]
    fn create_live_and_take() {
        let mut store = BufferStore::new();
        let handle = store.create_live();
        let id = handle.id;
        store.append_line(
            id,
            SnapshotLine {
                spans: vec![SnapshotSpan {
                    text: "test".into(),
                    style: SpanStyle::Default,
                }],
            },
        );
        assert_eq!(store.len(id), 1);
        let snap = store.take(id);
        assert!(snap.is_some());
        assert_eq!(snap.unwrap().lines.len(), 1);
    }

    #[test]
    fn create_live_second_call_does_not_overwrite_first() {
        let mut store = BufferStore::new();
        let handle1 = store.create_live();
        let handle2 = store.create_live();
        assert_ne!(handle1.id, handle2.id);
        handle1.buf.append(SnapshotLine { spans: vec![] });
        let live = store.live_buf().unwrap();
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn clear_resets_live_buf() {
        let mut store = BufferStore::new();
        store.create_live();
        assert!(store.live_buf().is_some());
        store.clear();
        assert!(store.live_buf().is_none());
    }

    #[test]
    fn live_buf_returns_none_before_create_live() {
        let store = BufferStore::new();
        assert!(store.live_buf().is_none());
    }

    #[test]
    fn live_buf_reflects_writes_through_handle() {
        let mut store = BufferStore::new();
        let handle = store.create_live();
        handle.buf.append(SnapshotLine {
            spans: vec![SnapshotSpan {
                text: "via arc".into(),
                style: SpanStyle::Default,
            }],
        });
        assert_eq!(store.len(handle.id), 1);
        assert_eq!(store.live_buf().unwrap().len(), 1);
    }

    #[test]
    fn set_lines_replaces_content() {
        let lua = test_lua();
        let handle = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            store.create()
        };
        let ud = lua.create_userdata(handle).unwrap();
        lua.globals().set("buf", ud).unwrap();

        lua.load(r#"buf:lines({ "a", "b", "c", "d", "e" })"#)
            .exec()
            .unwrap();
        let len: usize = lua.load("return buf:len()").eval().unwrap();
        assert_eq!(len, 5);

        lua.load(r#"buf:set_lines({ "x", "y" })"#).exec().unwrap();
        let len: usize = lua.load("return buf:len()").eval().unwrap();
        assert_eq!(len, 2, "set_lines should replace, not append");
    }

    #[test]
    fn create_live_mixed_with_create_regular() {
        let mut store = BufferStore::new();
        let regular = store.create().id;
        let live_id = store.create_live().id;
        store.append_line(regular, SnapshotLine { spans: vec![] });
        store.append_line(live_id, SnapshotLine { spans: vec![] });
        assert_eq!(store.len(regular), 1);
        assert_eq!(store.len(live_id), 1);
        assert!(store.live_buf().is_some());
    }

    #[test]
    fn buf_on_unsupported_event_errors() {
        let lua = test_lua();
        let handle = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            store.create()
        };
        let ud = lua.create_userdata(handle).unwrap();
        lua.globals().set("buf", ud).unwrap();

        let result = lua.load(r#"buf:on("hover", function() end)"#).exec();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsupported event"), "got: {err}");
    }

    #[test]
    fn buf_on_click_without_live_ctx_errors() {
        let lua = test_lua();
        lua.set_app_data(HashMap::<String, mlua::RegistryKey>::new());
        let handle = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            store.create()
        };
        let ud = lua.create_userdata(handle).unwrap();
        lua.globals().set("buf", ud).unwrap();

        let result = lua.load(r#"buf:on("click", function() end)"#).exec();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("live tool context"), "got: {err}");
    }
}
