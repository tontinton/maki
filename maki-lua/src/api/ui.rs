use std::path::PathBuf;
use std::time::Duration;

use humantime::format_duration;
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::command::{UiAction, WinCommand, WinEvent, WinOpts};
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
                    let cursor_line: bool = opts_tbl.get("cursor_line").unwrap_or(true);
                    let footer = parse_footer(&opts_tbl)?;
                    let reserved_bottom: usize = opts_tbl.get("reserved_bottom").unwrap_or(0);

                    let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
                    let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);

                    let _ = open_win_tx.try_send(UiAction::OpenWin {
                        buf: buf_handle.buf.clone(),
                        opts: WinOpts {
                            title,
                            footer,
                            cursor_line,
                            reserved_bottom,
                        },
                        event_tx,
                        cmd_rx,
                    });

                    Ok(WinHandle::new(event_rx, cmd_tx))
                },
            )?,
        )?;
    }

    Ok(t)
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
