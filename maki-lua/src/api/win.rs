use mlua::{Table, UserData, UserDataMethods};

use crate::api::command::{Anchor, Border, FloatConfigPatch, TitlePos, WinCommand, WinEvent};
use crate::api::ui::{parse_footer, try_parse_dimension};

pub(crate) struct WinHandle {
    event_rx: flume::Receiver<WinEvent>,
    cmd_tx: flume::Sender<WinCommand>,
    closed: bool,
    init_width: u16,
    init_height: u16,
}

impl WinHandle {
    pub fn new(
        event_rx: flume::Receiver<WinEvent>,
        cmd_tx: flume::Sender<WinCommand>,
        init_width: u16,
        init_height: u16,
    ) -> Self {
        Self {
            event_rx,
            cmd_tx,
            closed: false,
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
}

impl Drop for WinHandle {
    fn drop(&mut self) {
        self.close();
    }
}

impl UserData for WinHandle {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("width", |_, this| Ok(this.init_width));
        fields.add_field_method_get("height", |_, this| Ok(this.init_height));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method_mut("recv", |lua, mut this, ()| async move {
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
        });

        methods.add_method_mut("set_config", |_, this, opts: Table| {
            if this.closed {
                return Ok(());
            }
            let mut patch = FloatConfigPatch::default();
            if let Ok(t) = opts.get::<String>("title") {
                patch.title = Some(t);
            }
            if let Ok(f) = parse_footer(&opts) {
                if !f.is_empty() {
                    patch.footer = Some(f);
                }
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
            patch.width = try_parse_dimension(&opts, "width");
            patch.height = try_parse_dimension(&opts, "height");
            let _ = this.cmd_tx.try_send(WinCommand::SetConfig(patch));
            Ok(())
        });

        methods.add_method_mut("set_cursor", |_, this, row: usize| {
            if this.closed {
                return Ok(());
            }
            let _ = this
                .cmd_tx
                .try_send(WinCommand::SetCursor(row.saturating_sub(1)));
            Ok(())
        });

        methods.add_method_mut("close", |_, this, ()| {
            this.close();
            Ok(())
        });

        methods.add_method("is_open", |_, this, ()| Ok(!this.closed));
    }
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
        let handle = WinHandle::new(event_rx, cmd_tx, 80, 24);
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
        let mut handle = WinHandle::new(event_rx, cmd_tx, 80, 24);
        drop(cmd_rx);
        handle.close();
        assert!(handle.closed);
        drop(event_tx);
    }
}
