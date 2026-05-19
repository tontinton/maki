use std::path::PathBuf;
use std::time::Duration;

use humantime::format_duration;
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::command::{
    Anchor, Border, Dimension, FloatConfig, TitlePos, UiAction, WinCommand, WinEvent,
};
use crate::api::win::WinHandle;
use crate::runtime::with_task_bufs;

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
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    t.set(
        "buf",
        lua.create_function(|lua, ()| {
            with_task_bufs(lua, |store| store.create_live())
                .ok_or_else(|| mlua::Error::runtime("buffer store not initialized"))
        })?,
    )?;
    t.set(
        "highlight",
        lua.create_async_function(|lua, (code, lang): (String, String)| async move {
            let segments =
                smol::unblock(move || maki_highlight::highlight_code(&lang, &code)).await;
            segments_to_lua_lines(&lua, &segments)
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
                    let buf_handle = buf_ud.borrow::<crate::api::buf::BufHandle>()?;
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
                    };

                    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
                    let border_chrome = match config.border {
                        Border::None => 0,
                        _ => 2,
                    };
                    let footer_h = u16::from(!config.footer.is_empty());
                    let est_w = config
                        .width
                        .resolve(term_cols)
                        .saturating_sub(border_chrome);
                    let est_h = config
                        .height
                        .resolve(term_rows)
                        .saturating_sub(border_chrome + footer_h);

                    let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
                    let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);

                    let _ = open_win_tx.try_send(UiAction::OpenWin {
                        buf: buf_handle.buf.clone(),
                        config,
                        focus,
                        event_tx,
                        cmd_rx,
                    });

                    Ok(WinHandle::new(event_rx, cmd_tx, est_w, est_h))
                },
            )?,
        )?;
    }

    Ok(t)
}

pub(crate) fn try_parse_dimension(tbl: &Table, key: &str) -> Option<Dimension> {
    if let Ok(s) = tbl.get::<String>(key) {
        if let Some(pct) = s.strip_suffix('%') {
            if let Ok(v) = pct.parse::<u16>() {
                return Some(Dimension::Percent(v));
            }
        }
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
}
