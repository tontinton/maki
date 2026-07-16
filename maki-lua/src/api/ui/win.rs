use maki_lua_macro::{lua_class, lua_fn};
use mlua::{Lua, Result as LuaResult, Table};

use super::{parse_footer, try_parse_dimension};
use crate::api::util::command::{
    Anchor, Border, FloatConfigPatch, Split, TitlePos, WinCommand, WinEvent,
};

pub(crate) struct WinHandle {
    event_rx: flume::Receiver<WinEvent>,
    cmd_tx: flume::Sender<WinCommand>,
    closed: bool,
    visible: bool,
    init_width: u16,
    init_height: u16,
}

impl WinHandle {
    pub fn new(
        event_rx: flume::Receiver<WinEvent>,
        cmd_tx: flume::Sender<WinCommand>,
        init_width: u16,
        init_height: u16,
        visible: bool,
    ) -> Self {
        Self {
            event_rx,
            cmd_tx,
            closed: false,
            visible,
            init_width,
            init_height,
        }
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = self.cmd_tx.try_send(WinCommand::Close);
    }

    fn send(&mut self, cmd: WinCommand) {
        if let Err(flume::TrySendError::Disconnected(_)) = self.cmd_tx.try_send(cmd) {
            self.closed = true;
        }
    }
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// Waits for the next event from this window. Call this in a loop to
/// build an interactive UI. Returns nil once the window is closed or
/// the channel disconnects.
///
/// Event tables by type:
/// - `{type="key", key}` -- keypress. Key is a string like "q", "j", or "<Esc>".
/// - `{type="resize", width, height}` -- terminal was resized.
/// - `{type="paste", text}` -- bracketed paste.
/// - `{type="close"}` -- window was closed externally.
///
/// @return (table|nil) Event table, or nil if the window has closed.
/// @example
/// while true do
///   local ev = win:recv()
///   if not ev or ev.key == "q" then break end
///   if ev.type == "key" and ev.key == "j" then
///     -- move cursor down
///   end
/// end
/// win:close()
#[lua_fn]
async fn recv(lua: Lua, mut this: mlua::UserDataRefMut<WinHandle>) -> LuaResult<mlua::Value> {
    if this.closed {
        return Ok(mlua::Value::Nil);
    }
    match this.event_rx.recv_async().await {
        Ok(WinEvent::Key { key }) => {
            let tbl = lua.create_table()?;
            tbl.set("type", "key")?;
            tbl.set("key", key)?;
            Ok(mlua::Value::Table(tbl))
        }
        Ok(WinEvent::Resize { width, height }) => {
            let tbl = lua.create_table()?;
            tbl.set("type", "resize")?;
            tbl.set("width", width)?;
            tbl.set("height", height)?;
            Ok(mlua::Value::Table(tbl))
        }
        Ok(WinEvent::Paste { text }) => {
            let tbl = lua.create_table()?;
            tbl.set("type", "paste")?;
            tbl.set("text", text)?;
            Ok(mlua::Value::Table(tbl))
        }
        Ok(WinEvent::Close) => {
            this.closed = true;
            let tbl = lua.create_table()?;
            tbl.set("type", "close")?;
            Ok(mlua::Value::Table(tbl))
        }
        Err(_) => {
            this.closed = true;
            Ok(mlua::Value::Nil)
        }
    }
}

/// Updates the window layout on the fly. Only the fields you include in
/// {opts} are changed, everything else stays the same.
///
/// @param opts table Partial float config. Accepted fields:
///   - title (string): border title text.
///   - title_pos (string): title alignment, "left", "center", or "right".
///   - footer (table): key-hint pairs `{{key, label}, ...}` shown in the bottom border.
///   - border (string): "rounded", "single", "double", or "none".
///   - anchor (string): corner origin, "NW", "NE", "SW", or "SE".
///   - width (integer|string): new width; integer or "N%".
///   - height (integer|string): new height; integer or "N%".
///   - zindex (integer): stacking order.
///   - cursor_line (boolean): highlight the focused row.
///   - reserved_top (integer): rows reserved at the top of the content area.
///   - split (string): edge docking, "above", "below", "left", "right", "panel", or "".
///   - order (integer): paint order among split windows.
/// @return
/// @example
/// win:set_config({ title = "Updated!", width = "80%" })
#[lua_fn]
fn set_config(_lua: &Lua, this: &mut WinHandle, opts: Table) -> LuaResult<()> {
    if this.closed {
        return Ok(());
    }
    let mut patch = FloatConfigPatch::default();
    if let Ok(t) = opts.get::<String>("title") {
        patch.title = Some(t);
    }
    if let Ok(f) = parse_footer(&opts)
        && !f.is_empty()
    {
        patch.footer = Some(f);
    }
    if let Ok(b) = opts.get::<String>("border") {
        patch.border = Some(Border::parse(&b));
    }
    if let Ok(tp) = opts.get::<String>("title_pos") {
        patch.title_pos = Some(TitlePos::parse(&tp));
    }
    if let Ok(a) = opts.get::<String>("anchor") {
        patch.anchor = Some(Anchor::parse(&a));
    }
    if let Ok(z) = opts.get::<u16>("zindex") {
        patch.zindex = Some(z);
    }
    if let Ok(cl) = opts.get::<bool>("cursor_line") {
        patch.cursor_line = Some(cl);
    }
    if let Ok(rt) = opts.get::<usize>("reserved_top") {
        patch.reserved_top = Some(rt);
    }
    if let Ok(s) = opts.get::<String>("split") {
        patch.split = Some(Split::parse(&s));
    }
    if let Ok(o) = opts.get::<u16>("order") {
        patch.order = Some(o);
    }
    patch.width = try_parse_dimension(&opts, "width");
    patch.height = try_parse_dimension(&opts, "height");
    this.send(WinCommand::SetConfig(patch));
    Ok(())
}

/// Moves the highlighted cursor line to {row} (1-indexed). Only has a
/// visible effect when the window was opened with `cursor_line = true`.
///
/// @param row integer Target row, 1-indexed.
/// @return
/// @example
/// win:set_cursor(3) -- highlight the third line
#[lua_fn]
fn set_cursor(_lua: &Lua, this: &mut WinHandle, row: usize) -> LuaResult<()> {
    if this.closed {
        return Ok(());
    }
    this.send(WinCommand::SetCursor(row.saturating_sub(1)));
    Ok(())
}

/// Closes the window and frees its resources. Safe to call more than
/// once. The window also closes automatically when the handle is
/// garbage collected.
///
/// @return
/// @example
/// win:close()
#[lua_fn]
fn close(_lua: &Lua, this: &mut WinHandle) -> LuaResult<()> {
    this.close();
    Ok(())
}

/// Returns true if the window is still alive (not closed). Useful for
/// checking before sending commands.
///
/// @return (boolean) true if open.
/// @example
/// if win:is_open() then
///   win:set_config({ title = "still here" })
/// end
#[lua_fn]
fn is_open(_lua: &Lua, this: &mut WinHandle) -> LuaResult<bool> {
    if !this.closed && this.cmd_tx.is_disconnected() {
        this.closed = true;
    }
    Ok(!this.closed)
}

/// Makes the window visible again after it was hidden with `hide()`.
///
/// @return
/// @example
/// win:show()
#[lua_fn]
fn show(_lua: &Lua, this: &mut WinHandle) -> LuaResult<()> {
    if this.closed {
        return Ok(());
    }
    this.visible = true;
    this.send(WinCommand::SetVisible(true));
    Ok(())
}

/// Hides the window without closing it. The window keeps its state
/// and buffer contents. Call `show()` to bring it back.
///
/// @return
/// @example
/// win:hide()
/// -- do some work...
/// win:show()
#[lua_fn]
fn hide(_lua: &Lua, this: &mut WinHandle) -> LuaResult<()> {
    if this.closed {
        return Ok(());
    }
    this.visible = false;
    this.send(WinCommand::SetVisible(false));
    Ok(())
}

/// Returns true if the window is both open and visible (not hidden).
///
/// @return (boolean) true if visible.
#[lua_fn]
fn is_visible(_lua: &Lua, this: &mut WinHandle) -> LuaResult<bool> {
    if !this.closed && this.cmd_tx.is_disconnected() {
        this.closed = true;
    }
    Ok(this.visible && !this.closed)
}

fn win_fields<F: mlua::UserDataFields<WinHandle>>(fields: &mut F) {
    fields.add_field_method_get("width", |_, this| Ok(this.init_width));
    fields.add_field_method_get("height", |_, this| Ok(this.init_height));
    fields.add_field_method_get("visible", |_, this| Ok(this.visible));
}

lua_class! {
    /// Handle to a floating or split window. You get one from
    /// `maki.ui.open_win()`. Use `recv()` in a loop to handle keyboard
    /// input, and call `close()` when done.
    ///
    /// Fields: `width`, `height` (initial content dimensions in columns/rows),
    /// `visible` (current visibility).
    ///
    /// ```lua
    /// local win = maki.ui.open_win(buf, { title = "Demo" })
    /// while true do
    ///   local ev = win:recv()
    ///   if not ev or ev.key == "q" then break end
    /// end
    /// win:close()
    /// ```
    "maki.ui.Win" => WinHandle, DOCS [recv, set_config, set_cursor, close, is_open, show, hide, is_visible] fields win_fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channels() -> (
        flume::Sender<WinEvent>,
        flume::Receiver<WinCommand>,
        WinHandle,
    ) {
        let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
        let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);
        let handle = WinHandle::new(event_rx, cmd_tx, 80, 24, true);
        (event_tx, cmd_rx, handle)
    }

    #[test]
    fn close_is_idempotent_including_drop() {
        let (_event_tx, cmd_rx, mut handle) = make_channels();
        handle.close();
        assert!(handle.closed);
        handle.close();
        drop(handle);
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::Close)));
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn drop_auto_closes() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        drop(handle);
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::Close)));
    }

    #[test]
    fn drop_after_close_does_not_resend() {
        let (_event_tx, cmd_rx, mut handle) = make_channels();
        handle.close();
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::Close)));
        drop(handle);
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn close_does_not_panic_when_receiver_dropped() {
        let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
        let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);
        let mut handle = WinHandle::new(event_rx, cmd_tx, 80, 24, true);
        drop(cmd_rx);
        handle.close();
        assert!(handle.closed);
        drop(event_tx);
    }

    #[test]
    fn send_detects_disconnect() {
        let (_event_tx, cmd_rx, mut handle) = make_channels();
        drop(cmd_rx);
        assert!(!handle.closed);
        handle.send(WinCommand::SetVisible(true));
        assert!(handle.closed);
    }

    #[test]
    fn is_disconnected_marks_closed() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        drop(cmd_rx);
        assert!(!handle.closed);
        assert!(handle.cmd_tx.is_disconnected());
    }
}
