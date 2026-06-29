use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use humantime::format_duration;
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::util::command::{
    Anchor, Border, Dimension, FloatConfig, HintEntries, HintWriter, Split, TitlePos, UiAction,
    WinCommand, WinEvent,
};
pub(crate) mod buf;
pub(crate) mod win;

use crate::runtime::with_task_bufs;
use win::WinHandle;

pub(crate) struct HintStore {
    hints: BTreeMap<Arc<str>, Vec<(String, String)>>,
}

impl HintStore {
    pub fn new() -> Self {
        Self {
            hints: BTreeMap::new(),
        }
    }

    pub fn set(&mut self, plugin: Arc<str>, spans: Vec<(String, String)>) {
        if spans.is_empty() {
            self.hints.remove(&plugin);
        } else {
            self.hints.insert(plugin, spans);
        }
    }

    pub fn clear_plugin(&mut self, plugin: &str) {
        self.hints.retain(|k, _| k.as_ref() != plugin);
    }

    pub fn snapshot_entries(&self) -> HintEntries {
        self.hints
            .iter()
            .map(|(k, v)| (Arc::clone(k), v.clone()))
            .collect()
    }
}

fn publish_hint_snapshot(lua: &Lua) {
    if let Some(store) = lua.app_data_ref::<HintStore>() {
        let entries = store.snapshot_entries();
        if let Some(writer) = lua.app_data_ref::<HintWriter>() {
            writer.publish(entries);
        }
    }
}

pub(crate) fn parse_footer(tbl: &Table) -> LuaResult<Vec<(String, String)>> {
    let footer_tbl: Table = match tbl.get("footer") {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()),
    };
    footer_tbl
        .sequence_values::<Table>()
        .map(|entry| {
            let entry = entry?;
            Ok((entry.get(1)?, entry.get(2)?))
        })
        .collect()
}

pub(crate) fn create_ui_table(
    lua: &Lua,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    plugin: Arc<str>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    t.set(
        "buf",
        lua.create_function(|lua, ()| Ok(with_task_bufs(lua, |store| store.create_live())))?,
    )?;
    t.set(
        "theme_color",
        lua.create_function(|lua, name: String| {
            let Some((r, g, b)) = maki_highlight::theme_color(&name) else {
                return Ok(mlua::Value::Nil);
            };
            Ok(mlua::Value::String(
                lua.create_string(format!("#{r:02x}{g:02x}{b:02x}"))?,
            ))
        })?,
    )?;
    t.set(
        "highlight",
        lua.create_async_function(
            |lua, (code, lang, opts): (String, String, Option<mlua::Table>)| async move {
                let independent = opts
                    .as_ref()
                    .and_then(|t| t.get::<bool>("independent").ok())
                    .unwrap_or(false);
                let prefix = opts
                    .and_then(|t| t.get::<String>("prefix").ok())
                    .unwrap_or_default();
                let segments = smol::unblock(move || {
                    if independent {
                        maki_highlight::highlight_lines_independent(&lang, &code)
                    } else {
                        maki_highlight::highlight_code(&lang, &code, &prefix)
                    }
                })
                .await;
                segments_to_lua_lines(&lua, &segments)
            },
        )?,
    )?;
    // maki.ui.markdown(text, width) -> lines, each `{ {text, style}, ... }`.
    // Async so the expensive code-block highlighting never blocks the UI.
    //
    // A style is a named string, resolved late at paint time so a theme
    // switch repaints without re-rendering. Plugins should lean on the
    // semantic palette ("accent", "active", "selected", "success", ...) since
    // those names stay stable; see `theme::style_by_name` for the full set.
    // Syntax tokens instead carry a `{fg, bold, italic, underline}` table.
    t.set(
        "markdown",
        lua.create_async_function(|lua, (text, width): (String, u16)| async move {
            let lines = smol::unblock(move || maki_markdown::render::render(&text, width)).await;
            markdown_lines_to_lua(&lua, &lines)
        })?,
    )?;
    t.set(
        "humantime",
        lua.create_function(|_, secs: u64| {
            Ok(format_duration(Duration::from_secs(secs))
                .to_string()
                .replace(' ', ""))
        })?,
    )?;

    t.set(
        "terminal_size",
        lua.create_function(|lua, ()| {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let tbl = lua.create_table()?;
            tbl.set("cols", cols)?;
            tbl.set("rows", rows)?;
            Ok(tbl)
        })?,
    )?;

    if let Some(tx) = ui_action_tx {
        let flash_tx = tx.clone();
        t.set(
            "flash",
            lua.create_function(move |_, msg: String| {
                let _ = flash_tx.try_send(UiAction::Flash(msg));
                Ok(())
            })?,
        )?;

        let editor_tx = tx.clone();
        t.set(
            "open_editor",
            lua.create_async_function(move |_, path: String| {
                let tx = editor_tx.clone();
                async move {
                    let (reply_tx, reply_rx) = flume::bounded::<i32>(1);
                    if tx
                        .try_send(UiAction::OpenEditor {
                            path: PathBuf::from(path),
                            reply_tx,
                        })
                        .is_err()
                    {
                        return Ok(-1);
                    }
                    Ok(reply_rx.recv_async().await.unwrap_or(-1))
                }
            })?,
        )?;

        let open_win_tx = tx;
        t.set(
            "open_win",
            lua.create_function(
                move |_lua, (buf_ud, opts_tbl): (mlua::AnyUserData, Table)| {
                    let buf_handle = buf_ud.borrow::<buf::BufHandle>()?;
                    let title: String = opts_tbl.get("title").unwrap_or_default();
                    let cursor_line: bool = opts_tbl.get("cursor_line").unwrap_or(false);
                    let footer = parse_footer(&opts_tbl)?;
                    let reserved_bottom: usize = opts_tbl.get("reserved_bottom").unwrap_or(0);
                    let reserved_top: usize = opts_tbl.get("reserved_top").unwrap_or(0);
                    let focus: bool = opts_tbl
                        .get::<Option<bool>>("focus")
                        .ok()
                        .flatten()
                        .unwrap_or(true);
                    let zindex: u16 = opts_tbl.get("zindex").unwrap_or(50);

                    let width = parse_dimension(&opts_tbl, "width", Dimension::Percent(60));
                    let height = parse_dimension(&opts_tbl, "height", Dimension::Percent(70));
                    let row: Option<i16> = opts_tbl.get("row").ok();
                    let col: Option<i16> = opts_tbl.get("col").ok();
                    let anchor = parse_anchor(&opts_tbl);
                    let border = parse_border(&opts_tbl);
                    let title_pos = parse_title_pos(&opts_tbl);
                    let split = parse_split(&opts_tbl);
                    let order: u16 = opts_tbl.get("order").unwrap_or(50);
                    let visible: bool = opts_tbl.get("visible").unwrap_or(true);

                    let config = FloatConfig {
                        width,
                        height,
                        row,
                        col,
                        anchor,
                        border,
                        title,
                        title_pos,
                        footer,
                        zindex,
                        cursor_line,
                        reserved_bottom,
                        reserved_top,
                        split,
                        order,
                        visible,
                    };

                    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let border_chrome = match config.border {
                        Border::None => 0,
                        _ => 2,
                    };
                    let est_w = config
                        .width
                        .resolve(term_cols)
                        .saturating_sub(border_chrome);
                    let est_h = config
                        .height
                        .resolve(term_rows)
                        .saturating_sub(border_chrome);

                    let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
                    let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);

                    let _ = open_win_tx.try_send(UiAction::OpenWin {
                        buf: buf_handle.buf.clone(),
                        config,
                        focus,
                        event_tx,
                        cmd_rx,
                    });

                    Ok(WinHandle::new(event_rx, cmd_tx, est_w, est_h, visible))
                },
            )?,
        )?;
    }

    let p = Arc::clone(&plugin);
    t.set(
        "set_status_hint",
        lua.create_function(move |lua, value: mlua::Value| {
            match value {
                mlua::Value::Nil => {
                    if let Some(mut store) = lua.app_data_mut::<HintStore>() {
                        store.clear_plugin(&p);
                    }
                }
                mlua::Value::Table(tbl) => {
                    let spans: Vec<(String, String)> = tbl
                        .sequence_values::<Table>()
                        .map(|entry| {
                            let entry = entry?;
                            Ok((
                                entry.get::<String>(1)?,
                                entry.get::<String>(2).unwrap_or_default(),
                            ))
                        })
                        .collect::<LuaResult<_>>()?;
                    if let Some(mut store) = lua.app_data_mut::<HintStore>() {
                        store.set(Arc::clone(&p), spans);
                    }
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "set_status_hint expects a table or nil",
                    ));
                }
            }
            publish_hint_snapshot(lua);
            Ok(())
        })?,
    )?;

    Ok(t)
}

pub(crate) fn try_parse_dimension(tbl: &Table, key: &str) -> Option<Dimension> {
    if let Ok(s) = tbl.get::<String>(key)
        && let Some(pct) = s.strip_suffix('%')
        && let Ok(v) = pct.parse::<u16>()
    {
        return Some(Dimension::Percent(v));
    }
    if let Ok(v) = tbl.get::<u16>(key) {
        return Some(Dimension::Abs(v));
    }
    None
}

pub(crate) fn parse_dimension(tbl: &Table, key: &str, default: Dimension) -> Dimension {
    try_parse_dimension(tbl, key).unwrap_or(default)
}

fn parse_anchor(tbl: &Table) -> Anchor {
    tbl.get::<String>("anchor")
        .map(|s| Anchor::parse(&s))
        .unwrap_or_default()
}

fn parse_split(tbl: &Table) -> Split {
    tbl.get::<String>("split")
        .map(|s| Split::parse(&s))
        .unwrap_or_default()
}

fn parse_border(tbl: &Table) -> Border {
    tbl.get::<String>("border")
        .map(|s| Border::parse(&s))
        .unwrap_or_default()
}

fn parse_title_pos(tbl: &Table) -> TitlePos {
    tbl.get::<String>("title_pos")
        .map(|s| TitlePos::parse(&s))
        .unwrap_or_default()
}

fn segments_to_lua_lines(
    lua: &Lua,
    lines: &[Vec<maki_highlight::StyledSegment>],
) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, segs) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(segs.len(), 0)?;
        for (j, seg) in segs.iter().enumerate() {
            let span = lua.create_table_with_capacity(2, 0)?;
            span.raw_set(1, seg.text.as_str())?;
            let style = lua.create_table_with_capacity(0, 4)?;
            let (r, g, b) = seg.fg;
            style.raw_set("fg", format!("#{r:02x}{g:02x}{b:02x}"))?;
            if seg.bold {
                style.raw_set("bold", true)?;
            }
            if seg.italic {
                style.raw_set("italic", true)?;
            }
            if seg.underline {
                style.raw_set("underline", true)?;
            }
            span.raw_set(2, style)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

/// Most spans become a named style string. `Highlight` tokens carry their
/// own rgb, so they become an inline `{fg, bold, italic, underline}` table.
///
/// No wildcard arm on `StyleToken`: adding a variant is a compile error
/// here, so we can't forget to map it.
fn span_style_to_lua(lua: &Lua, span: &maki_markdown::render::Span) -> LuaResult<mlua::Value> {
    use maki_markdown::render::StyleToken;

    let v = match &span.style {
        StyleToken::Text => {
            let name = emphasis_style_name(span.emphasis);
            mlua::Value::String(lua.create_string(name)?)
        }
        StyleToken::InlineCode => mlua::Value::String(lua.create_string("inline_code")?),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let tbl = lua.create_table()?;
            tbl.set("fg", format!("#{:02x}{:02x}{:02x}", fg.0, fg.1, fg.2))?;
            if *bold {
                tbl.set("bold", true)?;
            }
            if *italic {
                tbl.set("italic", true)?;
            }
            if *underline {
                tbl.set("underline", true)?;
            }
            mlua::Value::Table(tbl)
        }
        StyleToken::CodeBar => mlua::Value::String(lua.create_string("code_gutter")?),
        StyleToken::Heading => mlua::Value::String(lua.create_string("heading")?),
        StyleToken::ListMarker => mlua::Value::String(lua.create_string("list_marker")?),
        StyleToken::TableBorder => mlua::Value::String(lua.create_string("table_border")?),
        StyleToken::HorizontalRule => mlua::Value::String(lua.create_string("horizontal_rule")?),
    };
    Ok(v)
}

/// Flatten `Emphasis` to a single named style. Strike wins over bold/italic
/// (the Lua theme has no combined slot). Underline only appears in
/// `Highlight` tokens, not here.
fn emphasis_style_name(e: maki_markdown::Emphasis) -> &'static str {
    if e.strike {
        "strikethrough"
    } else if e.bold && e.italic {
        "bold_italic"
    } else if e.bold {
        "bold"
    } else if e.italic {
        "italic"
    } else {
        ""
    }
}

fn markdown_lines_to_lua(lua: &Lua, lines: &[maki_markdown::render::Line]) -> LuaResult<Table> {
    let result = lua.create_table_with_capacity(lines.len(), 0)?;
    for (i, rendered) in lines.iter().enumerate() {
        let line_tbl = lua.create_table_with_capacity(rendered.spans.len(), 0)?;
        for (j, sp) in rendered.spans.iter().enumerate() {
            let span_tbl = lua.create_table_with_capacity(2, 0)?;
            span_tbl.raw_set(1, sp.text.as_str())?;
            span_tbl.raw_set(2, span_style_to_lua(lua, sp)?)?;
            line_tbl.raw_set(i32::try_from(j + 1).unwrap(), span_tbl)?;
        }
        result.raw_set(i32::try_from(i + 1).unwrap(), line_tbl)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_highlight::StyledSegment;
    use mlua::Lua;
    use test_case::test_case;

    const MISSING_KEY: &str = "missing";
    const ORANGE_HEX: &str = "#ff8000";

    fn footer_entry(lua: &Lua, key: &str, label: &str) -> Table {
        let t = lua.create_table().unwrap();
        t.raw_set(1, key).unwrap();
        t.raw_set(2, label).unwrap();
        t
    }

    #[test]
    fn parse_footer_missing_returns_empty() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert!(parse_footer(&tbl).unwrap().is_empty());
    }

    #[test]
    fn parse_footer_non_table_value_returns_empty() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("footer", "not a table").unwrap();
        assert!(parse_footer(&tbl).unwrap().is_empty());
    }

    #[test]
    fn parse_footer_preserves_entry_order() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        entries.raw_set(1, footer_entry(&lua, "q", "quit")).unwrap();
        entries.raw_set(2, footer_entry(&lua, "j", "down")).unwrap();
        entries.raw_set(3, footer_entry(&lua, "k", "up")).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        let parsed = parse_footer(&tbl).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("q".into(), "quit".into()),
                ("j".into(), "down".into()),
                ("k".into(), "up".into()),
            ]
        );
    }

    #[test]
    fn parse_footer_missing_label_errors() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        let one_elem = lua.create_table().unwrap();
        one_elem.raw_set(1, "q").unwrap();
        entries.raw_set(1, one_elem).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        assert!(parse_footer(&tbl).is_err());
    }

    #[test]
    fn parse_footer_non_string_element_errors() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let entries = lua.create_table().unwrap();
        let bad = lua.create_table().unwrap();
        bad.raw_set(1, "q").unwrap();
        bad.raw_set(2, lua.create_table().unwrap()).unwrap();
        entries.raw_set(1, bad).unwrap();
        tbl.raw_set("footer", entries).unwrap();

        assert!(parse_footer(&tbl).is_err());
    }

    #[test]
    fn try_parse_dimension_numeric_is_abs() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", 42u16).unwrap();
        assert_eq!(try_parse_dimension(&tbl, "width"), Some(Dimension::Abs(42)));
    }

    #[test_case("0%", Dimension::Percent(0) ; "zero_percent")]
    #[test_case("50%", Dimension::Percent(50) ; "half_percent")]
    #[test_case("100%", Dimension::Percent(100) ; "full_percent")]
    #[test_case("200%", Dimension::Percent(200) ; "over_hundred_accepted")]
    fn try_parse_dimension_percent_strings(input: &str, expected: Dimension) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", input).unwrap();
        assert_eq!(try_parse_dimension(&tbl, "width"), Some(expected));
    }

    #[test]
    fn try_parse_dimension_missing_key_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert!(try_parse_dimension(&tbl, MISSING_KEY).is_none());
    }

    #[test]
    fn try_parse_dimension_non_numeric_string_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "abc").unwrap();
        assert!(try_parse_dimension(&tbl, "width").is_none());
    }

    #[test]
    fn try_parse_dimension_malformed_percent_is_none() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "xx%").unwrap();
        assert!(try_parse_dimension(&tbl, "width").is_none());
    }

    #[test]
    fn parse_dimension_missing_key_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        let default = Dimension::Percent(60);
        assert_eq!(parse_dimension(&tbl, MISSING_KEY, default), default);
    }

    #[test]
    fn parse_dimension_invalid_value_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("width", "garbage").unwrap();
        let default = Dimension::Abs(80);
        assert_eq!(parse_dimension(&tbl, "width", default), default);
    }

    #[test_case("NW", Anchor::NW ; "nw")]
    #[test_case("NE", Anchor::NE ; "ne")]
    #[test_case("SW", Anchor::SW ; "sw")]
    #[test_case("SE", Anchor::SE ; "se")]
    #[test_case("garbage", Anchor::NW ; "invalid_falls_back_to_default")]
    fn parse_anchor_cases(input: &str, expected: Anchor) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("anchor", input).unwrap();
        assert_eq!(parse_anchor(&tbl), expected);
    }

    #[test]
    fn parse_anchor_missing_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert_eq!(parse_anchor(&tbl), Anchor::default());
    }

    #[test_case("none", Border::None ; "none")]
    #[test_case("single", Border::Single ; "single")]
    #[test_case("double", Border::Double ; "double")]
    #[test_case("rounded", Border::Rounded ; "rounded")]
    #[test_case("garbage", Border::Rounded ; "invalid_falls_back_to_default")]
    fn parse_border_cases(input: &str, expected: Border) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("border", input).unwrap();
        assert_eq!(parse_border(&tbl), expected);
    }

    #[test]
    fn parse_border_missing_uses_default() {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        assert_eq!(parse_border(&tbl), Border::default());
    }

    #[test_case("left", TitlePos::Left ; "left")]
    #[test_case("center", TitlePos::Center ; "center")]
    #[test_case("right", TitlePos::Right ; "right")]
    #[test_case("garbage", TitlePos::Left ; "invalid_falls_back_to_default")]
    fn parse_title_pos_cases(input: &str, expected: TitlePos) {
        let lua = Lua::new();
        let tbl = lua.create_table().unwrap();
        tbl.raw_set("title_pos", input).unwrap();
        assert_eq!(parse_title_pos(&tbl), expected);
    }

    fn seg(text: &str, bold: bool) -> StyledSegment {
        StyledSegment {
            text: text.into(),
            fg: (255, 128, 0),
            bold,
            italic: false,
            underline: false,
        }
    }

    #[test]
    fn segments_to_lua_lines_empty_input() {
        let lua = Lua::new();
        let result = segments_to_lua_lines(&lua, &[]).unwrap();
        assert_eq!(result.len().unwrap(), 0);
    }

    #[test]
    fn segments_to_lua_lines_shape_and_fg_hex() {
        let lua = Lua::new();
        let lines = vec![vec![seg("fn ", true), seg("main", false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();

        assert_eq!(result.len().unwrap(), 1);
        let line: Table = result.get(1).unwrap();
        assert_eq!(line.len().unwrap(), 2);

        let span: Table = line.get(1).unwrap();
        let text: String = span.get(1).unwrap();
        assert_eq!(text, "fn ");
        let style: Table = span.get(2).unwrap();
        let fg: String = style.get("fg").unwrap();
        assert_eq!(fg, ORANGE_HEX);
        let bold: bool = style.get("bold").unwrap();
        assert!(bold);
        assert!(style.get::<Option<bool>>("italic").unwrap().is_none());

        let span2: Table = line.get(2).unwrap();
        let text2: String = span2.get(1).unwrap();
        assert_eq!(text2, "main");
        let style2: Table = span2.get(2).unwrap();
        assert!(style2.get::<Option<bool>>("bold").unwrap().is_none());
    }

    #[test]
    fn segments_to_lua_lines_preserves_utf8() {
        let lua = Lua::new();
        let utf8 = "héllo 🦀 ✨";
        let lines = vec![vec![seg(utf8, false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        let line: Table = result.get(1).unwrap();
        let span: Table = line.get(1).unwrap();
        let text: String = span.get(1).unwrap();
        assert_eq!(text, utf8);
    }

    const STYLE_BOLD: &str = "bold";
    const STYLE_BOLD_ITALIC: &str = "bold_italic";
    const STYLE_HEADING: &str = "heading";
    const STYLE_LIST_MARKER: &str = "list_marker";
    const STYLE_HR: &str = "horizontal_rule";
    const STYLE_PLAIN: &str = "";
    const STYLE_CODE: &str = "inline_code";
    const STYLE_CODE_BAR: &str = "code_gutter";
    const STYLE_ITALIC: &str = "italic";
    const STYLE_STRIKE: &str = "strikethrough";
    const STYLE_TABLE_BORDER: &str = "table_border";
    const MD_WIDTH: u16 = 80;

    fn render_markdown(lua: &Lua, input: &str) -> Table {
        let lines = maki_markdown::render::render(input, MD_WIDTH);
        markdown_lines_to_lua(lua, &lines).unwrap()
    }

    fn span_style(line: &Table, idx: usize) -> String {
        let span: Table = line.get(idx).unwrap();
        match span.get::<mlua::Value>(2).unwrap() {
            mlua::Value::String(s) => s.to_str().unwrap().to_string(),
            other => panic!("expected string style, got {other:?}"),
        }
    }

    fn span_text(line: &Table, idx: usize) -> String {
        let span: Table = line.get(idx).unwrap();
        span.get::<String>(1).unwrap()
    }

    #[test]
    fn markdown_returns_named_styles() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "hello **world**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "hello ");
        assert_eq!(span_style(&line, 1), STYLE_PLAIN);
        assert_eq!(span_text(&line, 2), "world");
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
    }

    #[test]
    fn markdown_bold_italic_emits_bold_italic_not_collapsed_to_bold() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "***x***");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_BOLD_ITALIC);
    }

    #[test]
    fn markdown_unknown_constructs_fall_through_as_plain() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "a*b");
        let line: Table = result.get(1).unwrap();
        for i in 1..=line.len().unwrap() {
            assert_eq!(span_style(&line, i as usize), STYLE_PLAIN);
        }
    }

    #[test]
    fn markdown_code_span_uses_inline_code_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "x `y` z");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 2), STYLE_CODE);
    }

    #[test]
    fn markdown_heading_overrides_inline_emphasis_with_heading_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# hello **world**");
        let line: Table = result.get(1).unwrap();
        for i in 1..=line.len().unwrap() {
            assert_eq!(
                span_style(&line, i as usize),
                STYLE_HEADING,
                "span {i} should be heading-styled"
            );
        }
    }

    #[test]
    fn markdown_list_marker_styled_separately_from_item_content() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "- **item**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_text(&line, 1), "• ");
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
        assert_eq!(span_text(&line, 2), "item");
    }

    #[test]
    fn markdown_horizontal_rule_emits_hr_span_filled_to_width() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "---");
        let line: Table = result.get(1).unwrap();
        assert_eq!(line.len().unwrap(), 1);
        assert_eq!(span_style(&line, 1), STYLE_HR);
        let text = span_text(&line, 1);
        assert_eq!(text.chars().count(), MD_WIDTH as usize);
        assert!(text.chars().all(|c| c == '─'));
    }

    #[test]
    fn markdown_code_inside_bold_collapses_to_inline_code_at_lua_boundary() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "**`code`**");
        let line: Table = result.get(1).unwrap();
        // Lua only sees one style name per span, so code wins over bold.
        // Rust renderers see both axes through the typed model.
        assert_eq!(span_style(&line, 1), STYLE_CODE);
    }

    #[test]
    fn markdown_multiline_emits_one_lua_line_per_logical_line() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "line one\nline two\nline three");
        assert_eq!(result.len().unwrap(), 3);
        let l1: Table = result.get(1).unwrap();
        let l2: Table = result.get(2).unwrap();
        let l3: Table = result.get(3).unwrap();
        assert_eq!(span_text(&l1, 1), "line one");
        assert_eq!(span_text(&l2, 1), "line two");
        assert_eq!(span_text(&l3, 1), "line three");
    }

    #[test]
    fn markdown_italic_alone_surfaces_as_italic_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "*italic*");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "italic");
        assert_eq!(span_style(&line, 1), STYLE_ITALIC);
    }

    #[test]
    fn markdown_strikethrough_surfaces_as_strikethrough_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "~~gone~~");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "gone");
        assert_eq!(span_style(&line, 1), STYLE_STRIKE);
    }

    #[test]
    fn markdown_ordered_list_marker_text_and_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "1. foo");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_text(&line, 1), "1. ");
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_text(&line, 2), "foo");
        assert_eq!(span_style(&line, 2), STYLE_PLAIN);
    }

    #[test]
    fn markdown_ordered_list_marker_keeps_list_marker_style_with_bold_content() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "1. **item**");
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_LIST_MARKER);
        assert_eq!(span_style(&line, 2), STYLE_BOLD);
        assert_eq!(span_text(&line, 2), "item");
    }

    #[test]
    fn markdown_code_fence_emits_code_bar_prefix_with_highlight_span_tables() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "```rust\nfn x() {}\n```");
        let lines = result.len().unwrap();
        let code_line: Table = (1..=lines)
            .find_map(|i| {
                let line: Table = result.get(i).ok()?;
                (line.len().ok()? > 0
                    && line
                        .get::<Table>(1)
                        .and_then(|s| s.get::<String>(2))
                        .ok()
                        .is_some_and(|s| s == STYLE_CODE_BAR))
                .then_some(line)
            })
            .expect("code bar line");
        assert_eq!(span_style(&code_line, 1), STYLE_CODE_BAR);
        let content_span: Table = code_line.get(2).unwrap();
        let style = content_span.get::<mlua::Value>(2).unwrap();
        assert!(
            matches!(style, mlua::Value::Table(_)),
            "highlight span style must be an inline table"
        );
    }

    #[test_case("# a" ; "h1")]
    #[test_case("## a" ; "h2")]
    #[test_case("### a" ; "h3")]
    #[test_case("###### a" ; "h6")]
    fn markdown_heading_levels_all_surface_as_heading_style(input: &str) {
        let lua = Lua::new();
        let result = render_markdown(&lua, input);
        let line: Table = result.get(1).unwrap();
        assert_eq!(span_style(&line, 1), STYLE_HEADING);
    }

    fn seg_full(text: &str, bold: bool, italic: bool, underline: bool) -> StyledSegment {
        StyledSegment {
            text: text.into(),
            fg: (255, 128, 0),
            bold,
            italic,
            underline,
        }
    }

    #[test]
    fn segments_to_lua_lines_modifier_flags_only_present_when_true() {
        let lua = Lua::new();
        let lines = vec![vec![
            seg_full("a", false, true, true),
            seg_full("b", false, false, false),
        ]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        let line: Table = result.get(1).unwrap();
        let s1: Table = line.get(1).unwrap();
        let st1: Table = s1.get(2).unwrap();
        assert!(st1.get::<bool>("italic").unwrap());
        assert!(st1.get::<bool>("underline").unwrap());
        let s2: Table = line.get(2).unwrap();
        let st2: Table = s2.get(2).unwrap();
        assert!(st2.get::<Option<bool>>("italic").unwrap().is_none());
        assert!(st2.get::<Option<bool>>("underline").unwrap().is_none());
    }

    #[test]
    fn segments_to_lua_lines_preserves_line_order() {
        let lua = Lua::new();
        let lines = vec![vec![seg("a", false)], vec![seg("b", false)]];
        let result = segments_to_lua_lines(&lua, &lines).unwrap();
        assert_eq!(result.len().unwrap(), 2);
        let l1: Table = result.get(1).unwrap();
        let l2: Table = result.get(2).unwrap();
        let s1: Table = l1.get(1).unwrap();
        let s2: Table = l2.get(1).unwrap();
        assert_eq!(s1.get::<String>(1).unwrap(), "a");
        assert_eq!(s2.get::<String>(1).unwrap(), "b");
    }

    #[test]
    fn markdown_table_has_border_and_data_spans() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "| col1 | col2 |\n|------|------|\n| a    | b    |");
        let mut saw_border = false;
        let mut saw_plain = false;
        for i in 1..=result.len().unwrap() {
            let line: Table = result.get(i).unwrap();
            for j in 1..=line.len().unwrap() {
                let span: Table = line.get(j).unwrap();
                if let mlua::Value::String(s) = span.get::<mlua::Value>(2).unwrap() {
                    let s = s.to_str().unwrap();
                    if s == STYLE_TABLE_BORDER {
                        saw_border = true;
                    } else if s == STYLE_PLAIN {
                        saw_plain = true;
                    }
                }
            }
        }
        assert!(saw_border, "table must have border spans");
        assert!(saw_plain, "table must have data/content spans");
    }

    #[test]
    fn markdown_large_input_does_not_panic() {
        let lua = Lua::new();
        let mut input = String::with_capacity(2048);
        for i in 0..200 {
            input.push_str(&format!(
                "# h{i}\n\npara **b{i}** *i{i}* `c{i}` ~~s{i}~~\n\n- item {i}\n\n"
            ));
        }
        assert!(input.len() >= 2000);
        let result = render_markdown(&lua, &input);
        assert!(result.len().unwrap() > 0);
    }

    #[test]
    fn markdown_code_inside_heading_keeps_inline_code_style() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# foo `bar`");
        let line: Table = result.get(1).unwrap();
        let bar_idx = (1..=line.len().unwrap())
            .find(|&i| span_text(&line, i as usize) == "bar")
            .expect("bar span");
        assert_eq!(span_style(&line, bar_idx as usize), STYLE_CODE);
        let foo_idx = (1..=line.len().unwrap())
            .find(|&i| span_text(&line, i as usize).contains("foo"))
            .expect("foo span");
        assert_eq!(span_style(&line, foo_idx as usize), STYLE_HEADING);
    }

    #[test_case(false, false, false, false, "" ; "default_emphasis_is_empty")]
    #[test_case(true, false, false, false, "bold" ; "bold_only")]
    #[test_case(false, true, false, false, "italic" ; "italic_only")]
    #[test_case(true, true, false, false, "bold_italic" ; "bold_and_italic")]
    #[test_case(false, false, true, false, "strikethrough" ; "strike_only")]
    #[test_case(true, false, true, false, "strikethrough" ; "strike_wins_over_bold")]
    #[test_case(false, true, true, false, "strikethrough" ; "strike_wins_over_italic")]
    #[test_case(false, false, false, true, "" ; "underline_alone_not_surfaced")]
    #[test_case(true, false, false, true, "bold" ; "underline_ignored_with_bold")]
    fn emphasis_style_name_combos(
        bold: bool,
        italic: bool,
        strike: bool,
        underline: bool,
        expected: &str,
    ) {
        let e = maki_markdown::Emphasis {
            bold,
            italic,
            strike,
            underline,
        };
        assert_eq!(emphasis_style_name(e), expected);
    }

    #[test]
    fn span_style_to_lua_highlight() {
        let lua = Lua::new();
        for (bold, italic, underline) in [(true, false, false), (true, true, true)] {
            let span = maki_markdown::render::Span {
                text: "tok".into(),
                style: maki_markdown::render::StyleToken::Highlight {
                    fg: (255, 128, 0),
                    bold,
                    italic,
                    underline,
                },
                emphasis: maki_markdown::Emphasis::default(),
            };
            let val = span_style_to_lua(&lua, &span).unwrap();
            let tbl = match val {
                mlua::Value::Table(t) => t,
                other => panic!("expected table, got {other:?}"),
            };
            assert_eq!(tbl.get::<String>("fg").unwrap(), ORANGE_HEX);
            assert_eq!(tbl.get::<bool>("bold").unwrap(), bold);
            assert_eq!(
                tbl.get::<Option<bool>>("italic").unwrap().unwrap_or(false),
                italic
            );
            assert_eq!(
                tbl.get::<Option<bool>>("underline")
                    .unwrap()
                    .unwrap_or(false),
                underline
            );
        }
    }

    #[test]
    fn markdown_mixed_document_routes_styles_per_block_kind() {
        let lua = Lua::new();
        let result = render_markdown(&lua, "# Title\n\nBody **bold** here.\n\n- item");
        assert!(result.len().unwrap() >= 5);

        let heading_line: Table = result.get(1).unwrap();
        for i in 1..=heading_line.len().unwrap() {
            assert_eq!(span_style(&heading_line, i as usize), STYLE_HEADING);
        }

        let body_line: Table = result.get(3).unwrap();
        let mut saw_bold = false;
        for i in 1..=body_line.len().unwrap() {
            if span_style(&body_line, i as usize) == STYLE_BOLD {
                assert_eq!(span_text(&body_line, i as usize), "bold");
                saw_bold = true;
            }
        }
        assert!(saw_bold, "body line should contain a bold span");

        let list_line: Table = result.get(5).unwrap();
        assert_eq!(span_style(&list_line, 1), STYLE_LIST_MARKER);
    }

    #[test]
    fn hint_store_set_and_clear() {
        let mut store = HintStore::new();
        store.set(Arc::from("plug"), vec![("a".into(), "b".into())]);
        assert_eq!(store.snapshot_entries().len(), 1);

        store.clear_plugin("plug");
        assert!(store.snapshot_entries().is_empty());
    }

    #[test]
    fn hint_store_deterministic_order() {
        let mut store = HintStore::new();
        store.set(Arc::from("zzz"), vec![("z".into(), "z".into())]);
        store.set(Arc::from("aaa"), vec![("a".into(), "a".into())]);
        let entries = store.snapshot_entries();
        assert_eq!(entries[0].0.as_ref(), "aaa");
        assert_eq!(entries[1].0.as_ref(), "zzz");
    }

    #[test]
    fn hint_store_empty_clears() {
        let mut store = HintStore::new();
        store.set(Arc::from("plug"), vec![("a".into(), "b".into())]);
        store.set(Arc::from("plug"), vec![]);
        assert!(store.snapshot_entries().is_empty());
    }
}
