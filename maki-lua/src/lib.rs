mod api;
mod error;
pub mod language;
mod loader;
pub(crate) mod plugin_permissions;
mod runtime;

pub use api::command::{
    Anchor, Axis, Border, Dimension, Edge, FloatConfig, FloatConfigPatch, HintReader, HintSnapshot,
    LuaCommandInfo, LuaCommandReader, Split, TitlePos, UiAction, WinCommand, WinEvent,
};
pub use api::keymap::{KeymapEntry, KeymapReader, KeymapSnapshot};
pub use error::PluginError;
pub use loader::{EventHandle, PluginHost};
pub use plugin_permissions::{Permission, PluginPermissions};
pub use runtime::{ClickReply, RestoreItem};

pub mod test_support {
    use crate::api::command::{LuaCommandInfo, LuaCommandReader, LuaCommandWriter};

    pub struct LuaCommandWriterHandle(LuaCommandWriter);

    impl LuaCommandWriterHandle {
        pub fn publish(&self, commands: Vec<LuaCommandInfo>) {
            self.0.publish(commands);
        }
    }

    pub fn lua_command_writer_pair() -> (LuaCommandWriterHandle, LuaCommandReader) {
        let (writer, reader) = LuaCommandWriter::new();
        (LuaCommandWriterHandle(writer), reader)
    }
}
