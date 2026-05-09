use mlua::{Table, UserData, UserDataMethods};

use crate::api::command::{WinCommand, WinEvent};
use crate::api::ui::parse_footer;

pub(crate) struct WinHandle {
    event_rx: flume::Receiver<WinEvent>,
    cmd_tx: flume::Sender<WinCommand>,
    closed: bool,
}

impl WinHandle {
    pub fn new(event_rx: flume::Receiver<WinEvent>, cmd_tx: flume::Sender<WinCommand>) -> Self {
        Self {
            event_rx,
            cmd_tx,
            closed: false,
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
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_async_method_mut("recv", |lua, mut this, ()| async move {
            if this.closed {
                return Ok(mlua::Value::Nil);
            }
            match this.event_rx.recv_async().await {
                Ok(WinEvent::Key { key, cursor }) => {
                    let tbl = lua.create_table()?;
                    tbl.set("type", "key")?;
                    tbl.set("key", key)?;
                    tbl.set("cursor", cursor + 1)?;
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
            let title: Option<String> = opts.get("title").ok();
            let footer = match parse_footer(&opts) {
                Ok(f) if !f.is_empty() => Some(f),
                _ => None,
            };
            let _ = this
                .cmd_tx
                .try_send(WinCommand::SetConfig { title, footer });
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
        let handle = WinHandle::new(event_rx, cmd_tx);
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
}
