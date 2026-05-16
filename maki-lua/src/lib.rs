mod api;
mod error;
pub mod language;
mod loader;
mod runtime;

pub use api::command::{
    Anchor, Border, Dimension, FloatConfig, FloatConfigPatch, LuaCommandInfo, LuaCommandReader,
    TitlePos, UiAction, WinCommand, WinEvent,
};
pub use error::PluginError;
pub use loader::{EventHandle, PluginHost};
pub use runtime::RestoreReply;

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
