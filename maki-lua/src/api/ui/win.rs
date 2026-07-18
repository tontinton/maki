use std::cell::Cell;
use std::time::Duration;

use maki_lua_macro::{lua_class, lua_fn};
use mlua::{AnyUserData, Lua, Result as LuaResult, Table};

use super::{parse_footer, try_parse_dimension};
use crate::api::util::command::{
    Anchor, Border, FloatConfigPatch, Split, TitlePos, WinCommand, WinEvent,
};
use crate::docs::{FnDoc, ParamDoc};

/// All mutable state is in `Cell`s so every Lua method takes a shared
/// borrow and `recv` never needs to re-borrow mutably after waking.
/// mlua's userdata lock is exclusive even for shared borrows, so `recv`
/// additionally must not hold any borrow across its await; see below.
pub(crate) struct WinHandle {
    event_rx: flume::Receiver<WinEvent>,
    cmd_tx: flume::Sender<WinCommand>,
    closed: Cell<bool>,
    visible: Cell<bool>,
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
            closed: Cell::new(false),
            visible: Cell::new(visible),
            init_width,
            init_height,
        }
    }

    fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        let _ = self.cmd_tx.try_send(WinCommand::Close);
    }

    fn send(&self, cmd: WinCommand) {
        if let Err(flume::TrySendError::Disconnected(_)) = self.cmd_tx.try_send(cmd) {
            self.closed.set(true);
        }
    }
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        self.close();
    }
}

fn tagged(lua: &Lua, ty: &str) -> LuaResult<Table> {
    let tbl = lua.create_table()?;
    tbl.set("type", ty)?;
    Ok(tbl)
}

fn event_table(lua: &Lua, event: WinEvent) -> LuaResult<Table> {
    match event {
        WinEvent::Key { key } => {
            let tbl = tagged(lua, "key")?;
            tbl.set("key", key)?;
            Ok(tbl)
        }
        WinEvent::Resize { width, height } => {
            let tbl = tagged(lua, "resize")?;
            tbl.set("width", width)?;
            tbl.set("height", height)?;
            Ok(tbl)
        }
        WinEvent::Paste { text } => {
            let tbl = tagged(lua, "paste")?;
            tbl.set("text", text)?;
            Ok(tbl)
        }
        WinEvent::Close => tagged(lua, "close"),
    }
}

#[allow(non_upper_case_globals)]
const recv__doc: FnDoc = FnDoc {
    name: "recv",
    args: "{timeout_ms?}",
    desc: "Waits for the next event from this window. Call this in a loop to \
        build an interactive UI. Returns nil once the window is closed or the \
        channel disconnects. Pass {timeout_ms} to also get `{type=\"timeout\"}` \
        events so your plugin can animate while idle.\n\n\
        Event tables by type:\n\
        - `{type=\"key\", key}` -- keypress. Key is a string like \"q\", \"j\", or \"esc\".\n\
        - `{type=\"resize\", width, height}` -- terminal was resized.\n\
        - `{type=\"paste\", text}` -- bracketed paste.\n\
        - `{type=\"close\"}` -- window was closed externally.\n\
        - `{type=\"timeout\"}` -- no event arrived within {timeout_ms}.",
    params: &[ParamDoc {
        name: "{timeout_ms?}",
        ty: "integer",
        desc: "Max milliseconds to wait before a timeout event is returned.",
    }],
    returns: "(table|nil) Event table, or nil if the window has closed.",
    example: "while true do\n  local ev = win:recv()\n  if not ev or ev.key == \"q\" then break end\n  if ev.type == \"key\" and ev.key == \"j\" then\n    -- move cursor down\n  end\nend\nwin:close()",
};

// recv() blocks until the next event; recv(timeout_ms) additionally
// resolves to `{ type = "timeout" }` so plugins can animate.
//
// Registered by hand, not as a `#[lua_fn]` async method: an async method's
// userdata borrow is held across the await, and mlua's lock rejects ALL
// other borrows meanwhile (shared ones included), so any win call from
// another coroutine would fail while a recv is parked, which is
// virtually always for an event-loop plugin. Only the cloned receiver is
// kept across the suspension.
fn win_extra<M: mlua::UserDataMethods<WinHandle>>(methods: &mut M) {
    methods.add_async_function(
        "recv",
        |lua, (ud, timeout_ms): (AnyUserData, Option<u64>)| async move {
            let rx = {
                let this = ud.borrow::<WinHandle>()?;
                if this.closed.get() {
                    return Ok(mlua::Value::Nil);
                }
                this.event_rx.clone()
            };
            let event = match timeout_ms {
                Some(ms) => {
                    let recv = async { Some(rx.recv_async().await) };
                    let timeout = async {
                        smol::Timer::after(Duration::from_millis(ms)).await;
                        None
                    };
                    match smol::future::or(recv, timeout).await {
                        Some(res) => res,
                        None => return Ok(mlua::Value::Table(tagged(&lua, "timeout")?)),
                    }
                }
                None => rx.recv_async().await,
            };
            match event {
                Ok(event) => {
                    if matches!(event, WinEvent::Close) {
                        ud.borrow::<WinHandle>()?.closed.set(true);
                    }
                    Ok(mlua::Value::Table(event_table(&lua, event)?))
                }
                Err(_) => {
                    ud.borrow::<WinHandle>()?.closed.set(true);
                    Ok(mlua::Value::Nil)
                }
            }
        },
    );
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
fn set_config(_lua: &Lua, this: &WinHandle, opts: Table) -> LuaResult<()> {
    if this.closed.get() {
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
fn set_cursor(_lua: &Lua, this: &WinHandle, row: usize) -> LuaResult<()> {
    if this.closed.get() {
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
fn close(_lua: &Lua, this: &WinHandle) -> LuaResult<()> {
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
fn is_open(_lua: &Lua, this: &WinHandle) -> LuaResult<bool> {
    if !this.closed.get() && this.cmd_tx.is_disconnected() {
        this.closed.set(true);
    }
    Ok(!this.closed.get())
}

/// Makes the window visible again after it was hidden with `hide()`.
///
/// @return
/// @example
/// win:show()
#[lua_fn]
fn show(_lua: &Lua, this: &WinHandle) -> LuaResult<()> {
    if this.closed.get() {
        return Ok(());
    }
    this.visible.set(true);
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
fn hide(_lua: &Lua, this: &WinHandle) -> LuaResult<()> {
    if this.closed.get() {
        return Ok(());
    }
    this.visible.set(false);
    this.send(WinCommand::SetVisible(false));
    Ok(())
}

/// Returns true if the window is both open and visible (not hidden).
///
/// @return (boolean) true if visible.
#[lua_fn]
fn is_visible(_lua: &Lua, this: &WinHandle) -> LuaResult<bool> {
    if !this.closed.get() && this.cmd_tx.is_disconnected() {
        this.closed.set(true);
    }
    Ok(this.visible.get() && !this.closed.get())
}

fn win_fields<F: mlua::UserDataFields<WinHandle>>(fields: &mut F) {
    fields.add_field_method_get("width", |_, this| Ok(this.init_width));
    fields.add_field_method_get("height", |_, this| Ok(this.init_height));
    fields.add_field_method_get("visible", |_, this| Ok(this.visible.get()));
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
    "maki.ui.Win" => WinHandle, DOCS [manual recv, set_config, set_cursor, close, is_open, show, hide, is_visible] fields win_fields, extra win_extra
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
        let (_event_tx, cmd_rx, handle) = make_channels();
        handle.close();
        assert!(handle.closed.get());
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
        let (_event_tx, cmd_rx, handle) = make_channels();
        handle.close();
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::Close)));
        drop(handle);
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn close_does_not_panic_when_receiver_dropped() {
        let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
        let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);
        let handle = WinHandle::new(event_rx, cmd_tx, 80, 24, true);
        drop(cmd_rx);
        handle.close();
        assert!(handle.closed.get());
        drop(event_tx);
    }

    #[test]
    fn send_detects_disconnect() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        drop(cmd_rx);
        assert!(!handle.closed.get());
        handle.send(WinCommand::SetVisible(true));
        assert!(handle.closed.get());
    }

    #[test]
    fn recv_timeout_returns_timeout_event() {
        let lua = mlua::Lua::new();
        let (_event_tx, _cmd_rx, handle) = make_channels();
        lua.globals().set("win", handle).unwrap();
        let ty: String = smol::block_on(lua.load("return win:recv(5).type").eval_async()).unwrap();
        assert_eq!(ty, "timeout");
    }

    #[test]
    fn recv_timeout_delivers_pending_event() {
        let lua = mlua::Lua::new();
        let (event_tx, _cmd_rx, handle) = make_channels();
        event_tx
            .try_send(WinEvent::Key {
                key: "enter".into(),
            })
            .unwrap();
        lua.globals().set("win", handle).unwrap();
        let got: String = smol::block_on(
            lua.load("local ev = win:recv(1000) return ev.type .. ':' .. ev.key")
                .eval_async(),
        )
        .unwrap();
        assert_eq!(got, "key:enter");
    }

    #[test]
    fn win_methods_work_while_recv_is_parked() {
        let lua = mlua::Lua::new();
        let (event_tx, cmd_rx, handle) = make_channels();
        lua.globals().set("win", handle).unwrap();
        let ex = smol::LocalExecutor::new();
        let recv_task = ex.spawn(
            lua.load("return win:recv(5000).type")
                .eval_async::<String>(),
        );
        smol::block_on(ex.run(async {
            for _ in 0..10 {
                smol::future::yield_now().await;
            }
            lua.load("win:set_cursor(3)").exec_async().await.unwrap();
            event_tx
                .send_async(WinEvent::Key { key: "x".into() })
                .await
                .unwrap();
            assert_eq!(recv_task.await.unwrap(), "key");
        }));
        assert!(matches!(cmd_rx.try_recv(), Ok(WinCommand::SetCursor(2))));
    }

    #[test]
    fn is_disconnected_marks_closed() {
        let (_event_tx, cmd_rx, handle) = make_channels();
        drop(cmd_rx);
        assert!(!handle.closed.get());
        assert!(handle.cmd_tx.is_disconnected());
    }
}
