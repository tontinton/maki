use std::path::PathBuf;
use std::time::Duration;

use humantime::format_duration;
use mlua::{Lua, Result as LuaResult, Table};

use crate::api::command::{SelectEvent, SelectItem, SelectOpts, UiAction};
use crate::runtime::with_task_bufs;

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
            lua.create_function(move |_, path: String| {
                let _ = editor_tx.try_send(UiAction::OpenEditor(PathBuf::from(path)));
                Ok(())
            })?,
        )?;

        let select_tx = tx;
        t.set(
            "select",
            lua.create_async_function(move |lua, (items_tbl, opts_tbl): (Table, Table)| {
                let tx = select_tx.clone();
                async move {
                    let format_fn: Option<mlua::Function> = opts_tbl.get("format").ok();

                    let mut items = Vec::new();
                    for pair in items_tbl.sequence_values::<mlua::Value>() {
                        let val = pair?;
                        let (label, detail) = if let Some(ref fmt) = format_fn {
                            let tbl: Table = fmt.call(val.clone())?;
                            let label: String = tbl.get("label")?;
                            let detail: Option<String> = tbl.get("detail").ok();
                            (label, detail)
                        } else {
                            let s = match &val {
                                mlua::Value::String(s) => s.to_str()?.to_owned(),
                                _ => {
                                    return Err(mlua::Error::runtime(
                                        "select: items must be strings or use opts.format",
                                    ));
                                }
                            };
                            (s, None)
                        };
                        items.push(SelectItem { label, detail });
                    }

                    let title: String = opts_tbl.get("title").unwrap_or_default();
                    let has_on_delete: bool = opts_tbl.get("on_delete").unwrap_or(false);

                    let footer: Vec<(String, String)> =
                        if let Ok(footer_tbl) = opts_tbl.get::<Table>("footer") {
                            let mut pairs = Vec::new();
                            for entry in footer_tbl.sequence_values::<Table>() {
                                let entry = entry?;
                                let key: String = entry.get(1)?;
                                let desc: String = entry.get(2)?;
                                pairs.push((key, desc));
                            }
                            pairs
                        } else {
                            Vec::new()
                        };

                    let (reply_tx, reply_rx) = flume::bounded::<SelectEvent>(1);
                    if tx
                        .try_send(UiAction::Select {
                            items,
                            opts: SelectOpts {
                                title,
                                has_on_delete,
                                footer,
                            },
                            reply_tx,
                        })
                        .is_err()
                    {
                        return Ok(mlua::Value::Nil);
                    }

                    let event = match reply_rx.recv_async().await {
                        Ok(e) => e,
                        Err(_) => return Ok(mlua::Value::Nil),
                    };

                    let result = lua.create_table()?;
                    match event {
                        SelectEvent::Choice { index } => {
                            result.set("type", "choice")?;
                            result.set("index", index + 1)?;
                        }
                        SelectEvent::Delete { index } => {
                            result.set("type", "delete")?;
                            result.set("index", index + 1)?;
                        }
                        SelectEvent::OpenEditor { index } => {
                            result.set("type", "open_editor")?;
                            result.set("index", index + 1)?;
                        }
                        SelectEvent::Close => {
                            result.set("type", "close")?;
                        }
                    }
                    Ok(mlua::Value::Table(result))
                }
            })?,
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
